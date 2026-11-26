//! Verification engine: judge completion on *evidence*, not on what a model
//! says it did.

use crate::answer;
use crate::engine::{Inner, TaskCtx};
use dfae_core::*;
use regex::Regex;
use std::sync::Arc;

impl Inner {
    /// Evaluate a declarative check against a step output.
    pub(crate) fn eval_check(&self, c: &Check, output: &str) -> (bool, String) {
        match c {
            Check::NonEmpty => (!output.trim().is_empty(), format!("{} bytes of output", output.len())),
            Check::Contains { value } => (output.to_lowercase().contains(&value.to_lowercase()), format!("looked for '{value}'")),
            Check::NotContains { value } => (!output.to_lowercase().contains(&value.to_lowercase()), format!("looked for absence of '{value}'")),
            Check::Matches { pattern } => match Regex::new(pattern) {
                Ok(re) => (re.is_match(output), format!("/{pattern}/")),
                Err(e) => (false, format!("bad pattern: {e}")),
            },
            Check::ExitOk => (true, "tool reported success".into()),
            Check::FileExists { path } => match self.policy.guard().exists(path) {
                Ok(found) => (found, path.clone()),
                Err(e) => (false, e.message),
            },
            Check::FileContains { path, value } => match self.policy.guard().read_path(path) {
                Ok(p) => match std::fs::read_to_string(&p) {
                    Ok(text) => (text.contains(value.as_str()), format!("looked for '{value}' in {}", p.display())),
                    Err(e) => (false, format!("{}: {e}", p.display())),
                },
                Err(e) => (false, e.message),
            },
        }
    }

    pub(crate) async fn stage_verify(self: &Arc<Self>, ctx: &Arc<TaskCtx>) -> VerificationResult {
        let rec = ctx.rec.lock().await.clone();
        let mut checks: Vec<CheckResult> = Vec::new();
        let mut tool_ok = 0usize;
        let (mut ok, mut failed, mut skipped) = (0usize, 0usize, 0usize);
        if let Some(plan) = &rec.plan {
            for s in &plan.steps {
                let label = s.tool.as_deref().unwrap_or(if s.kind == StepKind::Agent { "agent" } else { "reasoning" });
                match s.status {
                    StepStatus::Succeeded => {
                        ok += 1;
                        // a delegated step counts as evidence only if the agent itself verified its work
                        let agent_verified = s.kind == StepKind::Agent && s.evidence.iter().any(|e| e.contains(":verified") || e.contains(":partial"));
                        if s.kind == StepKind::Tool || agent_verified {
                            tool_ok += 1;
                        }
                        checks.push(CheckResult {
                            check_id: format!("{}:{}", s.id, label),
                            description: match s.kind {
                                StepKind::Tool => "tool reported success".into(),
                                StepKind::Agent => format!(
                                    "delegated agent finished ({})",
                                    s.evidence.iter().find(|e| e.starts_with("child:")).map(|e| e.rsplit(':').next().unwrap_or("")).unwrap_or("unverified")
                                ),
                                StepKind::Reasoning => "reasoning step produced output".into(),
                            },
                            status: CheckStatus::Passed,
                            evidence: tokens::truncate_tokens(s.summary.as_deref().unwrap_or(""), 24),
                        });
                        let out = s.output.as_deref().unwrap_or("");
                        for c in s.checks.iter().filter(|c| **c != Check::ExitOk) {
                            let (pass, ev) = self.eval_check(c, out);
                            checks.push(CheckResult {
                                check_id: format!("{}:check", s.id),
                                description: c.describe(),
                                status: if pass { CheckStatus::Passed } else { CheckStatus::Failed },
                                evidence: ev,
                            });
                        }
                        for ev in &s.evidence {
                            checks.push(CheckResult {
                                check_id: format!("{}:post", s.id),
                                description: "independent post-condition".into(),
                                status: CheckStatus::Passed,
                                evidence: ev.clone(),
                            });
                        }
                    }
                    StepStatus::Failed => {
                        failed += 1;
                        checks.push(CheckResult {
                            check_id: format!("{}:{}", s.id, label),
                            description: "step completed".into(),
                            status: CheckStatus::Failed,
                            evidence: s.error.clone().unwrap_or_default(),
                        });
                    }
                    StepStatus::Skipped | StepStatus::Cancelled => {
                        skipped += 1;
                        checks.push(CheckResult {
                            check_id: format!("{}:{}", s.id, label),
                            description: "step ran".into(),
                            status: CheckStatus::Skipped,
                            evidence: s.error.clone().unwrap_or_default(),
                        });
                    }
                    _ => {}
                }
            }
        }
        let total = rec.plan.as_ref().map(|p| p.steps.len()).unwrap_or(0);
        let checks_passed = checks.iter().filter(|c| c.status == CheckStatus::Passed).count();
        let checks_failed = checks.iter().filter(|c| c.status == CheckStatus::Failed).count();
        let failed_checks: Vec<String> = checks.iter().filter(|c| c.status == CheckStatus::Failed).map(|c| format!("{}: {}", c.check_id, c.evidence)).collect();

        // The decision model is consulted only where evidence alone is ambiguous
        // (reasoning steps, failures, skips); pure tool plans are judged by checks.
        let needs_judgement = rec.plan.as_ref().map(|p| p.has_reasoning()).unwrap_or(false) || failed > 0 || skipped > 0;
        let completion = if needs_judgement && total > 0 {
            let mut st = self.decision_state(&rec.goal, &rec.entities);
            st.completion_facts = Some(CompletionFacts {
                steps_ok: ok,
                steps_failed: failed,
                steps_total: total,
                checks_passed,
                checks_failed,
                evidence: answer::evidence_block(&rec, 300), // the decision model is self-hosted: show it the evidence
            });
            Some(self.note(ctx, self.decider.assess_completion(st).await).await)
        } else {
            None
        };

        let status = if checks_failed > 0 || failed > 0 {
            VerificationStatus::Failed
        } else if tool_ok == 0 {
            VerificationStatus::Unverified
        } else if skipped > 0 || completion.map(|p| p < 0.5).unwrap_or(false) {
            VerificationStatus::Partial
        } else {
            VerificationStatus::Verified
        };
        VerificationResult { status, checks, failed_checks, unverified_claims: vec![], completion_confidence: completion }
    }
}
