# DFAE — Decision-First Agentic Engine

A token-frugal agent runtime written in Rust. **The runtime is the agent; models are
specialised services inside it.** A self-hosted open-weight decision model routes, selects and judges;
GLiNER structures the input; a small language model (SLM) is called only when generation or
flexible reasoning is actually needed; a stronger LLM is an escalation path, not the default
brain. Execution, security, memory and verification are deterministic code.

```
docker compose up -d --build     # engine + Postgres/pgvector + GLiNER + Ollama (SLM)
docker compose run --rm tui      # the terminal UI
# or both:  ./dfae
```

> First start downloads the GLiNER model (~600 MB) and the SLM (`qwen2.5:3b`, ~2 GB).
> Until the SLM download finishes, requests fail with `model 'qwen2.5:3b' not found` and `./dfae status` shows `slm: down`.
> `./dfae` (TUI) and `./dfae ask` wait for it; with plain `docker compose up`, watch `docker compose logs -f ollama-pull`
> (re-run a failed pull with `docker compose run --rm ollama-pull`). If you use a different model name, set `SLM_MODEL` in `.env`.
> The engine answers immediately and degrades gracefully while they load (see
> [What works without what](#what-works-without-what)).

## Why it is cheap

## Plan with the best model, act with the local ones

With an external model configured (`STRONG_*`), the work of a medium or complex task is split into two phases that never mix:

1. **Planning** — the external model writes a *brief plan*: a few steps, each an **intent** in one sentence (every concrete path, name, pattern
   or text in it), an optional tool, the steps it depends on, and what the result must show (`expect`). It writes no tool arguments, so the paid
   output stays short. It is used by default for every task the decision model rates medium or higher
   (`DFAE_EXTERNAL_MIN_COMPLEXITY`, default 0.3) that does not bind deterministically — and for conversation of that complexity.
2. **Acting** — the program drives the self-hosted triple, step by step, right before each step runs (so it sees the real results of earlier steps):
   GLiNER and the rules read the sentence for paths, names and quoted text; the tools' own patterns and the entities bind what they can; the
   decision model chooses the tool when the planner left it open and later judges the result against `expect`; the SLM fills in what is still
   missing (and writes the reasoning steps); only if the SLM cannot, the external model is asked for the arguments. A step nothing can carry out
   fails with a precise message and recovery takes over: retry, deterministic repair, then a **re-plan by the external model**. Every
   implementation is recorded (`step_implemented` event, "done by" in the Plan tab: `rules`, `rules+slm`, `strong`, …).

So the external model spends tokens on *thinking about the task* (plan, re-plans, the final answer, steps it flags `strong`), and the free,
self-hosted components do the work. Below the threshold, or with no external model, the self-hosted models plan and act on their own as before.

**What counts as cost.** Only the **external** model (`STRONG_*`, paid per token) is a cost. The decision model, GLiNER and the SLM
are self-hosted: their tokens are free, so they are **never** starved to save tokens — they get the fullest context, parameter help and
worked examples in their prompts, and the decision model sees every tool, the whole request and the recent conversation. Token
optimisation (compact prompts, summaries, tool narrowing, the paid-call budget, the cost model) applies to calls that go to the
external model. Budgets are split the same way: `DFAE_MAX_MODEL_CALLS` meters external calls, `DFAE_MAX_LOCAL_CALLS` (default 200)
is only a runaway guard for self-hosted ones. The ledger's `external_tokens` is the number to watch; savings are measured against what a
naive agent would have spent on an external model.

Every request takes the cheapest path that can do the job:

| Mode | Components | Generative tokens | Used when |
|---|---|---|---|
| **Direct** | decision model + code + tools | **0** external | the request binds to tools deterministically ("what is 12*(3+4)", "list the files in ./ and what time is it", a replayed learned skill) |
| **Assisted** | + SLM | 0 external (SLM tokens are free); planning + reasoning steps only | planning, writing, analysis, unusual arguments |
| **Escalated** | + stronger model | the only paid mode: lean prompts, a call budget | the SLM's plan is invalid, repeated failures, or high complexity (and a stronger model is configured) |

Requests that split into sub-tasks are handled by a **team of subagents** that forms automatically
(see [Teams](#teams-subagents-swarm-and-dispatch)); each agent picks its own mode, so a deterministic
sub-task inside a team still costs no generative tokens.

Mechanisms that cut tokens (all implemented and tested):

- **Direct binding** — tools declare regex patterns with named groups; a request that binds
  completely never reaches a model. Anything that smells like reasoning ("explain", "why", …)
  is refused by the binder and goes to the SLM.
- **Tool narrowing** — the decision layer judges every tool; a plan prompt contains only the relevant signatures, not every schema (up to six
  for the paid model, all judged relevant for a self-hosted one, which also gets parameter descriptions). The plan is
  *validated against that subset*, so it is also a security property.
- **Compact, budgeted context (external model)** — entities, ranked + de-duplicated memories and observations
  are packed into a profile-sized budget (`minimal` 450 / `standard` 1300 / `reasoning` 3200 tokens),
  dropping the least important material first. A self-hosted model always gets the `reasoning` profile, step outputs up to ~900 tokens
  (the paid model ~300), eight turns of conversation and worked planner examples.
- **Observation compression** — tool output is redacted, de-duplicated and head/tail-elided
  before it re-enters a prompt; full output stays in state.
- **No redundant calls** — a lone reasoning leaf *is* the answer (no synthesis call); decisions
  and extractions are cached; system prompts are byte-stable so local KV-prefix caches
  (llama.cpp / Ollama) stay warm. Anthropic `cache_control` is sent only when the stable prefix
  clears the provider's minimum cacheable length (historically ~1,024 tokens for Sonnet-class
  models); the shipped ~60–200-token prompts are below it, so hosted prompt caching does not apply to them.
- **Skill learning** — a *verified* plan is generalised into a parameterised skill
  (procedural memory). A similar request later replays it with new entity values: no planning
  call, and no model at all if the plan has no reasoning step.
- **Calibrated trust** — a decision's confidence is blended with its observed success rate on
  *this* agent's verified outcomes, so an over-confident decision stops being trusted.

The TUI's **Tokens** view and the `ledger` on every task show actual generative tokens next to an
**estimated** baseline for a naive agent that re-sends the system prompt, every tool schema and
the full raw history on each step (formula in [`docs/evaluation.md`](docs/evaluation.md)). The
baseline is a model, not a measurement — benchmark against a real agent before quoting a number.

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
   └─ join            (SLM · one call)                      c3 patch     (operator)
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
| **Supervision** | Failure isolation (siblings finish; the join is skipped; partial results are kept). The decision layer retries transient failures with a *fresh* agent, escalates to the stronger model, and never retries a policy denial. |
| **Shared cache** | Agents of a team share read-only tool results (`fs_read`, `fs_list`, `fs_search`, `calc`); *any* side effect by any agent invalidates it, and a read that raced a write is never cached. |
| **Security inheritance** | Taint, auto-approve and the policy engine are inherited — delegation is never a way around an approval. Approvals raised by an agent are answered on that agent. Cancelling the coordinator cancels every agent. |
| **Accounting** | Agents' token ledgers roll up into the coordinator (the team's real cost) without double-counting globally; the baseline models *one* naive agent doing all the work. |
| **Resume** | After a restart a coordinator adopts agents that had finished and marks unknown ones failed — it never silently re-runs work that may have had side effects. |

**What teams buy you, honestly.** Parallelism, context isolation (each agent's prompt holds its own sub-goal and
upstream summary), least privilege, and serialised side effects. Raw token savings appear when sub-goals replace
*planned tool steps* with free deterministic agents (fan-out + join). A pipeline of model-only agents costs *more*
than one plan — every agent pays its own planning call (~330 tokens by default) — which is why the cost model charges
that against splits justified only by parallelism. The TUI **Team** view shows agents, roles, dependencies and the live worker pools.

## Architecture

```
          ┌───────────────────────── dfae (TUI)  ──── HTTP/SSE ────┐
          │                                                        ▼
 request ─▶ extract ─▶ retrieve ─▶ route ─▶ bind | skill ─▶ mode ─▶ plan ─▶ execute ─▶ verify ─▶ answer ─▶ learn
           GLiNER+     memory      decision  (no model)    decision   SLM     DAG        evidence  grounded   skills,
           rules       ranking     model                    model    /strong executor   checks    synthesis  calibration
                                                                              │
                       policy engine (deterministic) ◀── every step, on final args ──▶ approvals · audit chain
```

| Crate / service | Role |
|---|---|
| `dfae-core` | state model, plan schema, events, token accounting, decision contract, provider traits |
| `dfae-decision` | open-model provider (typed answers from a self-hosted model's log-probabilities) · local rule-based decider (same typed questions) · calibrator · cache + fallback |
| `dfae-extract` | GLiNER client · rule extractor · hybrid merge (structured rules win, semantic spans from GLiNER) |
| `dfae-models` | OpenAI-compatible and Anthropic providers, tier gateway, JSON extraction + one repair retry |
| `dfae-memory` | hash embedder · Postgres (+pgvector) and in-memory stores · ranking · context packing · hash-chained audit |
| `dfae-tools` | registry · **policy engine** · 11 native tools · direct binder · calculator |
| `dfae-runtime` | pipeline, planner/validator, DAG executor, approvals, recovery/replan, verification, skills, resume · **team: decomposer, roles, dispatcher, subagent supervision** |
| `dfae-server` | REST + SSE API, bearer auth, Prometheus metrics |
| `dfae-tui` | the `dfae` terminal client (also `dfae --ask "…"`) |
| `services/gliner-service` | FastAPI wrapper around GLiNER (chunking, offsets, health) |

### The decision contract

`Question`/`Answer` are typed like a decision model's wire format (`noul` probability, `choice`, `score`).
The self-hosted provider turns each question into a single-token multiple choice for an open-weight instruct
model and reads the **probability mass of the answer tokens** from the first token's log-probabilities
(`noul` = P(yes)/(P(yes)+P(no)); `choice` = lettered options; `score` = expected level), so answers are real
probabilities and always schema-valid — one prompt and one generated token per question; all questions of a
decision share the same state prefix, so prompt caching helps. The engine asks bounded questions
(`route_task`, `select_mode`, `select_tools`, `assess_plan`, `assess_recovery`,
`assess_completion`) over a compact typed state. Provider answers are **validated** (right type,
choice within the offered options), then **hard rules** apply: a provider can never pick *direct*
mode without a deterministic binding, *escalated* without a configured stronger model, or retry a
policy denial. On any provider error the same questions are answered by the local decider
(`local-fallback` in the decision log).

### Security model

The policy engine ([`config/policy.toml`](config/policy.toml)) is the boundary — neither the decision model nor
the SLM can override it. It runs when a plan is validated **and again on the final, template-resolved
arguments** immediately before each tool executes (tools re-check too).

- **no human in the loop (default):** `DFAE_APPROVALS=never` — no tool call waits for anyone and the agent never asks "shall I proceed?". The policy still **refuses** what it forbids (a path outside the workspace, a private address…); it just doesn't ask. `DFAE_APPROVALS=ask` brings the approval step back (writes, mutating commands, unlisted hosts and tainted tasks then wait in the TUI; approvals bind to `sha256(tool + canonical args)`, expire, and are re-checked)
- filesystem: read/write roots (the workspace), secret-name denylist, symlink-escape defence — for every file tool
- shell: in the container, `shell.mode = "sandbox"` gives a **full bash** in the workspace (pipes, redirects, `&&`, git, build/test tools, timeouts, background jobs) and **the container is the boundary** (read-only root, no capabilities, unprivileged user, memory/pid limits). Outside a container the default `allowlist` mode applies: one allowlisted command, no shell, path-checked arguments, `find -exec` & co. denied
- network: private/loopback/link-local/metadata addresses blocked (checked after DNS resolution, on every redirect hop); `open_web` lets `http_get` reach any public host. **The shell's own network access (`curl`, `pip`, `git clone`) is the container's, not this policy's**
- tool output is untrusted: secrets redacted, injection patterns flagged → the task is *tainted* (shown in the audit; with `DFAE_APPROVALS=ask` side-effecting steps then require approval — in `never` mode a successful prompt injection can drive file changes and shell commands inside the sandbox, which is the price of not asking)
- append-only audit log, hash-chained per task (`GET /v1/tasks/{id}/audit` reports `chain_valid`)
- runs as an unprivileged user in a read-only container with all capabilities dropped; API published on loopback only

See [`docs/security.md`](docs/security.md) for the threat model and known limits.

## Using it

### TUI

| Key | Action |
|---|---|
| `Enter` | submit the request |
| `Tab` / `Shift+Tab` | **Chat** (home) · Timeline · Plan · Team · Cognition · Tokens · Memory · Tools |
| `↑`/`↓` | select task (empty input) or recall history |
| `y` / `n` | approve / deny the pending approval |
| `Ctrl+A` | jump to a task waiting for approval |
| `F2` | cycle mode: auto → direct → assisted → escalated |
| `Ctrl+X` | cancel the selected task |
| `PgUp`/`PgDn`/`End` | scroll the timeline |
| `F1` | help |

Commands: `/mode`, `/team auto|on|off`, `/memory <q>`, `/remember <fact>`, `/new`, `/tools`, `/cancel`, `/autoapprove on`, `/quit`.

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

Simple phrasings of the file operations bind directly and cost no model tokens (`move a.txt to b/a.txt`, `delete notes/old.txt`,
`replace "foo()" with "bar()" in src/lib.rs`, `find files matching **/*.rs`, ``run `cargo test` ``). The container image ships git, ripgrep, curl, jq, make, gcc, python3 and friends
(`DEV_TOOLS=0` builds it without them); its root is read-only, so use a virtualenv or project-local installs in the workspace.

### Answers

Answers are terse by design: the final answer gives only the information asked for — no preamble, no recap of steps, no offers of help,
no questions. Prompts say so, and a deterministic pass removes openers ("Sure!") and trailing offers ("Let me know if…") the model adds anyway.

Things to try: `hi` · `what is a mutex?` · `search the web for rust async book` · `weather in Paris` (found on the web) · `what is 12 * (3 + 4)?` · `list the files in ./ and what time is it` ·
`write 'hello' to notes/hello.txt` · `remember that the staging db is db-staging-01`
then `what do you remember about the staging db?` · with the SLM up:
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
`/v1/tasks/{id}/audit` · `/v1/events` · `/v1/dispatch` · `/v1/cost` · `/v1/memory[/search]` · `/v1/stats` · `/v1/calibration` · `/metrics`.
Set `DFAE_API_TOKEN` to require `Authorization: Bearer …` (everything except `/health`).

### Configuration

Copy [`.env.example`](.env.example) to `.env`. The ones that matter:

| Variable | Effect |
|---|---|
| `DECISION_MODEL` | open-weight decision model pulled into Ollama (`qwen3:4b-instruct-2507-q4_K_M` default); `DECISION_PROVIDER=local` turns it off |
| `DECISION_KINDS` | which decisions the model makes (default `route_task,select_tools,assess_recovery,assess_escalation,assess_completion`; `all` for every kind). The rest — mode, plan check, team split — use the built-in rules. The model is loaded in the background at start (decisions use the rules until it answers), a failure pauses it for 30 s instead of making every decision wait out a timeout, and tool selection asks it about the 4 best-ranked tools only |
| `SLM_MODEL` | model pulled into Ollama (`qwen2.5:3b` default; `1.5b` faster, `7b` smarter) |
| `STRONG_PROVIDER` / `STRONG_API_KEY` / `STRONG_MODEL` | optional escalation model (`anthropic` or `openai`-compatible) |
| `DFAE_API_TOKEN` | bearer token for the API |
| `DFAE_APPROVALS` | `never` (default): nothing waits for a human. `ask`: risky calls wait for approval |
| `DFAE_ALLOW_AUTO_APPROVE` | with `ask`: allow `auto_approve` requests (benchmarks only) |
| `DFAE_MAX_STEPS`, `_MAX_MODEL_CALLS`, `_MAX_TOOL_CALLS`, `_MAX_REPLANS`, `_MAX_SECONDS`, `_MAX_CHILDREN` | per-task budgets |
| `DFAE_REPAIR` | `off` disables deterministic repair |
| `DFAE_LAZY_CONTEXT` | `on` enables lazy upstream context (`NEED <step>` expansion); off by default |
| `DFAE_TEAM` | `off` disables automatic delegation |
| `DFAE_MAX_CONCURRENT_TASKS` / `DFAE_MAX_QUEUED_TASKS` | admission control: tasks running at once / waiting before load shedding (8 / 64) |
| `DFAE_MAX_AGENTS` / `DFAE_ROLE_SLOTS` | subagents running at once (8) / per-role pools (`researcher=4,operator=1,analyst=4,verifier=2`) |

Without Docker: `cargo run --release -p dfae-server` (in-memory storage unless `DATABASE_URL` is set), then
`cargo run --release -p dfae-tui`. NVIDIA GPU for the SLM: add `-f compose.gpu.yaml`.

### What works without what

| Missing | Behaviour |
|---|---|
| decision model (not pulled yet / unreachable / `DECISION_PROVIDER=local`) | local rule-based decisions (same typed questions, visible in the *Cognition* tab) |
| GLiNER model (still downloading) | rule extraction (paths, URLs, versions, expressions, constraints, …) |
| SLM | everything that binds deterministically; others fail with a message saying so |
| stronger model | no escalation; SLM failures end in an honest failure |
| Postgres | volatile in-memory storage (the compose stack always includes Postgres) |

## Development

```bash
cargo test --workspace                       # 230+ tests, hermetic
DFAE_TEST_DATABASE_URL=postgres://… cargo test -p dfae-memory --test pg
cd services/gliner-service && pytest tests   # needs fastapi, httpx, pytest
scripts/bench.sh                             # token/mode table over examples/tasks.txt
```

## Status — what is verified and what is not

| Area | State |
|---|---|
| Rust workspace: build, `clippy -D warnings` clean, 230+ unit/integration/e2e tests | verified |
| Direct / assisted / escalated pipelines, planning repair, replanning, retries, approvals, policy, injection taint, skills, resume, budgets, cancellation | verified with scripted models in `crates/dfae-runtime/tests/e2e.rs` |
| Teams: decomposition, roles + scopes, dispatcher pools, handoff, supervision/retry, failure isolation, budget slices, shared cache, approvals, cancellation, admission control, resume | verified with content-routed fake models (`crates/dfae-runtime/tests/team.rs`, stable over repeated runs) and with the real server/TUI against a fake OpenAI-compatible server |
| Token economy: repair (plan-time and runtime, incl. non-guessing rules), budget gate, learned prices, output/tool contracts, lazy context | verified (`crates/dfae-runtime/tests/economy.rs`) and in the real server (misplaced file repaired with 0 model calls; plans priced; cost model learning) |
| Token economy payoff on a *real* model | **unmeasured** — priors come from the shipped prompts; savings per avoided repair (~300–500 tokens) are estimates; lazy context may cost more than it saves for some workloads |
| Quality of automatic splits with a *real* SLM/decision model | **untested** — the decomposer and its decision are deterministic and conservative, but how often real requests split well must be measured on your tasks |
| Conversation (small talk, chat turns, session history, "it"/"that file"/"my city" resolution), file-creation phrasings, planner-slip recovery | verified (`crates/dfae-runtime/tests/chat.rs`) and with the real server |
| `web_search` / `http_get` | parsers unit-tested on fixtures; **run live from the build sandbox**: DuckDuckGo search + page read worked once and was bot-blocked later (Wikipedia answered instead); `http_get` on `wttr.in/<city>?format=3` returned the weather. Whether a *real SLM* plans weather lookups well is **untested**. Third-party services change — treat as best effort |
| Postgres store, audit chain | verified against PostgreSQL 16 (`real[]` fallback path) |
| HTTP API + SSE + auth, TUI rendering and key handling | verified (integration tests; TUI also driven in a real terminal) |
| GLiNER service with the real model, called by the Rust engine | verified manually (extraction of the blueprint example) |
| Open decision model | the provider, prompts, probability extraction, fallback and health are tested against a **fake** OpenAI-compatible server returning logprobs; **never run against a real model here** (no Ollama/GPU in the build sandbox). How accurate `qwen3:4b-instruct-2507` is on these questions — and whether it beats the local rules — is **unmeasured**: compare on your own requests (the *Cognition* tab shows each decision and its provider) |
| OpenAI-compatible / Anthropic providers | wire-format tested against fake servers; not run against real services |
| SLM plan quality (`qwen2.5:3b` etc.) | **untested** — prompts are compact and validated/repaired, but real-model reliability must be measured on your tasks |
| pgvector path | written, **untested** (extension unavailable here); the `real[]` fallback is what was tested |
| `docker compose up` | `compose config` validates and the Rust build uses `--locked`; images were **not built or run here** (no Docker daemon) |

### Not implemented (from the blueprint)

MCP client/server, agent-to-agent messaging, nested teams (agents cannot spawn agents), task priorities / work stealing, browser and microVM/WASM sandboxes (the container is
the isolation boundary), OpenTelemetry (Prometheus metrics, structured logs and the audit chain
instead), distributed workers, trajectory export / fine-tuning pipeline, model-based embeddings
(a deterministic hash embedder is used; the `Embedder` trait is the seam), multi-user authentication.

## License

Apache-2.0
