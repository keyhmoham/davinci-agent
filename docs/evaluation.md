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
