# Validation log — 2026-11-27 (real models through Vercel AI Gateway)

Planner `gpt-6-luna` (reasoning `low`) and worker `glm-5.3-flash`, both through `https://ai-gateway.vercel.sh/v1`. The decision model is
the built-in rules (no Ollama in this run). Probe scripts were run against the gateway directly; the live suite is
`LIVE_PLANNER=1 cargo test -p dfae-runtime --test live_worker -- --ignored --nocapture`.

## Hypotheses from `docs/upgrade-plan.md`, tested

| Item | Hypothesis | Measured | Verdict |
|---|---|---|---|
| F1 | `reasoning:{enabled:false}` fails, and GLM then thinks at `max` | Accepted (no 400). With the repo's real prompts GLM spends **0–85 reasoning tokens** at the default effort, and up to 325 with `max` on the `zai` provider. `effort:"low"` gives **0** with identical answers (bind, reasoning, code fix, content) | **Mostly refuted.** Switching to `effort:low` is a small gain (latency, a few tokens), not a large saving |
| F2 | Output limits leave no room for thinking | No `length` finishes in any run | **Refuted**: no change needed |
| F3/F6 | Reasoning tokens and cost are invisible | The gateway returns `usage.cost`, `reasoning_tokens`, `cache_creation_input_tokens` | **Done**: the ledger now records all three; the live test prints `$` |
| F4 | The gateway drops `cached_tokens` | Reported on every call (planner cache reads ≈ 2,250 tokens per plan) | **Refuted today**; a defensive fix stays low priority |
| C1 | A schema enum that changes per task breaks the planner cache | +1 tool in the enum → `cached=0`, a full write, **4.7× the cost** of a cached call | **Confirmed** |
| C2 | A different reasoning effort breaks the cache | `low` → `medium` → `cached=0`, a full write | **Confirmed** |
| D1 | The Decisions API answers DFAE's routing questions | 5 questions in one call, 0.4 s, 802 input tokens (≈ $0.00008), calibrated probabilities | **Works** |

## Cost per task (5 live cases × 6 runs)

| Case | $ per task | Dominant cost |
|---|---:|---|
| landing page | $0.0013 – $0.0027 | worker content (≈ 85–95%) |
| edit a config value | $0.00016 – $0.00019 | planner (≈ 60%) |
| read and summarise | $0.00005 – $0.00013 | planner |
| search and explain | $0.00014 – $0.0032 (a re-plan) | planner on re-plans |
| two files | $0.00029 – $0.00038 | worker content |

A plan with a warm cache costs about **$0.0001**. Costs are already very low. What decides the value of further work is **quality**.

## Quality failures found and fixed (not in the original plan)

1. **Questions answered with raw tool output.** In 2 of 6 runs the planner ended a question ("what changed in version 2?") with a
   read step and no reasoning step, and the reply was the raw file. Fix: a routing question `asks_answer` (decision model, with a
   rule fallback). A question answered by tool steps alone now gets a grounded answer written by the worker.
   Files: `answer.rs`, `dfae-decision/{questions,local,decider}.rs`, `Routing::asks`.
