# Architecture

## Principle

> The model proposes; the engine decides what is authorised, executes, observes and verifies.

The runtime is the agent. The decision model, GLiNER and the worker model are services it calls for *bounded* questions.
No model owns control flow, permissions, or the claim that a task is done.

## Pipeline (`crates/dfae-runtime/src/pipeline.rs`)

1. **Extract** — GLiNER (semantic spans, dynamic labels) + rules (URLs, paths, versions, arithmetic,
   constraints). Structured rules win overlaps; semantic spans fill the rest; a *constraint* may wrap other
   entities. If the GLiNER service is down or still loading, rules alone are used (`rules(fallback)`).
   Output is a list of **candidates** — nothing extracted is trusted for permissions or stored as fact
   without corroboration (see *Learning*).
2. **Retrieve** — episodic + semantic memory by similarity, re-ranked
   `0.55·similarity + 0.15·recency + 0.15·importance + 0.15·entity-overlap`, de-duplicated; plus the last
   turns of the same session.
3. **Route** (`route_task`) — task type, complexity, risk, needs-tools.
4. **Deterministic candidates** — `bind_direct` (regex-bound tool calls) and `find_skill` (procedural memory).
5. **Mode** (`select_mode`) — see *Mode selection*.
6. **Plan** — Direct: from bindings or the skill. Otherwise `select_tools` narrows the tool set and a model
   writes a JSON plan; it is parsed, **validated** (schema, known tools, only the selected tools, argument
   types, DAG acyclicity, step/tool-call budgets), repaired once with the exact problems listed, and — if the
   worker model still fails and a planner model exists — re-planned there.
   With a planner model the plan is requested through a strict JSON Schema (tools as an enum), read leniently, repaired by
   code, and sent back to the planner with typed issues only when code cannot fix it; the planner sees the whole tool
   catalogue (the decision model's tool choice only serves quick tasks and the worker-plans fallback).
   **MCP.** After routing, a request about an outside service attaches MCP servers to the task (`dfae-mcp`: connected ones that fit,
   else the best of what the Glama directory finds for a short worker-written query); their tools are registered as `mcp__<server>__<tool>`
   and visible to that task only (`TaskCtx::in_scope`).
7. **Execute** — each brief step is bound just before it runs: rules and the extractor first, then the worker model
   (asked only for the parameters the step needs; file content is written as plain text, not squeezed into JSON), then the
   planner model. A bound `cwd` must exist; a background command that dies within 1.5 s is a failed step with its output.
   DAG waves with bounded parallelism (`DFAE_MAX_PARALLEL`).
8. **Verify** — evidence-based; one continuation round if the completion judgment is low.
9. **Answer** — templated (no model) unless reasoning steps produced material to compose; model-written
   answers are checked for ungrounded numbers/paths/URLs.
10. **Learn** — calibration, episodic memory, validated fact promotion, skill crystallisation.

## What decides, and what only enforces

Judgement is the decision model's: every question above is a typed question over `DecisionState` (the request, the entities, the recent turns,
`known` things from the session, mode/plan/step/completion facts and the policy). Code does three things only: it supplies *facts* (a tool
binds or it does not; a planner is configured or it is not; a step's side effects), it *enforces limits* (policy, budgets, the schema
of every answer), and it does *mechanical work* the answer points at (substituting the file a request referred to, canned replies by
small-talk kind). The sentence is never re-read by the runtime to second-guess an answer. The local fallback decider (`local.rs`) is the only
place with phrase lists and regexes, and it answers exactly the same questions.

## Planning and acting (`plandoc.rs`, `planner.rs`, `implement.rs`, `executor.rs`)

Two hosted models and a worker system. The **planner model** writes and revises plans. The **worker system** carries them out: rules and
the tools' own patterns, the extractor (GLiNER), the self-hosted decision model, and the **worker model** (hosted, cheaper), under the
program's control. The decision model chooses the handling (`select_mode`): `direct` (binds to tools outright), `assisted` = *quick* (one
action or a reply the worker system handles alone) or `escalated` = *needs a plan*. Code only enforces limits: `escalated` without a planner
falls back to the worker model; a low-confidence `assisted` is escalated; a user's explicit mode wins. There is no complexity rule on top
(`DFAE_PLAN_MIN_COMPLEXITY` parameterises the built-in rules that stand in when the decision model is down). With a planner configured **all
planning is its**: a quick request becomes one brief step (`PlanSource::Quick`: the best-fitting tool the decision model picked, or none for a
reply) that the acting phase binds and runs; if it fails, the re-plan is the planner's; an unreachable planner fails the task unless
`DFAE_PLAN_FALLBACK=worker`. Conversation is answered by the worker system.

