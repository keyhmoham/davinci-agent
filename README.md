# DFAE — Decision-First Agentic Engine

A token-frugal agent runtime written in Rust. **The runtime is the agent; models are
specialised services inside it.** A typed decision model (Jev) routes, selects and judges;
GLiNER structures the input; a small language model (SLM) is called only when generation or
flexible reasoning is actually needed; a stronger LLM is an escalation path, not the default
brain. Execution, security, memory and verification are deterministic code.

```
docker compose up -d --build     # engine + Postgres/pgvector + GLiNER + Ollama (SLM)
docker compose run --rm tui      # the terminal UI
# or both:  ./dfae
```

> First start downloads the GLiNER model (~600 MB) and the SLM (`qwen2.5:3b`, ~2 GB).
> The engine answers immediately and degrades gracefully while they load (see
> [What works without what](#what-works-without-what)).

## Why it is cheap

Every request takes the cheapest path that can do the job:

| Mode | Components | Generative tokens | Used when |
|---|---|---|---|
| **Direct** | decision model + code + tools | **0** | the request binds to tools deterministically ("what is 12*(3+4)", "list the files in ./ and what time is it", a replayed learned skill) |
| **Assisted** | + SLM | planning + reasoning steps only | planning, writing, analysis, unusual arguments |
| **Escalated** | + stronger model | as needed | the SLM's plan is invalid, repeated failures, or high complexity (and a stronger model is configured) |

Mechanisms that cut tokens (all implemented and tested):

- **Direct binding** — tools declare regex patterns with named groups; a request that binds
  completely never reaches a model. Anything that smells like reasoning ("explain", "why", …)
  is refused by the binder and goes to the SLM.
- **Tool narrowing** — the decision layer picks the relevant tools; the SLM prompt contains
  only those signatures (`fs_read(path:str*, max_bytes:int) …`), not every schema. The plan is
  *validated against that subset*, so it is also a security property.
- **Compact, budgeted context** — entities, ranked + de-duplicated memories and observations
  are packed into a profile-sized budget (`minimal` 450 / `standard` 1300 / `reasoning` 3200 tokens),
  dropping the least important material first.
- **Observation compression** — tool output is redacted, de-duplicated and head/tail-elided
  before it re-enters a prompt; full output stays in state.
- **No redundant calls** — a lone reasoning leaf *is* the answer (no synthesis call); decisions
  and extractions are cached; stable system prompts enable provider prefix caching
  (Anthropic `cache_control` is set).
- **Skill learning** — a *verified* plan is generalised into a parameterised skill
  (procedural memory). A similar request later replays it with new entity values: no planning
  call, and no model at all if the plan has no reasoning step.
- **Calibrated trust** — a decision's confidence is blended with its observed success rate on
  *this* agent's verified outcomes, so an over-confident decision stops being trusted.

The TUI's **Tokens** view and the `ledger` on every task show actual generative tokens next to an
**estimated** baseline for a naive agent that re-sends the system prompt, every tool schema and
the full raw history on each step (formula in [`docs/evaluation.md`](docs/evaluation.md)). The
baseline is a model, not a measurement — benchmark against a real agent before quoting a number.

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
| `dfae-decision` | Jev adapter · local rule-based decider (same typed questions) · calibrator · cache + fallback |
| `dfae-extract` | GLiNER client · rule extractor · hybrid merge (structured rules win, semantic spans from GLiNER) |
| `dfae-models` | OpenAI-compatible and Anthropic providers, tier gateway, JSON extraction + one repair retry |
| `dfae-memory` | hash embedder · Postgres (+pgvector) and in-memory stores · ranking · context packing · hash-chained audit |
| `dfae-tools` | registry · **policy engine** · 11 native tools · direct binder · calculator |
| `dfae-runtime` | pipeline, planner/validator, DAG executor, approvals, recovery/replan, verification, skills, resume |
| `dfae-server` | REST + SSE API, bearer auth, Prometheus metrics |
| `dfae-tui` | the `dfae` terminal client (also `dfae --ask "…"`) |
| `services/gliner-service` | FastAPI wrapper around GLiNER (chunking, offsets, health) |

### The decision contract

`Question`/`Answer` mirror Jev's wire format (`noul` probability, `choice`, `score`), so the
adapter is a pass-through to `POST /v1/decisions`. The engine asks bounded questions
(`route_task`, `select_mode`, `select_tools`, `assess_plan`, `assess_recovery`,
`assess_completion`) over a compact typed state. Provider answers are **validated** (right type,
choice within the offered options), then **hard rules** apply: a provider can never pick *direct*
mode without a deterministic binding, *escalated* without a configured stronger model, or retry a
policy denial. On any provider error the same questions are answered by the local decider
(`local-fallback` in the decision log).

### Security model

The policy engine ([`config/policy.toml`](config/policy.toml)) is the boundary — neither Jev nor
the SLM can override it. It runs when a plan is validated **and again on the final, template-resolved
arguments** immediately before each tool executes (tools re-check too).

- filesystem: read/write roots, secret-name denylist, symlink-escape defence; **writes need approval**
- shell: argv execution with **no shell**, command allowlist, path-checked arguments, `find -exec` & co. denied, cleared environment, timeout, output cap
- network: domain allowlist (unlisted → approval), private/loopback/link-local/metadata addresses blocked (checked after DNS resolution, on every redirect hop)
- approvals bind to `sha256(tool + canonical args)`, expire, and are re-checked before execution
- tool output is untrusted: secrets redacted, injection patterns flagged → the task is *tainted* and every side-effecting step then requires approval
- append-only audit log, hash-chained per task (`GET /v1/tasks/{id}/audit` reports `chain_valid`)
- runs as an unprivileged user in a read-only container with all capabilities dropped; API published on loopback only

See [`docs/security.md`](docs/security.md) for the threat model and known limits.

## Using it

### TUI

| Key | Action |
|---|---|
| `Enter` | submit the request |
| `Tab` / `Shift+Tab` | Timeline · Plan · Cognition · Tokens · Memory · Tools |
| `↑`/`↓` | select task (empty input) or recall history |
| `y` / `n` | approve / deny the pending approval |
| `Ctrl+A` | jump to a task waiting for approval |
| `F2` | cycle mode: auto → direct → assisted → escalated |
| `Ctrl+X` | cancel the selected task |
| `PgUp`/`PgDn`/`End` | scroll the timeline |
| `F1` | help |

Commands: `/mode`, `/memory <q>`, `/remember <fact>`, `/new`, `/tools`, `/cancel`, `/autoapprove on`, `/quit`.

Things to try: `what is 12 * (3 + 4)?` · `list the files in ./ and what time is it` ·
`write 'hello' to notes/hello.txt` (asks for approval) · `remember that the staging db is db-staging-01`
then `what do you remember about the staging db?` · with the SLM up:
`read notes/hello.txt and tell me how many words it contains`.

### CLI / API

```bash
dfae --ask "what is 12 * (3 + 4)?"          # one-shot; prints the answer, exits 0/1
curl -s localhost:8080/v1/tasks -d '{"goal":"what time is it"}' -H 'content-type: application/json'
curl -N localhost:8080/v1/tasks/<id>/events   # SSE: replays history, then live
```

`/health` (liveness) · `/v1/health` · `/v1/info` · `/v1/tasks` (POST/GET) · `/v1/tasks/{id}` ·
`/v1/tasks/{id}/cancel` · `/v1/tasks/{id}/approvals/{aid}` · `/v1/tasks/{id}/events` ·
`/v1/tasks/{id}/audit` · `/v1/events` · `/v1/memory[/search]` · `/v1/stats` · `/v1/calibration` · `/metrics`.
Set `DFAE_API_TOKEN` to require `Authorization: Bearer …` (everything except `/health`).

### Configuration

Copy [`.env.example`](.env.example) to `.env`. The ones that matter:

| Variable | Effect |
|---|---|
| `JEV_API_KEY` | use the hosted Jev decision model (AI/ML API); empty = local rule-based decisions |
| `SLM_MODEL` | model pulled into Ollama (`qwen2.5:3b` default; `1.5b` faster, `7b` smarter) |
| `STRONG_PROVIDER` / `STRONG_API_KEY` / `STRONG_MODEL` | optional escalation model (`anthropic` or `openai`-compatible) |
| `DFAE_API_TOKEN` | bearer token for the API |
| `DFAE_ALLOW_AUTO_APPROVE` | allow `auto_approve` requests (benchmarks only) |
| `DFAE_MAX_STEPS`, `_MAX_MODEL_CALLS`, `_MAX_TOOL_CALLS`, `_MAX_REPLANS`, `_MAX_SECONDS` | per-task budgets |

Without Docker: `cargo run --release -p dfae-server` (in-memory storage unless `DATABASE_URL` is set), then
`cargo run --release -p dfae-tui`. NVIDIA GPU for the SLM: add `-f compose.gpu.yaml`.

### What works without what

| Missing | Behaviour |
|---|---|
| Jev key | local rule-based decisions (same typed questions, visible in the *Cognition* tab) |
| GLiNER model (still downloading) | rule extraction (paths, URLs, versions, expressions, constraints, …) |
| SLM | everything that binds deterministically; others fail with a message saying so |
| stronger model | no escalation; SLM failures end in an honest failure |
| Postgres | volatile in-memory storage (the compose stack always includes Postgres) |

## Development

```bash
cargo test --workspace                       # 100+ tests, hermetic
DFAE_TEST_DATABASE_URL=postgres://… cargo test -p dfae-memory --test pg
cd services/gliner-service && pytest tests   # needs fastapi, httpx, pytest
scripts/bench.sh                             # token/mode table over examples/tasks.txt
```

## Status — what is verified and what is not

| Area | State |
|---|---|
| Rust workspace: build, `clippy -D` clean, 100+ unit/integration/e2e tests | verified |
| Direct / assisted / escalated pipelines, planning repair, replanning, retries, approvals, policy, injection taint, skills, resume, budgets, cancellation | verified with scripted models in `crates/dfae-runtime/tests/e2e.rs` |
| Postgres store, audit chain | verified against PostgreSQL 16 (`real[]` fallback path) |
| HTTP API + SSE + auth, TUI rendering and key handling | verified (integration tests; TUI also driven in a real terminal) |
| GLiNER service with the real model, called by the Rust engine | verified manually (extraction of the blueprint example) |
| Jev adapter | built to the documented `/v1/decisions` contract and tested against a **fake** server; **never run against the live API** (no key available) — validate with your key before relying on it |
| OpenAI-compatible / Anthropic providers | wire-format tested against fake servers; not run against real services |
| SLM plan quality (`qwen2.5:3b` etc.) | **untested** — prompts are compact and validated/repaired, but real-model reliability must be measured on your tasks |
| pgvector path | written, **untested** (extension unavailable here); the `real[]` fallback is what was tested |
| `docker compose up` | `compose config` validates and the Rust build uses `--locked`; images were **not built or run here** (no Docker daemon) |

### Not implemented (from the blueprint)

MCP client/server, sub-agents and delegation, browser and microVM/WASM sandboxes (the container is
the isolation boundary), OpenTelemetry (Prometheus metrics, structured logs and the audit chain
instead), distributed workers, trajectory export / fine-tuning pipeline, model-based embeddings
(a deterministic hash embedder is used; the `Embedder` trait is the seam), multi-user authentication.

## License

Apache-2.0