2. **Markdown files cut down to their code blocks.** `strip_fences` returned the text between the first and last ``` anywhere in the
   reply, so a README with a code sample was saved as just its code (`git clone … / cd brewster`). Fix: a reply is unwrapped only when
   it is one fenced block, optionally after a one-line preamble. Regression tests added.
3. **Content that ignores what it is about.** A prose or page file that never mentions a name the step gives ("a tool called
   Brewster") is rejected and rewritten with the reason (`missing_names`).

After the fixes: **15/15** live runs pass (3 runs × 5 cases); 2 of 3 runs failed before.

## Round 2: hosted decisions, worker provider pinning, `low` worker effort

**Decisions API (`openai/gpt-6-luna-decisions`) as the default decision provider** (`DECISION_PROVIDER=decisions`, or by default when no
self-hosted model is set and the planner's key reaches the gateway; the rules stay the fallback). Routing accuracy over 5 labels per
request (`needs_tools`, `changes`, `asks`, `smalltalk`, `fresh_data`), `cargo test -p dfae-decision --test routing_eval -- --ignored`:

| Set | Rules | Hosted (as first written) | Hosted + context line + clearer wording |
|---|---:|---:|---:|
| tuning set (36 requests, 180 labels) | 94.4% | 93.9% | 97.8% (the wording was tuned on this set) |
| **held-out set** (30 requests, 150 labels, labelled before any run) | **91.3%** | — | **98.0%** |

* The first hosted run did **not** beat the rules: the model was not told the agent has files, a shell and web tools, so "which files
  mention TODO?" looked like general knowledge (`needs_tools=false`). One context line fixed that.
* On held-out phrasings the rules miss things that matter: "add a dark mode toggle to site/index.html" → no tools (it would be answered
  as chat), "kill the process on port 8001" / "upgrade the dependencies" → no change, "will it rain in London tomorrow" → not fresh.
* Cost: about **$0.0003–0.0004 per task** (3–5 decision calls of ≈ 1,000 input tokens; output is free), about 3× the planner's cost on a
  small task, $0.35 per 1,000 tasks. It replaces the 2.5 GB self-hosted model.
* The `fresh_data` question no longer counts "the state of this machine" (it sent "check disk usage" toward web tools), and
  `asks_answer` no longer counts "list / show / find / check something".

**Worker providers pinned** (`WORKER_PROVIDERS`, default `baseten,nebius,fireworks`: unquantised, zero data retention, no training,
$0.15/$0.50, best p95 latency on the gateway's endpoint list) through `providerOptions.gateway.order` + `only`.
**`WORKER_REASONING=low`** by default (GLM-5.3-Flash cannot switch thinking off; `low` measured 0 reasoning tokens on ordinary steps).

Live suite after these changes: 5/5 × 3 runs pass.

## Round 3: a harder end-to-end suite through the real bootstrap

`crates/dfae-runtime/tests/live_e2e.rs` runs six harder tasks through `build_engine` (the container's sandbox policy, the hosted
decisions, the real planner and worker, automatic routing), each checked by an oracle on the outcome:

| Case | What the oracle checks |
|---|---|
| bug fix against failing tests | `pytest` reports 5 passed, the tests are unchanged |
| CLI with tests | `wordcount.py sample.txt` prints 9, its tests pass |
| CSV analysis | the answer names the right region; `totals.csv` has the right total for every region |
| rename across a JS project | no `calcTotal` left in `src/`, `node test.js` passes |
| a served two-page site | the address in the answer serves the home page, which links to an about page that is served too |
| a question about code | the answer gives the right value (72) |

Failures found, and the fixes (each with a test):

| Seen | Cause | Fix |
|---|---|---|
| a re-plan in 3 of 6 tasks | a one-file step (`fs_read`, `fs_edit`) naming two files: the worker binds the first | `split_multi_file_steps`: one step per existing file, dependents wait for all |
| a rename that failed after 4 re-plans | `fs_edit` "in each file the search found" changes one file; partial edits loop | `fs_edit` with a directory as `path` replaces in every file under it |
| "replanning failed: dependency cycle" | a patch edits a waiting step to run after the new steps, and the new steps wait for it | the back edge is dropped, the edit kept |
| the CSV answer was "wrote 56 bytes to totals.csv" | a question in a request that also changes something was not answered | `asks` answers whether or not the request changes something |
| `totals.csv` with invented totals | the worker "wrote" computed data | `invented_numbers`: a data file whose numbers are in no input fails the step with "compute it with a script" |
| a served site answered with a list of every step (the dead port first) | — | a request that started a server is answered with where to reach it |
| the server served the workspace root, 2 re-plans | "for the site directory" was not read as the cwd; a bound cwd of `.` | the name before "directory/folder" is the cwd; `.` counts as unset |
| false "not in the evidence" warnings | the address came from the command; `**url/**` markdown; a page under the address | served addresses and pages under them count as evidence |

**Caching (C1, C3).** The planner's system prompt and schema now list every native tool whatever the task's scope; MCP tools attached
to a task are listed in the request (`EXTRA TOOLS`) and added to that task's schema only; the step limit left the schema. Two different
tasks send byte-identical system prompts and schemas (test). The prefix policy uses the planner's cache lifetime: 25 minutes for
OpenAI models (`PLANNER_CACHE_TTL_SECS`), 270 s otherwise. C2 (one effort for plans and re-plans) was **not** done: a re-plan thinks
harder on purpose, and its separate cache line costs ≈ $0.0003 per re-plan.
