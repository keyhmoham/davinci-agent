//! Final answer synthesis. Prefer deterministic assembly; use a model only
//! when reasoning steps produced material that must be composed, and always
//! check the result against the evidence.

use crate::engine::{Inner, TaskCtx};
use crate::prompts;
use dfae_core::*;
use dfae_models::Tier;
use regex::Regex;
use std::sync::{Arc, LazyLock};

/// What to call a step in prose: the tool, the delegated sub-goal, or "reasoning".
fn label(s: &Step) -> String {
    match s.kind {
        StepKind::Tool => s.tool.clone().unwrap_or_default(),
        StepKind::Agent => tokens::truncate_tokens(&s.description, 14),
        StepKind::Reasoning => "reasoning".into(),
    }
}

/// `id tool: summary` lines for completed steps (token-bounded per step).
pub fn evidence_block(rec: &TaskRecord, per_step_tokens: usize) -> String {
    let Some(plan) = &rec.plan else { return String::new() };
    let mut lines = Vec::new();
    for s in &plan.steps {
        match s.status {
            StepStatus::Succeeded => lines.push(format!(
                "{} {}: {}",
                s.id,
                label(s),
                tokens::truncate_middle(s.summary.as_deref().or(s.output.as_deref()).unwrap_or(""), per_step_tokens)
            )),
            StepStatus::Failed => lines.push(format!("{} {} FAILED: {}", s.id, label(s), s.error.as_deref().unwrap_or("error"))),
            StepStatus::Skipped => lines.push(format!("{} SKIPPED", s.id)),
            _ => {}
        }
    }
    lines.join("\n")
}

/// The reply the planner wrote ahead of time, with each `{{id}}` replaced by that step's output. `None` unless every
/// step of the plan succeeded and every placeholder could be filled: a report describes the run that was planned.
pub fn render_report(plan: &Plan) -> Option<String> {
    let report = plan.report.as_deref()?;
    if plan.steps.is_empty() || plan.steps.iter().any(|s| s.status != StepStatus::Succeeded) {
        return None;
    }
    static REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{\s*([A-Za-z][A-Za-z0-9_]*)\s*\}\}").unwrap());
    let mut missing = false;
    let out = REF.replace_all(report, |c: &regex::Captures| match plan.steps.iter().find(|s| s.id == c[1]) {
        Some(s) => tokens::truncate_tokens(s.output.as_deref().or(s.summary.as_deref()).unwrap_or("").trim(), 700),
        None => {
            missing = true;
            String::new()
        }
    });
    (!missing && !out.trim().is_empty()).then(|| out.trim().to_string())
}

/// Tools whose steps change something: their one-line result is what the user needs to read.
const CHANGES: &[&str] = &["fs_write", "fs_edit", "fs_mkdir", "fs_move", "fs_copy", "fs_delete", "shell_kill", "memory_store"];

/// Tools that only look: when the run also changed something, what they read was for the change (the page that was read to be
/// edited is not part of the answer).
const LOOKS: &[&str] = &["fs_read", "fs_list", "fs_search", "fs_stat", "fs_find"];

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
        let changed = ok.iter().any(|s| s.tool.as_deref().is_some_and(|t| CHANGES.contains(&t)));
        for s in &ok {
            if changed && s.tool.as_deref().is_some_and(|t| LOOKS.contains(&t)) {
                continue;
            }
            // a step that changed something says what it did in one line ("wrote 2070 bytes to site/index.html"): its arguments
            // (a whole page, say) are not an answer
            if s.tool.as_deref().is_some_and(|t| CHANGES.contains(&t)) {
                let line = tokens::truncate_tokens(s.summary.as_deref().or(s.output.as_deref()).unwrap_or("").lines().next().unwrap_or(""), 40);
                // an edit says what became what: that is the part of its arguments the user wants to see
                let edit = (s.tool.as_deref() == Some("fs_edit"))
                    .then(|| (s.args.get("old_string").and_then(|v| v.as_str()), s.args.get("new_string").and_then(|v| v.as_str())))
                    .and_then(|(o, n)| Some((o?, n?)))
                    .map(|(o, n)| format!(" (\"{}\" → \"{}\")", tokens::truncate_tokens(o, 12), tokens::truncate_tokens(n, 12)))
                    .unwrap_or_default();
                out.push_str(&format!("• {line}{edit}\n"));
                continue;
            }
            let body = tokens::truncate_tokens(s.summary.as_deref().or(s.output.as_deref()).unwrap_or(""), 350);
            out.push_str(&format!(
                "▸ {}{}\n{}\n",
                label(s),
                if s.kind != StepKind::Tool || s.args.is_null() || s.args == serde_json::json!({}) {
                    String::new()
                } else {
                    format!(" {}", compact_args(&s.args))
                },
                body
            ));
        }
    }
    let failed: Vec<&Step> = plan.steps.iter().filter(|s| s.status == StepStatus::Failed).collect();
    for s in failed {
        out.push_str(&format!("✗ {} failed: {}\n", label(s), s.error.as_deref().unwrap_or("error")));
    }
    out.trim().to_string()
}