* **The typed plan** (`plandoc.rs`): one JSON object — `{"steps":[{"id","tool","do","after","expect","planner"}], "report"?}` — whose JSON Schema
  (`plan_schema`, strict: every property required, none extra) lists the tools that exist as an `enum`, so a provider with structured output cannot
  answer outside it. `PlanDoc::from_value` reads any reply leniently into typed `StepDoc`s and repairs in code what it can — tool spellings
  (`FS-Write`, `functions.fs_write`, a one-letter slip, only when unambiguous), a list under another key, numeric or duplicate ids, dependencies on
  steps that do not exist, a step with a tool and arguments but no sentence — recording each repair as a `repaired` event; anything else is a typed
  issue ("step s2: there is no tool \"teleport\" (tools: …)") that the planner gets in one repair round (`try_plan`). Planner-supplied `args` are
  kept as hints. `parse_brief_plan` then turns the typed document into `Step`s (every step `brief`), `finish_plan` orders and validates them.
* **Acting phase** (event `phase: acting`): `run_step` calls `Inner::implement_step` for a brief step, which decides the action just in time:
  entities (GLiNER + rules) from the step sentence → kind (the planner's tool; if none, a tool only when the sentence binds outright via
  `bind_direct`) → arguments: planner hints, `bind_direct` on the sentence, entities that are *unambiguous* for a parameter (two candidate
  paths are never guessed) → a file's **content** is written by the worker model as plain text (`write_content`: loops and cut-off files are
  detected and rewritten) → remaining arguments by the ladder worker model (JSON schema), worker model ×3 as a vote, planner, with the validation
  error fed back each time → validation (schema, `{{id}}` only from this step's dependencies, never in a path). The result is written back into
  the plan (`tool`, `args`, `how`) and the normal machinery runs it: policy, contracts, repair, retries. A reasoning step is written by the worker
  model unless `planner`.
* **Judging**: if the planner gave `expect`, the parts code can read are checked by code; the decision model judges the rest. A miss is not a
  failure of the step: it is a *checkpoint* that sends the planner back to the plan with the actual result in front of it.
* **Patch re-planning** (`patch_plan`): when recovery decides to re-plan, the planner gets the state of the plan (`s1 DONE …`, `s2 FAILED …`,
  `s3 TODO …`) and the problem, and answers with a typed patch (`patch_schema`): `{"edit":[…],"add":[…],"drop":[…]}`. `PatchDoc::from_value`
  repairs it like a plan (and reads a whole plan in its place as additions); `planner::apply_patch` applies it: finished steps are never touched, an
  edited step runs again from a clean state, new steps are numbered `r{n}_{i}`, nothing may wait for a dropped step, and a failed step that is
  neither edited nor dropped is an issue sent back for one repair round. Without a planner the worker model re-plans in concrete steps as before.
* **Fault tolerance**: argument problems are repaired in code first; a worker call that fails because the provider is unreachable is made by the
  planner instead (`generate_text`); a malformed written file, a reasoning step that misses its expectation, and a reply that only asks the user a
  question are redone locally with the reason; only then does the planner hear of it.
* **Reviewing** (event `phase: reviewing`): verification and, if the evidence says the request is not accomplished, one continuation plan.

### The cost of planning (`prompts.rs`, `pipeline.rs::brief_request`, `economy/prefix.rs`)

* **Cacheable prefix**: when the provider caches prompts and traffic keeps the cache warm, the planner's system prompt is rules + the *whole* tool
  catalogue (sorted, with parameter names) + worked examples — byte-identical from task to task and shared by planning and re-planning (what a
  re-plan needs is in its user message). The per-task part is a shortlist (`LIKELY TOOLS:`), the context and the workspace line. Otherwise the
  narrow per-task tool list is sent. `PrefixPolicy` decides (a prefixed call in the last 4½ minutes, or three planner calls in ten; never after
  three prefixed calls in a row without a cache hit).
* **Answers**: a reasoning leaf is the answer; otherwise the *worker model* writes it from the evidence and `ungrounded_claims` checks that every
  number, path, URL and code span in it occurs in the evidence — only a failed check is escalated to the planner. A plan of tool steps only is
  answered deterministically (one line per step that changed something). `DFAE_PLAN_REPORT=on` additionally lets the planner pre-write the reply.
* **Worker economy**: both hosted models are billed, so the worker's ordinary calls are lean — compact signatures, trimmed inputs, a small output
  limit (`WORKER_MAX_TOKENS`: binding, short reasoning, answers) — and only writing a file's content (`write_content`, or a reasoning step whose
  sentence says it writes something) may run long (`WORKER_CONTENT_MAX_TOKENS`).
