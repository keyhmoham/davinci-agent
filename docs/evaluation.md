# Evaluation

Do not call this system efficient on the strength of a demo. This page says what the built-in numbers mean and
how to benchmark it properly.

## The token ledger

Every model call is recorded (`slm`, `strong`, `decision`) with provider-reported token usage, falling back to an
estimator (`max(chars/4, words·4/3)`, deliberately slightly high). GLiNER calls are counted but cost no LLM tokens.

* **generative tokens** = SLM + strong-model tokens — what an LLM bill is made of.
* **decision tokens** = decision-model input+output tokens (a local model: compute, not a bill).
* **external tokens** = strong-model input+output tokens: the only ones that cost anything. The saved percentage compares
  *these* with the naive baseline (an agent that sends everything to an external model each step); SLM and decision tokens are reported but free.

## The baseline (an estimate, not a measurement)

`naive_baseline_tokens` models a tool-calling agent that, on each of *N+1* model calls (N tool steps + the final
answer), re-sends

```
system prompt (350) + every tool schema (registry.full_schema_tokens()) + the goal + the raw history so far
```

and emits ~40 tokens per tool call (or the final answer). Raw tool outputs (uncompressed, capped at 4000 tokens
each) accumulate in the history. This is a plausible but simple cost model; real agents vary (prompt caching, tool
search, history trimming). **"X% saved" is therefore model-relative.** A direct-mode task shows ~100% because it
used zero generative tokens, which is real; the baseline it is compared with is hypothetical.

## Estimates versus actuals

Every accepted plan carries `estimate {model_calls, tokens}` (the *remaining* cost, priced by the learned cost model). Compare it
with the task's ledger to see how well the model fits your deployment; `GET /v1/cost` shows each purpose's prior, observed mean,
sample count and the estimate in use. The shipped priors come from the shipped prompts (planner system prompt ≈ 200 tokens,
replanner ≈ 150, step ≈ 60, answer ≈ 70) and are replaced by evidence as it accumulates. Estimated savings from repair
(~300–500 tokens per avoided repair/replan call) are derived from those numbers, not measured on a real model.

## Teams

A coordinator's **baseline** models one naive agent doing *all* the work (every agent's steps in one history), not the sum of
per-agent baselines, and agents' own baselines are excluded from global totals. Their generative tokens roll up into the
coordinator's ledger (`origin` = agent id) for the team's cost, while `/v1/stats` counts each call once. Compare
team vs flat on your tasks with `/team off` (or `delegate:false`): expect the clearest wins on fan-out + join and on
mixed read-only / state-changing pipelines, and roughly neutral token cost on model-only pipelines. Measure latency too —
parallel agents are mostly a wall-clock gain.

## External-token optimisation: the ablation benchmark

`crates/dfae-runtime/tests/bench.rs` runs the same six requests four times each (24 tasks) through the whole engine, once per
configuration, with a scripted external planner that makes *the same decisions in every run* (it answers in whatever format its prompt asks
for) and a metered external model that simulates provider prompt caching (a prefix of ≥1,024 tokens is billed as a cache read the second
time it is seen). "Effective" tokens count what a bill is made of: uncached input + ⅒ × cached input + 5 × output.

```bash
cargo test -p dfae-runtime --test bench -- --nocapture
```

| arm | external calls | raw tokens | from cache | effective | per task |
|---|---:|---:|---:|---:|---:|
| baseline: JSON plans, narrow per-task tool list, answer call | 28 | 15,612 | 0 | 21,740 | 906 |
| + compact plan lines (36 % fewer output tokens for these plans: 241 vs 374, `plan_lines_are_a_smaller_output…`) | 28 | 14,096 | 0 | 18,096 | 754 |
| + plan-time reply template (`DFAE_PLAN_REPORT=on`) | 24 | 15,656 | 0 | 20,504 | 854 |
| + local-first answer | 24 | 14,836 | 0 | 20,820 | 868 |
| + workspace snapshot | 28 | 15,856 | 0 | 21,984 | 916 |
| + cached catalogue prefix | 28 | 28,528 | 22,953 | 13,992 | 583 |
| **all defaults** (lines, prefix when traffic keeps the cache warm, local-first answers, snapshot) | **24** | 31,759 | 27,132 | **11,192** | **466** |
| all defaults on a provider **without** prompt caching (the prefix switches itself off) | 24 | 13,564 | 0 | 17,420 | 726 |

