# Evaluation

Do not call this system efficient on the strength of a demo. This page says what the built-in numbers mean and
how to benchmark it properly.

## The token ledger

Every model call is recorded (`slm`, `strong`, `jev`) with provider-reported token usage, falling back to an
estimator (`max(chars/4, words·4/3)`, deliberately slightly high). GLiNER calls are counted but cost no LLM tokens.

* **generative tokens** = SLM + strong-model tokens — what an LLM bill is made of.
* **decision tokens** = Jev input+output tokens (billed separately, usually far cheaper).

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

## Benchmark properly

Compare, on *your* task distribution:

1. a plain SLM agent (no tools);
2. the same SLM with native tool calling;
3. DFAE (Jev + GLiNER + SLM);
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