* **What the planner is told**: a one-line workspace snapshot (two levels, capped), the condensed form of big outputs, a lesson from an earlier
  similar task, and for a re-plan the state of the plan and the step's actual result.

### Checking what was done (`planner::expect_checks`, `syntax.rs`, `implement.rs::judge_step`)

`expect` is natural language for the decision model, but the parts code can read are checked by code first (`FileExists`, `FileContains`,
`Contains`, exit status). If every clause was understood the checks *are* the verdict; otherwise the decision model still judges. After a
write, the file is parsed by type; a malformed file is rewritten by the local model with the parser's complaint (`implement_step(…, feedback)`),
and only if that fails does recovery (retry, repair, re-plan) take over. A local reasoning step that misses its expectation is rewritten once
with the reason before the checkpoint goes to the planner.

### Learning (`pipeline.rs::{learn, apply_feedback, shadow_evaluate}`)

* skills made from a planner plan keep intents (`Skill.intent`) and match only the same request shape;
* a task that revised its plan and then succeeded stores `Lesson: for "…": <what went wrong> — what worked: <steps>`;
* the decision model also answers whether a message judges the previous result (`feedback`: negative / positive); a negative verdict counts
  against the calibration of the decisions behind it and the skill that produced it, and leaves a lesson;
* `DFAE_DECISION_LOG=path` appends every decision with its eventual outcome (`completed`, `failed`, `rejected_by_user`, `confirmed_by_user`) as
  JSONL — the data to evaluate or fine-tune the decision model; `DFAE_SHADOW_EVAL=0.05` also puts 5 % of the routing questions to the planner
  model and logs agreement (off by default: it spends planner tokens).

## Conversation (`session.rs`, `pipeline.rs::run_chat`, `executor.rs::run_chat_reply`)

Routing already says whether a request needs tools. If it does not (and nothing binds directly or matches a skill) — or if
tool selection finds nothing that fits — the pipeline builds a one-step `PlanSource::Chat` plan **without a planning call**.
Its single step is a reply: greetings, thanks, "how are you" and "what can you do" are answered by rules (0 tokens);
anything else is one call with `CHAT_SYSTEM`, a one-line `CAN:` capability list (tool categories), `NOTE: no tool fits` when
a tool-shaped request has no tool (so the model says so instead of inventing), and the session's last four turns as real
chat messages (≤48/80 tokens each). Chat replies skip the evidence-grounding check (there is no tool evidence) and the
continuation round. Before routing, `session::resolve` rewrites references to earlier turns using the entities of the
session's finished tasks (newest wins): `it`/`that file` → last file, `that link` → last URL, `my city`/`here` (with a weather
word) → the place given in this message or earlier. The rewrite is recorded as a `repaired` event on step `goal`; the user's own
words stay in `request.goal` and are what the Chat view shows.

Planner robustness that came with it: a read-only, approval-free tool the selector missed is admitted instead of failing the plan;
a request routed as *changing something* whose plan has no side-effecting step is sent back once with the tool names
(`missing_effect`); dangling `after` ids are dropped; replan ids are not double-prefixed; a model's reply that claims a change
when no side-effecting step finished gets a deterministic "nothing was changed" note.

## Token economy (`crates/dfae-runtime/src/economy/`)

Zero-cost mechanisms, one module, one cost model. Inspired by the Dagwood preprint's compile-validate-repair idea;
what was *not* adopted: the YAML representation (plans are already typed JSON at ~40 tokens/step), progressive
expansion for the planner (it never sees a workflow), and the headline compression ratios (they compare against
injecting every skill file, which this runtime never does).

* **`cost.rs` — `CostModel`.** Per-purpose (`plan`, `plan_repair`, `replan`, `step`, `step_expand`, `answer`) running mean
  of total tokens, blended with a measured prior (`(prior·3 + mean·n)/(3 + n)`, `n` capped at 50 so it keeps adapting),
  updated from each finished task's *own* ledger entries and persisted (`kv: costmodel`). `estimate_plan` prices the
  remaining work; `flat_vs_team` prices a split. `plan_model_calls` counts what the executor will spend: one per unfinished
  reasoning step, plus the answer-composing call unless a single reasoning leaf *is* the answer.
* **Budget gate.** `validate_plan` receives the *remaining* budget (`Inner::remaining_budget`) and reports
  "the plan needs about N model calls but only M remain" like any other plan defect — so the existing one-shot repair
  prompt tells the planner exactly what to fix, on every path (model plans, skills, replans, team plans).