* With prompt caching: **−49 % effective external tokens, −14 % external calls**; without it: **−20 % effective, −13 % raw**. Raw tokens go
  *up* with caching because the whole tool catalogue and worked examples ride in the prefix — they are billed at a tenth, which is why the
  prefix is sent only to a provider that caches (`LanguageModel::caches_prompts`: Anthropic, OpenAI's API, or `STRONG_PROMPT_CACHE=on`), only
  while the cache is warm or traffic is dense enough to warm it (`economy/prefix.rs`: a prefixed call in the last 4½ minutes, or three planner calls in
  ten), and never after three prefixed planner calls in a row came back with no cache hit.
* Honest negatives, kept in the table: the **reply template costs more than it saves** here (the planner pays five-times-priced output
  tokens for a reply a free local model writes just as well), so it is **off by default**; the **workspace snapshot adds ~1.5 %** tokens — it
  is a quality feature (the planner stops spending steps, and re-plans, on exploring), not a saving, and the scripted planner cannot show
  that benefit; the **cache write premium** (≈ 1.25× on the first call after the cache expires) is not in the "effective" number, which is
  why the prefix policy refuses to bet on a cold cache.
* **Repeated work is free**: a request of a shape seen before (`Read notes/f3.md and tell me how many lines it has` after `f0`…) replays the
  skill the first one left behind. Skills made from the external model's plans now keep *intents* (no baked-in generated content), so the
  replay costs **0 external tokens** — measured `[523, 0, 0, 0, 0, 0]` for six requests of one shape. Such a skill only matches a request that
  differs from the original in its slot values (entities), because its sentences are phrased for the original.
* What the benchmark does **not** show: how smart a real planner is. It measures what prompts, formats and flow cost. Plan quality, the
  tool-call accuracy of a real 3B model, and the real cache-hit rate of your provider have to be measured on your own traffic: the *Tokens* tab
  and `/v1/stats` (`external_by_purpose`, `external_cached_tokens`, `external_effective_tokens`, `effective_per_completed`,
  `effective_per_verified`) are the instruments.

### Quality, verified by test

Each item is pinned by a test that fails without the change (`crates/dfae-runtime/tests/planned.rs` unless noted):

| Change | What the test shows |
|---|---|
| expectations code can read ("`b.txt` exists", "`config.toml` now contains `port = 9090`", "the command exits 0", "the output lists X") become checks | a step that wrote `a.txt` when `b.txt` was expected is caught with no model asked, and the planner is told the fact |
| written files are parsed (JSON, TOML, HTML balance, Python, shell, JS) | a JSON object cut off by the token limit is caught, the *local* model rewrites it with the parser's complaint, and the external model is not called again |
| a reasoning step that misses its expectation is rewritten once locally, with the judge's reason | external calls stay at 1 |
| arguments are bound under a JSON schema (servers that support `response_format: json_schema`; others fall back by themselves) and a second local attempt is a 3-sample vote | wire test (`dfae-models/tests/wire.rs`) and unit test |
| a big tool output is condensed by the SLM, kept only if it states nothing the output lacks | the external model's step prompt holds the condensed text, not 600 lines (`tiers.rs`) |
| work the external model plans is one plan, not a team of planners | `work_the_external_model_plans_is_one_plan_not_a_team_of_planners` fails without the change |
| a task that went wrong and then worked leaves a one-line lesson that the next planner sees | lesson stored from `problems`, retrieved into the next plan prompt |
| "that's wrong, try again" (decision model: `feedback`) marks the previous result as rejected | calibration, the skill that produced it, a lesson and a decision-log line (`rejected_by_user`) |
| `DFAE_SHADOW_EVAL` | the external model answers the routing questions for a sample of requests; agreement is in `/v1/stats` and the decision log |

## Benchmark properly

Compare, on *your* task distribution:

1. a plain SLM agent (no tools);
2. the same SLM with native tool calling;
3. DFAE (decision model + GLiNER + SLM);
4. DFAE with a stronger model enabled for escalation (or a stronger model alone).

`scripts/bench.sh [file]` runs a list of requests and prints mode, verification and tokens per request;
`GET /v1/stats` aggregates. Metrics from the blueprint and where to read them:

| Metric | Source |
|---|---|
| task success / verified success | `completed` vs `verification.status == verified` (`/v1/stats`) |
| tool accuracy, planning quality | plan + step statuses in the task record; `invalid plan` warnings; repair/replan counts |
| extraction precision/recall | not built in — label a set and diff `entities` |
| recovery rate | tasks with `usage.replans > 0` or `attempts > 1` that still verify |
| escalation quality | `mode == escalated` vs outcome, `strong_tokens`, calibration table (`/v1/calibration`) |
| cost / tokens per task | `ledger` (per task), `dfae_tokens*` (`/metrics`) |
| latency | event timestamps; `latency_ms` on every ledger entry |
| security | `crates/dfae-tools` policy tests; add your own red-team cases to `crates/dfae-runtime/tests/e2e.rs` |
| persistence | `interrupted_tasks_resume_from_their_checkpoint` |

A cheaper agent that silently fails is not an optimisation: read **verified** success next to tokens.

## What the test suite proves (and does not)

The end-to-end tests use *scripted* models, so they prove the **runtime** — validation, repair, policy, approvals,
recovery, grounding, skills, resume — behaves correctly for given model outputs. They say nothing about how often a
real 3B model produces a valid plan. Measure that before trusting assisted mode with anything important.
