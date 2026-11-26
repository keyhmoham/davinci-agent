# Security

## Read this first: the agent runs without asking

By default (`DFAE_APPROVALS=never`) **no tool call waits for a human** and the agent never asks for confirmation. The policy still
refuses what it forbids; it does not ask about it. In the shipped container policy the `shell` tool is a **full bash**
(`shell.mode = "sandbox"`), so the container is the security boundary: read-only root filesystem, all capabilities dropped,
`no-new-privileges`, unprivileged user, memory and pid limits, API on loopback. The workspace volume is writable by design and the
container has network egress: a prompt injection that gets past the (heuristic) detector can make the agent read or change anything in
the workspace and send it anywhere the container can reach. Do not mount anything you cannot lose into `/workspace`, keep the API token
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
| filesystem | read/write roots (default: the workspace); path normalisation **and** symlink resolution of the deepest existing ancestor; `.git`, `.env`, `id_*`, `.ssh`, `.aws` always denied; **writes need approval**; `fs_write` verifies by reading back |
| shell (`mode = "sandbox"`, container only) | a full bash in the workspace; `cwd` must stay inside it; timeout (default 120 s, max `max_timeout_secs`), output cap; background jobs killed with the process; no per-command rules — the container is the boundary |
| shell (`mode = "allowlist"`, the built-in default) | argv execution (no shell, so no pipes/redirects/globbing/substitution); command allowlist; bare names only; **every non-flag argument and `--flag=value` is path-resolved** (so `cat link-to-outside`, `du --files0-from=/etc/passwd` are denied); per-command denied flags (`find -exec/-delete/-fprint`, `sort -o`, `date -s`); environment cleared; 10 s timeout; 64 KB output cap; mutating commands (`mkdir cp mv touch`) need approval |
| network | domain allowlist; with `open_web = true` (shipped) other **public** hosts are read-only GETs without approval — unless the task is tainted by a suspected prompt injection, when they need approval again; `http(s)` only; no URL credentials; loopback/private/link-local/metadata addresses blocked **after DNS resolution and at every redirect hop**; response size and time caps |
| approvals | **off by default (`DFAE_APPROVALS=never`)**; with `ask`: bound to `sha256(tool|canonical-args)`; expire; re-verified before running; API can require a bearer token |
| injection | tool output is wrapped as `<data>`; secrets redacted before storage; heuristics flag instruction-like text → task **tainted** → every side-effecting step then needs approval; step prompts tell models that `<data>` is never instructions |
| audit | append-only API, **hash-chained per task** (`sha256(prev ‖ kind ‖ canonical payload)`); `GET /v1/tasks/{id}/audit` returns `chain_valid` |
| runtime | unprivileged user, read-only root filesystem, all capabilities dropped, `no-new-privileges`, memory and pid limits, API bound to loopback |
| budgets | steps, model calls, tool calls, replans, wall-clock — enforced, not advisory |

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

- **Decision model**: each decision sends the request text, extracted entity text and a few facts to the configured
  `DECISION_BASE_URL`. By default that is the `ollama` service in the compose network, so nothing leaves the machine;
  point it at a remote server and the request text goes there. With `DECISION_PROVIDER=local` decisions use built-in rules.
- **Stronger model** (only if configured): plan/answer prompts, including compressed tool outputs, go to that provider.
- **worker model / GLiNER / Postgres** are local services in the compose network.
- `http_get` contacts the domains the policy allows; with `open_web = true` any public host (never private ranges). `web_search` calls fixed third-party backends (SearXNG/Brave if configured, DuckDuckGo, Wikipedia) through the same private-address and size/time guards. What you search for, and any URL you fetch, leaves the machine.

## Known limits

- **No kernel-level sandbox** for tool processes beyond the container, an unprivileged user and rlimits-by-container
  (`mem_limit`, `pids_limit`). A tool can still burn CPU for its timeout. For hostile workloads use gVisor/Firecracker.
- **DNS rebinding**: the address is validated, then the HTTP client resolves again. A malicious DNS server could
  answer differently the second time. Pin resolved IPs if this matters.
- **Approvals have no user identity**: whoever holds the API token can approve. `decided_by` records the client name only.
- **Audit tamper-evidence, not tamper-proofing**: a database administrator can rewrite a whole chain. Ship the head
  hash to external storage for stronger guarantees.
- **Injection and secret detection are heuristics.** They reduce risk; the approval gate and policy are what bound it.
- **Allowlisted binaries are trusted**: the shell allowlist denies the flags known to write/execute, but review it for
  your base image before widening it.
- Default policy trusts the `example.com`, GitHub, Wikipedia, docs.rs… domains for unattended fetches; narrow it
  for your environment.
- Without `DFAE_API_TOKEN` the API is unauthenticated (the server warns). Compose binds it to `127.0.0.1`.
