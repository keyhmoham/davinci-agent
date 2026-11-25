//! The decision-first task pipeline.
//!
//! extract → retrieve → route → (direct binding | skill match) → select mode →
//! plan → execute → verify → answer → learn.
//!
//! Every stage prefers the cheapest component that can do the job: code, then
//! the decision model, then the SLM, then (only when needed) a stronger model.

use crate::engine::{ContextBase, Inner, TaskCtx};
use crate::{answer, planner, prompts, skills};
use chrono::Utc;
use dfae_core::*;
use dfae_decision::Decided;
use dfae_memory::{ContextInput, build_context, dedupe_memories, rank_memories};
use dfae_models::Tier;
use dfae_tools::{DirectCall, bind_direct};
use futures_util::FutureExt;
use serde_json::json;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

pub(crate) struct SkillMatch {
    pub memory_id: String,
    pub similarity: f32,
    pub plan: Plan,
}

impl Inner {
    pub(crate) async fn run_task(self: Arc<Self>, ctx: Arc<TaskCtx>) {
        let outcome = match AssertUnwindSafe(self.pipeline(&ctx)).catch_unwind().await {
            Ok(r) => r,
            Err(_) => Err("internal error: the task pipeline panicked".to_string()),
        };
        self.finalize(&ctx, outcome).await;
        self.live.lock().remove(&ctx.id);
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

        // ---- 1. extraction (GLiNER + rules) --------------------------------
        let entities = self.stage_extract(ctx, &goal).await;
        // ---- 2. retrieval ---------------------------------------------------
        self.stage_retrieve(ctx, &goal, &entities, req.profile.unwrap_or_default()).await;

        // ---- 3. route -------------------------------------------------------
        let state = self.decision_state(&goal, &entities);
        let routing = self.note(ctx, self.decider.route_task(state.clone()).await).await;
        ctx.rec.lock().await.route = Some(routing.clone());

        // ---- 4. deterministic candidates -----------------------------------
        let metas = self.tools.metas();
        let calls: Option<Vec<DirectCall>> =
            if req.mode == ModeRequest::Assisted || req.mode == ModeRequest::Escalated { None } else { bind_direct(&metas, &goal, &entities) };
        let skill = if matches!(req.mode, ModeRequest::Auto | ModeRequest::Direct) { self.find_skill(&goal, &entities).await } else { None };
        let skill_direct = skill.as_ref().map(|s| !s.plan.has_reasoning()).unwrap_or(false);

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
            if let Err(errs) = planner::validate_plan(&plan.steps, &self.tools, &allowed, &ctx.rec.lock().await.budget) {
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
        ctx.rec.lock().await.plan = Some(plan.clone());
        self.emit(ctx, EventKind::PlanCreated { plan }).await;
        self.persist(ctx).await;

        self.execute_and_conclude(ctx, &goal, mode).await
    }

    /// execute → verify (+ at most one continuation) → answer. Shared by fresh
    /// runs and by tasks resumed after a restart.
    async fn execute_and_conclude(self: &Arc<Self>, ctx: &Arc<TaskCtx>, goal: &str, mode: ExecMode) -> Result<(), String> {
        let mut exec_err = self.execute_plan(ctx).await.err();
        if self.is_cancelled(ctx).await {
            return Err("cancelled".into());
        }
        self.set_status(ctx, TaskStatus::Verifying).await;
        let mut verification = self.stage_verify(ctx).await;
        if exec_err.is_none()
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
        let (goal, mode) = {
            let mut r = ctx.rec.lock().await;
            let Some(plan) = r.plan.as_mut() else { return Err("interrupted by a restart before a plan existed".into()) };
            for s in &mut plan.steps {
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
                },
            )
            .await;
        }
        ctx.rec.lock().await.entities = entities.clone();
        self.emit(ctx, EventKind::Extraction { entities: entities.clone(), backend, latency_ms: latency }).await;
        entities
    }

    async fn stage_retrieve(&self, ctx: &Arc<TaskCtx>, goal: &str, entities: &[Entity], profile: ContextProfile) {
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
        if let Ok(tasks) = self.store.list_tasks(40).await {
            let mut same: Vec<&TaskRecord> = tasks.iter().filter(|t| t.session_id == session && t.task_id != ctx.id && t.status.is_terminal()).collect();
            same.sort_by_key(|t| t.created_at);
            for t in same.into_iter().rev().take(4).rev() {
                prior.push(format!("{} → {}", tokens::truncate_tokens(&t.goal, 24), tokens::truncate_tokens(t.final_answer.as_deref().unwrap_or(""), 30)));
            }
        }
        *ctx.base_ctx.lock() = ContextBase { memories: ranked, prior_turns: prior };
        if profile != ContextProfile::Minimal {
            ctx.rec.lock().await.retrieved = refs.clone();
            self.emit(ctx, EventKind::Retrieval { items: refs }).await;
        }
    }

