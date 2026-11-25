use dfae_core::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;

/// Typical total tokens (prompt + completion) per call purpose, used until the
/// ledger has taught the model better. Measured on the shipped prompts.
const PRIORS: &[(&str, f64)] = &[
    ("plan", 330.0),
    ("plan_repair", 380.0),
    ("plan_escalated", 330.0),
    ("replan", 420.0),
    ("step", 150.0),
    ("step_expand", 170.0),
    ("answer", 160.0),
    ("chat", 140.0),
];
const PRIOR_WEIGHT: f64 = 3.0;
const MAX_SAMPLES: f64 = 50.0;

/// Learns what each kind of model call costs *on this deployment* and prices plans with it.
#[derive(Default)]
pub struct CostModel {
    /// purpose -> (running mean of total tokens, samples)
    stats: Mutex<HashMap<String, (f64, f64)>>,
}

fn prior(purpose: &str) -> f64 {
    PRIORS.iter().find(|(p, _)| *p == purpose).map(|(_, t)| *t).unwrap_or(200.0)
}

impl CostModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Expected total tokens of one call: the prior, shifted toward what was observed.
    pub fn tokens(&self, purpose: &str) -> u64 {
        let (mean, n) = self.stats.lock().get(purpose).copied().unwrap_or((0.0, 0.0));
        ((prior(purpose) * PRIOR_WEIGHT + mean * n) / (PRIOR_WEIGHT + n)).round() as u64
    }

    pub fn observe(&self, purpose: &str, total_tokens: u64) {
        let mut g = self.stats.lock();
        let e = g.entry(purpose.to_string()).or_insert((0.0, 0.0));
        e.1 = (e.1 + 1.0).min(MAX_SAMPLES);
        e.0 += (total_tokens as f64 - e.0) / e.1; // running mean; the cap makes it adapt slowly forever
    }

    pub fn samples(&self, purpose: &str) -> u64 {
        self.stats.lock().get(purpose).map(|(_, n)| *n as u64).unwrap_or(0)
    }

    pub fn snapshot(&self) -> Value {
        let g = self.stats.lock();
        let mut out = serde_json::Map::new();
        for (p, base) in PRIORS {
            let (mean, n) = g.get(*p).copied().unwrap_or((0.0, 0.0));
            out.insert(p.to_string(), json!({"prior": base, "observed_mean": mean.round(), "samples": n as u64, "estimate": ((base * PRIOR_WEIGHT + mean * n) / (PRIOR_WEIGHT + n)).round()}));
        }
        Value::Object(out)
    }

    pub fn state(&self) -> Value {
        let g = self.stats.lock();
        Value::Object(g.iter().map(|(k, (m, n))| (k.clone(), json!([m, n]))).collect())
    }

    pub fn load(&self, v: &Value) {
        if let Some(m) = v.as_object() {
            let mut g = self.stats.lock();
            for (k, val) in m {
                if let (Some(mean), Some(n)) = (val.get(0).and_then(|x| x.as_f64()), val.get(1).and_then(|x| x.as_f64())) {
                    g.insert(k.clone(), (mean, n));
                }
            }
        }
    }

    /// Expected *remaining* cost of finishing `plan` (planning calls already made are not included).
    pub fn estimate_plan(&self, plan: &Plan, external: bool) -> CostEstimate {
        if plan.source == PlanSource::Chat {
            return CostEstimate { model_calls: 1, tokens: self.tokens("chat"), external };
        }
        let own = plan_model_calls(&plan.steps);
        let reasoning = plan.steps.iter().filter(|s| s.kind == StepKind::Reasoning && !s.status.is_done()).count() as u64;
        let answer_call = u64::from(own as u64 > reasoning);
        let mut calls = own;
        let mut tokens = reasoning * self.tokens("step") + answer_call * self.tokens("answer");
        // an agent that needs a model pays for its own plan *and* its step; a deterministic one is free
        for s in plan.steps.iter().filter(|s| s.kind == StepKind::Agent && !s.status.is_done()) {
            if s.args.get("direct").and_then(|d| d.as_bool()) != Some(true) {
                calls += 2;
                tokens += self.tokens("plan") + self.tokens("step");
            }
        }
        CostEstimate { model_calls: calls, tokens, external }
    }

    /// Cost of doing `model_parts` model-needing sub-tasks (plus an optional join) as
    /// one plan versus as a team of agents: every agent pays for its own plan.
    pub fn flat_vs_team(&self, model_parts: usize, has_join: bool, external: bool) -> (u64, u64) {
        // self-hosted models cost nothing, so splitting model work can never be "too expensive"
        if !external {
            return (0, 0);
        }
        let (plan, step) = (self.tokens("plan"), self.tokens("step"));
        let n = model_parts as u64;
        let join = u64::from(has_join);
        if n == 0 {
            return (join * (plan + step), join * step);
        }
        (plan + (n + join) * step, n * (plan + step) + join * step)
    }
}

