//! Plan executor: DAG scheduling with bounded parallelism, just-in-time policy
//! checks, approval gating bound to exact arguments, post-condition checks,
//! and decision-driven recovery (retry / replan / escalate / skip / fail).

use crate::engine::{Inner, TaskCtx};
use crate::team::TeamShared;
use crate::{planner, prompts};
use chrono::Utc;
use dfae_core::*;
use dfae_models::Tier;
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinSet;

pub(crate) struct StepFailure {
    pub kind: ErrorKind,
    pub message: String,
}

impl StepFailure {
    fn new(kind: ErrorKind, m: impl Into<String>) -> Self {
        Self { kind, message: m.into() }
    }
}

impl From<ToolError> for StepFailure {
    fn from(e: ToolError) -> Self {
        Self { kind: e.kind, message: e.message }
    }
}

pub(crate) struct StepOutput {
    pub content: String,
    pub evidence: Vec<String>,
}

/// What a step is called in events: its tool, or `agent` for a delegated step.
fn step_label(s: &Step) -> Option<String> {
    s.tool.clone().or_else(|| (s.kind == StepKind::Agent).then(|| "agent".to_string()))
}

fn kind_str(k: ErrorKind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "other".into())
}

impl Inner {
    pub(crate) async fn execute_plan(self: &Arc<Self>, ctx: &Arc<TaskCtx>) -> Result<(), String> {
        let sem = Arc::new(Semaphore::new(self.cfg.max_parallel.max(1)));
        let mut isolated: Option<String> = None;
        loop {
            if ctx.cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            self.check_deadline(ctx).await?;

            // ---- which steps can run now? -------------------------------------
            let mut blocked_ids: Vec<(String, Option<String>)> = Vec::new();
            let ready: Vec<String> = {
                let mut r = ctx.rec.lock().await;
                let Some(plan) = r.plan.as_mut() else { return Ok(()) };
                let status_of: HashMap<String, StepStatus> = plan.steps.iter().map(|s| (s.id.clone(), s.status)).collect();
                let mut ready = Vec::new();
                for s in plan.steps.iter_mut().filter(|s| s.status == StepStatus::Pending) {
                    let (mut blocked, mut waiting) = (false, false);
                    for d in &s.depends_on {
                        match status_of.get(d) {
                            Some(StepStatus::Succeeded) | Some(StepStatus::Skipped) => {}
                            Some(StepStatus::Failed) | Some(StepStatus::Cancelled) => blocked = true,
                            _ => waiting = true,
                        }
                    }
                    if blocked {
                        s.status = StepStatus::Cancelled;
                        s.error = Some("blocked by a failed dependency".into());
                        blocked_ids.push((s.id.clone(), s.tool.clone()));
                    } else if !waiting {
                        ready.push(s.id.clone());
                    }
                }
                ready
            };
            for (id, tool) in blocked_ids {
                self.emit(
                    ctx,
                    EventKind::StepUpdate {
                        step_id: id,
                        status: StepStatus::Cancelled,
                        tool,
                        summary: None,
                        error: Some("blocked by a failed dependency".into()),
                        attempts: 0,
                    },
                )
                .await;
            }
            if ready.is_empty() {
                break;
            }
            self.set_status(ctx, TaskStatus::WaitingForTool).await;

            // ---- run the ready layer concurrently -------------------------------
            let mut set: JoinSet<(String, Result<(), StepFailure>)> = JoinSet::new();
            for id in ready {
                let (inner, ctx2, sem2) = (self.clone(), ctx.clone(), sem.clone());
                set.spawn(async move {
                    let _permit = sem2.acquire().await;
                    let r = AssertUnwindSafe(inner.run_step(&ctx2, &id))
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| Err(StepFailure::new(ErrorKind::Other, "internal error while running step")));
                    (id, r)
                });
            }
            let mut failures: Vec<(String, StepFailure)> = Vec::new();
            while let Some(joined) = set.join_next().await {
                if let Ok((id, Err(f))) = joined {
                    failures.push((id, f));
                }
            }
            self.persist(ctx).await;

            // ---- recovery -------------------------------------------------------
            let mut fatal: Option<String> = None;
            for (id, f) in failures {
                let is_agent = ctx.rec.lock().await.plan.as_ref().and_then(|p| p.step(&id)).map(|s| s.kind == StepKind::Agent).unwrap_or(false);
                if let Some(msg) = self.recover(ctx, &id, f).await {
                    if is_agent {
                        // failure isolation: siblings that do not depend on this agent keep running;
                        // the task fails at the end with their results preserved
                        isolated.get_or_insert(msg);
                    } else {
                        fatal.get_or_insert(msg);
                    }
                }
            }
            if let Some(msg) = fatal {
                return Err(msg);
            }
            self.set_status(ctx, TaskStatus::Running).await;
        }
        if let Some(msg) = isolated {
            return Err(msg);
        }
        // anything still failed (e.g. no recovery needed) fails the task
        let r = ctx.rec.lock().await;
        if let Some(s) = r.plan.as_ref().and_then(|p| p.steps.iter().find(|s| s.status == StepStatus::Failed)) {
            return Err(format!("step {} failed: {}", s.id, s.error.clone().unwrap_or_default()));
        }
        Ok(())
    }

    async fn run_step(self: &Arc<Self>, ctx: &Arc<TaskCtx>, id: &str) -> Result<(), StepFailure> {
        // mark running and snapshot what the step needs
        let (step, mode, tainted, deps) = {
            let mut r = ctx.rec.lock().await;
            let mode = r.mode.unwrap_or(ExecMode::Assisted);
            let tainted = r.tainted;
            let plan = r.plan.as_mut().ok_or_else(|| StepFailure::new(ErrorKind::Other, "no plan"))?;
            let deps: Vec<(String, Option<String>, String, String)> = {
                let s = plan.step(id).ok_or_else(|| StepFailure::new(ErrorKind::Other, "unknown step"))?;
                s.depends_on
                    .iter()
                    .filter_map(|d| plan.step(d))
                    .map(|d| {
                        let label = d.tool.clone().or_else(|| (d.kind == StepKind::Agent).then(|| "agent".to_string()));
                        (d.id.clone(), label, d.summary.clone().unwrap_or_default(), d.output.clone().unwrap_or_default())
                    })
                    .collect()
            };
            let s = plan.step_mut(id).unwrap();
            s.status = StepStatus::Running;
            s.attempts += 1;
            s.started_at = Some(Utc::now());
            s.error = None;
            (s.clone(), mode, tainted, deps)
        };
        self.emit(
            ctx,
            EventKind::StepUpdate {
                step_id: id.into(),
                status: StepStatus::Running,
                tool: step_label(&step),
                summary: None,
                error: None,
                attempts: step.attempts,
            },
        )
        .await;

        let result = match step.kind {
            StepKind::Reasoning => self.run_reasoning(ctx, &step, mode, &deps).await,
            StepKind::Tool => self.run_tool(ctx, &step, tainted, &deps).await,
            StepKind::Agent => self.run_agent(ctx, &step, &deps).await,
        };

        match result {
            Ok(out) => {
                let content = out.content;
                let hits = dfae_core::safety::scan_injection(&content);
                let mut summary = tokens::compress_text(&content, 300);
                if !hits.is_empty() && step.kind == StepKind::Tool {
                    summary = format!("[flagged: possible prompt injection ({})] {summary}", hits.join(", "));
                    {
                        ctx.rec.lock().await.tainted = true;
                    }
                    self.emit(
                        ctx,
                        EventKind::Warning {
                            message: format!(
                                "step {id}: tool output looks like a prompt injection ({}); side-effecting steps now require approval",
                                hits.join(", ")
                            ),
                        },
                    )
                    .await;
                }
                {
                    let mut r = ctx.rec.lock().await;
                    if let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(id)) {
                        s.status = StepStatus::Succeeded;
                        s.output = Some(content.chars().take(20_000).collect());
                        s.summary = Some(summary.clone());
                        s.evidence = out.evidence;
                        s.finished_at = Some(Utc::now());
                    }
                }
                self.emit(
                    ctx,
                    EventKind::StepUpdate {
                        step_id: id.into(),
                        status: StepStatus::Succeeded,
                        tool: step_label(&step),
                        summary: Some(tokens::truncate_tokens(&summary, 40)),
                        error: None,
                        attempts: step.attempts,
                    },
                )
                .await;
                Ok(())
            }
            Err(f) => {
                {
                    let mut r = ctx.rec.lock().await;
                    if let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(id)) {
                        s.status = StepStatus::Failed;
                        s.error = Some(f.message.clone());
                        s.error_kind = Some(f.kind);
                        s.finished_at = Some(Utc::now());
                    }
                }
                self.emit(
                    ctx,
                    EventKind::StepUpdate {
                        step_id: id.into(),
                        status: StepStatus::Failed,
                        tool: step_label(&step),
                        summary: None,
                        error: Some(f.message.clone()),
                        attempts: step.attempts,
                    },
                )
                .await;
                Err(f)
            }
        }
    }

    async fn run_reasoning(
        self: &Arc<Self>,
        ctx: &Arc<TaskCtx>,
        step: &Step,
        mode: ExecMode,
        deps: &[(String, Option<String>, String, String)],
    ) -> Result<StepOutput, StepFailure> {
        let tier = self.pick_tier(mode).ok_or_else(|| StepFailure::new(ErrorKind::Policy, "no language model is configured for reasoning steps"))?;
        let ctx_text = self.context_text(ctx, false).await;
        let inputs: String = deps
            .iter()
            .map(|(id, tool, summary, _)| dfae_core::safety::wrap_untrusted(&format!("{id} {}", tool.as_deref().unwrap_or("reasoning")), summary))
            .collect::<Vec<_>>()
            .join("\n");
        let user = format!("STEP: {}\n{ctx_text}\nINPUTS:\n{inputs}", step.description);
        let text = self.generate_text(ctx, tier, self.gen_request(tier, prompts::STEP_SYSTEM, user, "step")).await.map_err(|e| {
            let kind = if e.contains("budget") { ErrorKind::Policy } else { ErrorKind::Transient };
            StepFailure::new(kind, e)
        })?;
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(StepFailure::new(ErrorKind::Other, "the model returned an empty answer"));
        }
        Ok(StepOutput { content: text, evidence: vec![] })
    }

    async fn run_tool(
        self: &Arc<Self>,
        ctx: &Arc<TaskCtx>,
        step: &Step,
        tainted: bool,
        deps: &[(String, Option<String>, String, String)],
    ) -> Result<StepOutput, StepFailure> {
        let name = step.tool.clone().unwrap_or_default();
        let meta = self.tools.meta(&name).ok_or_else(|| StepFailure::new(ErrorKind::NotFound, format!("unknown tool '{name}'")))?;
        if !ctx.allowed.lock().iter().any(|a| a == &name) {
            return Err(StepFailure::new(ErrorKind::Policy, format!("tool '{name}' is not available for this task")));
        }
        if !ctx.in_scope(&name) {
            return Err(StepFailure::new(ErrorKind::Policy, format!("tool '{name}' is outside this agent's role scope")));
        }
        // resolve {{sN}} templates against dependency outputs
        let outputs: HashMap<&str, &str> = deps.iter().map(|(id, _, _, out)| (id.as_str(), out.as_str())).collect();
        let args = planner::substitute(&step.args, &|id| {
            outputs.get(id).map(|o| tokens::compress_text(o, 800).chars().take(4000).collect::<String>().trim().to_string())
        });
        meta.validate_args(&args).map_err(|e| StepFailure::new(ErrorKind::InvalidArgs, e))?;

        // ---- policy (authoritative, just-in-time, on the final arguments) ----
        let tainted = tainted || ctx.rec.lock().await.tainted;
        let mut approval_reason: Option<String> = None;
        match self.policy.check(&meta, &args, tainted) {
            dfae_tools::PolicyDecision::Deny(m) => return Err(StepFailure::new(ErrorKind::Policy, m)),
            dfae_tools::PolicyDecision::RequireApproval(r) => approval_reason = Some(r),
            dfae_tools::PolicyDecision::Allow => {}
        }
        if step.approval_required && approval_reason.is_none() {
            approval_reason = Some("the plan marked this step as requiring approval".into());
        }
        if let Some(reason) = approval_reason {
            self.await_approval(ctx, step, &name, &args, &reason, meta.risk).await.map_err(|e| StepFailure::new(ErrorKind::Policy, e))?;
        }
        // a read another member of the team already did (and nothing has changed since)
        let shared = ctx.shared.get().cloned();
        let cache_key = shared.as_ref().filter(|_| meta.cacheable).map(|_| TeamShared::key(&name, &args));
        let cached = shared.as_ref().zip(cache_key.as_ref()).and_then(|(sh, k)| sh.get(k));
        let tool = self.tools.get(&name).ok_or_else(|| StepFailure::new(ErrorKind::NotFound, "tool vanished"))?;
        let tctx =
            ToolContext { task_id: ctx.id.clone(), step_id: step.id.clone(), workspace: self.policy.workspace().to_path_buf(), cancel: ctx.cancel.clone() };
        let generation = shared.as_ref().map(|s| s.generation()).unwrap_or(0);
        let out = match cached {
            Some(content) => ToolOutput { content, ok: true, exit_code: Some(0), evidence: vec!["served from the team's shared cache".into()] },
            None => {
                self.take_tool_call(ctx).await.map_err(|e| StepFailure::new(ErrorKind::Policy, e))?;
                let res = tokio::time::timeout(Duration::from_secs(120), tool.execute(args.clone(), tctx.clone())).await;
                if meta.side_effects
                    && let Some(sh) = &shared
                {
                    sh.invalidate(); // whatever happened, cached reads of the old state are stale
                }
                match res {
                    Ok(r) => r.map_err(StepFailure::from)?,
                    Err(_) => return Err(StepFailure::new(ErrorKind::Timeout, "tool timed out after 120s")),
                }
            }
        };
        let content = dfae_core::safety::redact_secrets(&out.content);
        if out.ok
            && let (Some(sh), Some(k)) = (&shared, cache_key)
            && !out.evidence.iter().any(|e| e.contains("shared cache"))
        {
            sh.put(k, content.clone(), generation);
        }
        if !out.ok {
            let first = content.lines().next().unwrap_or("").chars().take(160).collect::<String>();
            return Err(StepFailure::new(ErrorKind::Other, format!("{name} reported failure (exit {}): {first}", out.exit_code.unwrap_or(-1))));
        }
        for c in step.checks.iter().filter(|c| **c != Check::ExitOk) {
            let (pass, ev) = self.eval_check(c, &content);
            if !pass {
                return Err(StepFailure::new(ErrorKind::Other, format!("check failed: {} ({ev})", c.describe())));
            }
        }
        let mut evidence = out.evidence.clone();
        if let Some(v) = tool.verify(&args, &out, &tctx).await {
            match v {
                Ok(ev) => evidence.push(format!("verified: {ev}")),
                Err(e) => return Err(StepFailure::new(ErrorKind::Other, format!("post-condition failed: {e}"))),
            }
        }
        Ok(StepOutput { content, evidence })
    }

    /// Suspend the step until a human approves *this exact action*.
    async fn await_approval(self: &Arc<Self>, ctx: &Arc<TaskCtx>, step: &Step, tool: &str, args: &Value, reason: &str, risk: Risk) -> Result<(), String> {
        let hash = sha256_hex(&format!("{tool}|{}", canonical_json(args)));
        let now = Utc::now();
        let auto = {
            let r = ctx.rec.lock().await;
            r.request.auto_approve && self.cfg.allow_auto_approve
        };
        let mut approval = Approval {
            id: new_id(),
            step_id: step.id.clone(),
            tool: tool.into(),
            args: args.clone(),
            args_hash: hash.clone(),
            reason: reason.into(),
            risk,
            status: if auto { ApprovalStatus::Approved } else { ApprovalStatus::Pending },
            created_at: now,
            expires_at: now + chrono::Duration::seconds(self.cfg.approval_ttl_secs as i64),
            decided_by: if auto { Some("auto-approve (benchmark mode)".into()) } else { None },
        };
        let (tx, rx) = oneshot::channel::<bool>();
        {
            if !auto {
                ctx.waiters.lock().insert(approval.id.clone(), tx);
            }
            let mut r = ctx.rec.lock().await;
            r.approvals.push(approval.clone());
            if let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(&step.id)) {
                s.status = StepStatus::WaitingApproval;
            }
        }
        self.emit(ctx, EventKind::ApprovalRequested { approval: approval.clone() }).await;
        if auto {
            self.emit(ctx, EventKind::ApprovalResolved { approval_id: approval.id.clone(), status: ApprovalStatus::Approved }).await;
            return Ok(());
        }
        self.set_status(ctx, TaskStatus::WaitingForApproval).await;
        self.persist(ctx).await;
        let ttl = Duration::from_secs(self.cfg.approval_ttl_secs);
        let decision = tokio::select! {
            _ = ctx.cancel.cancelled() => Err("cancelled".to_string()),
            r = tokio::time::timeout(ttl, rx) => match r {
                Ok(Ok(b)) => Ok(b),
                Ok(Err(_)) => Err("approval was withdrawn".to_string()),
                Err(_) => Err("approval expired".to_string()),
            },
        };
        let (status, result) = match decision {
            Ok(true) => (ApprovalStatus::Approved, Ok(())),
            Ok(false) => (ApprovalStatus::Denied, Err("approval denied".to_string())),
            Err(e) => (if e.contains("expired") { ApprovalStatus::Expired } else { ApprovalStatus::Denied }, Err(e)),
        };
        {
            let mut r = ctx.rec.lock().await;
            if let Some(a) = r.approvals.iter_mut().find(|a| a.id == approval.id) {
                a.status = status;
                approval = a.clone();
            }
            if let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(&step.id)) {
                s.status = StepStatus::Running;
            }
        }
        self.waiters_cleanup(ctx, &approval.id);
        self.emit(ctx, EventKind::ApprovalResolved { approval_id: approval.id.clone(), status }).await;
        // binding check: the action to execute must still be the approved action
        if result.is_ok() && sha256_hex(&format!("{tool}|{}", canonical_json(args))) != approval.args_hash {
            return Err("approval does not match the action to execute".into());
        }
        if result.is_ok() {
            self.set_status(ctx, TaskStatus::Running).await;
        }
        result
    }

    fn waiters_cleanup(&self, ctx: &Arc<TaskCtx>, approval_id: &str) {
        ctx.waiters.lock().remove(approval_id);
    }

    // ----------------------------------------------------------- recovery ----

    /// Returns `Some(message)` if the task must fail.
    async fn recover(self: &Arc<Self>, ctx: &Arc<TaskCtx>, id: &str, f: StepFailure) -> Option<String> {
        if ctx.cancel.is_cancelled() {
            return Some("cancelled".into());
        }
        self.set_status(ctx, TaskStatus::Recovering).await;
        let (step, usage, budget, goal, entities) = {
            let r = ctx.rec.lock().await;
            (r.plan.as_ref().and_then(|p| p.step(id).cloned()), r.usage.clone(), r.budget.clone(), r.goal.clone(), r.entities.clone())
        };
        let Some(step) = step else { return Some(f.message) };
        let generative = self.models.has(Tier::Slm) || self.models.has(Tier::Strong);
        let mut st = self.decision_state(&goal, &entities);
        st.step_facts = Some(StepFacts {
            step_id: id.into(),
            tool: step.tool.clone(),
            error: tokens::truncate_tokens(&f.message, 40),
            error_kind: kind_str(f.kind),
            attempts: step.attempts,
            max_attempts: step.retry.max_attempts,
            replans_used: usage.replans,
            // an agent step is retried or escalated, never replanned: its own pipeline already replans internally
            max_replans: if generative && step.kind != StepKind::Agent { budget.max_replans } else { 0 },
            optional: false,
            strong_available: self.models.has(Tier::Strong),
        });
        let action = self.note(ctx, self.decider.assess_recovery(st).await).await;
        let label = format!("step {id} ({}) failed: {}", step.tool.as_deref().unwrap_or("reasoning"), f.message);
        use dfae_decision::RecoveryAction::*;
        match action {
            Retry => {
                let wait = Duration::from_millis(step.retry.backoff_ms.saturating_mul(step.attempts as u64).min(5000));
                tokio::select! { _ = ctx.cancel.cancelled() => return Some("cancelled".into()), _ = tokio::time::sleep(wait) => {} }
                {
                    let mut r = ctx.rec.lock().await;
                    if let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(id)) {
                        s.status = StepStatus::Pending;
                    }
                }
                self.emit(ctx, EventKind::Warning { message: format!("retrying {label}") }).await;
                None
            }
            Skip => {
                {
                    let mut r = ctx.rec.lock().await;
                    if let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(id)) {
                        s.status = StepStatus::Skipped;
                    }
                }
                None
            }
            Escalate if step.kind == StepKind::Agent => {
                // supervision: run the sub-task again on the stronger model
                {
                    let mut r = ctx.rec.lock().await;
                    if let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(id)) {
                        s.status = StepStatus::Pending;
                        s.args = json!({"mode": "escalated"});
                    }
                }
                self.emit(ctx, EventKind::Warning { message: format!("escalating {label}") }).await;
                None
            }
            Replan | Escalate => {
                if step.kind == StepKind::Agent {
                    return Some(label);
                }
                let mode = ctx.rec.lock().await.mode.unwrap_or(ExecMode::Assisted);
                let tier = if action == Escalate { Tier::Strong } else { self.pick_tier(mode).unwrap_or(Tier::Slm) };
                if !self.models.has(tier) {
                    return Some(label);
                }
                if tier == Tier::Strong {
                    ctx.rec.lock().await.mode = Some(ExecMode::Escalated);
                }
                let problem = format!(
                    "step {id} ({} {}) failed with {}: {}",
                    step.tool.as_deref().unwrap_or("reasoning"),
                    tokens::truncate_tokens(&step.args.to_string(), 30),
                    kind_str(f.kind),
                    tokens::truncate_tokens(&f.message, 50)
                );
                match self.replan(ctx, tier, &goal, &problem, Some(id)).await {
                    Ok(true) => None,
                    Ok(false) => Some(label),
                    Err(e) => Some(format!("{label}; replanning failed: {e}")),
                }
            }
            Fail => Some(label),
        }
    }

    /// Ask a model to revise the plan. The failed step and every not-yet-run
    /// step are replaced; completed steps (and their outputs) are kept.
    pub(crate) async fn replan(self: &Arc<Self>, ctx: &Arc<TaskCtx>, tier: Tier, goal: &str, problem: &str, failed: Option<&str>) -> Result<bool, String> {
        let (replans, budget) = {
            let mut r = ctx.rec.lock().await;
            if r.usage.replans >= r.budget.max_replans {
                return Ok(false);
            }
            r.usage.replans += 1;
            (r.usage.replans, r.budget.clone())
        };
        self.set_status(ctx, TaskStatus::Recovering).await;
        // widen the tool set through the decision layer (direct/skill plans start narrow)
        let (entities, base) = {
            let r = ctx.rec.lock().await;
            (r.entities.clone(), r.goal.clone())
        };
        let mut tstate = self.decision_state(&base, &entities);
        tstate.tools = self.scoped_briefs(ctx);
        let picked = self.note(ctx, self.decider.select_tools(tstate).await).await;
        let allowed: Vec<String> = {
            let mut a = ctx.allowed.lock();
            for (n, _) in picked {
                if !a.contains(&n) {
                    a.push(n);
                }
            }
            // a recovery plan may need a sibling of a tool that already failed
            // (same category), but never an unrelated one
            let cats: Vec<String> = a.iter().filter_map(|n| self.tools.meta(n)).map(|m| m.category).collect();
            for m in self.scoped_metas(ctx) {
                if cats.contains(&m.category) && !a.contains(&m.name) && !m.side_effects {
                    a.push(m.name);
                }
            }
            a.clone()
        };
        let (done, kept_ids) = {
            let r = ctx.rec.lock().await;
            let plan = r.plan.as_ref().ok_or("no plan")?;
            let done: Vec<String> = plan
                .steps
                .iter()
                .filter(|s| matches!(s.status, StepStatus::Succeeded | StepStatus::Skipped))
                .map(|s| format!("{} {}: {}", s.id, s.tool.as_deref().unwrap_or("reasoning"), tokens::truncate_tokens(s.summary.as_deref().unwrap_or(""), 40)))
                .collect();
            let kept: Vec<String> =
                plan.steps.iter().filter(|s| matches!(s.status, StepStatus::Succeeded | StepStatus::Skipped)).map(|s| s.id.clone()).collect();
            (done.join("\n"), kept)
        };
        let prefix = format!("r{replans}_");
        let ctx_text = self.context_text(ctx, false).await;
        let user = prompts::replan_user(&self.tools.signatures(&allowed), &ctx_text, &done, problem, &prefix, budget.max_steps.min(12));
        let mut req = self.gen_request(tier, prompts::REPLAN_SYSTEM, user, "replan");
        req.json = true;
        let v = self.gen_json(ctx, tier, req).await?;
        let new_steps = planner::parse_plan(&v, goal, PlanSource::Replan, &prefix, &self.tools)?;
        if new_steps.is_empty() {
            return Ok(false);
        }
        let merged: Vec<Step> = {
            let r = ctx.rec.lock().await;
            let plan = r.plan.as_ref().ok_or("no plan")?;
            let mut m: Vec<Step> = plan.steps.iter().filter(|s| kept_ids.contains(&s.id)).cloned().collect();
            m.extend(new_steps);
            m
        };
        planner::validate_plan(&merged, &self.tools, &allowed, &budget).map_err(|e| e.join("; "))?;
        let revised = {
            let mut r = ctx.rec.lock().await;
            let p = r.plan.as_mut().ok_or("no plan")?;
            p.steps = merged;
            p.revision += 1;
            p.clone()
        };
        let _ = failed;
        self.emit(ctx, EventKind::PlanRevised { plan: revised, reason: tokens::truncate_tokens(problem, 40) }).await;
        self.persist(ctx).await;
        Ok(true)
    }
}

#[allow(dead_code)]
fn _unused(_: Value) -> Value {
    json!(null)
}
