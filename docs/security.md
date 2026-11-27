# Security

## Read this first: the agent runs without asking

By default (`DFAE_APPROVALS=never`) **no tool call waits for a human** and the agent never asks for confirmation. The policy still
refuses what it forbids; it does not ask about it. In the shipped container policy the `shell` tool is a **full bash**
(`shell.mode = "sandbox"`), so the container is the security boundary: read-only root filesystem, all capabilities dropped,
`no-new-privileges`, unprivileged user, memory and pid limits, API on loopback. The workspace volume is writable by design and the
container has network egress. Content from outside (web pages, MCP servers) is guarded (see *Outside content*): once a task has read
it, changing, moving or removing a file that was there before needs the request to name it, and a value read locally is not sent to the
network. That guard recognises tool calls and the common shell and script forms; a command it does not recognise can still change the
workspace or send data anywhere the container can reach. Do not mount anything you cannot lose into `/workspace`, keep the API token
set if the API is exposed, and use `DFAE_APPROVALS=ask` (and `shell.mode = "allowlist"`) when that is not acceptable.

## Boundary

The **policy engine** (`crates/dfae-tools/src/policy.rs`, configured by `config/policy.toml`) is the only
component that grants or refuses actions. The decision model, the worker model and GLiNER are *advisors*: a decision of "low risk"
never skips a policy check, and a model-authored plan can only use tools the decision layer selected.

Evaluation points for a tool call: (1) plan validation (tool known + selected, arguments typed); (2) **just
before execution on the final arguments** (templates resolved); (3) again **inside the tool** (`enforce`).

## Controls

| Surface | Control |
|---|---|
| filesystem | read/write roots (default: the workspace); path normalisation **and** symlink resolution of the deepest existing ancestor; `.git`, `.env`, `id_*`, `.ssh`, `.aws` always denied; **writes need approval** (with `DFAE_APPROVALS=ask`); `fs_write` verifies by reading back |
| shell (`mode = "sandbox"`, container only) | a full bash in the workspace; `cwd` must stay inside it; timeout (default 120 s, max `max_timeout_secs`), output cap; background jobs killed with the process; no per-command rules — the container is the boundary |
| shell (`mode = "allowlist"`, the built-in default) | argv execution (no shell, so no pipes/redirects/globbing/substitution); command allowlist; bare names only; **every non-flag argument and `--flag=value` is path-resolved** (so `cat link-to-outside`, `du --files0-from=/etc/passwd` are denied); per-command denied flags (`find -exec/-delete/-fprint`, `sort -o`, `date -s`); environment cleared; 10 s timeout; 64 KB output cap; mutating commands (`mkdir cp mv touch`) need approval |
| network | domain allowlist; with `open_web = true` (shipped) other **public** hosts are read-only GETs without approval — unless the task is tainted by a suspected prompt injection, when they need approval again; `http(s)` only; no URL credentials; loopback/private/link-local/metadata addresses blocked **after DNS resolution and at every redirect hop**; response size and time caps |
| approvals | **off by default (`DFAE_APPROVALS=never`)**; with `ask`: bound to `sha256(tool|canonical-args)`; expire; re-verified before running; API can require a bearer token |
| injection | tool output is wrapped as `<data>`; secrets redacted before storage; heuristics flag instruction-like text → task **tainted** → every side-effecting step then needs approval (with `ask`); step prompts tell models that `<data>` is never instructions |
| outside content | see below: extracted from by line number, never summarised; plan written before it is read; step agent off; existing files and local data guarded |
| personal data | e-mail addresses, phone numbers, card numbers (Luhn-checked) and IBANs are redacted before anything is kept in long-term memory or the decision log (`DFAE_REDACT_PII`, default on); with `DFAE_PII_NER_URL` a GLiNER PII model also redacts person names and street addresses. The task's own record keeps the request as asked |
| audit | append-only API, **hash-chained per task** (`sha256(prev ‖ kind ‖ canonical payload)`); `GET /v1/tasks/{id}/audit` returns `chain_valid` |
| runtime | unprivileged user, read-only root filesystem, all capabilities dropped, `no-new-privileges`, memory and pid limits, API bound to loopback |
| budgets | steps, model calls, tool calls, replans, wall-clock — enforced, not advisory |

## Outside content

