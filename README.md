# DFAE — Decision-First Agentic Engine

A token-frugal agent runtime written in Rust. **The runtime is the agent; models are specialised services inside it.**
A hosted **planner model** (default `gpt-6-luna`) writes a typed, validated plan and revises it when something cannot be solved;
a **worker system** carries the plan out step by step: deterministic rules and extractors, a **decision model** that routes, selects tools
and judges (by default the hosted Decisions API, `openai/gpt-6-luna-decisions`; a self-hosted open-weight model or built-in rules instead),
and a hosted, cheaper **worker model** (default `glm-5.3-flash`, pinned to unquantised providers) that fills in what the rules cannot, writes
content and fixes a failed step with a short script loop before the planner is asked. Execution, security, memory and verification are deterministic code.

```
docker compose up -d --build     # engine + Postgres/pgvector (decisions: hosted Decisions API; memory: hosted embeddings; profiles add GLiNER and Ollama)
docker compose run --rm tui      # the terminal UI
# or both:  ./dfae
```

> Set `PLANNER_API_KEY` in `.env` (the worker model uses the same gateway and key unless `WORKER_API_KEY` / `WORKER_BASE_URL` say otherwise).
> Decisions (the hosted Decisions API) and memory embeddings (`voyage/voyage-4-lite`) go through the same gateway and key: nothing is
> downloaded. [`.env.example`](.env.example) lists every setting with its default. Optional profiles add local services:
> `COMPOSE_PROFILES=local-decisions` (an Ollama decision model, `qwen3:4b-instruct-2507-q4_K_M`, ~2.5 GB, with `DECISION_BASE_URL=http://ollama:11434/v1`),
> `pii` (a GLiNER PII model that also redacts names and addresses from memory), `gliner` (GLiNER on the hot path: measured no benefit).
> The engine answers immediately and degrades gracefully when a service is missing (see [What works without what](#what-works-without-what)).

## Why it is cheap

## Plan with the planner model, act with the worker system

**Planning is always the planner model's, and the decision model decides whether a request needs planning at all.** For every request it
chooses between three handlings: *direct* (the request binds to tools outright), *quick* (one action or a reply that the worker system —
worker model + decision model + extractor — handles alone: one step, no plan, no planner call) and *needs a plan* (several dependent steps,
something to build and run or serve, work whose next step depends on results). A quick step that turns out not to be enough fails with its
actual result and the planner re-plans. With no planner configured the worker model has to plan; with one configured it never does — if the
planner cannot be reached the task fails with the reason (`DFAE_PLAN_FALLBACK=worker` lets the worker model plan instead). Conversation is a
reply, not a plan: the worker system answers it. A task that needs a plan is split into two phases that never mix:

1. **Planning** — the planner writes a **typed plan** as one JSON object whose shape is a strict JSON Schema (`plandoc::plan_schema`): steps with an
   `id`, a `tool` that is an *enum of the tools that exist* (or `null` for a step that thinks or writes), `do` (the intent in one sentence with every
   concrete path, name, pattern, text, command in it), `after` (dependencies), `expect` (what a correct result shows, machine-checked where it
   can be) and `planner` (true only if the step needs the planner's own reasoning). It writes no tool arguments, so the output stays short.
   A reply is never trusted: `PlanDoc::from_value` reads it leniently into typed steps and **repairs in code** what it can (tool spellings, a
   renamed list, duplicate or numeric ids, dependencies on steps that do not exist), saying so in the log; what it cannot repair becomes typed
   issues that the planner is told in one repair round.
2. **Acting** — the program drives the worker system, step by step, right before each step runs (so it sees the real results of earlier steps):
   GLiNER and the rules read the sentence for paths, names and quoted text; the tools' own patterns and the entities bind what they can; the
   decision model chooses the tool when the planner left it open and later judges the result against `expect`; the worker model fills in what is
   still missing (arguments under a JSON schema, a 3-sample vote on retry), writes a file's content as plain text and writes reasoning steps. A step
   nothing can carry out fails with a precise message and recovery takes over: retry, deterministic repair, local rewrite, and then **the
   planner patches the plan** — a typed patch (`plandoc::patch_schema`): `edit` the failed step (or the one before it that caused the problem), `add`
   steps, `drop` steps. Only what changes is written and only that runs again; finished steps are never touched. A worker model that cannot be
   reached does not stop the task: the planner does that call. Every implementation is recorded (`step_implemented` event, "done by" in the Plan tab:
   `rules`, `rules+worker`, `worker-vote`, `planner`, …).

**What counts as cost.** Two hosted models are billed by the token: the **planner** (expensive: plans, patches, steps it keeps for itself, answers
that fail the grounding check) and the **worker** (cheap: argument binding, reasoning steps, content, answers, step fixes). The hosted
decision model answers typed questions for ≈ $0.0003 per task; a self-hosted one is free. Worker tokens are optimised too: ordinary work (binding arguments, short reasoning, answers)
is held to a small output limit (`WORKER_MAX_TOKENS`, default 800) with compact prompts, while writing a file's content — a page, code, data — may run
long (`WORKER_CONTENT_MAX_TOKENS`, default 16000). Budgets: `DFAE_MAX_MODEL_CALLS` meters planner calls, `DFAE_MAX_WORKER_CALLS` (default 200) is a
runaway guard for worker calls. The ledger reports `planner_tokens` and `worker_tokens` (with cached tokens, effective tokens and the purpose each
was spent on) separately; savings are measured against what a naive single-model agent would have spent.

Every request takes the cheapest path that can do the job:

| Mode | Components | Hosted tokens | Used when |
|---|---|---|---|
| **Direct** | decision model + code + tools | **0** | the request binds to tools deterministically ("what is 12*(3+4)", "list the files in ./ and what time is it", a replayed learned skill) |
| **Assisted** (quick) | + worker model | worker tokens only; one step, no plan | one quick action or a reply the worker system handles alone (with no planner configured: the worker model also plans) |
| **Escalated** (plan) | + planner model | planner (plan, patches) + worker (steps) | the decision model says the request needs a plan (and a planner is configured) |

Requests that split into sub-tasks are handled by a **team of subagents** that forms automatically
(see [Teams](#teams-subagents-swarm-and-dispatch)); each agent picks its own mode, so a deterministic
sub-task inside a team still costs no generative tokens.

Mechanisms that cut tokens (all implemented and tested):

- **Direct binding** — tools declare regex patterns with named groups; a request that binds
  completely never reaches a model. Anything that smells like reasoning ("explain", "why", …)
  is refused by the binder and goes to the worker model.
- **Tool narrowing** — the decision layer judges every tool; a prompt names only the relevant tools (the planner's cached prefix carries the whole
  catalogue). The plan is *validated against that subset*, so it is also a security property.
- **Compact, budgeted context** — entities, ranked + de-duplicated memories and observations are packed into a profile-sized budget
  (`minimal` 450 / `standard` 1300 / `reasoning` 3200 tokens) that follows the request's complexity, dropping the least important material
  first. Step outputs reach the planner as ~300-token summaries and the worker as ~600. A big output is condensed by **line selection**: code
  ranks candidate lines (head and tail, errors and warnings, lines that share words with the task, JSON as one record per line) and the worker
  answers with line numbers only, so what is passed on is verbatim (measured: 19/19 facts, 0 invented values, −78% output tokens vs a summary).
- **Observation compression** — tool output is redacted, de-duplicated and head/tail-elided
  before it re-enters a prompt; full output stays in state.
- **No redundant calls** — a lone reasoning leaf *is* the answer (no synthesis call); decisions
  and extractions are cached; system prompts are byte-stable so prompt caches stay warm. The planner's prompt is a large stable prefix (rules,
  the whole tool catalogue, worked examples) sent only to a provider that caches prompts and only while traffic keeps the cache warm.
- **Skill learning** — a *verified* plan is generalised into a parameterised skill
  (procedural memory). A similar request later replays it with new entity values: no planning
  call, and no model at all if the plan has no reasoning step.
- **Calibrated trust** — a decision's confidence is blended with its observed success rate on
  *this* agent's verified outcomes, so an over-confident decision stops being trusted.

The TUI's **Tokens** view and the `ledger` on every task show actual generative tokens next to an
**estimated** baseline for a naive agent that re-sends the system prompt, every tool schema and
the full raw history on each step (formula in [`docs/evaluation.md`](docs/evaluation.md)). The
baseline is a model, not a measurement — benchmark against a real agent before quoting a number.

### What the hosted models cost, and how that is kept down

Measured with the ablation benchmark (`cargo test -p dfae-runtime --test bench -- --nocapture`; see `docs/evaluation.md`): against the previous
flow (per-task tool list, an answer call), the defaults spend **−37 % effective planner tokens** (−3 % on a provider without prompt caching, where the
cacheable prefix switches itself off) and 14 % fewer planner calls for the same outcomes. What does it: the whole tool catalogue and examples as a
prompt-cache prefix, sent only while the provider's cache is warm; final answers written by the worker model and checked against the evidence (the
planner only if the check fails); and skills that replay *intents*, so a repeated kind of request costs 0 planner tokens after the first. What keeps
quality up while doing it: expectations that code can read are checked by code, written files are parsed and rewritten when malformed, a rejected
reasoning step is rewritten before the planner hears of it, big outputs are condensed, work the planner plans is one plan rather than a team of
planners, a task that went wrong then worked leaves a lesson, and "that's wrong, try again" teaches the calibration and the skills.

## Token economy: zero-cost mechanisms that save tokens

One layer (`crates/dfae-runtime/src/economy`) groups everything that spends no generative tokens to avoid
spending some. It was shaped by the [Dagwood](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=7506739) preprint's
"compile, validate, repair before asking the LLM" discipline, adapted to a runtime whose plans are already typed.

| Mechanism | What it does | Saves |
|---|---|---|
| **Learned cost model** | Prices every accepted plan (`est. 2 model calls ≈ 410 tok`) from per-purpose costs learned from the ledger and persisted across restarts (`/v1/cost`). | Feeds the next three rows |
| **Pre-execution budget gate** | An unaffordable plan is rejected *with the numbers* and sent back to the planner before any tool runs — not discovered mid-run. | Wasted tool runs and half-finished work |
| **Team pricing** | The same model prices team vs one plan: splitting model work costs an extra planning call per agent, and that is charged against splits whose only benefit is parallelism (not against capability-separating pipelines). | Splits that cost more than they return |
| **Deterministic repair** | Wrong types, unknown arguments, values the request itself supplies, and a file that lives one directory over are fixed *in code* — at plan-parse time and when a step fails (`InvalidArgs`/`NotFound`). Ambiguous paths are never guessed; policy denials are never repaired. | A repair or replan call (~300–500 tokens) each |
| **Output contracts** | Empty or degenerate model output and echoed prompts are rejected at the step that produced them; a step never runs on an empty upstream result; tools declare output contracts (`calc` must return `… = n`, `http_get` must start `HTTP nnn`). | Downstream model calls on garbage |
| **Lazy context** *(opt-in)* | `DFAE_LAZY_CONTEXT=on`: reasoning steps get ~40-token upstream summaries and may reply `NEED s1` for the full output. | Prompt size — **at the risk of an extra call; unmeasured with a real model, so off by default** |

## Teams: subagents, swarm and dispatch

No command is needed. When a request splits into sub-tasks, the runtime forms a team on its own:

```
read a.txt, b.txt and c.txt and summarize them          Find the latest PostgreSQL vulnerabilities, check whether my
   ├─ c1 read a.txt   (researcher · direct · 0 tokens)   server is affected, and patch it if necessary
   ├─ c2 read b.txt   (researcher · direct · 0 tokens)      c1 research  (researcher)  ──▶ upstream summary
   ├─ c3 read c.txt   (researcher · direct · 0 tokens)      c2 check     (verifier)    ──▶ upstream summary
   └─ join            (worker model · one call)                      c3 patch     (operator)
```

**What triggers it.** A deterministic *decomposer* proposes a split (clauses, comma lists, numbered items,
"read a, b and c" enumerations, a trailing "…and summarize them" as the join). The decision layer then judges
`assess_decomposition` — and says yes only when splitting helps: parallel model work, a fan-out whose agents need
no model, or a pipeline that separates read-only from state-changing work. A request like *"read todo.md and tell
me which tasks are open"* stays one task (one plan does it as well, without the handoff). The judgement is calibrated
against how earlier splits turned out. Opt out per request (`/team off`, `delegate:false`) or server-wide (`DFAE_TEAM=off`).

**Delegation is a plan step** (`kind: agent`), so the DAG executor, approvals, retries, verification and the
UI all work on teams unchanged. A subagent is an ordinary child task running the same pipeline.

| Mechanism | What it does |
|---|---|
| **Roles** | `researcher` (read-only), `operator` (may change state), `analyst` (reasoning only), `verifier` (read-only checks) — chosen from what the sub-task will actually do. A role's **tool scope** is enforced in selection, binding, skills *and* at execution. |
| **Dispatcher** | Admission control for top-level tasks (`DFAE_MAX_CONCURRENT_TASKS`; a bounded queue, then load shedding) and **worker pools per role**. Default `operator=1`: agents that may change state never run concurrently; researchers fan out. |
| **Handoff** | A dependent agent receives its predecessors' results as a compact `UPSTREAM` section — not the whole history. Independent agents run in parallel. |
| **Budgets** | Each agent gets a *slice* of the coordinator's remaining model calls / tool calls / time; usage rolls up, so a team cannot overspend. Agents cannot spawn agents (depth 1), which also makes the pools deadlock-free by construction. |
| **Supervision** | Failure isolation (siblings finish; the join is skipped; partial results are kept). The decision layer retries transient failures with a *fresh* agent, escalates to the planner model, and never retries a policy denial. |
| **Shared cache** | Agents of a team share read-only tool results (`fs_read`, `fs_list`, `fs_search`, `calc`); *any* side effect by any agent invalidates it, and a read that raced a write is never cached. |
| **Security inheritance** | Taint, auto-approve and the policy engine are inherited — delegation is never a way around an approval. Approvals raised by an agent are answered on that agent. Cancelling the coordinator cancels every agent. |
| **Accounting** | Agents' token ledgers roll up into the coordinator (the team's real cost) without double-counting globally; the baseline models *one* naive agent doing all the work. |
| **Resume** | After a restart a coordinator adopts agents that had finished and marks unknown ones failed — it never silently re-runs work that may have had side effects. |

**What teams buy you, honestly.** Parallelism, context isolation (each agent's prompt holds its own sub-goal and
upstream summary), least privilege, and serialised side effects. Raw token savings appear when sub-goals replace
*planned tool steps* with free deterministic agents (fan-out + join). A pipeline of model-only agents costs *more*
than one plan — every agent pays its own planning call (~330 tokens by default) — which is why the cost model charges
that against splits justified only by parallelism. The TUI **Team** view shows agents, roles, dependencies and the live worker pools.

### Custom subagents, planned and in parallel

Besides the automatic split, **the planner delegates** when the work is broad: a plan step may name an agent instead of a tool, and fan
it out over parts. The instances run in parallel as child tasks, and a later step that waits for the step receives every result.

```json
{"id": "s2", "tool": null, "do": "review {item} for real bugs, with file and line", "after": ["s1"],
 "agent": {"name": "reviewer", "instructions": "read the code, list real bugs only", "tools": ["fs_read", "fs_search"], "writes": false},
 "each": ["src/a.py", "src/b.py", "src/c.py"]}
```

- **Profiles**: a name, what it is for, **how it works** (its skill: instructions every model call of the agent sees first) and the
  tools it may use. Inline in a step for one task, or **saved** to be used by name (`agent_define` / `agent_list` / `agent_remove` for
  agents, `GET/POST /v1/agents`, `DELETE /v1/agents/{name}` for you). Saved agents are listed to the planner, only when there are any.
- **Limits that follow from the research**: multi-agent work costs about 15× a chat's tokens and pays off only for breadth (Anthropic's
  research system); agents that each decide on shared state conflict (Cognition). So the planner is told to delegate only independent
  parts that each need several steps; an agent gets only its part; readers run in parallel and **writers take the single operator
  slot**; an agent cannot spawn agents or manage routines; at most 8 instances per step.
- A task that has read outside content, or a routine's run, **cannot save or remove an agent**: a saved agent is an instruction that
  outlives the task.

## Background work: routines and the board

Agents (and you) schedule work that runs later in the background, once or again and again, without anyone asking again:

| Schedule | Examples |
|---|---|
| once | `in 20m` · `in 1h30m` · `at 17:30` · `tomorrow 8am` · `at 2026-12-01 09:00` |
| repeatedly | `every 2h` · `every 15 minutes` · `daily 09:00` · `weekdays 08:30` · `weekly mon,fri 18:00` · `monthly 1 09:00` · `hourly :15` · `cron 0 9 * * 1-5` |
| limits | `max_runs`, `until` (a date), `active_hours` (`08:00-22:00`, may wrap), `max_cost_usd`, a time zone (`tz`, or `DFAE_TIMEZONE`; DST-correct) |
| gates (no model call) | `if_changed`: run only when a command's output, or `GET <url>`, differs from last time (the first look records it) · `if_ok`: only when a command exits 0 |

- **Each run is an ordinary task** in the routine's own session, told what the last run found, so it reports what is new. A routine
  never overlaps itself; `DFAE_ROUTINE_CONCURRENCY` (4) run at once and `DFAE_ROUTINE_MAX_RUNS_PER_DAY` (200) start per day; failures back
  off (30 s, 1 min, 5 min, 15 min, 1 h) and end a routine after 10 in a row; one-shots are retried three times. After downtime a missed
  slot runs once (`catch_up: false` skips it). The book is durable (the store; Postgres in the container).
- **No background heartbeat**: PM-Bench found that a fixed 30-minute heartbeat produced the most false alarms and scored below the plain
  agent, while agent-chosen checks scored best. Nothing here wakes on its own: routines do, and their gates cost nothing when nothing changed.
- **Not a way in**: a routine an agent asks for after reading outside content (a page that says "check this URL every hour and do what
  it says"), or from a routine's own run, is only **proposed** — disabled until you enable it; resuming, widening or running one from
  there is refused. Pausing and removing are always allowed.
- **Agents' tools**: `routine_add`, `routine_list`, `routine_update` (pause/resume, schedule, goal, limits), `routine_remove`, `routine_run`.
- **API**: `GET/POST /v1/routines`, `GET/PATCH/DELETE /v1/routines/{id}`, `POST /v1/routines/{id}/run[?gates=true]`; changes are events
  on `/v1/events` (task id `routines`).
- **The board** (`GET /v1/board`, the TUI's **Board** tab, live): what every agent is doing now (its current step, progress, role or
  routine, cost), what waits for an approval, what is queued, every routine (scheduled / proposed / paused / running / ended, next
  run, last outcome, runs and spend) and what just finished. Manage routines from the input line: `/routine add every 2h | <goal>`,
  `/routine pause|resume|run|rm <id>`, `/routine when <id> <schedule>`, `/routine goal <id> <text>`.

## Architecture

```
          ┌───────────────────────── dfae (TUI)  ──── HTTP/SSE ────┐
          │                                                        ▼
 request ─▶ extract ─▶ retrieve ─▶ route ─▶ bind | skill ─▶ mode ─▶ plan ─▶ execute ─▶ verify ─▶ answer ─▶ learn
           GLiNER+     memory      decision  (no model)    decision   worker model     DAG        evidence  grounded   skills,
           rules       ranking     model                    model    /planner executor   checks    synthesis  calibration
                                                                              │
                       policy engine (deterministic) ◀── every step, on final args ──▶ approvals · audit chain
```

| Crate / service | Role |
|---|---|
| `dfae-core` | state model, plan schema, events, token accounting, decision contract, provider traits |
| `dfae-decision` | hosted Decisions API provider (default) · open-model provider (typed answers from a self-hosted model's log-probabilities) · local rule-based decider (same typed questions) · calibrator · cache + fallback |
| `dfae-extract` | GLiNER client · rule extractor · hybrid merge (structured rules win, semantic spans from GLiNER) |
| `dfae-models` | OpenAI-compatible and Anthropic providers, tier gateway, JSON extraction + one repair retry |
| `dfae-memory` | hash and hosted semantic embedders (both kept per item) · Postgres (+pgvector) and in-memory stores · ranking · context packing · hash-chained audit |
| `dfae-tools` | registry · **policy engine** · 11 native tools · direct binder · calculator |
| `dfae-mcp` | MCP client (Streamable HTTP and stdio) · the hub that turns servers' tools into engine tools · **Glama directory** search and on-demand connection · the **mcpc** broker that authenticates and opens remote servers |
| `dfae-runtime` | pipeline, planner/validator, DAG executor, approvals, recovery/replan, verification, skills, resume · **team: decomposer, roles, dispatcher, subagent supervision** |
| `dfae-server` | REST + SSE API, bearer auth, Prometheus metrics |
| `dfae-tui` | the `dfae` terminal client (also `dfae --ask "…"`) |
| `services/gliner-service` | FastAPI wrapper around GLiNER (chunking, offsets, health) |

### The decision contract

`Question`/`Answer` are typed like a decision model's wire format (`noul` probability, `choice`, `score`).
The hosted Decisions API (the default) takes the typed questions as they are and returns calibrated probabilities and choices.
The self-hosted provider turns each question into a single-token multiple choice for an open-weight instruct
model and reads the **probability mass of the answer tokens** from the first token's log-probabilities
(`noul` = P(yes)/(P(yes)+P(no)); `choice` = lettered options; `score` = expected level), so answers are real
probabilities and always schema-valid — one prompt and one generated token per question; all questions of a
decision share the same state prefix, so prompt caching helps. **The decision model makes the decisions; code supplies facts and
enforces limits.** The engine asks bounded questions (`route_task` — task type, complexity, tools needed, risk, *does it change something*,
*is that change only conditional*, *is it just small talk (and which kind)*, *which kind of agent fits* — plus `resolve_reference`
("does the request lean on something said earlier — the file, the link, the place, the code?"), `select_mode`, `select_tools`,
`assess_plan`, `assess_recovery`, `assess_completion` and a step-level judge) over a compact typed state that includes the recent
conversation and the policy ("planning is always the planner model's; decide whether a request needs a plan"). Provider answers are **validated** (right type,
choice within the offered options), then only **capability limits** apply: a provider can never pick *direct* mode without a
deterministic binding, *escalated* without a configured planner model, or retry a policy denial. The phrase lists and regexes that used
to decide these things now live only in the local fallback decider, which answers the same questions when no model is configured or it
is down; the runtime reads the answers (`Routing.changes`, `.conditional`, `.smalltalk`, `.role`), never the sentence. On any provider error the same questions are answered by the local decider
(`local-fallback` in the decision log).

### Security model

The policy engine ([`config/policy.toml`](config/policy.toml)) is the boundary — neither the decision model nor
the worker model can override it. It runs when a plan is validated **and again on the final, template-resolved
arguments** immediately before each tool executes (tools re-check too).

- **no human in the loop (default):** `DFAE_APPROVALS=never` — no tool call waits for anyone and the agent never asks "shall I proceed?". The policy still **refuses** what it forbids (a path outside the workspace, a private address…); it just doesn't ask. `DFAE_APPROVALS=ask` brings the approval step back (writes, mutating commands, unlisted hosts and tainted tasks then wait in the TUI; approvals bind to `sha256(tool + canonical args)`, expire, and are re-checked)
- filesystem: read/write roots (the workspace), secret-name denylist, symlink-escape defence — for every file tool
- shell: in the container, `shell.mode = "sandbox"` gives a **full bash** in the workspace (pipes, redirects, `&&`, git, build/test tools, timeouts, background jobs) and **the container is the boundary** (read-only root, no capabilities, unprivileged user, memory/pid limits). Outside a container the default `allowlist` mode applies: one allowlisted command, no shell, path-checked arguments, `find -exec` & co. denied
- network: private/loopback/link-local/metadata addresses blocked (checked after DNS resolution, on every redirect hop); `open_web` lets `http_get` reach any public host. **The shell's own network access (`curl`, `pip`, `git clone`) is the container's, not this policy's**
- tool output is untrusted: secrets redacted, injection patterns flagged → the task is *tainted* (shown in the audit; with `DFAE_APPROVALS=ask` side-effecting steps then require approval)
- content from outside (web pages, MCP servers) is **extracted from, never summarised**: lines that read like instructions are dropped and a model picks line numbers, so an injected sentence cannot reach the planner in a model's own words. The plan is written before such content is read (plan-then-execute), the step agent never runs on a task that has read it, and from then on **changing, moving or removing a file that was there before needs the request to name it** (or a human; in `never` mode it is refused and planned around), and a value read locally is not sent to the network. Measured on AgentDojo-style attacks: a setup page's injected `rm -rf tests` was carried out 11/11 times before, 0/15 with the guard. `DFAE_EXPOSED_CHANGES=allow` lifts it (for "upgrade the project as the guide at URL says"). What it does not stop: changes to files the request names, and anything the shell does outside the patterns it recognises — the sandbox remains the boundary
- append-only audit log, hash-chained per task (`GET /v1/tasks/{id}/audit` reports `chain_valid`)
- runs as an unprivileged user in a read-only container with all capabilities dropped; API published on loopback only

See [`docs/security.md`](docs/security.md) for the threat model and known limits.

## Using it

### TUI

| Key | Action |
|---|---|
| `Enter` | submit the request |
| `Tab` / `Shift+Tab` | **Chat** (home) · Timeline · Plan · Team · Cognition · Tokens · Memory · Tools · **Board** |
| `↑`/`↓` | select task (empty input) or recall history |
| `y` / `n` | approve / deny the pending approval |
| `Ctrl+A` | jump to a task waiting for approval |
| `F2` | cycle mode: auto → direct → assisted → escalated |
| `Ctrl+X` | cancel the selected task |
| `PgUp`/`PgDn`/`End` | scroll the timeline |
| `F1` | help |

Commands: `/mode`, `/team auto|on|off`, `/memory <q>`, `/remember <fact>`, `/new`, `/tools`, `/board`, `/routine …` (see *Background work*), `/cancel`, `/autoapprove on`, `/quit`.

### Conversation and the open web

A session is a conversation. Greetings and small talk cost **no model call**; a question that needs no tool is
**one** call (no plan) with the last few turns attached as real chat messages; and short references are resolved
by rules before anything else looks at the request — `read it`, `delete that file`, `fetch that link`,
`what's the weather in my city` (a place you mentioned earlier in the session, or in the same message).
The Chat tab shows the thread; Tab shows how each reply was produced.

| Tool | What it does | Notes |
|---|---|---|
| `web_search` | titles, URLs, snippets; `fetch_top=1..2` also reads the top pages | SearXNG (`SEARXNG_URL`) → Brave (`BRAVE_API_KEY`) → DuckDuckGo HTML → Wikipedia |
| `http_get` | fetch any public URL as text | `open_web = true` in `config/policy.toml` (shipped); set `false` to restrict to the allowlist. Its description tells the planner how to get current weather: `https://wttr.in/<city>?format=3` |

The open web is read-only and bounded: private, loopback, link-local and metadata addresses are always blocked (after DNS and on every
redirect), responses are size- and time-capped, and once any tool output looks like a prompt injection, fetching an unlisted host needs approval
again (a URL can carry data out). Search results and pages are untrusted data to the model.

### Coding and shell tools

The agent has the tool set of a coding agent. Every tool's description says how to use it (that text is all a model sees of it), error
messages say how to fix the call, and tools that belong together are offered together (`fs_edit` brings `fs_read`/`fs_search`/`fs_glob`;
`shell` brings `shell_output`/`shell_kill`).

| Tool | Use |
|---|---|
| `fs_read` | read a file; `offset`/`limit` select lines, `numbered=true` adds line numbers |
| `fs_edit` | exact string replacement (`old_string` must be unique unless `replace_all`); read-back verified |
| `fs_write` | create / overwrite a file, `append=true` |
| `fs_glob` | find files by glob (`**/*.rs`, `*.{ts,tsx}`), newest first |
| `fs_search` | regex search with `glob`, `context`, `files_only`, `case_sensitive` |
| `fs_list` | directory listing; `depth` shows a tree |
| `fs_mkdir` · `fs_move` · `fs_copy` · `fs_delete` | tree operations (delete needs `recursive` for directories; never the workspace root) |
| `shell` | bash in the workspace (or `cwd`) with `timeout_secs`; `background=true` for servers and long jobs |
| `shell_output` · `shell_kill` | read new output of / stop a background job |
| `http_get` · `web_search` | the web (see above) |

#### The shell is a sandbox the agent may install into

In sandbox mode (`shell.mode = "sandbox"`, what the container image ships) every model that decides what to run is told, in so many words, that it
is in a sandbox it fully controls and **may install whatever a task needs — and must, rather than refuse or ask the user to**. As an
unprivileged user in a read-only container, "install" means user-space installs into a *toolbox* (`{workspace}/.tools`, so it survives restarts)
that is on `PATH` in every command:

| Need | How |
|---|---|
| Python packages | `pip install x` (user installs; `python3 -m venv .venv` for a project) |
| Node (the image ships the newest LTS, built from nodejs.org; `NODE_VERSION=v24.22.3` pins one) | `npm install -g x`, `npx -y x`; pnpm/yarn: `npm install -g pnpm` |
| Rust · Go | `cargo install x` · `go install x@latest` (a toolchain itself: `curl` the official installer into the toolbox) |
| System packages | `dfae-apt <debian-package>` (apt downloads, `dpkg -x` unpacks into the toolbox; no root; `update-alternatives` links are made) |
| Anything else | a release binary fetched with `curl` into `$DFAE_TOOLS/bin` |

Commands run the way an agent needs them to: no terminal (nothing prompts; `DEBIAN_FRONTEND`, `GIT_TERMINAL_PROMPT`, `npm_config_yes` are set),
git has an identity and `main` as its default branch, proxies and certificate bundles are passed through (`env_passthrough`; the engine's keys
never are), stderr is merged into the output in the order it was written, colour codes and progress bars are cleaned up, a long log keeps its
start and its end, and a command that installs or builds gets 10 minutes instead of 2. A command that times out is killed *with everything it
started* and what it printed is kept. A failure explains itself: `command not found`, a missing Python or Node module, or a read-only system path
comes with a one-line hint that says what to install. A step that names a directory ("… in proj") runs in it. Without sandbox mode the models are
told the truth instead: one allowlisted read-only command at a time, nothing installable.

#### Work that is checked, not assumed

What the agent starts or builds is verified by asking it, not by trusting the step that made it:

- a **server** that names its port is refused up front when the port is taken (with the free ones listed), must be *listening and still alive* a few
  seconds later, and the planner is told which of 8000–8010 are in use;
- a **served page** is requested after it starts (`expect: http://localhost:8001 serves "Acme"`): localhost only, it waits for a dev server's first
  build, and it reads the *visible* text — a scaffold's default page with the project's name in its `<title>` does not pass;
- **files** are checked as they are written (JSON, TOML, HTML, Python, shell, JS, TypeScript/JSX with the project's own compiler), and a file is written
  knowing the files of its project written before it (a module's tests see the module);
- a **failed command** reaches the planner as the end of its output plus the distinct error lines above it (a test run's real cause is in the middle),
  a **patch** cannot lose work it does not replace, and an **edit** is bound with the file's text in front of the model and checked against it;
- the **answer** to a question a tool or an MCP server answered with data is written from it, not shown as raw JSON;
- **the code a task wrote is run and used before the task is judged** (`validating`): the project's own tests and build (`npm test`,
  `npm run build` or `tsc --noEmit` beside a running dev server, `pytest`, `cargo test`, `go test`), unless the plan already ran them after
  its last edit; the **API and its wiring** — backend routes (Express-style, FastAPI/Flask, Next.js route files) and the frontend's
  `fetch`/axios calls are read from the source and matched (a call to a route that does not exist, or with the wrong method, is caught with
  no request made), then every GET the page makes is asked through the page's own server (a missing proxy answers 404 there), backend GET
  routes must not answer 5xx (the server's log comes with the finding), and a cross-origin call needs its CORS header; and the **UI, end to
  end**, in headless Chrome with [agent-browser](https://github.com/vercel-labs/agent-browser): the worker model uses the page like a user
  (fill, click, submit, check the result shows), seeing only the interactive elements (`snapshot -i -c`) and, after each action, what changed
  (`diff snapshot`) — a few hundred tokens a turn, `DFAE_UI_TURNS` (8) turns at most — while page errors, console errors and failed requests
  are collected by the tool, not judged by the model. What fails goes to the planner as one repair round (`DFAE_VALIDATE_ROUNDS`) and is
  tested again; the checks join the verification, so an answer never says it works when it was seen not to. (agent-browser drives web pages;
  its sibling [agent-desktop](https://github.com/vercel-labs/agent-desktop) is for native desktop apps and itself says to use agent-browser
  for the web.) `DFAE_VALIDATE=off`, `DFAE_UI_CHECK=off`, or `UI_TOOLS=0` at build time (−450 MB) turn parts off.

Simple phrasings of the file operations bind directly and cost no model tokens (`move a.txt to b/a.txt`, `delete notes/old.txt`,
`replace "foo()" with "bar()" in src/lib.rs`, `find files matching **/*.rs`, ``run `cargo test` ``). The container image ships git, ripgrep, curl, jq, make, gcc, python3 and friends
(`DEV_TOOLS=0` builds it without them); its root is read-only, so use a virtualenv or project-local installs in the workspace.

### MCP servers: found and used on demand

Set `GLAMA_API_KEY` (a key from [glama.ai/settings/api-keys](https://glama.ai/settings/api-keys)) and nothing else. When a request is about an
outside service — GitHub, DeepWiki, Notion, a database, a calendar — the decision model says so (`external_service`; a service named outright
in a request that has something to do counts too, and so does saying "MCP"). The engine then:

1. looks at the servers it already has (the registry: connected earlier in this run, reopened from the previous one, or configured) and uses the
   ones whose tools fit the request;
2. if none does, has the worker model write, per outside service, the capability in a tool's own words and **2–3 short searches** (the only
   thing the directory hears of the request), searches the [Glama MCP directory](https://glama.ai/mcp/reference) filtered to servers it can
   use (`attributes=auth:none`, `auth:oauth2`, `auth:api_key` with keys), fuses the ranked lists, reads the best eight in detail (their tools),
   ranks them on their best tool, lets the decision model judge each on those tools, and connects the best ones that need nothing from you.
   One that wants an API key is used when `DFAE_MCP_KEY_<SLUG>` is set; when only a server behind a sign-in fits, **you are asked to sign in**
   (below);
3. registers their tools as `mcp__<server>__<tool>` (the JSON Schema becomes typed parameters; objects and arrays are written as JSON text and
   parsed before the call) and **attaches them to that task only** — the planner's catalogue for an unrelated request never lists them;
4. the planner plans with them, the worker binds their arguments, and a tool call is a normal step (retried, verified, re-planned).

A dropped connection is reopened once and the call repeated; a server that is down is not retried for a minute; a search that found nothing
is not repeated for ten. Servers from the directory ask for approval (`DFAE_APPROVALS=ask`) before any tool that is not read-only;
the output of every MCP tool is scanned for prompt injection like any other. Servers found are remembered in
`~/.local/share/dfae/mcp.json` (outside the workspace, so the agent cannot add servers by writing a file) and reopened at start.

**Authenticating and adding servers: mcpc.** Finding a server is the engine's (Glama, above); authenticating it and keeping it open
is [mcpc](https://github.com/apify/mcpc)'s. Every remote server the engine uses is an mcpc session (`@dfae-<server>`) with a local proxy
(`mcpc connect <url> --proxy`): the engine's client calls the server through that proxy with a bearer token it makes up per session,
so the server's credentials never reach the engine, and token refresh, keepalive and reconnecting (a session that died is opened again
on the next call) are mcpc's. In compose mcpc runs in its own container, the **`mcpc` service** (`dfae-server mcpc-broker`, mcpc
`0.8.0`, its own user and volume): without a keyring mcpc keeps tokens in files, and the agent's commands run as the engine's user, so
they must not be in the engine's container. `./dfae` makes the token the two share (`DFAE_MCPC_BROKER_TOKEN` in `.env`). Run the engine
directly and it runs mcpc itself if installed (`npm install -g @apify/mcpc@0.8.0`; its state in `~/.local/share/dfae/mcpc`), which is
fine on your own machine but lets the agent's commands read the sign-ins; without mcpc, only servers that need no sign-in are used.

**Signing in (OAuth).** A server that wants you to sign in is offered in the chat as a link: `sign in  Linear needs you to sign in. Open
this link, sign in, and the task goes on by itself`. The link is `mcpc login`'s: mcpc does what the [MCP authorization spec](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization)
asks — protected resource metadata (RFC 9728) and authorization server metadata (RFC 8414 / OpenID), its hosted Client ID Metadata
Document or else dynamic client registration (RFC 7591), PKCE (S256) and resource indicators (RFC 8707). The provider then sends your
browser to mcpc's callback, `http://127.0.0.1:13316/callback` (or 31613, 16133): compose publishes those ports on the host's loopback to
the broker's relay, which hands the answer to the sign-in it belongs to — only one the engine started, only with its `state`. When your
browser is on another machine than the engine, the page it lands on does not load: paste its address into the chat (the TUI knows it;
`/signin <address>`; `POST /v1/mcp/sign-ins {"url"}`), or bring its query to `DFAE_PUBLIC_URL/v1/mcp/oauth/callback`, or forward the
port (`ssh -L 13316:127.0.0.1:13316 host`). The task waits (`waiting_for_user`, `DFAE_MCP_SIGN_IN_WAIT_SECS`, default 10 minutes; then it
goes on without) and goes on by itself once mcpc has the tokens. A sign-in that no longer works (mcpc's session `unauthorized`) is
forgotten (`mcpc logout`) and asked for again. Checked against the real directory: Linear's server gives mcpc a sign-in link (its CIMD
client). Some providers accept only clients they approved and refuse self-registration (mcpc says so; its own docs name Figma's
server); they cannot be signed in to without a client id they issued. `POST /v1/mcp/servers/{name}/sign-in` starts a sign-in to a server you added.

Your own servers go in a file in the usual shape (`DFAE_MCP_SERVERS=/etc/dfae/mcp.json`):
`{"mcpServers": {"files": {"command": "npx", "args": ["-y", "…"], "env": {"TOKEN": "${TOKEN}"}}, "docs": {"url": "https://…/mcp", "headers": {"Authorization": "Bearer ${DOCS_KEY}"}, "always": true}}}`.
Only that file can start a local command (a stdio server gets a bare environment plus its own `env`, not the engine's keys); `always: true` offers
a server's tools to every task. `GET /v1/mcp/servers` lists what is connected, `GET /v1/mcp/search?q=…` searches the directory,
`POST /v1/mcp/servers {"name","url","headers"}` adds a public remote server by hand and `DELETE /v1/mcp/servers/{name}` removes one.
Glama's data licence asks for credit and a link to each server's listing: events and API answers carry both ("MCP data from Glama").

### Answers

Answers are terse by design: the final answer gives only the information asked for — no preamble, no recap of steps, no offers of help,
no questions. Prompts say so, and a deterministic pass removes openers ("Sure!") and trailing offers ("Let me know if…") the model adds anyway.

Things to try: `hi` · `what is a mutex?` · `search the web for rust async book` · `weather in Paris` (found on the web) · `what is 12 * (3 + 4)?` · `list the files in ./ and what time is it` ·
`write 'hello' to notes/hello.txt` · `remember that the staging db is db-staging-01`
then `what do you remember about the staging db?` · with the worker model up:
`read notes/hello.txt and tell me how many words it contains`.

If you update the code, rebuild **both** images: `./dfae` does (`docker compose up -d --build` alone does not rebuild the TUI,
which is behind a compose profile — an old TUI cannot read a newer server's task list and the UI then looks empty; the TUI now
says so in the status bar).

### CLI / API

```bash
dfae --ask "what is 12 * (3 + 4)?"          # one-shot; prints the answer, exits 0/1
curl -s localhost:8080/v1/tasks -d '{"goal":"what time is it"}' -H 'content-type: application/json'
curl -N localhost:8080/v1/tasks/<id>/events   # SSE: replays history, then live
```

`/health` (liveness) · `/v1/health` · `/v1/info` · `/v1/tasks` (POST/GET) · `/v1/tasks/{id}` ·
`/v1/tasks/{id}/cancel` · `/v1/tasks/{id}/approvals/{aid}` · `/v1/tasks/{id}/events` ·
`/v1/tasks/{id}/audit` · `/v1/events` · `/v1/board` · `/v1/routines[/{id}[/run]]` · `/v1/agents[/{name}]` · `/v1/dispatch` · `/v1/cost` · `/v1/memory[/search]` · `/v1/stats` · `/v1/calibration` · `/metrics`.
Set `DFAE_API_TOKEN` to require `Authorization: Bearer …` (everything except `/health`).

### Configuration

Copy [`.env.example`](.env.example) to `.env`. The ones that matter:

| Variable | Effect |
|---|---|
| `DECISION_PROVIDER` | `decisions` (default with a gateway key): the hosted Decisions API, `DECISION_API_MODEL=openai/gpt-6-luna-decisions` (measured 98% routing accuracy vs 91% for the rules, ≈ $0.0003/task). `local`: built-in rules only. Unset with `DECISION_BASE_URL` set: a self-hosted model (`DECISION_MODEL`, default `qwen3:4b-instruct-2507-q4_K_M` in Ollama with `COMPOSE_PROFILES=local-decisions`) |
| `DECISION_KINDS` | which decisions the model makes (default `all`; or a list such as `route_task,select_tools,assess_recovery`). The rest use the built-in rules. A failing model is paused for 30 s instead of making every decision wait out a timeout |
| `PLANNER_API_KEY` / `PLANNER_MODEL` / `PLANNER_PROVIDER` / `PLANNER_BASE_URL` | the planner model (default `gpt-6-luna`; provider `openai` = any OpenAI-compatible endpoint, default base URL `https://ai-gateway.vercel.sh/v1`; or `anthropic`) |
| `WORKER_API_KEY` / `WORKER_MODEL` / `WORKER_PROVIDER` / `WORKER_BASE_URL` | the worker model, configured exactly like the planner (default `glm-5.3-flash`; with no key of its own it uses the planner's when they share a base URL) |
| `WORKER_PROVIDERS` | gateway providers allowed to serve the worker (default `baseten,nebius,fireworks`: unquantised, zero data retention; `any`: the gateway chooses) |
| `PLANNER_MAX_TOKENS` / `WORKER_MAX_TOKENS` / `WORKER_CONTENT_MAX_TOKENS` | output limits: a plan; ordinary worker work (800); a file's content (16000) |
| `GLAMA_API_KEY` | turns on MCP discovery (see *MCP servers*); `DFAE_MCP_AUTO=off`, `DFAE_MCP_MAX_AUTO`, `DFAE_MCP_TIMEOUT_SECS`, `DFAE_MCP_STATE`, `DFAE_MCP_KEY_<SLUG>`, `DFAE_MCP_SERVERS` tune it |
| `DFAE_MCPC_BROKER_TOKEN` | shared by the engine and the `mcpc` broker that authenticates and opens MCP servers (`./dfae` makes one); `DFAE_MCPC_BROKER_URL`, `DFAE_MCPC=auto\|off`, `DFAE_MCPC_BIN`, `DFAE_MCPC_HOME`, `DFAE_MCPC_CALLBACK_PORT` tune it |
| `PLANNER_REASONING` / `WORKER_REASONING` | thinking effort of a hosted model: `off`, `low`, `medium`, `high` (both default `low`; GLM-5.3-Flash cannot turn thinking off). `PLANNER_REASONING_HARD` (default `medium`) applies from the second re-plan; `PLANNER_REASONING_ADAPTIVE=on` sets effort from the request's complexity instead (measured worse, off by default) |
| `*_TIMEOUT_SECS`, `PLANNER_PROMPT_CACHE` | request timeouts; whether an endpoint caches repeated prompt prefixes (known: Anthropic, `api.openai.com`, Vercel AI Gateway; `on` declares another; three misses turn the cacheable prefix off) |
| `DFAE_EMBEDDINGS` | semantic memory embeddings (default `voyage/voyage-4-lite` through the gateway; `hash` turns them off). `DFAE_FACTS=off` stops durable facts being extracted from verified tasks |
| `DFAE_REDACT_PII` / `DFAE_PII_NER_URL` | personal data (e-mail, phone, card numbers, IBANs) is redacted before anything is kept (default on); with a GLiNER PII service, names and street addresses too |
| `DFAE_STEP_AGENT` / `DFAE_STEP_AGENT_STYLE` | the worker fixes a failed step with a short loop before the planner is asked (default on; `script`, one script per action, measured half the actions and −47% cost vs `tools`) |
| `DFAE_ROUTINES` / `DFAE_TIMEZONE` / `DFAE_ROUTINE_CONCURRENCY` / `DFAE_ROUTINE_MAX_RUNS_PER_DAY` | the routine scheduler (on), the zone of clock times (UTC), runs at once (4), runs started per day (200) |
| `DFAE_EXPOSED_CHANGES` | `allow` lifts the guard on changing existing files and sending local data out after outside content was read (see *Security*) |
| `DFAE_API_TOKEN` | bearer token for the API |
| `DFAE_APPROVALS` | `never` (default): nothing waits for a human. `ask`: risky calls wait for approval |
| `DFAE_ALLOW_AUTO_APPROVE` | with `ask`: allow `auto_approve` requests (benchmarks only) |
| `DFAE_MAX_STEPS`, `_MAX_MODEL_CALLS`, `_MAX_TOOL_CALLS`, `_MAX_REPLANS`, `_MAX_SECONDS`, `_MAX_CHILDREN` | per-task budgets |
| `DFAE_REPAIR` | `off` disables deterministic repair |
| `DFAE_LAZY_CONTEXT` | `on` enables lazy upstream context (`NEED <step>` expansion); off by default |
| `DFAE_PLAN_FALLBACK` | `worker`: let the worker model plan when the planner cannot be reached (default: the task fails with the reason) |
| `DFAE_PLAN_PREFIX` | `narrow`/`off`: per-task tool list in the user message; `always`: send the cacheable prefix on every planner call; default: prefix only while the provider's cache is warm |
| `DFAE_PLAN_REPORT` | `on` lets the planner pre-write the final reply (off: measured to cost more than a worker-written answer) |
| `DFAE_ANSWER_LOCAL_FIRST` | `off` sends every final answer to the planner instead of the worker model |
| `DFAE_WORKSPACE_SNAPSHOT`, `DFAE_CONDENSE` | `off` disables the workspace line in plan prompts / line-selection condensation of big outputs |
| `DFAE_DECISION_LOG` | JSONL file of decisions and their outcomes (evaluation / training data) |
| `DFAE_SHADOW_EVAL` | 0..1: share of requests whose routing the planner also decides, to measure the decision model (spends planner tokens; default 0) |
| `DFAE_TEAM` | `off` disables automatic delegation |
| `DFAE_MAX_CONCURRENT_TASKS` / `DFAE_MAX_QUEUED_TASKS` | admission control: tasks running at once / waiting before load shedding (8 / 64) |
| `DFAE_MAX_AGENTS` / `DFAE_ROLE_SLOTS` | subagents running at once (8) / per-role pools (`researcher=4,operator=1,analyst=4,verifier=2`) |

Without Docker: `cargo run --release -p dfae-server` (in-memory storage unless `DATABASE_URL` is set), then
`cargo run --release -p dfae-tui`. NVIDIA GPU for a self-hosted decision model: add `-f compose.gpu.yaml`.

### What works without what

| Missing | Behaviour |
|---|---|
| decision model (unreachable, no key, or `DECISION_PROVIDER=local`) | local rule-based decisions (same typed questions, visible in the *Cognition* tab) |
| GLiNER (not enabled, the default) | rule extraction (paths, URLs, versions, expressions, constraints, …) |
| semantic embeddings (no key, or `DFAE_EMBEDDINGS=hash`) | hash embeddings only: exact and near-exact wording is found, paraphrases often not |
| worker model | the planner does its calls (and a task without any model runs only what binds deterministically) |
| planner model | the worker model plans (concrete steps, as before); with `PLANNER_API_KEY` set and the planner unreachable the task fails with the reason |
| Postgres | volatile in-memory storage (the compose stack always includes Postgres) |

## Development

```bash
cargo test --workspace                       # ~400 tests, hermetic (live suites are #[ignore]d: see docs/eval)
DFAE_TEST_DATABASE_URL=postgres://… cargo test -p dfae-memory --test pg
cd services/gliner-service && pytest tests   # needs fastapi, httpx, pytest
scripts/bench.sh                             # token/mode table over examples/tasks.txt
```

## Status — what is verified and what is not

| Area | State |
|---|---|
| Rust workspace: build, `clippy` clean, ~400 unit/integration/e2e tests | verified |
| Pipelines, planning repair, replanning, retries, approvals, policy, injection taint, the outside-content guard, skills, resume, budgets, cancellation, teams | verified with scripted models (`crates/dfae-runtime/tests/`) |
| The whole system against the real hosted models (`gpt-6-luna` planner, `glm-5.3-flash` worker, the hosted Decisions API, `voyage-4-lite` embeddings, Glama MCP discovery) | **run live**: a suite of harder end-to-end tasks with oracles that check the outcome (bug fixes with tests, a Next.js build, served pages, cross-session memory, MCP servers, AgentDojo-style injections), interleaved A/B runs for every change that claims a gain — results and costs in [`docs/eval/validation-2026-11-27.md`](docs/eval/validation-2026-11-27.md), status per item in [`docs/upgrade-plan.md`](docs/upgrade-plan.md) §16 |
| Postgres store with pgvector (semantic column, backfill), audit chain | verified against PostgreSQL 16 + pgvector |
| GLiNER and GLiNER-PII services with the real models | verified (PII: held-out set 14/14 names and addresses found, 0/12 lookalikes touched) |
| Self-hosted open decision model | tested against a **fake** OpenAI-compatible server returning logprobs; not run against a real local model here (no GPU) |
| Quality of automatic team splits with real models | **unmeasured** |
| `web_search` | best effort: DuckDuckGo may block automated requests (Wikipedia answers instead); SearXNG or Brave are dependable |
| `docker compose up` | `compose config` validates; images were **not built or run here** (no Docker daemon) |

### Not implemented (from the blueprint)

an MCP *server* (the client is implemented), agent-to-agent messaging, nested teams (agents cannot spawn agents), task priorities / work stealing, browser and microVM/WASM sandboxes (the container is
the isolation boundary), OpenTelemetry (Prometheus metrics, structured logs and the audit chain
instead), distributed workers, trajectory export / fine-tuning pipeline, multi-user authentication.

## License

Apache-2.0
