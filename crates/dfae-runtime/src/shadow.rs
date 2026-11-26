//! Shadow evaluation: how often does the self-hosted decision model decide what the planner model would have?
//!
//! Off by default (`DFAE_SHADOW_EVAL=0`). With a sampling rate above zero, that share of requests is also put to the
//! planner model — the same typed questions, answered in one JSON object — and the two answers are compared. The
//! agreement is a measurement of the decision model on *your* traffic, not on a benchmark; every line is also written to
//! the decision log when one is configured. It spends external tokens, which is why a person has to ask for it.

use dfae_core::*;
use serde_json::{Value, json};

pub const SHADOW_SYSTEM: &str = r#"You answer typed questions about a request to an agent. Reply with ONLY one JSON object that maps each question id to its answer: for a choice question one of its option keys; for a yes/no question "yes" or "no". Judge from the request and the recent conversation only."#;

/// Is this task one of the sampled ones? Deterministic in the task id, so a task is either always or never sampled.
pub fn sampled(task_id: &str, rate: f32) -> bool {
    if rate <= 0.0 {
        return false;
    }
    // FNV-1a, then a finaliser so that ids differing in their last characters still spread over the whole range
    let mut h = task_id.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    (h % 1000) as f32 / 1000.0 < rate
}

/// The routing questions as a prompt.
pub fn render(req: &DecisionRequest) -> String {
    let mut out = format!("REQUEST: {}\n", req.state.request);
    if !req.state.recent.is_empty() {
        out.push_str(&format!("RECENT:\n{}\n", req.state.recent.join("\n")));
    }
    out.push_str("QUESTIONS:\n");
    for (id, q) in &req.questions {
        match q {
            Question::Noul { instructions, .. } => out.push_str(&format!("- {id} (yes/no): {instructions}\n")),
            Question::Choice { instructions, criteria } => {
                let opts = criteria.iter().map(|(k, d)| format!("{k} = {d}")).collect::<Vec<_>>().join("; ");
                out.push_str(&format!("- {id} (choose one): {instructions} OPTIONS: {opts}\n"));
            }
            Question::Score { .. } => {} // a scale is not comparable between models
        }
    }
    out
}

/// `(agreeing, compared, differences)` between the decision model's routing and the planner model's answers.
pub fn compare(routing: &Routing, theirs: &Value) -> (usize, usize, Vec<String>) {
    let yes = |k: &str| theirs.get(k).and_then(|v| v.as_str()).map(|s| s.eq_ignore_ascii_case("yes"));
    let word = |k: &str| theirs.get(k).and_then(|v| v.as_str()).map(|s| s.to_ascii_lowercase());
    let mut pairs: Vec<(&str, Option<String>, String)> = vec![
        ("task_type", word("task_type"), routing.task_type.clone()),
        ("needs_tools", yes("needs_tools").map(|b| b.to_string()), routing.needs_tools.to_string()),
        ("changes_state", yes("changes_state").map(|b| b.to_string()), routing.changes.to_string()),
        ("conditional_change", yes("conditional_change").map(|b| b.to_string()), routing.conditional.to_string()),
        ("risk", word("risk"), format!("{:?}", routing.risk).to_ascii_lowercase()),
        ("smalltalk", word("smalltalk"), routing.smalltalk.clone().unwrap_or_else(|| "none".into())),
        ("feedback", word("feedback"), routing.feedback.clone().unwrap_or_else(|| "none".into())),
    ];
    if let Some(role) = &routing.role {
        pairs.push(("role", word("role"), role.clone()));
    }
    let (mut agree, mut total, mut diffs) = (0, 0, Vec::new());
    for (name, other, mine) in pairs {
        let Some(other) = other else { continue };
        total += 1;
        if other == mine {
            agree += 1;
        } else {
            diffs.push(format!("{name}: decision model {mine}, external {other}"));
        }
    }
    (agree, total, diffs)
}

pub fn log_line(task_id: &str, goal: &str, agree: usize, total: usize, diffs: &[String]) -> Value {
    json!({"shadow": true, "task_id": task_id, "goal": goal, "agree": agree, "compared": total, "differences": diffs})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_is_stable_and_proportional() {
        assert!(!sampled("abc", 0.0));
        assert!(sampled("abc", 1.0));
        let n = (0..2000).filter(|i| sampled(&format!("task-{i}"), 0.25)).count();
        assert!((300..700).contains(&n), "{n}");
        assert_eq!(sampled("task-7", 0.3), sampled("task-7", 0.3));
    }

    #[test]
    fn comparison_names_what_differs_and_ignores_what_was_not_answered() {
        let mut r = Routing::new("file_operations", 0.3, true, Risk::Low, 0.9);
        r.changes = true;
        let theirs = json!({"task_type": "coding", "needs_tools": "yes", "changes_state": "no", "risk": "low"});
        let (agree, total, diffs) = compare(&r, &theirs);
        assert_eq!((agree, total), (2, 4));
        assert!(diffs.iter().any(|d| d.starts_with("task_type")) && diffs.iter().any(|d| d.starts_with("changes_state")));
    }
}