/// How many model calls finishing these steps needs: one per unfinished reasoning
/// step, plus the answer-writing call when several results must be composed.
/// (Agents run on budgets of their own and are not counted here.)
pub fn plan_model_calls(steps: &[Step]) -> u32 {
    let reasoning = steps.iter().filter(|s| s.kind == StepKind::Reasoning).count();
    if reasoning == 0 {
        return 0;
    }
    // reasoning the local system does (a brief plan's steps) is not a call to the budgeted model
    let pending = steps.iter().filter(|s| s.kind == StepKind::Reasoning && !s.status.is_done() && !s.runs_local()).count() as u32;
    let leaves: Vec<&Step> = steps.iter().filter(|s| !steps.iter().any(|o| o.depends_on.contains(&s.id))).collect();
    let single_reasoning_leaf = leaves.len() == 1 && leaves[0].kind == StepKind::Reasoning;
    pending + u32::from(!single_reasoning_leaf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(steps: Vec<Step>) -> Plan {
        Plan { goal: "g".into(), steps, source: PlanSource::Slm, revision: 0, estimate: None }
    }

    #[test]
    fn learns_from_observations_and_stays_anchored_by_the_prior() {
        let m = CostModel::new();
        assert_eq!(m.tokens("step"), 150);
        m.observe("step", 450);
        let one = m.tokens("step");
        assert!(one > 150 && one < 450, "one sample moves the estimate only part of the way: {one}");
        for _ in 0..40 {
            m.observe("step", 450);
        }
        assert!(m.tokens("step") > 400, "enough evidence overrides the prior");
        let m2 = CostModel::new();
        m2.load(&m.state());
        assert_eq!(m2.tokens("step"), m.tokens("step"));
        assert_eq!(m.snapshot()["step"]["samples"], 41);
    }

    #[test]
    fn counts_model_calls_like_the_executor_will_spend_them() {
        let tool = Step::tool("s1", "fs_read", "r", json!({"path": "a"}));
        let mut r1 = Step::reasoning("s2", "think");
        r1.depends_on = vec!["s1".into()];
        assert_eq!(plan_model_calls(&[tool.clone(), r1.clone()]), 1, "a lone reasoning leaf is the answer: no extra call");
        let mut r2 = Step::reasoning("s3", "also think");
        r2.depends_on = vec!["s1".into()];
        assert_eq!(plan_model_calls(&[tool.clone(), r1.clone(), r2]), 3, "two leaves also need an answer-composing call");
        assert_eq!(plan_model_calls(std::slice::from_ref(&tool)), 0);
        let mut done = r1;
        done.status = StepStatus::Succeeded;
        assert_eq!(plan_model_calls(&[tool, done]), 0, "finished steps cost nothing more");
    }

    #[test]
    fn prices_plans_and_teams() {
        let m = CostModel::new();
        let mut r = Step::reasoning("s1", "think");
        r.depends_on = vec![];
        let e = m.estimate_plan(&plan(vec![r]), true);
        assert_eq!((e.model_calls, e.tokens), (1, 150));

        let mut direct = Step::reasoning("c1", "read a");
        direct.kind = StepKind::Agent;
        direct.args = json!({"direct": true});
        let mut model = Step::reasoning("c2", "explain");
        model.kind = StepKind::Agent;
        let e = m.estimate_plan(&plan(vec![direct, model]), true);
        assert_eq!((e.model_calls, e.tokens), (2, 330 + 150), "a deterministic agent is free; a model agent pays plan + step");

        // splitting model work across agents costs extra planning calls: a team never wins on tokens alone
        let (flat, team) = m.flat_vs_team(3, false, true);
        assert_eq!((flat, team), (330 + 3 * 150, 3 * (330 + 150)));
        assert!(team > flat);
        let (flat, team) = m.flat_vs_team(0, true, true);
        assert!(team < flat, "a fan-out of free agents plus a join beats planning it as one task");
        assert_eq!(m.flat_vs_team(3, true, false), (0, 0), "a self-hosted model is not billed, so a split is never penalised for its price");
    }
}