fn compact_args(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            m.iter().map(|(k, x)| format!("{k}={}", x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))).collect::<Vec<_>>().join(" ")
        }
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
        // a number the answer worked out (a day of the month, an hour after a time-zone conversion, a count of three items) is not a
        // claim about the evidence: only three digits or more, or a decimal, is
        let digits = m.as_str().chars().filter(|c| c.is_ascii_digit()).count();
        if digits >= 3 || (digits >= 2 && m.as_str().contains('.')) {
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
    /// A plan that only looked something up (nothing changed) and got back a blob — JSON, a page, a long listing — has not answered the
    /// question yet: the worker model writes the answer from it. A short plain result is the answer itself.
    fn data_needs_composing(&self, plan: &Plan) -> bool {
        let done: Vec<&Step> = plan.steps.iter().filter(|s| s.status == StepStatus::Succeeded).collect();
        // (a file or a command's output is shown as it is: that is what was asked for; what outside services and the web return is data)
        if done.is_empty()
            || done.iter().any(|s| {
                s.kind != StepKind::Tool
                    || s.tool.as_deref().and_then(|t| self.tools.meta(t)).is_none_or(|m| m.side_effects || !matches!(m.category.as_str(), "mcp" | "web"))
            })
        {
            return false;
        }
        let last = done.last().and_then(|s| s.output.as_deref()).unwrap_or("").trim();
        let blob = last.starts_with('{') || last.starts_with('[') || last.starts_with('<');
        blob || last.chars().count() > 600
    }

    /// Did any step that changes something finish?
    fn did_change(&self, plan: &Plan) -> bool {
        plan.steps.iter().any(|s| {
            s.status == StepStatus::Succeeded
                && (s.kind == StepKind::Agent || s.tool.as_deref().and_then(|t| self.tools.meta(t)).map(|m| m.side_effects).unwrap_or(false))
        })
    }

    pub(crate) async fn stage_answer(self: &Arc<Self>, ctx: &Arc<TaskCtx>, mode: ExecMode, exec_err: Option<&str>) {
        let snapshot = ctx.rec.lock().await.clone();
        let goal = snapshot.goal.clone();
        let Some(plan) = snapshot.plan.clone() else {
            ctx.rec.lock().await.final_answer = Some(templated_answer(&snapshot, exec_err));
            return;
        };
        // the planner wrote the reply together with the plan and the run went as planned: no answer call at all
        if exec_err.is_none()
            && self.cfg.economy.plan_report
            && let Some(text) = render_report(&plan)
        {
            let text = crate::style::compact(&text);
            let result = {
                let mut r = ctx.rec.lock().await;
                r.final_answer = Some(text);
                r.verification.clone()
            };
            if let Some(v) = result {
                self.emit(ctx, EventKind::Verification { result: v }).await;
            }
            return;
        }
        // a lone reasoning leaf *is* the answer: no extra model call
        let leaves: Vec<&Step> = plan.steps.iter().filter(|s| !plan.steps.iter().any(|o| o.depends_on.contains(&s.id))).collect();
        let single_leaf = if leaves.len() == 1 && leaves[0].kind == StepKind::Reasoning && leaves[0].status == StepStatus::Succeeded {
            leaves[0].output.clone()
        } else {
            None
        };

        // only tool output is ground truth: a model's own prose must never vouch for itself
        let evidence_full: String = plan.steps.iter().filter(|s| s.kind != StepKind::Reasoning).filter_map(|s| s.output.clone()).collect::<Vec<_>>().join("\n");
        let mut from_model = false;
        let text = if exec_err.is_some() || (!plan.has_reasoning() && !self.data_needs_composing(&plan)) {
            templated_answer(&snapshot, exec_err)
        } else if let Some(t) = single_leaf {
            from_model = true;
            t
        } else if let Some(tier) = self.pick_tier(mode) {
            // The self-hosted model writes the answer first (it is free, and gets the full evidence); the external one
            // is asked only when the draft states a number, path or address that the evidence does not contain.
            let local = if tier == Tier::Planner && self.cfg.economy.answer_local_first && self.models.has(Tier::Worker) {
                let user = format!("REQUEST: {goal}\nEVIDENCE:\n{}", evidence_block(&snapshot, 800));
                match self.generate_text(ctx, Tier::Worker, self.gen_request(Tier::Worker, prompts::ANSWER_SYSTEM, user, "answer_local")).await {
                    Ok(t) => {
                        let t = crate::style::compact(t.trim());
                        (!t.is_empty() && ungrounded_claims(&t, &evidence_full, &goal).is_empty()).then_some(t)
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            if let Some(t) = local {
                from_model = true;
                t
            } else {
                // the paid model gets tight evidence; a self-hosted one gets what the steps produced
                let ev = evidence_block(&snapshot, if tier == Tier::Planner { 220 } else { 300 });
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
            }
        } else {
            templated_answer(&snapshot, None)
        };
        let mut text = if from_model { crate::style::compact(&text) } else { text };
        let mut unverified = Vec::new();
        // a conversational reply has no tool evidence to be checked against
        if from_model && plan.source != PlanSource::Chat {
            unverified = ungrounded_claims(&text, &evidence_full, &goal);
            if !unverified.is_empty() {
                text.push_str(&format!("\n⚠ Not found in the collected evidence: {}", unverified.join(", ")));
            }
        }
        // a model must not tell the user something was changed when no step that changes anything finished
        let asked_to_change = snapshot.route.as_ref().map(|r| r.needs_tools && r.changes && !r.conditional).unwrap_or(false);
        let did_change = self.did_change(&plan);
        if from_model && asked_to_change && !did_change && plan.source != PlanSource::Chat {
            text.push_str("\n⚠ Nothing was changed: no step that modifies anything completed.");
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
    fn what_was_read_to_make_a_change_is_not_part_of_the_answer() {
        let mut rec = TaskRecord::new(TaskRequest { goal: "retitle the page".into(), ..Default::default() }, Budget::default());
        let mut read = Step::tool("s1", "fs_read", "read the page", serde_json::json!({"path": "index.html"}));
        read.status = StepStatus::Succeeded;
        read.output = Some("<html>the whole page</html>".into());
        let mut edit = Step::tool("s2", "fs_edit", "retitle", serde_json::json!({"path": "index.html", "old_string": "Old", "new_string": "New"}));
        edit.status = StepStatus::Succeeded;
        edit.summary = Some("replaced 1 occurrence in index.html".into());
        rec.plan = Some(Plan { goal: "g".into(), steps: vec![read.clone(), edit], source: PlanSource::Planner, revision: 0, estimate: None, report: None });
        let answer = templated_answer(&rec, None);
        assert!(answer.contains("replaced 1 occurrence") && !answer.contains("whole page"), "{answer}");
        // a request that only reads still shows what was read
        rec.plan.as_mut().unwrap().steps.truncate(1);
        assert!(templated_answer(&rec, None).contains("whole page"));
    }

    #[test]
    fn single_digit_numbers_are_not_flagged() {
        assert!(ungrounded_claims("There are 3 files.", "a\nb", "list").is_empty());
    }
}
