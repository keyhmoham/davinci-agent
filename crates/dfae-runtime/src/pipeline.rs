//! The decision-first task pipeline.
//!
//! extract → retrieve → route → (direct binding | skill match) → select mode →
//! plan → execute → verify → answer → learn.
//!
//! Every stage prefers the cheapest component that can do the job: code, then
//! the decision model, then the SLM, then (only when needed) a stronger model.

use crate::engine::{ContextBase, Inner, TaskCtx};
use crate::{answer, planner, prompts, session, skills};
use chrono::Utc;
use dfae_core::*;
use dfae_decision::Decided;
use dfae_memory::{ContextInput, build_context, dedupe_memories, rank_memories};
use dfae_models::Tier;
use dfae_tools::{DirectCall, bind_direct};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

pub(crate) struct SkillMatch {
    pub memory_id: String,
    pub similarity: f32,
    pub plan: Plan,
}

impl Inner {
    pub(crate) async fn run_task(self: Arc<Self>, ctx: Arc<TaskCtx>) {
        // Top-level tasks pass through admission control (agents are limited by
        // their worker pools instead). A queued task stays `created`.
        let mut admitted = true;
        let _admission = if ctx.depth == 0 {
            match self.dispatcher.try_admit() {
                Some(p) => Some(p),
                None => {
                    let position = self.dispatcher.snapshot().queued + 1;
                    self.emit(&ctx, EventKind::Queued { position }).await;
                    tokio::select! {
                        _ = ctx.cancel.cancelled() => {
                            admitted = false;
                            None
                        }
                        (p, _) = self.dispatcher.admit() => Some(p),
                    }
                }
            }
        } else {
            None
        };
        let outcome = if !admitted {
            Err("cancelled".to_string())
        } else {
            match AssertUnwindSafe(self.pipeline(&ctx)).catch_unwind().await {
                Ok(r) => r,
                Err(_) => Err("internal error: the task pipeline panicked".to_string()),
            }
        };
        self.finalize(&ctx, outcome).await;
        self.live.lock().remove(&ctx.id);
        ctx.permit.lock().take(); // free the worker slot before the coordinator wakes
        let done = ctx.done.lock().take();
        if let Some(tx) = done {
            let rec = ctx.rec.lock().await.clone();
            let _ = tx.send(rec);
        }
    }

    pub(crate) fn decision_state(&self, goal: &str, entities: &[Entity]) -> DecisionState {
        DecisionState {
            request: goal.to_string(),
            entities: entities.iter().filter(|e| e.label != "action").take(12).map(|e| EntityBrief { label: e.label.clone(), text: e.text.clone() }).collect(),
            ..Default::default()
        }
    }