* **`repair.rs`.** `fix_args` (types, unknown args, required args from entities/defaults — only if the result validates)
  and `relocate_path` (a *unique* match by name in a bounded workspace search). Applied by `repair_plan_args` right after a
  plan is parsed and by `repair_failed_step` on `InvalidArgs`/`NotFound`, at most twice per step, without consuming the
  step's retry budget, and never for policy/timeout/transient/tool failures. A repaired step still passes validation and
  the policy engine.
* **`contracts.rs`.** `resolve_templates` refuses to run a step whose `{{sN}}` input is missing or empty;
  `model_output_issue` rejects empty (no alphanumerics), degenerate-repetition and prompt-echo outputs (a legitimately
  short answer such as `4` passes); `ToolMetadata.contract` holds tool-declared post-conditions evaluated with the same
  `Check` machinery as plan checks.
* **`lazy.rs`.** Opt-in `NEED <step>` protocol, honoured only for short replies naming real dependencies.

Rationale for the team-pricing rule: with default costs, `n` model agents cost `n·(plan+step)` against `plan + n·step`
for one plan — `(n−1)` extra planning calls. The local decider subtracts up to 0.2 from a split's score in proportion to
that excess, except for capability-separating pipelines, whose value is safety rather than tokens.

## Teams (`crates/dfae-runtime/src/{team,dispatch,agents}.rs`)

Delegation is a **plan step kind** (`Agent`); an agent is a child `TaskRecord` run through the same pipeline.

