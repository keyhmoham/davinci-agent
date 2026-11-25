# Architecture

## Principle

> The model proposes; the engine decides what is authorised, executes, observes and verifies.

The runtime is the agent. Jev, GLiNER and the SLM are services it calls for *bounded* questions.
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
   SLM still fails and a stronger model exists — re-planned there.
7. **Execute** — DAG waves with bounded parallelism (`DFAE_MAX_PARALLEL`).
8. **Verify** — evidence-based; one continuation round if the completion judgment is low.
9. **Answer** — templated (no model) unless reasoning steps produced material to compose; model-written
   answers are checked for ungrounded numbers/paths/URLs.
10. **Learn** — calibration, episodic memory, validated fact promotion, skill crystallisation.

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
| `select_tools` | one `tool:<name>` (noul) per tool | which signatures the model sees |
| `assess_plan` | `plan_ok` (noul) | recorded; low value emits a warning (policy is the gate) |
| `assess_recovery` | `recovery` (choice: retry / replan / skip / escalate / fail) | failure handling |
| `assess_completion` | `complete` (noul) | only when evidence alone is ambiguous |

Provider answers are validated against the question (type, and `choice ∈ options`). Hard rules are applied
**after** the provider — they are the reason a mis-calibrated decision model cannot cause harm:

- `direct` requires a deterministic binding; otherwise → assisted.
- `escalated` requires a configured stronger model; otherwise → assisted.
- `assisted` without an SLM → escalated if available, else direct if bound.
- policy denials and cancellations are never retried or "worked around"; retries are capped by attempts; replans by budget.

Calibration (`Calibrator`) blends raw confidence with the observed verified-success rate of that exact
decision: `(raw·6 + successes) / (6 + n)`. The counters persist in the store (`kv: calibration`).

## Executor

- A step is *ready* when all dependencies succeeded/skipped; a failed/cancelled dependency cancels the step.
- Each ready wave runs concurrently under a semaphore; `{{sN}}` templates in arguments are resolved from
  dependency outputs just before execution, **then** validated and policy-checked.
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

Every model call (SLM, strong, Jev) is a `LedgerEntry` with provider-reported usage when available (otherwise
estimated), emitted as an event and aggregated per task and globally. See `evaluation.md` for the baseline model.
