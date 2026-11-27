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
