//! Token ledger: records every model call by component and estimates what a
//! monolithic "LLM thinks at every step" agent would have spent instead.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    Slm,
    Strong,
    /// The decision model.
    #[serde(alias = "jev")]
    Decision,
    Gliner,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub ts: DateTime<Utc>,
    pub component: Component,
    pub model: String,
    pub purpose: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub latency_ms: u64,
    /// True when the answer came from a cache (no tokens spent).
    #[serde(default)]
    pub cached: bool,
    /// Set on entries rolled up from a subagent (its task id). Global
    /// aggregation skips them: the agent's own record already counts them.
    #[serde(default)]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub entries: Vec<LedgerEntry>,
    /// Estimated tokens a naive tool-calling LLM agent would have used.
    pub baseline_tokens: u64,
}

/// `#[serde(default)]`: a client built before a field was added or renamed must still read the summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LedgerSummary {
    pub slm_tokens: u64,
    pub strong_tokens: u64,
    /// Tokens sent to and from the **external** (paid) model: the only ones that cost anything.
    pub external_tokens: u64,
    pub decision_tokens: u64,
    /// SLM + strong-model tokens (what an LLM bill would be made of).
    pub generative_tokens: u64,
    pub baseline_tokens: u64,
    pub saved_tokens: u64,
    pub saved_pct: f32,
    pub slm_calls: u32,
    pub strong_calls: u32,
    #[serde(alias = "jev_calls")]
    pub decision_calls: u32,
    pub gliner_calls: u32,
    pub cache_hits: u32,
}

impl Ledger {
    pub fn record(&mut self, e: LedgerEntry) {
        self.entries.push(e);
    }

    /// Totals including everything rolled up from subagents (the team's cost).
    pub fn summary(&self) -> LedgerSummary {
        self.summarize(true)
    }

    /// Totals for this task's own calls only (what global stats aggregate).
    pub fn summary_own(&self) -> LedgerSummary {
        self.summarize(false)
    }

    fn summarize(&self, include_rolled_up: bool) -> LedgerSummary {
        let mut s = LedgerSummary::default();
        for e in self.entries.iter().filter(|e| include_rolled_up || e.origin.is_none()) {
            let t = e.prompt_tokens + e.completion_tokens;
            if e.cached {
                s.cache_hits += 1;
            }
            match e.component {
                Component::Slm => {
                    s.slm_tokens += t;
                    s.slm_calls += 1;
                }
                Component::Strong => {
                    s.strong_tokens += t;
                    s.strong_calls += 1;
                }
                Component::Decision => {
                    s.decision_tokens += t;
                    s.decision_calls += 1;
                }
                Component::Gliner => s.gliner_calls += 1,
            }
        }
        s.generative_tokens = s.slm_tokens + s.strong_tokens;
        s.external_tokens = s.strong_tokens;
        s.baseline_tokens = self.baseline_tokens;
        // the baseline is what a monolithic agent would have spent on an external model; self-hosted models are free
        s.saved_tokens = s.baseline_tokens.saturating_sub(s.external_tokens);
        s.saved_pct = if s.baseline_tokens > 0 { 100.0 * s.saved_tokens as f32 / s.baseline_tokens as f32 } else { 0.0 };
        s
    }
}

/// Estimate of the naive baseline: a monolithic agent that re-sends the system
/// prompt, *every* tool schema, the goal and the entire raw history at each of
/// `n_calls` model invocations (one per tool step plus the final answer).
pub fn naive_baseline_tokens(system_tokens: u64, all_tool_schema_tokens: u64, goal_tokens: u64, raw_step_tokens: &[u64], final_answer_tokens: u64) -> u64 {
    let calls = raw_step_tokens.len() as u64 + 1;
    let mut total = 0u64;
    let mut history = 0u64;
    for i in 0..calls {
        total += system_tokens + all_tool_schema_tokens + goal_tokens + history;
        // each call emits a tool-call (~40 tokens) or the final answer
        total += if (i as usize) < raw_step_tokens.len() { 40 } else { final_answer_tokens.max(40) };
        if let Some(t) = raw_step_tokens.get(i as usize) {
            history += t + 40;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_grows_with_history() {
        let one = naive_baseline_tokens(300, 2000, 30, &[200], 100);
        let three = naive_baseline_tokens(300, 2000, 30, &[200, 200, 200], 100);
        assert!(three > 2 * one);
    }

    #[test]
    fn summary_counts() {
        let mut l = Ledger { baseline_tokens: 1000, ..Default::default() };
        l.record(LedgerEntry {
            ts: Utc::now(),
            component: Component::Slm,
            model: "m".into(),
            purpose: "plan".into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            latency_ms: 1,
            cached: false,
            origin: None,
        });
        let s = l.summary();
        assert_eq!((s.generative_tokens, s.external_tokens), (150, 0), "a self-hosted model's tokens are not external");
        assert_eq!(s.saved_tokens, 1000, "nothing was spent on an external model");
        assert!((s.saved_pct - 100.0).abs() < 0.01);
        l.record(LedgerEntry {
            ts: Utc::now(),
            component: Component::Strong,
            model: "big".into(),
            purpose: "plan".into(),
            prompt_tokens: 200,
            completion_tokens: 100,
            latency_ms: 1,
            cached: false,
            origin: None,
        });
        let s = l.summary();
        assert_eq!((s.external_tokens, s.saved_tokens), (300, 700));
        assert!((s.saved_pct - 70.0).abs() < 0.01);
    }

    #[test]
    fn summaries_from_older_or_newer_servers_still_parse() {
        // an older server's field names and missing fields
        let old: LedgerSummary = serde_json::from_value(serde_json::json!({"slm_tokens": 5, "jev_calls": 2})).unwrap();
        assert_eq!((old.slm_tokens, old.decision_calls, old.cache_hits), (5, 2, 0));
        // a newer server's extra fields
        let new: LedgerSummary = serde_json::from_value(serde_json::json!({"decision_calls": 3, "something_new": true})).unwrap();
        assert_eq!(new.decision_calls, 3);
    }
}