A web page or an MCP server's answer may carry instructions aimed at the model that reads it, and an instruction that reads like an
ordinary one (a setup page's "delete the tests folder") is not flagged by any scanner. Layers, from the plan down:

1. **Plan-then-execute.** The plan is written before outside content is read; the content never becomes a step by itself. This alone
   stopped every attack that needed a new step (AgentDojo-style cases: set `admin = true` in config.toml, send secrets.txt to a collector).
2. **Extraction, not summary** (`quarantine.rs`). Lines that read like instructions to a model are dropped; if the rest is long, the
   worker answers with line numbers and code passes those lines on verbatim, so the content cannot reach the planner in a model's words.
3. **No step agent.** The fix loop never runs on a task that has read outside content (measured: it worked around a refused removal by
   moving the folder, then by a script).
4. **Existing files and local data** (`dfae_tools::changes`, `exposed_change` in the executor). From then on, a call that changes,
   moves, empties or removes a file that existed before the task, and that the request does not name (the path, or a directory it is in),
   needs approval; with `DFAE_APPROVALS=never` it is refused, the planner is told why and plans around it, and a second attempt is skipped
   and reported. A value read locally (a key, a token, an id) that turns up in a network call, or a shell command that uploads a file or a
   command's output, is held back the same way. Files the task created are free. `DFAE_EXPOSED_CHANGES=allow` lifts this.
5. **Verification** asks the completion judge whether the request was met when such a task changed something, instead of trusting the
   steps' own checks (what "done" means came from the content).

Measured (`docs/eval/validation-2026-11-27.md`, round 8): a setup page's injected `rm -rf tests` was carried out in 11 of 11 runs without
layer 4 and in 0 of 15 with it, while the page's real steps were done. **Not covered:** changes to files the request names (an injected
edit of a named file goes through), shell constructs the parser does not recognise, data encoded before it is sent, and anything a
task does when `allow` is set.

## Subagents

Delegation never widens authority. An agent's **tool scope** comes from its role (researcher/verifier/analyst are read-only
or tool-less) and is checked at tool selection, at direct binding, when replaying skills, and again immediately before
execution; the policy engine then applies exactly as for a top-level task. Taint (suspected injection) and auto-approve are
inherited from the coordinator, and an agent's taint flows back, so a poisoned observation cannot be laundered through a
child. Agents cannot spawn agents; each gets a slice of the coordinator's budget; cancelling the coordinator cancels them
all; side-effecting agents are serialised by the `operator` pool. Approvals raised by an agent appear on that agent and
are answered there (the TUI's `Ctrl+A` finds them). Known limit: a role is chosen heuristically from the sub-task's text and
routing, so a mis-classified state-changing request fails closed (its write tool is outside a read-only scope) rather than open.

## Data egress (read this before adding keys)

- **Decision model**: by default the hosted Decisions API on Vercel AI Gateway: each decision sends the request text, extracted entities,
  recent turns and a few facts there. For nothing to leave the machine, run a self-hosted model (`COMPOSE_PROFILES=local-decisions`,
  `DECISION_BASE_URL=http://ollama:11434/v1`) or use built-in rules (`DECISION_PROVIDER=local`).
- **Planner and worker models** (hosted, through the gateway by default): plan, step, content and answer prompts, including summaries
  and selected lines of tool outputs, go to the gateway and the provider serving each model. The worker is pinned to providers with zero
  data retention (`WORKER_PROVIDERS`).
- **Memory embeddings** (default `voyage/voyage-4-lite` through the gateway): the text of every memory item (after PII redaction) is
  sent to be embedded. `DFAE_EMBEDDINGS=hash` keeps it local.
- **MCP directory** (with `GLAMA_API_KEY`): a few words describing what a request needs are sent to Glama's search; the servers found
  receive the tool calls made to them.
- **GLiNER / GLiNER-PII / Postgres** are local services in the compose network.
- `http_get` contacts the domains the policy allows; with `open_web = true` any public host (never private ranges). `web_search` calls fixed third-party backends (SearXNG/Brave if configured, DuckDuckGo, Wikipedia) through the same private-address and size/time guards. What you search for, and any URL you fetch, leaves the machine.

## Known limits

- **No kernel-level sandbox** for tool processes beyond the container, an unprivileged user and rlimits-by-container
  (`mem_limit`, `pids_limit`). A tool can still burn CPU for its timeout. For hostile workloads use gVisor/Firecracker.
- **DNS rebinding**: the address is validated, then the HTTP client resolves again. A malicious DNS server could
  answer differently the second time. Pin resolved IPs if this matters.
- **Approvals have no user identity**: whoever holds the API token can approve. `decided_by` records the client name only.
- **Audit tamper-evidence, not tamper-proofing**: a database administrator can rewrite a whole chain. Ship the head
  hash to external storage for stronger guarantees.
- **Injection and secret detection are heuristics**, and so is recognising what a shell command changes. They reduce risk; the approval
  gate, the policy and the container are what bound it.
- **Allowlisted binaries are trusted**: the shell allowlist denies the flags known to write/execute, but review it for
  your base image before widening it.
- Default policy trusts the `example.com`, GitHub, Wikipedia, docs.rs… domains for unattended fetches; narrow it
  for your environment.
- Without `DFAE_API_TOKEN` the API is unauthenticated (the server warns). Compose binds it to `127.0.0.1`.
