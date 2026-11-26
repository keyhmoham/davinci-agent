//! Subagents: automatic team formation, role assignment, dispatch and
//! supervised execution of delegated steps.
//!
//! A subagent is a *child task*: it runs the same pipeline as any task
//! (route → mode → plan → execute → verify → answer → learn), so a sub-goal that
//! binds deterministically costs no generative tokens even inside a team.
//! What delegation adds:
//!
//! * a **least-privilege tool scope** per role,
//! * a **worker slot** from the dispatcher (operators are serialised),
//! * a **slice of the parent's budget** (children cannot overspend it),
//! * **inherited taint and auto-approve** (never a way around an approval),
//! * a **read cache shared by the team**,
//! * **supervision**: failures stay isolated, are retried/escalated by the decision layer,
//!   and a child's usage and token ledger roll up into the coordinator.

use crate::engine::{Inner, TaskCtx};
use crate::executor::{StepFailure, StepOutput};
use crate::team::{self, TeamShared};
use chrono::Utc;
use dfae_core::*;
use dfae_models::Tier;
use dfae_tools::bind_direct;
use serde_json::json;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

/// Coordinator is only worth it above this calibrated decision confidence.
const SPLIT_THRESHOLD: f32 = 0.6;

pub(crate) struct ChildSpec {
    pub step_id: String,
    pub goal: String,
    pub upstream: Vec<Upstream>,
    pub entities: Vec<Entity>,
    pub mode: ModeRequest,
    pub budget: Budget,
}

fn status_word(s: VerificationStatus) -> &'static str {
    match s {
        VerificationStatus::Verified => "verified",
        VerificationStatus::Partial => "partial",
        VerificationStatus::Unverified => "unverified",
        VerificationStatus::Failed => "failed",
    }
}

impl Inner {
    /// Tools visible to this task: everything, or its role's scope.
    pub(crate) fn scoped_metas(&self, ctx: &Arc<TaskCtx>) -> Vec<ToolMetadata> {
        self.tools.metas().into_iter().filter(|m| ctx.in_scope(&m.name)).collect()
    }

    pub(crate) fn scoped_briefs(&self, ctx: &Arc<TaskCtx>) -> Vec<ToolBrief> {
        self.tools.briefs().into_iter().filter(|b| ctx.in_scope(&b.name)).collect()
    }

    // ------------------------------------------------------- team formation ----

    /// Decide whether to split the request across a team and, if so, run it.
    /// `None` means "not applicable": the caller continues with a single plan.
    pub(crate) async fn stage_team(
        self: &Arc<Self>,
        ctx: &Arc<TaskCtx>,
        goal: &str,
        entities: &[Entity],
        state: &DecisionState,
        force: bool,
    ) -> Option<Result<(), String>> {
        let metas = self.tools.metas();
        let dec = team::decompose(goal, entities, &|fragment| bind_direct(&metas, fragment, entities).is_some())?;
        let direct: Vec<bool> = dec.subgoals.iter().map(|s| bind_direct(&metas, &s.text, entities).is_some()).collect();
        let models_available = self.models.has(Tier::Worker) || self.models.has(Tier::Planner);
        // agents of a medium or complex request are planned by the planner model, each paying for its own plan
        let complexity = ctx.rec.lock().await.route.as_ref().map(|r| r.complexity).unwrap_or(0.0);
        let external_work = self.uses_planner(ExecMode::Assisted) || (self.models.has(Tier::Planner) && complexity >= self.cfg.thresholds.plan_min_complexity);
        // A team of agents that need a model would each pay the planner model for a plan of their own. One brief plan
        // with parallel steps covers the same work for one planner call, and the local system runs its steps in
        // parallel anyway: teams are for work no model has to plan (every sub-goal binds to a tool outright).
        if external_work && !force && direct.iter().any(|d| !d) {
            return None;
        }
        let facts = team::team_facts(&dec, &direct, models_available, &self.cost, external_work);
        let confidence = if force {
            1.0
        } else {
            let mut st = state.clone();
            st.team_facts = Some(facts.clone());
            self.note(ctx, self.decider.assess_decomposition(st).await).await
        };
        if confidence < SPLIT_THRESHOLD || (!models_available && (facts.direct < direct.len() || facts.has_synthesis)) {
            return None;
        }
        let mut plan = team::plan_from_decomposition(goal, &dec, &direct);
        plan.estimate = Some(self.cost.estimate_plan(&plan, external_work));
        let budget = self.remaining_budget(ctx).await;
        if let Err(errs) = crate::planner::validate_plan(&plan.steps, &self.tools, &[], &budget) {
            self.emit(ctx, EventKind::Warning { message: format!("team plan rejected ({}); planning as one task", errs.join("; ")) }).await;
            return None;
        }
        let reason = format!(
            "{} sub-tasks ({} need no model, {} dependent{}) · split confidence {confidence:.2}",
            dec.subgoals.len(),
            facts.direct,
            facts.dependent,
            if facts.has_synthesis { ", + join" } else { "" }
        );
        let mode = if dec.synthesis.is_some() { ExecMode::Assisted } else { ExecMode::Direct };
        {
            let mut r = ctx.rec.lock().await;
            r.team = Some(TeamInfo::coordinator(reason.clone()));
            r.mode = Some(mode);
            r.mode_reason = Some(format!("team: {reason}"));
            r.plan = Some(plan.clone());
        }
        let _ = ctx.shared.set(Arc::new(TeamShared::default()));
        let goals: Vec<TeamGoal> = dec
            .subgoals
            .iter()
            .zip(&direct)
            .map(|(s, d)| TeamGoal { id: s.id.clone(), goal: s.text.clone(), depends_on: s.depends_on.clone(), direct: *d, synth: false })
            .chain(dec.synthesis.iter().map(|t| TeamGoal {
                id: "join".into(),
                goal: t.clone(),
                depends_on: dec.subgoals.iter().map(|s| s.id.clone()).collect(),
                direct: false,
                synth: true,
            }))
            .collect();
        self.emit(ctx, EventKind::TeamFormed { goals, reason }).await;
        self.emit(ctx, EventKind::PlanCreated { plan }).await;
        self.persist(ctx).await;
        Some(self.execute_and_conclude(ctx, goal, mode).await)
    }