    async fn pipeline(self: &Arc<Self>, ctx: &Arc<TaskCtx>) -> Result<(), String> {
        self.set_status(ctx, TaskStatus::Running).await;
        let (goal, req) = {
            let r = ctx.rec.lock().await;
            (r.goal.clone(), r.request.clone())
        };

        // ---- 1. extraction (GLiNER + rules; agents inherit their slice) --------
        let entities = self.stage_extract(ctx, &goal).await;
        // ---- 1b. conversation: what "it", "that file" and "my city" refer to ---------
        let is_member = ctx.rec.lock().await.team.as_ref().and_then(|t| t.link.as_ref()).is_some();
        let history = if is_member { Vec::new() } else { self.session_history(ctx).await };
        let (goal, entities) = self.stage_resolve(ctx, goal, entities, &history).await;
        // ---- 2. retrieval ---------------------------------------------------
        self.stage_retrieve(ctx, &goal, &entities, req.profile.unwrap_or_default(), &history).await;

        // ---- 3. route -------------------------------------------------------
        let mut state = self.decision_state(&goal, &entities);
        state.recent = ctx.base_ctx.lock().turns.iter().map(|(q, a)| format!("user: {q} → assistant: {a}")).collect();
        let routing = self.note(ctx, self.decider.route_task(state.clone()).await).await;
        ctx.rec.lock().await.route = Some(routing.clone());

        // ---- 3b. subagents -----------------------------------------------------
        let is_agent = ctx.rec.lock().await.team.as_ref().and_then(|t| t.link.as_ref()).is_some();
        if is_agent {
            // role → tool scope → worker slot (waits if the role's pool is busy)
            self.stage_assign_role(ctx, &goal, &entities, &routing).await?;
        } else if self.cfg.team.enabled && req.delegate != Some(false) && req.mode == ModeRequest::Auto {
            // automatic delegation: split the request across a team when the decision layer agrees
            if let Some(result) = self.stage_team(ctx, &goal, &entities, &state, req.delegate == Some(true)).await {
                return result;
            }
        }

        // ---- 4. deterministic candidates -----------------------------------
        let metas = self.scoped_metas(ctx);
        let calls: Option<Vec<DirectCall>> =
            if req.mode == ModeRequest::Assisted || req.mode == ModeRequest::Escalated { None } else { bind_direct(&metas, &goal, &entities) };
        let skill = if matches!(req.mode, ModeRequest::Auto | ModeRequest::Direct) { self.find_skill(ctx, &goal, &entities).await } else { None };
        let skill_direct = skill.as_ref().map(|s| !s.plan.has_reasoning()).unwrap_or(false);

        // ---- 4b. conversation: nothing to run, so nothing to plan ----------------------
        if !is_agent && calls.is_none() && skill.is_none() && !routing.needs_tools && req.mode == ModeRequest::Auto {
            // a conversational reply of medium or higher complexity is written by the external model too
            let paid = self.models.has(Tier::Strong) && routing.complexity >= self.cfg.thresholds.external_min_complexity;
            return self.run_chat(ctx, &goal, false, if paid { ExecMode::Escalated } else { ExecMode::Assisted }).await;
        }

        // ---- 5. mode ---------------------------------------------------------
        let mut mstate = state.clone();
        mstate.mode_facts = Some(ModeFacts {
            direct_bindable: calls.is_some() || skill_direct,
            direct_tools: calls.as_ref().map(|c| c.iter().map(|x| x.tool.clone()).collect()).unwrap_or_default(),
            skill_score: skill.as_ref().map(|s| s.similarity),
            strong_available: self.models.has(Tier::Strong),
            slm_available: self.models.has(Tier::Slm),
            complexity: routing.complexity,
            risk: Some(routing.risk),
            task_type: routing.task_type.clone(),
            external_min_complexity: self.cfg.thresholds.external_min_complexity,
        });
        let choice = self.note(ctx, self.decider.select_mode(mstate, req.mode).await).await;
        let mut mode = choice.mode;
        let skill_id = skill.as_ref().map(|s| s.memory_id.clone());
        {
            let mut r = ctx.rec.lock().await;
            r.mode = Some(mode);
            r.mode_reason = Some(choice.reason.clone());
        }
        self.emit(ctx, EventKind::Mode { mode, reason: choice.reason.clone(), skill_id: skill_id.clone() }).await;

        // ---- 6. plan ---------------------------------------------------------
        let plan = match (&skill, mode) {
            (Some(s), _) if matches!(req.mode, ModeRequest::Auto | ModeRequest::Direct) && !(mode == ExecMode::Direct && s.plan.has_reasoning()) => {
                ctx.rec.lock().await.skill_id = Some(s.memory_id.clone());
                s.plan.clone()
            }
            (_, ExecMode::Direct) => match &calls {
                Some(c) => planner::plan_from_calls(&goal, c),
                None => return Err("direct mode needs a deterministic tool binding, but none matched this request".into()),
            },
            _ => {
                let (plan, used_mode) = self.stage_model_plan(ctx, mode, &goal, &entities, &state, calls.as_deref()).await?;
                if used_mode != mode {
                    mode = used_mode;
                    ctx.rec.lock().await.mode = Some(mode);
                }
                plan
            }
        };
        {
            let allowed: Vec<String> = {
                let mut a = ctx.allowed.lock();
                if a.is_empty() || plan.source == PlanSource::Skill || plan.source == PlanSource::Direct {
                    *a = plan.steps.iter().filter_map(|s| s.tool.clone()).collect();
                }
                a.clone()
            };
            // direct / skill plans are re-validated like any other plan
            let remaining = self.remaining_budget(ctx).await;
            if let Err(errs) = planner::validate_plan(&plan.steps, &self.tools, &allowed, &remaining) {
                return Err(format!("plan rejected: {}", errs.join("; ")));
            }
        }
        // bounded plan sanity check by the decision layer (recorded; hard rules live in policy)
        {
            let mut pstate = state.clone();
            pstate.plan_facts = Some(self.plan_facts(&plan));
            let p = self.note(ctx, self.decider.assess_plan(pstate).await).await;
            if p < 0.3 {
                self.emit(ctx, EventKind::Warning { message: format!("decision layer doubts this plan (p={p:.2}); execution continues under policy") }).await;
            }
        }
        let mut plan = plan;
        // a brief plan's remaining work is the local system's: free, apart from what the planner flagged
        let any_brief = plan.steps.iter().any(|s| s.brief);
        plan.estimate = Some(self.cost.estimate_plan(&plan, self.is_external(mode) && !any_brief));
        ctx.rec.lock().await.plan = Some(plan.clone());
        self.emit(ctx, EventKind::PlanCreated { plan }).await;
        self.persist(ctx).await;

        self.execute_and_conclude(ctx, &goal, mode).await
    }