1. **Decompose** (`team::decompose`, deterministic) — segments the request at `;`, newlines, sentence ends and soft
   separators (`and`, `,`, `then`, `after that`) *only where a new imperative clause starts* (a verb in the lexicon, or a
   fragment that binds to one of the registered tools' own patterns — so custom tools extend the splitter). Enumerations
   (`read a, b and c`) expand into one sub-goal per item. A trailing `summarise / compare / combine …` clause becomes the
   **join** (a reasoning step over all agents). Comparisons ("compare X and Y") are never split.
2. **Dependencies** are inferred conservatively: sequence words (`then`, `finally`), anaphora (`it`, `them`,
   `if necessary`, `whether … affected`, `based on`), and *an analysis clause right after an acquisition clause* ("read x
   and tell me …") make a sub-goal wait for its predecessor and receive its summary. A false dependency only costs
   parallelism; a false independence would cost correctness.
3. **Decide** — `assess_decomposition` over `TeamFacts` (how many sub-goals bind without a model, how many model parts run
   in parallel, how many wait on a deterministic one, how many change state, whether there is a join, whether a model exists).
   Local rule: split for parallel model work (≥2), fan-out with a join (≥2 free agents), a pipeline of ≥3 that separates
   read-only from state-changing work; don't split when one plan does as well or everything is already Direct-mode.
   With the decision model the same question is asked of it; either way the answer is calibrated on past outcomes.
4. **Dispatch** — each agent routes itself, then **fixes its role** (`pick_role`: a deterministic binding that changes
   state → operator; risky/maintenance → operator; no tools needed → analyst; "verify/check…" → verifier; else researcher),
   narrows its tool scope, and waits for a worker slot (`Dispatcher::acquire_agent`: role pool, then global pool).
5. **Run** — the agent inherits the entities of its sub-goal (no extractor call), a `Minimal` context profile, a slice of
   the coordinator's budget, and the coordinator's taint / auto-approve; it plans, executes, verifies and learns like any task.
6. **Supervise** — on failure the decision layer picks retry (fresh agent), escalate (re-run on the planner model) or fail;
   `max_replans` is 0 for agent steps because the agent already replans internally. Siblings are isolated from the failure.
7. **Join** — usage and ledger entries roll up (`origin` marks them), and the coordinator verifies: an agent step counts as
   evidence only if the agent itself was verified or partial. The answer is the join step's text, or each agent's answer under
   its sub-goal when there is no join.

**Dispatcher invariants.** Coordinators hold only an *admission* slot; agents hold a *role* slot then a *global* slot, always
in that order; agents never wait on other agents (depth 1). So a slot holder never waits for a slot another holder needs.
`operator=1` makes side-effecting agents strictly sequential.

## State machine

`created → running ⇄ waiting_for_tool / waiting_for_approval / recovering → verifying → completed | failed | cancelled`.
Every transition is validated against `TaskStatus::can_transition_to`; terminal states are final. The full
`TaskRecord` (plan, step outputs, approvals, decisions, ledger, budgets) is persisted after each layer, so a
restarted server resumes unfinished tasks: read-only steps that were mid-flight re-run, **steps with side
effects that were mid-flight are marked failed ("outcome unknown") and never silently repeated**.

## Decision contract

| Kind | Questions (type) | Used for |
|---|---|---|
| `route_task` | `task_type` (choice), `complexity` (score), `needs_tools` (noul), `risk` (choice) | context profile, calibration keys |
| `select_mode` | `mode` (choice: direct / assisted / escalated) | execution tier |
| `select_tools` | one multiple choice: the tools and `none` | which signatures the model sees (not asked when the planner model plans: it sees the whole catalogue) |
| `assess_plan` | `plan_ok` (noul) | recorded; low value emits a warning (policy is the gate) |
| `assess_recovery` | `recovery` (choice: retry / replan / skip / escalate / fail) | failure handling |
| `assess_completion` | `complete` (noul) | only when evidence alone is ambiguous |

Provider answers are validated against the question (type, and `choice ∈ options`). Hard rules are applied
**after** the provider — they are the reason a mis-calibrated decision model cannot cause harm:

- `direct` requires a deterministic binding; otherwise → assisted.
- `escalated` requires a configured planner model; otherwise → assisted.
- `assisted` without an worker model → escalated if available, else direct if bound.
- policy denials and cancellations are never retried or "worked around"; retries are capped by attempts; replans by budget.

Calibration (`Calibrator`) blends raw confidence with the observed verified-success rate of that exact
decision: `(raw·6 + successes) / (6 + n)`. The counters persist in the store (`kv: calibration`).

## Executor

- A step is *ready* when all dependencies succeeded/skipped; a failed/cancelled dependency cancels the step.
- Each ready wave runs concurrently under a semaphore; `{{sN}}` templates in arguments are resolved from
  dependency outputs just before execution, **then** validated and policy-checked.
- With `DFAE_APPROVALS=never` (default) the executor never suspends: `RequireApproval` decisions become `Allow` (except an unlisted host
  when `open_web` is off, which becomes a refusal), the `approval_required` flag on a plan step is ignored, and the policy's `Deny`s
  stand. The rest of this section describes `ask` mode.
- Approval-required steps suspend (`waiting_for_approval`) until `y` in the TUI / the approvals endpoint; the
  approval is bound to `sha256(tool|canonical-args)`, expires (`DFAE_APPROVAL_TTL_SECS`) and is re-verified.
- After a tool returns: `ok` flag, declared checks (`nonempty`, `contains:`, `matches:`, `exists:`), and the
  tool's own **post-condition** (e.g. `fs_write` reads the file back and compares the hash) must pass.
- Failure → `assess_recovery`. Retry waits `backoff × attempts`. Replan asks a model for the *remaining* steps
  (completed steps and outputs are kept, ids are prefixed `rN_`, the merged plan is re-validated). The tool
  set may widen to siblings of the same category, never to unrelated tools.

## Memory

| Layer | Content | Where |
|---|---|---|
| working | the persisted `TaskRecord` | `dfae_tasks` (JSONB) |
| episodic | task → outcome (failures kept, importance 0.7) | `dfae_memory` kind `episodic` |
| semantic | facts: explicit (`memory_store`, `/remember`) or **promoted entities** | kind `semantic` |
| procedural | skills: parameterised verified plans | kind `procedural` |

Embeddings are a deterministic signed feature-hash of stemmed unigrams+bigrams (256-d): no model, no network,
stable across restarts. With pgvector the search is `ORDER BY embedding <=> $q`; without it, `real[]` rows are
scored in-process. Swap in a neural embedder by implementing `Embedder`.

**Promotion rule**: an extracted entity becomes semantic memory only if the task was *verified*, its label is on
the allowlist (`server`, `database`, `repository`), its score ≥ 0.6, and its text also appears in a *tool*
output (corroboration). **Skill rule**: only fully-succeeded, replan-free, verified plans in assisted/escalated
mode with ≥1 tool step are crystallised; a skill with two net failures is deleted.

## Context budgeting

`ContextProfile` sets the token budget; sections have priorities (goal 0 never dropped; constraints/entities 1;
observations 2; memories 3; prior turns 4). Over budget → drop the least important section whole. Memories are
capped per profile and truncated; observations get a per-step share.

## Tokens

Every model call (worker model, planner, decision model) is a `LedgerEntry` with provider-reported usage when available (otherwise
estimated), emitted as an event and aggregated per task and globally. See `evaluation.md` for the baseline model.
