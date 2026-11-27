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
