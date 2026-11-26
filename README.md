# DFAE — Decision-First Agentic Engine

A token-frugal agent runtime written in Rust. **The runtime is the agent; models are specialised services inside it.**
A hosted **planner model** (default `gpt-6-luna`) writes a typed, validated plan and revises it when something cannot be solved;
a **worker system** carries the plan out step by step: deterministic rules, the extractor (GLiNER), a self-hosted open-weight **decision
model** that routes, selects tools and judges, and a hosted, cheaper **worker model** (default `glm-5.3-flash`) that fills in what the rules
cannot and writes content. Execution, security, memory and verification are deterministic code.

```
docker compose up -d --build     # engine + Postgres/pgvector + GLiNER + Ollama (decision model)
docker compose run --rm tui      # the terminal UI
# or both:  ./dfae
```

> Set `PLANNER_API_KEY` in `.env` (the worker model uses the same gateway and key unless `WORKER_API_KEY` / `WORKER_BASE_URL` say otherwise).
> First start downloads the GLiNER model (~600 MB) and the decision model (`qwen3:4b-instruct-2507-q4_K_M`, ~2.5 GB). Until the decision model is
> ready the built-in rules decide (`./dfae status`, the *Cognition* tab); `./dfae` (TUI) and `./dfae ask` wait for it. With plain
> `docker compose up`, watch `docker compose logs -f ollama-pull` (re-run a failed pull with `docker compose run --rm ollama-pull`).
> The engine answers immediately and degrades gracefully while models load (see [What works without what](#what-works-without-what)).

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
that fail the grounding check) and the **worker** (cheap: argument binding, reasoning steps, content, answers). The decision model and GLiNER are
self-hosted and free, so they get the fullest context. Worker tokens are optimised too: ordinary work (binding arguments, short reasoning, answers)
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
  first. Step outputs reach the planner as ~300-token summaries (condensed by the worker model when big) and the worker as ~600.
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
| `dfae-decision` | open-model provider (typed answers from a self-hosted model's log-probabilities) · local rule-based decider (same typed questions) · calibrator · cache + fallback |
| `dfae-extract` | GLiNER client · rule extractor · hybrid merge (structured rules win, semantic spans from GLiNER) |
| `dfae-models` | OpenAI-compatible and Anthropic providers, tier gateway, JSON extraction + one repair retry |
| `dfae-memory` | hash embedder · Postgres (+pgvector) and in-memory stores · ranking · context packing · hash-chained audit |
| `dfae-tools` | registry · **policy engine** · 11 native tools · direct binder · calculator |
| `dfae-mcp` | MCP client (Streamable HTTP and stdio) · the hub that turns servers' tools into engine tools · **Glama directory** search and on-demand connection |
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

#### The shell is a sandbox the agent may install into

In sandbox mode (`shell.mode = "sandbox"`, what the container image ships) every model that decides what to run is told, in so many words, that it
is in a sandbox it fully controls and **may install whatever a task needs — and must, rather than refuse or ask the user to**. As an
unprivileged user in a read-only container, "install" means user-space installs into a *toolbox* (`{workspace}/.tools`, so it survives restarts)
that is on `PATH` in every command:

| Need | How |
|---|---|
| Python packages | `pip install x` (user installs; `python3 -m venv .venv` for a project) |
| Node | `npm install -g x`, `npx -y x` |
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

Simple phrasings of the file operations bind directly and cost no model tokens (`move a.txt to b/a.txt`, `delete notes/old.txt`,
`replace "foo()" with "bar()" in src/lib.rs`, `find files matching **/*.rs`, ``run `cargo test` ``). The container image ships git, ripgrep, curl, jq, make, gcc, python3 and friends
(`DEV_TOOLS=0` builds it without them); its root is read-only, so use a virtualenv or project-local installs in the workspace.

### MCP servers: found and used on demand

Set `GLAMA_API_KEY` (a key from [glama.ai/settings/api-keys](https://glama.ai/settings/api-keys)) and nothing else. When a request is about an
outside service — GitHub, DeepWiki, Notion, a database, a calendar — the decision model says so (`external_service`; a service named outright
in a request that has something to do counts too, and so does saying "MCP"). The engine then:

1. looks at the servers it already has (the registry: connected earlier in this run, reopened from the previous one, or configured) and uses the
   ones whose tools fit the request;
2. if none does, has the worker model write a **2–5 word search query** (the only thing the directory hears of the request), searches the
   [Glama MCP directory](https://glama.ai/mcp/reference) for remote servers, and connects the best ones that can be used as they are —
   streamable HTTP, no login, not deprecated, not reported down. OAuth servers are skipped; one that wants an API key is used when
   `DFAE_MCP_KEY_<SLUG>` is set;
3. registers their tools as `mcp__<server>__<tool>` (the JSON Schema becomes typed parameters; objects and arrays are written as JSON text and
   parsed before the call) and **attaches them to that task only** — the planner's catalogue for an unrelated request never lists them;
4. the planner plans with them, the worker binds their arguments, and a tool call is a normal step (retried, verified, re-planned).

A dropped connection is reopened once and the call repeated; a server that is down is not retried for a minute; a search that found nothing
is not repeated for ten. Servers from the directory ask for approval (`DFAE_APPROVALS=ask`) before any tool that is not read-only;
the output of every MCP tool is scanned for prompt injection like any other. Servers found are remembered in
`~/.local/share/dfae/mcp.json` (outside the workspace, so the agent cannot add servers by writing a file) and reopened at start.

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
`/v1/tasks/{id}/audit` · `/v1/events` · `/v1/dispatch` · `/v1/cost` · `/v1/memory[/search]` · `/v1/stats` · `/v1/calibration` · `/metrics`.
Set `DFAE_API_TOKEN` to require `Authorization: Bearer …` (everything except `/health`).

### Configuration

Copy [`.env.example`](.env.example) to `.env`. The ones that matter:

| Variable | Effect |
|---|---|
| `DECISION_MODEL` | open-weight decision model pulled into Ollama (`qwen3:4b-instruct-2507-q4_K_M` default); `DECISION_PROVIDER=local` turns it off |
| `DECISION_KINDS` | which decisions the model makes (default `route_task,select_tools,assess_recovery,assess_escalation,assess_completion`; `all` for every kind). The rest — mode, plan check, team split — use the built-in rules. The model is loaded in the background at start (decisions use the rules until it answers), a failure pauses it for 30 s instead of making every decision wait out a timeout, and tool selection is one multiple-choice question over the tools (and `none`) |
| `PLANNER_API_KEY` / `PLANNER_MODEL` / `PLANNER_PROVIDER` / `PLANNER_BASE_URL` | the planner model (default `gpt-6-luna`; provider `openai` = any OpenAI-compatible endpoint, default base URL `https://ai-gateway.vercel.sh/v1`; or `anthropic`) |
| `WORKER_API_KEY` / `WORKER_MODEL` / `WORKER_PROVIDER` / `WORKER_BASE_URL` | the worker model, configured exactly like the planner (default `glm-5.3-flash`; with no key of its own it uses the planner's when they share a base URL) |
| `PLANNER_MAX_TOKENS` / `WORKER_MAX_TOKENS` / `WORKER_CONTENT_MAX_TOKENS` | output limits: a plan; ordinary worker work (800); a file's content (16000) |
| `GLAMA_API_KEY` | turns on MCP discovery (see *MCP servers*); `DFAE_MCP_AUTO=off`, `DFAE_MCP_MAX_AUTO`, `DFAE_MCP_TIMEOUT_SECS`, `DFAE_MCP_STATE`, `DFAE_MCP_KEY_<SLUG>`, `DFAE_MCP_SERVERS` tune it |
| `PLANNER_REASONING` / `WORKER_REASONING` | thinking effort of a hosted model: `off`, `low`, `medium`, `high` (planner default `low`, worker `off`: hidden thinking is billed and eats the output limit; a gateway that rejects the field is asked without it from then on) |
| `*_TIMEOUT_SECS`, `PLANNER_PROMPT_CACHE` | request timeouts; declare that an OpenAI-compatible endpoint caches repeated prompt prefixes (Anthropic and `api.openai.com` are known to) |
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
| `DFAE_WORKSPACE_SNAPSHOT`, `DFAE_CONDENSE` | `off` disables the workspace line in plan prompts / local condensation of big outputs |
| `DFAE_DECISION_LOG` | JSONL file of decisions and their outcomes (evaluation / training data) |
| `DFAE_SHADOW_EVAL` | 0..1: share of requests whose routing the planner also decides, to measure the decision model (spends planner tokens; default 0) |
| `DFAE_TEAM` | `off` disables automatic delegation |
| `DFAE_MAX_CONCURRENT_TASKS` / `DFAE_MAX_QUEUED_TASKS` | admission control: tasks running at once / waiting before load shedding (8 / 64) |
| `DFAE_MAX_AGENTS` / `DFAE_ROLE_SLOTS` | subagents running at once (8) / per-role pools (`researcher=4,operator=1,analyst=4,verifier=2`) |

Without Docker: `cargo run --release -p dfae-server` (in-memory storage unless `DATABASE_URL` is set), then
`cargo run --release -p dfae-tui`. NVIDIA GPU for the decision model: add `-f compose.gpu.yaml`.

### What works without what

| Missing | Behaviour |
|---|---|
| decision model (not pulled yet / unreachable / `DECISION_PROVIDER=local`) | local rule-based decisions (same typed questions, visible in the *Cognition* tab) |
| GLiNER model (still downloading) | rule extraction (paths, URLs, versions, expressions, constraints, …) |
| worker model | the planner does its calls (and a task without any model runs only what binds deterministically) |
| planner model | the worker model plans (concrete steps, as before); with `PLANNER_API_KEY` set and the planner unreachable the task fails with the reason |
| Postgres | volatile in-memory storage (the compose stack always includes Postgres) |

## Development

```bash
cargo test --workspace                       # 300 tests, hermetic
DFAE_TEST_DATABASE_URL=postgres://… cargo test -p dfae-memory --test pg
cd services/gliner-service && pytest tests   # needs fastapi, httpx, pytest
scripts/bench.sh                             # token/mode table over examples/tasks.txt
```

## Status — what is verified and what is not

| Area | State |
|---|---|
| Rust workspace: build, `clippy -D warnings` clean, 300 unit/integration/e2e tests | verified |
| Direct / assisted / escalated pipelines, planning repair, replanning, retries, approvals, policy, injection taint, skills, resume, budgets, cancellation | verified with scripted models in `crates/dfae-runtime/tests/e2e.rs` |
| Teams: decomposition, roles + scopes, dispatcher pools, handoff, supervision/retry, failure isolation, budget slices, shared cache, approvals, cancellation, admission control, resume | verified with content-routed fake models (`crates/dfae-runtime/tests/team.rs`, stable over repeated runs) and with the real server/TUI against a fake OpenAI-compatible server |
| Token economy: repair (plan-time and runtime, incl. non-guessing rules), budget gate, learned prices, output/tool contracts, lazy context | verified (`crates/dfae-runtime/tests/economy.rs`) and in the real server (misplaced file repaired with 0 model calls; plans priced; cost model learning) |
| External-token reductions (plan lines, cached prefix, local-first answers, intent skills) | verified by the ablation benchmark with a **scripted** planner and a **simulated** prompt cache (`tests/bench.rs`); real-provider cache-hit rates and real-planner behaviour in the new line format are **unmeasured** |
| Token economy payoff on a *real* model | **unmeasured** — priors come from the shipped prompts; savings per avoided repair (~300–500 tokens) are estimates; lazy context may cost more than it saves for some workloads |
| Quality of automatic splits with a *real* worker model/decision model | **untested** — the decomposer and its decision are deterministic and conservative, but how often real requests split well must be measured on your tasks |
| Conversation (small talk, chat turns, session history, "it"/"that file"/"my city" resolution), file-creation phrasings, planner-slip recovery | verified (`crates/dfae-runtime/tests/chat.rs`) and with the real server |
| `web_search` / `http_get` | parsers unit-tested on fixtures; **run live from the build sandbox**: DuckDuckGo search + page read worked once and was bot-blocked later (Wikipedia answered instead); `http_get` on `wttr.in/<city>?format=3` returned the weather. Whether a *real worker model* plans weather lookups well is **untested**. Third-party services change — treat as best effort |
| Postgres store, audit chain | verified against PostgreSQL 16 (`real[]` fallback path) |
| HTTP API + SSE + auth, TUI rendering and key handling | verified (integration tests; TUI also driven in a real terminal) |
| GLiNER service with the real model, called by the Rust engine | verified manually (extraction of the blueprint example) |
| Open decision model | the provider, prompts, probability extraction, fallback and health are tested against a **fake** OpenAI-compatible server returning logprobs; **never run against a real model here** (no Ollama/GPU in the build sandbox). How accurate `qwen3:4b-instruct-2507` is on these questions — and whether it beats the local rules — is **unmeasured**: compare on your own requests (the *Cognition* tab shows each decision and its provider) |
| OpenAI-compatible / Anthropic providers | wire-format tested against fake servers; not run against real services |
| Planner and worker models (`gpt-6-luna`, `glm-5.3-flash`) | wire format and the typed plan/patch machinery are tested with scripted and metered models; **never run here against the real hosted models** — `cargo test -p dfae-runtime --test live_worker -- --ignored` runs the worker system against them (and, with `LIVE_PLANNER=1`, the planner too) with your keys |
| pgvector path | written, **untested** (extension unavailable here); the `real[]` fallback is what was tested |
| `docker compose up` | `compose config` validates and the Rust build uses `--locked`; images were **not built or run here** (no Docker daemon) |

### Not implemented (from the blueprint)

MCP client/server, agent-to-agent messaging, nested teams (agents cannot spawn agents), task priorities / work stealing, browser and microVM/WASM sandboxes (the container is
the isolation boundary), OpenTelemetry (Prometheus metrics, structured logs and the audit chain
instead), distributed workers, trajectory export / fine-tuning pipeline, model-based embeddings
(a deterministic hash embedder is used; the `Embedder` trait is the seam), multi-user authentication.

## License

Apache-2.0