    /// execute → verify (+ at most one continuation) → answer. Shared by fresh
    /// runs and by tasks resumed after a restart.
    pub(crate) async fn execute_and_conclude(self: &Arc<Self>, ctx: &Arc<TaskCtx>, goal: &str, mode: ExecMode) -> Result<(), String> {
        let phased = ctx.rec.lock().await.plan.as_ref().map(|p| p.steps.iter().any(|s| s.brief || s.how.is_some())).unwrap_or(false);
        if phased {
            self.emit(ctx, EventKind::Phase { phase: "acting".into(), detail: "the local system implements and runs each step of the plan".into() }).await;
        }
        let mut exec_err = self.execute_plan(ctx).await.err();
        if self.is_cancelled(ctx).await {
            return Err("cancelled".into());
        }
        self.set_status(ctx, TaskStatus::Verifying).await;
        if phased {
            self.emit(ctx, EventKind::Phase { phase: "reviewing".into(), detail: "the result is judged against the request".into() }).await;
        }
        let mut verification = self.stage_verify(ctx).await;
        let is_chat = ctx.rec.lock().await.plan.as_ref().map(|p| p.source == PlanSource::Chat).unwrap_or(false);
        if exec_err.is_none()
            && !is_chat
            && verification.completion_confidence.map(|p| p < 0.35).unwrap_or(false)
            && let Ok(true) = self.continue_plan(ctx, goal).await
        {
            self.set_status(ctx, TaskStatus::Running).await;
            exec_err = self.execute_plan(ctx).await.err();
            self.set_status(ctx, TaskStatus::Verifying).await;
            verification = self.stage_verify(ctx).await;
        }
        ctx.rec.lock().await.verification = Some(verification);
        let mode = ctx.rec.lock().await.mode.unwrap_or(mode);
        self.stage_answer(ctx, mode, exec_err.as_deref()).await;
        match exec_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Continue a task that was interrupted by a restart, from its checkpoint.
    pub(crate) async fn resume_task(self: Arc<Self>, ctx: Arc<TaskCtx>) {
        let outcome = match AssertUnwindSafe(self.resume_pipeline(&ctx)).catch_unwind().await {
            Ok(r) => r,
            Err(_) => Err("internal error: the resumed task panicked".to_string()),
        };
        self.finalize(&ctx, outcome).await;
        self.live.lock().remove(&ctx.id);
    }

    async fn resume_pipeline(self: &Arc<Self>, ctx: &Arc<TaskCtx>) -> Result<(), String> {
        // delegated steps that were mid-flight: what became of their agents?
        let mut agents: std::collections::HashMap<String, TaskRecord> = Default::default();
        let running: Vec<String> = {
            let r = ctx.rec.lock().await;
            r.plan
                .as_ref()
                .map(|p| {
                    p.steps
                        .iter()
                        .filter(|s| s.kind == StepKind::Agent && s.status == StepStatus::Running)
                        .filter_map(|s| s.args.get("child").and_then(|c| c.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        for id in running {
            if let Ok(Some(c)) = self.store.get_task(&id).await {
                agents.insert(id, c);
            }
        }
        if ctx.rec.lock().await.team.as_ref().map(|t| t.link.is_none()).unwrap_or(false) {
            let _ = ctx.shared.set(Arc::new(crate::team::TeamShared::default()));
        }
        let (goal, mode) = {
            let mut r = ctx.rec.lock().await;
            let Some(plan) = r.plan.as_mut() else { return Err("interrupted by a restart before a plan existed".into()) };
            for s in &mut plan.steps {
                if s.kind == StepKind::Agent && s.status == StepStatus::Running {
                    let finished = s.args.get("child").and_then(|c| c.as_str()).and_then(|id| agents.get(id)).filter(|c| c.status == TaskStatus::Completed);
                    match finished {
                        // the agent completed before the restart: adopt its result
                        Some(c) => {
                            let answer = c.final_answer.clone().unwrap_or_default();
                            s.status = StepStatus::Succeeded;
                            s.summary = Some(tokens::compress_text(&answer, 300));
                            s.output = Some(answer);
                            s.evidence = vec![format!(
                                "child:{}:{}",
                                short(&c.task_id),
                                c.verification.as_ref().map(|v| format!("{:?}", v.status).to_lowercase()).unwrap_or_else(|| "unverified".into())
                            )];
                        }
                        // otherwise its outcome is unknown and its side effects cannot be ruled out
                        None => {
                            s.status = StepStatus::Failed;
                            s.error = Some("interrupted by a restart; the agent's outcome is unknown and it was not re-run".into());
                        }
                    }
                    continue;
                }
                match s.status {
                    // outcome unknown: never silently re-run something with side effects
                    StepStatus::Running => {
                        let side = s.tool.as_ref().and_then(|t| self.tools.meta(t)).map(|m| m.side_effects).unwrap_or(false);
                        if side {
                            s.status = StepStatus::Failed;
                            s.error = Some("interrupted by a restart; the outcome is unknown and the step was not re-run".into());
                        } else {
                            s.status = StepStatus::Pending;
                        }
                    }
                    StepStatus::WaitingApproval => s.status = StepStatus::Pending,
                    _ => {}
                }
            }
            for a in r.approvals.iter_mut().filter(|a| a.status == ApprovalStatus::Pending) {
                a.status = ApprovalStatus::Expired;
            }
            let allowed: Vec<String> = r.plan.as_ref().map(|p| p.steps.iter().filter_map(|s| s.tool.clone()).collect()).unwrap_or_default();
            *ctx.allowed.lock() = allowed;
            (r.goal.clone(), r.mode.unwrap_or(ExecMode::Assisted))
        };
        self.set_status(ctx, TaskStatus::Running).await;
        self.emit(ctx, EventKind::Warning { message: "task resumed after a restart".into() }).await;
        self.execute_and_conclude(ctx, &goal, mode).await
    }

    fn plan_facts(&self, plan: &Plan) -> PlanFacts {
        let side = plan.steps.iter().filter(|s| s.tool.as_ref().and_then(|t| self.tools.meta(t)).map(|m| m.side_effects).unwrap_or(false)).count();
        let max_risk = plan.steps.iter().filter_map(|s| s.tool.as_ref().and_then(|t| self.tools.meta(t))).map(|m| m.risk).max();
        PlanFacts {
            steps: plan.steps.len(),
            tool_steps: plan.tool_steps(),
            reasoning_steps: plan.steps.len() - plan.tool_steps(),
            side_effect_steps: side,
            max_risk,
            summary: plan.steps.iter().map(|s| format!("{}:{}", s.id, s.tool.as_deref().unwrap_or("reason"))).collect::<Vec<_>>().join(","),
        }
    }

    // ------------------------------------------------------------ stages ----

    async fn stage_extract(&self, ctx: &Arc<TaskCtx>, goal: &str) -> Vec<Entity> {
        // an agent inherits the entities of its sub-goal from the coordinator: no extractor call
        let inherited = ctx.rec.lock().await.team.as_ref().and_then(|t| t.link.as_ref()).map(|l| l.entities.clone()).filter(|e| !e.is_empty());
        if let Some(entities) = inherited {
            ctx.rec.lock().await.entities = entities.clone();
            self.emit(ctx, EventKind::Extraction { entities: entities.clone(), backend: "inherited".into(), latency_ms: 0 }).await;
            return entities;
        }
        let labels = dfae_extract::labels_for(None);
        let res = self.extractor.extract(ExtractionRequest { text: goal.to_string(), labels, threshold: 0.4 }).await;
        let (entities, backend, latency, cached) = match res {
            Ok(r) => (r.entities, r.backend, r.latency_ms, r.cached),
            Err(e) => {
                self.emit(ctx, EventKind::Warning { message: format!("entity extraction failed: {e}") }).await;
                (vec![], "none".into(), 0, false)
            }
        };
        if backend.contains("gliner") {
            self.add_ledger(
                ctx,
                LedgerEntry {
                    ts: Utc::now(),
                    component: Component::Gliner,
                    model: backend.clone(),
                    purpose: "extract".into(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    latency_ms: latency,
                    cached,
                    origin: None,
                },
            )
            .await;
        }
        ctx.rec.lock().await.entities = entities.clone();
        self.emit(ctx, EventKind::Extraction { entities: entities.clone(), backend, latency_ms: latency }).await;
        entities
    }

    /// Earlier finished turns of this conversation (oldest first), without sub-agent tasks.
    pub(crate) async fn session_history(&self, ctx: &Arc<TaskCtx>) -> Vec<TaskRecord> {
        let session = ctx.rec.lock().await.session_id.clone();
        let mut v: Vec<TaskRecord> = self
            .store
            .list_tasks(60)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.session_id == session && t.task_id != ctx.id && t.status.is_terminal() && t.team.as_ref().and_then(|x| x.link.as_ref()).is_none())
            .collect();
        v.sort_by_key(|t| t.created_at);
        v
    }

    /// Does the request lean on something said earlier ("it", "that file", "my city")? The decision model is shown what
    /// exists and decides whether and which; the code then makes that explicit before anything else reads the request.
    async fn stage_resolve(&self, ctx: &Arc<TaskCtx>, goal: String, entities: Vec<Entity>, history: &[TaskRecord]) -> (String, Vec<Entity>) {
        let facts = session::facts_from(history.iter());
        let own_place = entities.iter().find(|e| e.label == "location").map(|e| e.text.clone());
        let has_code = session::artifacts_from(history).iter().any(|(n, _, _)| n == "prev_code");
        let (known, options) = session::known_things(&facts, own_place.as_deref(), has_code);
        if options.is_empty() {
            return (goal, entities); // nothing was said that a request could refer to
        }
        let mut st = self.decision_state(&goal, &entities);
        st.known = known;
        st.recent = history
            .iter()
            .rev()
            .take(4)
            .rev()
            .map(|t| {
                format!("user: {} → assistant: {}", tokens::truncate_tokens(&t.goal, 60), tokens::truncate_tokens(t.final_answer.as_deref().unwrap_or(""), 100))
            })
            .collect();
        let Some(kind) = self.note(ctx, self.decider.resolve_reference(st, &options).await).await else { return (goal, entities) };
        let (new_goal, new_entities, notes) = session::apply(&goal, &entities, &facts, own_place.as_deref(), &kind);
        if notes.is_empty() {
            return (goal, entities);
        }
        {
            let mut r = ctx.rec.lock().await;
            r.goal = new_goal.clone();
            r.entities = new_entities.clone();
        }
        self.emit(ctx, EventKind::Repaired { step_id: "goal".into(), note: format!("understood from the conversation: {}", notes.join("; ")) }).await;
        (new_goal, new_entities)
    }

    /// What the agent can do, in a few words (tool categories).
    pub(crate) fn capabilities(&self, ctx: &Arc<TaskCtx>) -> String {
        let mut cats: Vec<String> = self.scoped_metas(ctx).into_iter().map(|m| m.category).collect();
        cats.sort();
        cats.dedup();
        cats.join(", ")
    }

    /// A conversational turn: one reply, no planning. `gap` = a tool-shaped request no tool fits.
    async fn run_chat(self: &Arc<Self>, ctx: &Arc<TaskCtx>, goal: &str, gap: bool, mode: ExecMode) -> Result<(), String> {
        let reason = if mode == ExecMode::Escalated {
            "conversation of medium or higher complexity: the external model answers".to_string()
        } else {
            "conversation: no tool needed, so no plan".to_string()
        };
        {
            let mut r = ctx.rec.lock().await;
            r.mode = Some(mode);
            r.mode_reason = Some(reason.clone());
        }
        self.emit(ctx, EventKind::Mode { mode, reason, skill_id: None }).await;
        let mut plan = planner::chat_plan(goal, gap);
        plan.estimate = Some(self.cost.estimate_plan(&plan, self.is_external(mode)));
        ctx.rec.lock().await.plan = Some(plan.clone());
        self.emit(ctx, EventKind::PlanCreated { plan }).await;
        self.persist(ctx).await;
        self.execute_and_conclude(ctx, goal, mode).await
    }

    async fn stage_retrieve(&self, ctx: &Arc<TaskCtx>, goal: &str, entities: &[Entity], profile: ContextProfile, history: &[TaskRecord]) {
        let session = ctx.rec.lock().await.session_id.clone();
        let cands = self
            .store
            .retrieve(MemoryQuery {
                text: goal.to_string(),
                kinds: vec![MemoryKind::Episodic, MemoryKind::Semantic],
                limit: 12,
                session_id: Some(session.clone()),
            })
            .await
            .unwrap_or_default();
        let ranked: Vec<_> = dedupe_memories(rank_memories(cands, entities)).into_iter().filter(|m| m.similarity >= 0.2).take(6).collect();
        let refs: Vec<RetrievedRef> = ranked
            .iter()
            .map(|m| RetrievedRef {
                id: m.item.id.clone(),
                kind: m.item.kind.as_str().into(),
                score: m.score,
                preview: tokens::truncate_tokens(&m.item.text, 24),
            })
            .collect();
        // earlier turns of the same session (cheap conversational continuity)
        let mut prior: Vec<String> = Vec::new();
        for t in history.iter().rev().take(4).rev() {
            prior.push(format!("{} → {}", tokens::truncate_tokens(&t.goal, 24), tokens::truncate_tokens(t.final_answer.as_deref().unwrap_or(""), 30)));
        }
        let turns: Vec<(String, String)> = history
            .iter()
            .rev()
            .take(8)
            .rev()
            .map(|t| (tokens::truncate_tokens(&t.goal, 150), tokens::truncate_tokens(t.final_answer.as_deref().or(t.error.as_deref()).unwrap_or(""), 250)))
            .collect();
        let upstream = ctx
            .rec
            .lock()
            .await
            .team
            .as_ref()
            .and_then(|t| t.link.as_ref())
            .map(|l| l.upstream.iter().map(|u| (u.label.clone(), u.summary.clone())).collect())
            .unwrap_or_default();
        let artifacts = session::artifacts_from(history);
        *ctx.base_ctx.lock() = ContextBase { memories: ranked, prior_turns: prior, turns, artifacts, upstream };
        if profile != ContextProfile::Minimal {
            ctx.rec.lock().await.retrieved = refs.clone();
            self.emit(ctx, EventKind::Retrieval { items: refs }).await;
        }
    }

    /// Context text for prompts (rebuilt from the current task state).
    /// The context a model is given. A self-hosted model (free per token) always gets the fullest profile;
    /// the external one the smallest that fits the task — unless the request pinned a profile.
    pub(crate) async fn context_text(&self, ctx: &Arc<TaskCtx>, include_observations: bool, tier: Tier) -> String {
        let base = ctx.base_ctx.lock().clone();
        let r = ctx.rec.lock().await;
        let obs: Vec<(String, String)> = if include_observations {
            r.plan
                .as_ref()
                .map(|p| {
                    p.steps
                        .iter()
                        .filter(|s| s.status == StepStatus::Succeeded)
                        .map(|s| {
                            (
                                format!("{} {}", s.id, s.tool.as_deref().unwrap_or(if s.kind == StepKind::Agent { "agent" } else { "reasoning" })),
                                s.summary.clone().unwrap_or_default(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        };
        let profile = r.request.profile.unwrap_or(if tier != Tier::Strong {
            ContextProfile::Reasoning
        } else {
            match r.route.as_ref().map(|x| x.complexity).unwrap_or(0.3) {
                c if c >= 0.65 => ContextProfile::Reasoning,
                c if c < 0.25 => ContextProfile::Minimal,
                _ => ContextProfile::Standard,
            }
        });
        let pkg = build_context(&ContextInput {
            goal: &r.goal,
            entities: &r.entities,
            memories: &base.memories,
            prior_turns: &base.prior_turns,
            observations: &obs,
            upstream: &base.upstream,
            profile,
        });
        let mut text = pkg.render();
        // what "it", "that code" and "the page above" can refer to, by name: a step uses `{{prev_code}}`, nobody copies the code
        if !base.artifacts.is_empty() {
            let list = base.artifacts.iter().map(|(n, _, d)| format!("{n} = {d}")).collect::<Vec<_>>().join("; ");
            text.push_str(&format!("\nVALUES from the conversation (write {{{{name}}}} in an argument or a step instead of copying them): {list}"));
        }
        text
    }

    async fn find_skill(&self, ctx: &Arc<TaskCtx>, goal: &str, entities: &[Entity]) -> Option<SkillMatch> {
        let slots = skills::slot_values(goal, entities);
        let masked = skills::mask_request(goal, &slots);
        let cands = self.store.retrieve(MemoryQuery { text: masked, kinds: vec![MemoryKind::Procedural], limit: 3, session_id: None }).await.ok()?;
        for c in cands {
            if c.similarity < self.cfg.thresholds.skill_min_similarity {
                continue;
            }
            let Some(skill) = c.item.metadata.get("skill").and_then(|v| serde_json::from_value::<skills::Skill>(v.clone()).ok()) else { continue };
            if let Some(plan) =
                skills::instantiate(&skill, goal, entities).filter(|p| p.steps.iter().filter_map(|s| s.tool.as_deref()).all(|t| ctx.in_scope(t)))
            {
                return Some(SkillMatch { memory_id: c.item.id, similarity: c.similarity, plan });
            }
        }
        None
    }

    /// Ask the decision layer which tools matter, then have a model plan with
    /// only those tools in its prompt. Returns the plan and the mode actually used.
    async fn stage_model_plan(
        self: &Arc<Self>,
        ctx: &Arc<TaskCtx>,
        mode: ExecMode,
        goal: &str,
        entities: &[Entity],
        base_state: &DecisionState,
        calls: Option<&[DirectCall]>,
    ) -> Result<(Plan, ExecMode), String> {
        // tool selection (decision layer), unioned with deterministic matches
        let mut tstate = base_state.clone();
        tstate.tools = self.scoped_briefs(ctx);
        let picked = self.note(ctx, self.decider.select_tools(tstate).await).await;
        let mut allowed: Vec<String> = picked.into_iter().map(|(n, _)| n).collect();
        // every tool listed is prompt text: a paid model gets the six best, a self-hosted one all that were judged relevant
        if self.pick_tier(mode) == Some(Tier::Strong) {
            allowed.truncate(6);
        }
        for c in calls.unwrap_or(&[]) {
            if !allowed.contains(&c.tool) {
                allowed.push(c.tool.clone());
            }
        }
        // tools that are only useful together: a model that edits must be able to read and find, one that
        // starts a background job must be able to read its output and stop it
        if !allowed.is_empty() {
            for name in allowed.clone() {
                for friend in planner::companions(&name) {
                    // background-job tools only exist for a shell that can start jobs
                    let relevant =
                        !friend.starts_with("shell_") || self.tools.meta("shell").map(|m| m.params.iter().any(|p| p.name == "background")).unwrap_or(false);
                    if relevant && self.tools.meta(friend).is_some() && ctx.in_scope(friend) && !allowed.iter().any(|a| a == friend) {
                        allowed.push((*friend).to_string());
                    }
                }
            }
        }
        // no tool fits: this is a conversation (or a request we have no tool for) — answer it, don't plan it
        let is_member = ctx.rec.lock().await.team.as_ref().and_then(|t| t.link.as_ref()).is_some();
        if allowed.is_empty() && !is_member {
            return Ok((planner::chat_plan(goal, true), ExecMode::Assisted));
        }
        *ctx.allowed.lock() = allowed.clone();

        let budget = ctx.rec.lock().await.budget.clone();
        let mut tier = self
            .pick_tier(mode)
            .ok_or("no language model is configured (set SLM_BASE_URL or STRONG_API_KEY) and this request cannot be handled deterministically")?;
        let mut used_mode = if tier == Tier::Strong { ExecMode::Escalated } else { mode };
        let mut last_err = String::new();
        for attempt in 0..2 {
            let ctx_text = self.context_text(ctx, false, tier).await;
            // a self-hosted model gets parameter help and worked examples; the paid one gets the leanest prompt
            // planning phase: the external model writes a brief plan of intents (no arguments: the local system
            // supplies them step by step); a self-hosted planner writes a concrete plan as before
            let brief = tier == Tier::Strong;
            if brief {
                self.emit(
                    ctx,
                    EventKind::Phase { phase: "planning".into(), detail: "the external model writes a brief plan; the local system will carry it out".into() },
                )
                .await;
            }
            let (sigs, system) = if brief {
                (self.tools.brief_lines(&allowed), prompts::BRIEF_PLAN_SYSTEM)
            } else {
                (self.tools.signatures_detailed(&allowed), prompts::PLAN_SYSTEM_LOCAL)
            };
            let user = prompts::plan_user(&sigs, &ctx_text, budget.max_steps.min(12));
            let mut req = self.gen_request(tier, system, user, if attempt == 0 { "plan" } else { "plan_escalated" });
            req.json = true;
            let source = if tier == Tier::Strong { PlanSource::Strong } else { PlanSource::Slm };
            match self.try_plan(ctx, tier, req, goal, source, &allowed, brief).await {
                Ok(steps) => {
                    let _ = entities;
                    return Ok((Plan { goal: goal.into(), steps, source, revision: 0, estimate: None }, used_mode));
                }
                Err(e) => {
                    last_err = e;
                    // one escalation to the stronger model, if there is one
                    if tier == Tier::Slm && self.models.has(Tier::Strong) {
                        self.emit(ctx, EventKind::Warning { message: format!("SLM plan failed ({last_err}); escalating the plan to the stronger model") })
                            .await;
                        tier = Tier::Strong;
                        used_mode = ExecMode::Escalated;
                    } else {
                        break;
                    }
                }
            }
        }
        Err(format!("could not produce a valid plan: {last_err}"))
    }

    /// Generate → parse → validate, with a single repair round naming the problems.
    #[allow(clippy::too_many_arguments)]
    async fn try_plan(
        &self,
        ctx: &Arc<TaskCtx>,
        tier: Tier,
        mut req: GenerationRequest,
        goal: &str,
        source: PlanSource,
        allowed: &[String],
        brief: bool,
    ) -> Result<Vec<Step>, String> {
        let parse =
            |v: &Value| if brief { planner::parse_brief_plan(v, goal, source, "", &self.tools) } else { planner::parse_plan(v, goal, source, "", &self.tools) };
        let v = self.gen_json(ctx, tier, req.clone()).await?;
        let _ = allowed; // the live list is ctx.allowed: admitting a tool below widens it
        let problems = match parse(&v) {
            Ok(steps) => match self.finish_plan(ctx, steps, goal, true).await {
                Ok(steps) => return Ok(steps),
                Err(errs) => errs,
            },
            Err(e) => vec![e],
        };
        req.messages.push(Message::assistant(tokens::truncate_tokens(&v.to_string(), 500)));
        req.messages.push(Message::user(format!("Invalid plan:\n- {}\nReturn the corrected JSON only.", problems.join("\n- "))));
        req.purpose = "plan_repair".into();
        let v2 = self.gen_json(ctx, tier, req).await?;
        let steps = parse(&v2)?;
        // the second attempt is judged on validity alone: a plan that is merely less ambitious is still a plan
        self.finish_plan(ctx, steps, goal, false).await.map_err(|e| e.join("; "))
    }

    /// Everything code can do for a freshly parsed plan before a model is asked again:
    /// fix arguments, admit harmless tools the selector missed, drop dangling dependencies,
    /// then validate (and, on the first attempt, check that a request to change something plans a change).
    async fn finish_plan(&self, ctx: &Arc<TaskCtx>, mut steps: Vec<Step>, goal: &str, expect_effect: bool) -> Result<Vec<Step>, Vec<String>> {
        self.repair_plan_args(ctx, &mut steps, goal).await;
        self.admit_readonly_tools(ctx, &steps).await;
        let dropped = planner::prune_unknown_deps(&mut steps);
        if !dropped.is_empty() {
            self.emit(
                ctx,
                EventKind::Repaired { step_id: "plan".into(), note: format!("dropped dependencies on steps that do not exist: {}", dropped.join(", ")) },
            )
            .await;
        }
        let allowed = ctx.allowed.lock().clone();
        let remaining = self.remaining_budget(ctx).await;
        planner::validate_plan(&steps, &self.tools, &allowed, &remaining)?;
        if expect_effect {
            let member = ctx.rec.lock().await.team.as_ref().and_then(|t| t.link.as_ref()).is_some();
            // the decision model said whether the request asks for a change, and whether that change is only conditional
            let changes = !member && ctx.rec.lock().await.route.as_ref().map(|r| r.needs_tools && r.changes && !r.conditional).unwrap_or(false);
            if changes && let Some(p) = planner::missing_effect(&steps, &self.tools, &allowed) {
                return Err(vec![p]);
            }
        }
        Ok(steps)
    }

    /// A model that names a real, harmless tool the selector did not offer (it is read-only, needs no
    /// approval, and is inside this agent's scope) gets it, instead of a failed task and a repair call.
    async fn admit_readonly_tools(&self, ctx: &Arc<TaskCtx>, steps: &[Step]) {
        for s in steps.iter().filter(|s| s.kind == StepKind::Tool) {
            let Some(name) = s.tool.as_deref() else { continue };
            let Some(m) = self.tools.meta(name) else { continue };
            let harmless = !m.side_effects && !m.requires_approval && m.risk == Risk::Low && ctx.in_scope(name);
            let admitted = harmless && {
                let mut a = ctx.allowed.lock();
                let fresh = !a.iter().any(|x| x == name);
                if fresh {
                    a.push(name.to_string());
                }
                fresh
            };
            if admitted {
                self.emit(
                    ctx,
                    EventKind::Repaired { step_id: s.id.clone(), note: format!("'{name}' was not pre-selected, but it is read-only, so it is allowed") },
                )
                .await;
            }
        }
    }

    /// Evidence was judged insufficient: ask a model for the missing steps (once).
    async fn continue_plan(self: &Arc<Self>, ctx: &Arc<TaskCtx>, goal: &str) -> Result<bool, String> {
        let (replans, max, coordinator) = {
            let r = ctx.rec.lock().await;
            (r.usage.replans, r.budget.max_replans, r.team.as_ref().map(|t| t.link.is_none()).unwrap_or(false))
        };
        if replans >= max || coordinator {
            return Ok(false);
        }
        let mode = ctx.rec.lock().await.mode.unwrap_or(ExecMode::Assisted);
        let Some(tier) = self.pick_tier(mode) else { return Ok(false) };
        let added =
            self.replan(ctx, tier, goal, "the evidence collected so far does not show the request is fully accomplished; add the missing steps", None).await?;
        Ok(added)
    }

    // -------------------------------------------------------- finalize ----

    async fn finalize(self: &Arc<Self>, ctx: &Arc<TaskCtx>, outcome: Result<(), String>) {
        let cancelled = ctx.cancel.is_cancelled();
        // move remaining non-terminal steps out of "pending"
        {
            let mut r = ctx.rec.lock().await;
            if let Some(p) = r.plan.as_mut() {
                for s in &mut p.steps {
                    if !s.status.is_done() {
                        s.status = StepStatus::Cancelled;
                    }
                }
            }
        }
        let status = if cancelled {
            TaskStatus::Cancelled
        } else if outcome.is_err() {
            TaskStatus::Failed
        } else {
            TaskStatus::Completed
        };
        if let Err(e) = &outcome {
            let mut r = ctx.rec.lock().await;
            r.error = Some(e.clone());
            if r.final_answer.is_none() {
                r.final_answer = Some(answer::templated_answer(&r, Some(e)));
            }
        }
        if cancelled {
            ctx.rec.lock().await.final_answer.get_or_insert_with(|| "Task cancelled.".into());
        }
        // coordinators: fold in what the team did (agents' modes, cache hits, the work they performed)
        let child_ids: Vec<String> = {
            let r = ctx.rec.lock().await;
            match &r.team {
                Some(t) if t.link.is_none() => t.children.iter().map(|c| c.task_id.clone()).collect(),
                _ => vec![],
            }
        };
        let mut team_steps_raw: Vec<u64> = Vec::new();
        let mut team_mode: Option<ExecMode> = None;
        for id in &child_ids {
            if let Ok(Some(c)) = self.store.get_task(id).await {
                team_mode = team_mode.max(c.mode);
                if let Some(p) = &c.plan {
                    team_steps_raw.extend(
                        p.steps.iter().filter(|s| s.status.is_done()).map(|s| tokens::estimate_tokens(s.output.as_deref().unwrap_or("")).min(4000) as u64),
                    );
                }
            }
        }
        {
            let mut r = ctx.rec.lock().await;
            if let Some(t) = r.team.as_mut().filter(|t| t.link.is_none()) {
                t.cache_hits = ctx.shared.get().map(|s| s.hits.load(std::sync::atomic::Ordering::SeqCst)).unwrap_or(0);
            }
            if !child_ids.is_empty() {
                r.mode = r.mode.max(team_mode);
            }
        }
        // token ledger: baseline for a naive tool-calling LLM agent. For a team the
        // baseline is ONE naive agent doing all of the work, not a sum of per-agent baselines.
        {
            let mut r = ctx.rec.lock().await;
            let mut raw: Vec<u64> = r
                .plan
                .as_ref()
                .map(|p| {
                    p.steps
                        .iter()
                        .filter(|s| s.status.is_done() && s.kind != StepKind::Agent)
                        .map(|s| tokens::estimate_tokens(s.output.as_deref().unwrap_or("")).min(4000) as u64)
                        .collect()
                })
                .unwrap_or_default();
            raw.extend(team_steps_raw);
            let b = naive_baseline_tokens(
                350,
                self.tools.full_schema_tokens() as u64,
                tokens::estimate_tokens(&r.goal) as u64,
                &raw,
                tokens::estimate_tokens(r.final_answer.as_deref().unwrap_or("")) as u64,
            );
            r.ledger.baseline_tokens = b;
        }
        // status transition (running → terminal)
        self.set_status(ctx, status).await;
        let snapshot = ctx.rec.lock().await.clone();
        self.learn(&snapshot).await;
        let summary = snapshot.ledger.summary();
        self.emit(ctx, EventKind::Ledger { summary }).await;
        self.emit(ctx, EventKind::Final { answer: snapshot.final_answer.clone().unwrap_or_default(), status }).await;
        self.persist(ctx).await;
    }

    /// Calibration, episodic memory, validated fact promotion, skill learning.
    async fn learn(&self, rec: &TaskRecord) {
        let verified = rec.verification.as_ref().map(|v| v.status == VerificationStatus::Verified).unwrap_or(false);
        let success = rec.status == TaskStatus::Completed && rec.verification.as_ref().map(|v| v.status != VerificationStatus::Failed).unwrap_or(false);
        if rec.status == TaskStatus::Cancelled {
            return;
        }
        // 1. calibration: did the decisions the engine relied on hold up?
        let coordinator = rec.team.as_ref().map(|t| t.link.is_none()).unwrap_or(false);
        if coordinator {
            // the split itself is the decision that needs judging
            self.decider.record_outcome(DecisionKind::AssessDecomposition, "split", success);
        }
        if let (Some(m), false) = (rec.mode, coordinator) {
            let key = match m {
                ExecMode::Direct => "direct",
                ExecMode::Assisted => "assisted",
                ExecMode::Escalated => "escalated",
            };
            self.decider.record_outcome(DecisionKind::SelectMode, key, success);
        }
        if let Some(r) = &rec.route {
            self.decider.record_outcome(DecisionKind::RouteTask, &r.task_type, success);
        }
        let _ = self.store.kv_set("calibration", self.decider.calibrator.snapshot()).await;

        // 1b. the cost model learns what this task's model calls cost
        self.learn_costs(rec).await;

        // 2. episodic memory (failures are kept too: they are the most useful lessons)
        let answer = tokens::truncate_tokens(rec.final_answer.as_deref().unwrap_or(""), 60);
        let text = format!("{} → {} [{}]", rec.goal, answer, if success { "ok" } else { "failed" });
        let mut item = MemoryItem::new(MemoryKind::Episodic, text, if success { 0.5 } else { 0.7 });
        item.entities = rec.entities.clone();
        item.session_id = Some(rec.session_id.clone());
        item.metadata = json!({"task_id": rec.task_id, "success": success, "mode": rec.mode});
        let _ = self.store.store(item).await;

        // 3. validated fact promotion: extractor output is only a candidate. It
        //    becomes semantic memory if the task was verified, the label is on
        //    the allowlist and the value was corroborated by tool evidence.
        if verified {
            let evidence: String = rec
                .plan
                .as_ref()
                .map(|p| p.steps.iter().filter(|s| s.kind == StepKind::Tool).filter_map(|s| s.output.clone()).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            for e in rec.entities.iter().filter(|e| matches!(e.label.as_str(), "server" | "database" | "repository") && e.score >= 0.6) {
                if evidence.contains(&e.text) {
                    let fact = format!("{}: {}", e.label, e.text);
                    let dup = self
                        .store
                        .retrieve(MemoryQuery { text: fact.clone(), kinds: vec![MemoryKind::Semantic], limit: 1, session_id: None })
                        .await
                        .ok()
                        .and_then(|r| r.into_iter().next())
                        .map(|r| r.similarity > 0.95)
                        .unwrap_or(false);
                    if !dup {
                        let mut it = MemoryItem::new(MemoryKind::Semantic, fact, 0.6);
                        it.entities = vec![e.clone()];
                        it.metadata = json!({"source": "promoted-entity", "task_id": rec.task_id});
                        let _ = self.store.store(it).await;
                    }
                }
            }
        }

        // 4. skills: replay bookkeeping, or crystallise a new one
        if let Some(sid) = &rec.skill_id {
            let masked = skills::mask_request(&rec.goal, &skills::slot_values(&rec.goal, &rec.entities));
            if let Ok(found) = self.store.retrieve(MemoryQuery { text: masked, kinds: vec![MemoryKind::Procedural], limit: 5, session_id: None }).await
                && let Some(m) = found.into_iter().find(|m| &m.item.id == sid)
                && let Some(mut skill) = m.item.metadata.get("skill").and_then(|v| serde_json::from_value::<skills::Skill>(v.clone()).ok())
            {
                if success {
                    skill.successes += 1
                } else {
                    skill.failures += 1
                }
                if skill.failures >= 2 && skill.failures > skill.successes {
                    let _ = self.store.delete(sid).await;
                } else {
                    let _ = self.store.update_metadata(sid, json!({"skill": skill})).await;
                }
            }
        } else if verified
            && matches!(rec.mode, Some(ExecMode::Assisted) | Some(ExecMode::Escalated))
            && rec.usage.replans == 0
            && let Some(plan) = &rec.plan
            && let Some(skill) = skills::crystallize(&rec.goal, &rec.entities, plan)
        {
            let mut it = MemoryItem::new(MemoryKind::Procedural, skill.masked_request.clone(), 0.8);
            it.metadata = json!({"skill": skill});
            let _ = self.store.store(it).await;
        }
    }
}

#[allow(dead_code)]
fn _assert_send(_: &Decided<()>) {}
