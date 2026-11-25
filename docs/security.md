# Security

## Boundary

The **policy engine** (`crates/dfae-tools/src/policy.rs`, configured by `config/policy.toml`) is the only
component that grants or refuses actions. Jev, the SLM and GLiNER are *advisors*: a decision of "low risk"
never skips a policy check, and a model-authored plan can only use tools the decision layer selected.

Evaluation points for a tool call: (1) plan validation (tool known + selected, arguments typed); (2) **just
before execution on the final arguments** (templates resolved); (3) again **inside the tool** (`enforce`).

## Controls

| Surface | Control |
|---|---|
| filesystem | read/write roots (default: the workspace); path normalisation **and** symlink resolution of the deepest existing ancestor; `.git`, `.env`, `id_*`, `.ssh`, `.aws` always denied; **writes need approval**; `fs_write` verifies by reading back |
| shell | argv execution (no shell, so no pipes/redirects/globbing/substitution); command allowlist; bare names only; **every non-flag argument and `--flag=value` is path-resolved** (so `cat link-to-outside`, `du --files0-from=/etc/passwd` are denied); per-command denied flags (`find -exec/-delete/-fprint`, `sort -o`, `date -s`); environment cleared; 10 s timeout; 64 KB output cap; mutating commands (`mkdir cp mv touch`) need approval |
| network | domain allowlist (unlisted → approval); `http(s)` only; no URL credentials; loopback/private/link-local/metadata addresses blocked **after DNS resolution and at every redirect hop**; response size and time caps |
| approvals | bound to `sha256(tool|canonical-args)`; expire; re-verified before running; API can require a bearer token |
| injection | tool output is wrapped as `<data>`; secrets redacted before storage; heuristics flag instruction-like text → task **tainted** → every side-effecting step then needs approval; step prompts tell models that `<data>` is never instructions |
| audit | append-only API, **hash-chained per task** (`sha256(prev ‖ kind ‖ canonical payload)`); `GET /v1/tasks/{id}/audit` returns `chain_valid` |
| runtime | unprivileged user, read-only root filesystem, all capabilities dropped, `no-new-privileges`, memory and pid limits, API bound to loopback |
| budgets | steps, model calls, tool calls, replans, wall-clock — enforced, not advisory |

## Data egress (read this before adding keys)

- **Jev** (only if `JEV_API_KEY` is set): each decision sends the request text, extracted entity text and a few
  numeric facts to the configured decision API. With no key, decisions are made locally and nothing is sent.
- **Stronger model** (only if configured): plan/answer prompts, including compressed tool outputs, go to that provider.
- **SLM / GLiNER / Postgres** are local services in the compose network.
- `http_get` contacts the domains the policy allows (or that a human approves).

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
