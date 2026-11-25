//! Token ledger: records every model call by component and estimates what a
//! monolithic "LLM thinks at every step" agent would have spent instead.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    Slm,
    Strong,
    Jev,
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub entries: Vec<LedgerEntry>,
    /// Estimated tokens a naive tool-calling LLM agent would have used.
    pub baseline_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LedgerSummary {
    pub slm_tokens: u64,
    pub strong_tokens: u64,
    pub decision_tokens: u64,
    /// SLM + strong-model tokens (what an LLM bill would be made of).
    pub generative_tokens: u64,
    pub baseline_tokens: u64,
    pub saved_tokens: u64,
    pub saved_pct: f32,
    pub slm_calls: u32,
    pub strong_calls: u32,
    pub jev_calls: u32,
    pub gliner_calls: u32,
    pub cache_hits: u32,
}

impl Ledger {
    pub fn record(&mut self, e: LedgerEntry) {
        self.entries.push(e);
    }

    pub fn summary(&self) -> LedgerSummary {
        let mut s = LedgerSummary::default();
        for e in &self.entries {
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
                Component::Jev => {
                    s.decision_tokens += t;
                    s.jev_calls += 1;
                }
                Component::Gliner => s.gliner_calls += 1,
            }
        }
        s.generative_tokens = s.slm_tokens + s.strong_tokens;
        s.baseline_tokens = self.baseline_tokens;
        s.saved_tokens = s.baseline_tokens.saturating_sub(s.generative_tokens);
        s.saved_pct = if s.baseline_tokens > 0 {
            100.0 * s.saved_tokens as f32 / s.baseline_tokens as f32
        } else {
            0.0
        };
        s
    }
}

/// Estimate of the naive baseline: a monolithic agent that re-sends the system
/// prompt, *every* tool schema, the goal and the entire raw history at each of
/// `n_calls` model invocations (one per tool step plus the final answer).
pub fn naive_baseline_tokens(
    system_tokens: u64,
    all_tool_schema_tokens: u64,
    goal_tokens: u64,
    raw_step_tokens: &[u64],
    final_answer_tokens: u64,
) -> u64 {
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
        let mut l = Ledger::default();
        l.baseline_tokens = 1000;
        l.record(LedgerEntry {
            ts: Utc::now(),
            component: Component::Slm,
            model: "m".into(),
            purpose: "plan".into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            latency_ms: 1,
            cached: false,
        });
        let s = l.summary();
        assert_eq!(s.generative_tokens, 150);
        assert_eq!(s.saved_tokens, 850);
        assert!((s.saved_pct - 85.0).abs() < 0.01);
    }
}