    /// Context text for prompts (rebuilt from the current task state).
    pub(crate) async fn context_text(&self, ctx: &Arc<TaskCtx>, include_observations: bool) -> String {
        let base = ctx.base_ctx.lock().clone();
        let r = ctx.rec.lock().await;
        let obs: Vec<(String, String)> = if include_observations {
            r.plan
                .as_ref()
                .map(|p| {
                    p.steps
                        .iter()
                        .filter(|s| s.status == StepStatus::Succeeded)
                        .map(|s| (format!("{} {}", s.id, s.tool.as_deref().unwrap_or("reasoning")), s.summary.clone().unwrap_or_default()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        };
        let profile = r.request.profile.unwrap_or(match r.route.as_ref().map(|x| x.complexity).unwrap_or(0.3) {
            c if c >= 0.65 => ContextProfile::Reasoning,
            c if c < 0.25 => ContextProfile::Minimal,
            _ => ContextProfile::Standard,
        });
        let pkg = build_context(&ContextInput {
            goal: &r.goal,
            entities: &r.entities,
            memories: &base.memories,
            prior_turns: &base.prior_turns,
            observations: &obs,
            profile,
        });
        pkg.render()
    }

    async fn find_skill(&self, goal: &str, entities: &[Entity]) -> Option<SkillMatch> {
        let slots = skills::slot_values(goal, entities);
        let masked = skills::mask_request(goal, &slots);
        let cands = self.store.retrieve(MemoryQuery { text: masked, kinds: vec![MemoryKind::Procedural], limit: 3, session_id: None }).await.ok()?;
        for c in cands {
            if c.similarity < self.cfg.thresholds.skill_min_similarity {
                continue;
            }
            let Some(skill) = c.item.metadata.get("skill").and_then(|v| serde_json::from_value::<skills::Skill>(v.clone()).ok()) else { continue };
            if let Some(plan) = skills::instantiate(&skill, goal, entities) {
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
        tstate.tools = self.tools.briefs();
        let picked = self.note(ctx, self.decider.select_tools(tstate).await).await;
        let mut allowed: Vec<String> = picked.into_iter().map(|(n, _)| n).collect();
        for c in calls.unwrap_or(&[]) {
            if !allowed.contains(&c.tool) {
                allowed.push(c.tool.clone());
            }
        }
        *ctx.allowed.lock() = allowed.clone();

        let budget = ctx.rec.lock().await.budget.clone();
        let mut tier = self
            .pick_tier(mode)
            .ok_or("no language model is configured (set SLM_BASE_URL or STRONG_API_KEY) and this request cannot be handled deterministically")?;
        let mut used_mode = if tier == Tier::Strong { ExecMode::Escalated } else { mode };
        let mut last_err = String::new();
        for attempt in 0..2 {
            let ctx_text = self.context_text(ctx, false).await;
            let user = prompts::plan_user(&self.tools.signatures(&allowed), &ctx_text, budget.max_steps.min(12));
            let mut req = self.gen_request(tier, prompts::PLAN_SYSTEM, user, if attempt == 0 { "plan" } else { "plan_escalated" });
            req.json = true;
            let source = if tier == Tier::Strong { PlanSource::Strong } else { PlanSource::Slm };
            match self.try_plan(ctx, tier, req, goal, source, &allowed, &budget).await {
                Ok(steps) => {
                    let _ = entities;
                    return Ok((Plan { goal: goal.into(), steps, source, revision: 0 }, used_mode));
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
        budget: &Budget,
    ) -> Result<Vec<Step>, String> {
        let v = self.gen_json(ctx, tier, req.clone()).await?;
        let problems = match planner::parse_plan(&v, goal, source, "", &self.tools) {
            Ok(steps) => match planner::validate_plan(&steps, &self.tools, allowed, budget) {
                Ok(()) => return Ok(steps),
                Err(errs) => errs,
            },
            Err(e) => vec![e],
        };
        req.messages.push(Message::assistant(tokens::truncate_tokens(&v.to_string(), 500)));
        req.messages.push(Message::user(format!("Invalid plan:\n- {}\nReturn the corrected JSON only.", problems.join("\n- "))));
        req.purpose = "plan_repair".into();
        let v2 = self.gen_json(ctx, tier, req).await?;
        let steps = planner::parse_plan(&v2, goal, source, "", &self.tools)?;
        planner::validate_plan(&steps, &self.tools, allowed, budget).map_err(|e| e.join("; "))?;
        Ok(steps)
    }

    /// Evidence was judged insufficient: ask a model for the missing steps (once).
    async fn continue_plan(self: &Arc<Self>, ctx: &Arc<TaskCtx>, goal: &str) -> Result<bool, String> {
        let (replans, max) = {
            let r = ctx.rec.lock().await;
            (r.usage.replans, r.budget.max_replans)
        };
        if replans >= max {
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
        // token ledger: baseline for a naive tool-calling LLM agent
        {
            let mut r = ctx.rec.lock().await;
            let raw: Vec<u64> = r
                .plan
                .as_ref()
                .map(|p| {
                    p.steps.iter().filter(|s| s.status.is_done()).map(|s| tokens::estimate_tokens(s.output.as_deref().unwrap_or("")).min(4000) as u64).collect()
                })
                .unwrap_or_default();
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
        if let Some(m) = rec.mode {
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