    // ------------------------------------------------------ role & dispatch ----

    /// Fix this agent's role, narrow its tools, and wait for a worker slot.
    pub(crate) async fn stage_assign_role(self: &Arc<Self>, ctx: &Arc<TaskCtx>, goal: &str, entities: &[Entity], routing: &Routing) -> Result<(), String> {
        let metas = self.tools.metas();
        let bound: Option<Vec<ToolMetadata>> = bind_direct(&metas, goal, entities).map(|calls| calls.iter().filter_map(|c| self.tools.meta(&c.tool)).collect());
        let role = team::pick_role(goal, routing, bound.as_deref());
        let scope = team::role_scope(role, &metas);
        let n_tools = scope.as_ref().map(|s| s.len()).unwrap_or(metas.len());
        *ctx.scope.lock() = scope.map(|v| v.into_iter().collect::<HashSet<_>>());
        let permit = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err("cancelled".into()),
            p = self.dispatcher.acquire_agent(role) => p,
        };
        *ctx.permit.lock() = Some(permit);
        {
            let mut r = ctx.rec.lock().await;
            if let Some(t) = r.team.as_mut() {
                t.role = role;
            }
        }
        self.emit(ctx, EventKind::RoleAssigned { role, tools: n_tools }).await;
        Ok(())
    }

    // ------------------------------------------------------------- spawning ----

    /// (Boxed with an explicit `Send` type: agent steps recurse into `run_task`,
    /// and a named future type is what keeps that cycle checkable.)
    pub(crate) fn spawn_child<'a>(
        self: &'a Arc<Self>,
        parent: &'a Arc<TaskCtx>,
        spec: ChildSpec,
    ) -> Pin<Box<dyn Future<Output = (String, oneshot::Receiver<TaskRecord>)> + Send + 'a>> {
        Box::pin(self.spawn_child_inner(parent, spec))
    }

    async fn spawn_child_inner(self: &Arc<Self>, parent: &Arc<TaskCtx>, spec: ChildSpec) -> (String, oneshot::Receiver<TaskRecord>) {
        let (session, auto, tainted) = {
            let r = parent.rec.lock().await;
            (r.session_id.clone(), r.request.auto_approve, r.tainted)
        };
        let request = TaskRequest {
            goal: spec.goal.clone(),
            session_id: Some(session),
            mode: spec.mode,
            profile: Some(ContextProfile::Minimal),
            auto_approve: auto,
            delegate: Some(false),
        };
        let mut rec = TaskRecord::new(request, spec.budget);
        rec.tainted = tainted;
        rec.team = Some(TeamInfo {
            role: AgentRole::Researcher, // provisional; fixed once the agent has routed itself
            link: Some(TeamLink {
                parent_id: parent.id.clone(),
                parent_step: spec.step_id,
                depth: parent.depth + 1,
                upstream: spec.upstream,
                entities: spec.entities,
                inherited_taint: tainted,
            }),
            children: vec![],
            reason: None,
            cache_hits: 0,
        });
        let id = rec.task_id.clone();
        let goal = rec.goal.clone();
        // a child token: cancelling the coordinator cancels every agent
        let ctx = TaskCtx::new(rec, parent.cancel.child_token(), parent.depth + 1);
        if let Some(shared) = parent.shared.get() {
            let _ = ctx.shared.set(shared.clone());
        }
        let (tx, rx) = oneshot::channel();
        *ctx.done.lock() = Some(tx);
        self.live.lock().insert(id.clone(), ctx.clone());
        self.persist(&ctx).await;
        self.emit(&ctx, EventKind::TaskCreated { goal }).await;
        let inner = self.clone();
        tokio::spawn(async move { inner.run_task(ctx).await });
        (id, rx)
    }

    // ------------------------------------------------------------ execution ----

    /// Execute one delegated step: dispatch an agent, wait for it under
    /// supervision, and fold its cost and result back into the coordinator.
    pub(crate) async fn run_agent(
        self: &Arc<Self>,
        ctx: &Arc<TaskCtx>,
        step: &Step,
        deps: &[(String, Option<String>, String, String)],
    ) -> Result<StepOutput, StepFailure> {
        let fail = |kind: ErrorKind, m: String| StepFailure { kind, message: m };
        if ctx.depth > 0 {
            return Err(fail(ErrorKind::Policy, "agents cannot spawn agents".into()));
        }
        // ---- slice the budget -------------------------------------------------
        let (budget, upstream, entities, attempt) = {
            let mut r = ctx.rec.lock().await;
            if r.usage.children >= r.budget.max_children {
                return Err(fail(ErrorKind::Policy, format!("agent budget exceeded ({} allowed)", r.budget.max_children)));
            }
            let elapsed = (Utc::now() - r.created_at).num_seconds().max(0) as u64;
            let remaining_secs = r.budget.max_seconds.saturating_sub(elapsed);
            if remaining_secs == 0 {
                return Err(fail(ErrorKind::Policy, "time budget exceeded".into()));
            }
            let plan = r.plan.as_ref().ok_or_else(|| fail(ErrorKind::Other, "no plan".into()))?;
            // the join step (if any) keeps a small reserve of model calls
            let reserve = if plan.steps.iter().any(|s| s.kind == StepKind::Reasoning && !s.status.is_done()) { 2 } else { 0 };
            let pending_agents = plan.steps.iter().filter(|s| s.kind == StepKind::Agent && !s.status.is_done() || s.id == step.id).count().max(1) as u32;
            let models_left = r.budget.max_model_calls.saturating_sub(r.usage.model_calls).saturating_sub(reserve);
            let local_left = r.budget.max_worker_calls.saturating_sub(r.usage.worker_calls).saturating_sub(reserve);
            let tools_left = r.budget.max_tool_calls.saturating_sub(r.usage.tool_calls);
            let share = |left: u32| if left == 0 { 0 } else { (left / pending_agents).max(1) };
            let upstream: Vec<Upstream> = deps
                .iter()
                .map(|(id, _, summary, _)| Upstream {
                    label: format!("{id}: {}", tokens::truncate_tokens(&plan.step(id).map(|s| s.description.clone()).unwrap_or_default(), 14)),
                    summary: tokens::truncate_tokens(summary, 90),
                })
                .collect();
            let entities: Vec<Entity> = r.entities.iter().filter(|e| step.description.contains(e.text.as_str())).cloned().collect();
            let budget = Budget {
                max_steps: r.budget.max_steps,
                max_model_calls: share(models_left),
                max_worker_calls: share(local_left),
                max_tool_calls: share(tools_left),
                max_replans: r.budget.max_replans,
                max_seconds: remaining_secs,
                max_children: 0,
            };
            r.usage.children += 1;
            (budget, upstream, entities, step.attempts)
        };
        let mode = match step.args.get("mode").and_then(|m| m.as_str()) {
            Some("escalated") => ModeRequest::Escalated,
            _ => ModeRequest::Auto,
        };

        // ---- dispatch ------------------------------------------------------------
        let (child_id, rx) =
            self.spawn_child(ctx, ChildSpec { step_id: step.id.clone(), goal: step.description.clone(), upstream, entities, mode, budget }).await;
        {
            let mut r = ctx.rec.lock().await;
            if let Some(s) = r.plan.as_mut().and_then(|p| p.step_mut(&step.id)) {
                s.args = json!({"child": child_id, "mode": if mode == ModeRequest::Escalated { "escalated" } else { "auto" }});
            }
            if let Some(t) = r.team.as_mut() {
                t.children.push(ChildRef {
                    task_id: child_id.clone(),
                    step_id: step.id.clone(),
                    goal: step.description.clone(),
                    role: None,
                    mode: None,
                    status: TaskStatus::Running,
                });
            }
        }
        self.emit(ctx, EventKind::AgentSpawned { step_id: step.id.clone(), child_id: child_id.clone(), goal: step.description.clone(), attempt }).await;

        // ---- supervise -----------------------------------------------------------
        let child = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(fail(ErrorKind::Cancelled, "cancelled".into())),
            r = tokio::time::timeout(Duration::from_secs(self.cfg.team.agent_timeout_secs), rx) => match r {
                Ok(Ok(rec)) => rec,
                Ok(Err(_)) => return Err(fail(ErrorKind::Other, "the agent ended without a result".into())),
                Err(_) => {
                    if let Some(c) = self.live.lock().get(&child_id).cloned() {
                        c.cancel.cancel();
                    }
                    return Err(fail(ErrorKind::Timeout, format!("agent did not finish within {}s", self.cfg.team.agent_timeout_secs)));
                }
            },
        };

        // ---- roll up ----------------------------------------------------------------
        let role = child.team.as_ref().map(|t| t.role).unwrap_or(AgentRole::Researcher);
        let generative = child.ledger.summary_own().generative_tokens;
        {
            let mut r = ctx.rec.lock().await;
            r.usage.model_calls += child.usage.model_calls;
            r.usage.worker_calls += child.usage.worker_calls;
            r.usage.tool_calls += child.usage.tool_calls;
            for e in child.ledger.entries.iter().filter(|e| e.origin.is_none()) {
                let mut e = e.clone();
                e.origin = Some(child.task_id.clone());
                r.ledger.record(e);
            }
            if child.tainted {
                r.tainted = true; // what an agent saw may steer the join step
            }
            if let Some(c) = r.team.as_mut().and_then(|t| t.children.iter_mut().find(|c| c.task_id == child.task_id)) {
                c.role = Some(role);
                c.mode = child.mode;
                c.status = child.status;
            }
        }
        self.emit(
            ctx,
            EventKind::AgentDone {
                step_id: step.id.clone(),
                child_id: child.task_id.clone(),
                role,
                status: child.status,
                mode: child.mode,
                generative_tokens: generative,
            },
        )
        .await;

        match child.status {
            TaskStatus::Completed => {
                let ver = child.verification.as_ref().map(|v| v.status).unwrap_or(VerificationStatus::Unverified);
                if ver == VerificationStatus::Failed {
                    return Err(fail(
                        ErrorKind::Other,
                        format!("agent '{}' finished but its verification failed", tokens::truncate_tokens(&step.description, 12)),
                    ));
                }
                let content = child.final_answer.clone().filter(|a| !a.trim().is_empty()).unwrap_or_else(|| "(no output)".into());
                Ok(StepOutput { content, evidence: vec![format!("child:{}:{}", short(&child.task_id), status_word(ver)), format!("role:{}", role.as_str())] })
            }
            TaskStatus::Cancelled => Err(fail(ErrorKind::Cancelled, "agent was cancelled".into())),
            _ => Err(fail(
                classify_child_failure(&child),
                format!("agent '{}' failed: {}", tokens::truncate_tokens(&step.description, 12), child.error.clone().unwrap_or_else(|| "unknown error".into())),
            )),
        }
    }
}

/// What kind of failure was it? Decides whether supervision may retry it.
fn classify_child_failure(c: &TaskRecord) -> ErrorKind {
    if let Some(k) = c.plan.as_ref().and_then(|p| p.steps.iter().find(|s| s.status == StepStatus::Failed)).and_then(|s| s.error_kind) {
        return k;
    }
    if c.approvals.iter().any(|a| matches!(a.status, ApprovalStatus::Denied | ApprovalStatus::Expired)) {
        return ErrorKind::Policy;
    }
    let e = c.error.as_deref().unwrap_or("");
    if e.contains("budget") || e.contains("no language model") || e.contains("not available for this task") {
        return ErrorKind::Policy;
    }
    ErrorKind::Other
}
