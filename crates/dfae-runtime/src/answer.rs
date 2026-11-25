//! Final answer synthesis. Prefer deterministic assembly; use a model only
//! when reasoning steps produced material that must be composed, and always
//! check the result against the evidence.

use crate::engine::{Inner, TaskCtx};
use crate::prompts;
use dfae_core::*;
use regex::Regex;
use std::sync::{Arc, LazyLock};

/// `id tool: summary` lines for completed steps (token-bounded per step).
pub fn evidence_block(rec: &TaskRecord, per_step_tokens: usize) -> String {
    let Some(plan) = &rec.plan else { return String::new() };
    let mut lines = Vec::new();
    for s in &plan.steps {
        match s.status {
            StepStatus::Succeeded => lines.push(format!(
                "{} {}: {}",
                s.id,
                s.tool.as_deref().unwrap_or("reasoning"),
                tokens::truncate_tokens(s.summary.as_deref().or(s.output.as_deref()).unwrap_or(""), per_step_tokens)
            )),
            StepStatus::Failed => lines.push(format!("{} {} FAILED: {}", s.id, s.tool.as_deref().unwrap_or("reasoning"), s.error.as_deref().unwrap_or("error"))),
            StepStatus::Skipped => lines.push(format!("{} SKIPPED", s.id)),
            _ => {}
        }
    }
    lines.join("\n")
}

/// Deterministic answer built from step outputs (no model involved).
pub fn templated_answer(rec: &TaskRecord, exec_error: Option<&str>) -> String {
    let Some(plan) = &rec.plan else {
        return match exec_error {
            Some(e) => format!("Could not complete the request: {e}"),
            None => "No plan was produced.".into(),
        };
    };
    let ok: Vec<&Step> = plan.steps.iter().filter(|s| s.status == StepStatus::Succeeded).collect();
    let mut out = String::new();
    if let Some(e) = exec_error {
        out.push_str(&format!("Could not fully complete the request: {e}\n"));
    }
    if ok.len() == 1 && exec_error.is_none() {
        out.push_str(&tokens::truncate_tokens(ok[0].summary.as_deref().or(ok[0].output.as_deref()).unwrap_or(""), 700));
    } else {
        for s in &ok {
            let body = tokens::truncate_tokens(s.summary.as_deref().or(s.output.as_deref()).unwrap_or(""), 350);
            out.push_str(&format!("▸ {}{}\n{}\n", s.tool.as_deref().unwrap_or("reasoning"), if s.args.is_null() || s.args == serde_json::json!({}) { String::new() } else { format!(" {}", compact_args(&s.args)) }, body));
        }
    }
    let failed: Vec<&Step> = plan.steps.iter().filter(|s| s.status == StepStatus::Failed).collect();
    for s in failed {
        out.push_str(&format!("✗ {} failed: {}\n", s.tool.as_deref().unwrap_or("reasoning"), s.error.as_deref().unwrap_or("error")));
    }
    out.trim().to_string()
}

fn compact_args(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => m.iter().map(|(k, x)| format!("{k}={}", x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))).collect::<Vec<_>>().join(" "),
        other => other.to_string(),
    }
}

