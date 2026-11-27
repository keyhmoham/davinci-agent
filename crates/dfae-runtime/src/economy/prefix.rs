//! When is the planner model's long, cacheable prompt prefix worth sending?
//!
//! A provider that caches prompts bills a repeated prefix at a fraction — but only while the cache is warm (minutes),
//! and writing it costs a little more than sending it plain. So the prefix pays off for a stream of tasks and loses on
//! an occasional one. The policy: send it while the cache is known to be warm (a prefixed call was made less than
//! `ttl` ago), or when planner calls are frequent enough (`min_calls` within `window`) that it will be warm soon.
//! Otherwise the plan is requested with the narrow per-task tool list, as before.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct PrefixPolicy {
    ttl: Duration,
    window: Duration,
    min_calls: usize,
    calls: VecDeque<Instant>,
    last_prefixed: Option<Instant>,
}

impl Default for PrefixPolicy {
    fn default() -> Self {
        // Anthropic's default cache lifetime is five minutes; a margin keeps us from betting on the last seconds.
        Self::new(Duration::from_secs(270), Duration::from_secs(600), 3)
    }
}

impl PrefixPolicy {
    /// The policy for a planner model, by how long its provider keeps a cached prefix: OpenAI's newer models keep one for 30 minutes
    /// (`gpt-*`, `o*`; also through a gateway), Anthropic's for 5. A cache write costs 1.25x and a read 0.1x the input price, so a prefix
    /// pays for itself the first time it is read again: a prefixed call within the lifetime is enough to keep sending it.
    pub fn for_model(model: &str) -> Self {
        let m = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
        let ttl_secs = std::env::var("PLANNER_CACHE_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(if m.starts_with("gpt-") || m.starts_with('o') { 1500 } else { 270 });
        let ttl = Duration::from_secs(ttl_secs);
        Self::new(ttl, ttl.max(Duration::from_secs(600)), 2)
    }

    pub fn new(ttl: Duration, window: Duration, min_calls: usize) -> Self {
        Self { ttl, window, min_calls, calls: VecDeque::new(), last_prefixed: None }
    }

    /// Record a planner call at `now` and say whether it should carry the prefix.
    pub fn decide(&mut self, now: Instant) -> bool {
        while self.calls.front().is_some_and(|t| now.duration_since(*t) > self.window) {
            self.calls.pop_front();
        }
        let warm = self.last_prefixed.is_some_and(|t| now.duration_since(t) < self.ttl);
        let dense = self.calls.len() + 1 >= self.min_calls;
        self.calls.push_back(now);
        let use_prefix = warm || dense;
        if use_prefix {
            self.last_prefixed = Some(now);
        }
        use_prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_of_tasks_turns_the_prefix_on_and_keeps_it_warm() {
        let mut p = PrefixPolicy::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(!p.decide(at(0)));
        assert!(!p.decide(at(20)));
        assert!(p.decide(at(40)), "the third call within the window: the cache will be used again soon");
        assert!(p.decide(at(200)) && p.decide(at(450)), "each prefixed call keeps the cache warm");
    }

    #[test]
    fn a_planner_whose_cache_lives_30_minutes_keeps_the_prefix_between_tasks_minutes_apart() {
        let mut p = PrefixPolicy::for_model("openai/gpt-6-luna");
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(!p.decide(at(0)), "one call alone does not bet on a cache");
        assert!(p.decide(at(400)), "a second within the lifetime: the prefix will be read again");
        assert!(p.decide(at(1200)) && p.decide(at(2400)), "each prefixed call within 25 minutes keeps it warm");
        assert!(!PrefixPolicy::for_model("claude-sonnet-5").decide(at(0)));
    }

    #[test]
    fn occasional_tasks_never_pay_for_a_prefix_nobody_reads() {
        let mut p = PrefixPolicy::default();
        let t0 = Instant::now();
        for i in 0..6 {
            assert!(!p.decide(t0 + Duration::from_secs(i * 700)), "one task every ~12 minutes");
        }
    }

    #[test]
    fn an_idle_gap_lets_the_cache_go_cold_and_the_prefix_is_dropped_until_traffic_returns() {
        let mut p = PrefixPolicy::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        for s in [0, 10, 20] {
            p.decide(at(s));
        }
        assert!(!p.decide(at(2000)), "long idle: the cache is cold");
    }
}