static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d[\d,]*(?:\.\d+)?\b").unwrap());
static PATHLIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:/[\w.\-]+){2,}|https?://[^\s)\]]+").unwrap());
static BACKTICK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]{2,80})`").unwrap());

/// Specific values in `answer` (numbers, paths, URLs, code spans) that appear in
/// neither the evidence nor the request. A cheap, deterministic hallucination guard.
pub fn ungrounded_claims(answer: &str, evidence: &str, goal: &str) -> Vec<String> {
    let norm = |s: &str| s.replace(',', "");
    let (ev, gl) = (norm(evidence), norm(goal));
    let mut out: Vec<String> = Vec::new();
    let mut check = |tok: &str| {
        let t = norm(tok);
        if !(ev.contains(&t) || gl.contains(&t)) && !out.contains(&tok.to_string()) {
            out.push(tok.to_string());
        }
    };
    for m in NUMBER.find_iter(answer) {
        let digits = m.as_str().chars().filter(|c| c.is_ascii_digit()).count();
        if digits >= 2 {
            check(m.as_str());
        }
    }
    for m in PATHLIKE.find_iter(answer) {
        check(m.as_str().trim_end_matches(['.', ',', ')']));
    }
    for c in BACKTICK.captures_iter(answer) {
        check(&c[1]);
    }
    out.truncate(6);
    out
}

impl Inner {
    pub(crate) async fn stage_answer(self: &Arc<Self>, ctx: &Arc<TaskCtx>, mode: ExecMode, exec_err: Option<&str>) {
        let snapshot = ctx.rec.lock().await.clone();
        let goal = snapshot.goal.clone();
        let Some(plan) = snapshot.plan.clone() else {
            ctx.rec.lock().await.final_answer = Some(templated_answer(&snapshot, exec_err));
            return;
        };
        // a lone reasoning leaf *is* the answer: no extra model call
        let leaves: Vec<&Step> = plan.steps.iter().filter(|s| !plan.steps.iter().any(|o| o.depends_on.contains(&s.id))).collect();
        let single_leaf = if leaves.len() == 1 && leaves[0].kind == StepKind::Reasoning && leaves[0].status == StepStatus::Succeeded { leaves[0].output.clone() } else { None };

        // only tool output is ground truth: a model's own prose must never vouch for itself
        let evidence_full: String = plan.steps.iter().filter(|s| s.kind == StepKind::Tool).filter_map(|s| s.output.clone()).collect::<Vec<_>>().join("\n");
        let mut from_model = false;
        let text = if exec_err.is_some() || !plan.has_reasoning() {
            templated_answer(&snapshot, exec_err)
        } else if let Some(t) = single_leaf {
            from_model = true;
            t
        } else if let Some(tier) = self.pick_tier(mode) {
            let ev = evidence_block(&snapshot, 220);
            let user = format!("REQUEST: {goal}\nEVIDENCE:\n{ev}");
            match self.generate_text(ctx, tier, self.gen_request(tier, prompts::ANSWER_SYSTEM, user, "answer")).await {
                Ok(t) => {
                    from_model = true;
                    t.trim().to_string()
                }
                Err(e) => {
                    self.emit(ctx, EventKind::Warning { message: format!("answer synthesis failed ({e}); returning raw evidence") }).await;
                    templated_answer(&snapshot, None)
                }
            }
        } else {
            templated_answer(&snapshot, None)
        };
        let mut text = text;
        let mut unverified = Vec::new();
        if from_model {
            unverified = ungrounded_claims(&text, &evidence_full, &goal);
            if !unverified.is_empty() {
                text.push_str(&format!("\n⚠ Not found in the collected evidence: {}", unverified.join(", ")));
            }
        }
        let result = {
            let mut r = ctx.rec.lock().await;
            r.final_answer = Some(text);
            if let Some(v) = r.verification.as_mut() {
                v.unverified_claims = unverified;
                if !v.unverified_claims.is_empty() && v.status == VerificationStatus::Verified {
                    v.status = VerificationStatus::Partial;
                }
            }
            r.verification.clone()
        };
        if let Some(v) = result {
            self.emit(ctx, EventKind::Verification { result: v }).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_invented_values_only() {
        let evidence = "load: 0.52 0.40 0.31 (cpus: 4)\nmem: total 15.6G";
        let goal = "check system status";
        assert!(ungrounded_claims("The load is 0.52 with 4 cpus and 15.6G of memory.", evidence, goal).is_empty());
        let bad = ungrounded_claims("The load is 0.99 and /var/log/app/errors.log is full", evidence, goal);
        assert!(bad.contains(&"0.99".to_string()));
        assert!(bad.contains(&"/var/log/app/errors.log".to_string()));
    }

    #[test]
    fn single_digit_numbers_are_not_flagged() {
        assert!(ungrounded_claims("There are 3 files.", "a\nb", "list").is_empty());
    }
}
