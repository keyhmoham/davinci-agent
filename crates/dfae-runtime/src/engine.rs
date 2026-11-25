//! Engine: shared state, helpers (events, ledger, budgets, model calls) and
//! the public API. The task pipeline lives in `pipeline.rs`, step execution in
//! `executor.rs`, verification in `verify.rs`.

use crate::bus::EventBus;
use crate::dispatch::{AgentPermit, DispatchSnapshot, Dispatcher};
use crate::team::TeamShared;
use chrono::Utc;
use dfae_core::*;
use dfae_decision::{Decided, Decider};
use dfae_models::{Generated, ModelGateway, Tier};
use dfae_tools::{PolicyEngine, ToolRegistry};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub struct EngineDeps {
    pub config: Config,
    pub store: Arc<dyn MemoryStore>,
    pub extractor: Arc<dyn EntityExtractor>,
    pub decider: Arc<Decider>,
    pub models: ModelGateway,
    pub tools: Arc<ToolRegistry>,
    pub policy: Arc<PolicyEngine>,
}

pub(crate) struct TaskCtx {
    pub id: String,
    pub rec: tokio::sync::Mutex<TaskRecord>,
    pub cancel: CancellationToken,
    pub seq: AtomicU64,
    pub waiters: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    /// Tools this task may use (selected by the decision layer).
    pub allowed: Mutex<Vec<String>>,
    /// Compact context text for prompts (rebuilt as observations accumulate).
    pub base_ctx: Mutex<ContextBase>,
    /// 0 for top-level tasks, 1 for subagents (agents cannot spawn agents).
    pub depth: u8,
    /// Tools this agent's role may use (`None` = unrestricted). Policy still applies.
    pub scope: Mutex<Option<HashSet<String>>>,
    /// State shared by a team (read cache); set once a team is formed.
    pub shared: OnceLock<Arc<TeamShared>>,
    /// Worker-pool slot held while an agent runs.
    pub permit: Mutex<Option<AgentPermit>>,
    /// Receives the final record when this task is a subagent.
    pub done: Mutex<Option<oneshot::Sender<TaskRecord>>>,
}

impl TaskCtx {
    pub fn new(rec: TaskRecord, cancel: CancellationToken, depth: u8) -> Arc<Self> {
        Arc::new(Self {
            id: rec.task_id.clone(),
            rec: tokio::sync::Mutex::new(rec),
            cancel,
            seq: AtomicU64::new(0),
            waiters: Mutex::new(HashMap::new()),
            allowed: Mutex::new(vec![]),
            base_ctx: Mutex::new(ContextBase::default()),
            depth,
            scope: Mutex::new(None),
            shared: OnceLock::new(),
            permit: Mutex::new(None),
            done: Mutex::new(None),
        })
    }

    pub fn in_scope(&self, tool: &str) -> bool {
        self.scope.lock().as_ref().map(|s| s.contains(tool)).unwrap_or(true)
    }
}

#[derive(Default, Clone)]
pub(crate) struct ContextBase {
    pub memories: Vec<dfae_memory::RankedMemory>,
    pub prior_turns: Vec<String>,
    /// Results of the agents this one depends on `(label, summary)`.
    pub upstream: Vec<(String, String)>,
}

pub(crate) struct Inner {
    pub cfg: Config,
    pub store: Arc<dyn MemoryStore>,
    pub extractor: Arc<dyn EntityExtractor>,
    pub decider: Arc<Decider>,
    pub models: ModelGateway,
    pub tools: Arc<ToolRegistry>,
    pub policy: Arc<PolicyEngine>,
    pub bus: EventBus,
    pub dispatcher: Dispatcher,
    pub live: Mutex<HashMap<String, Arc<TaskCtx>>>,
}

/// The agent engine. Cheap to clone.
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ComponentHealth {
    pub name: String,
    pub status: String, // ok | down | off
    pub detail: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthReport {
    pub ok: bool,
    pub components: Vec<ComponentHealth>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StatsReport {
    pub tasks: usize,
    pub completed: usize,
    pub failed: usize,
    pub verified: usize,
    /// Teams formed / agents spawned / reads served from a team's shared cache.
    pub teams: usize,
    pub agents: usize,
    pub team_cache_hits: u32,
    pub direct: usize,
    pub assisted: usize,
    pub escalated: usize,
    pub totals: LedgerSummary,
    pub memory: MemoryStats,
    pub decision_provider: String,
}

impl Engine {
    pub fn new(deps: EngineDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                dispatcher: Dispatcher::new(&deps.config.team),
                cfg: deps.config,
                store: deps.store,
                extractor: deps.extractor,
                decider: deps.decider,
                models: deps.models,
                tools: deps.tools,
                policy: deps.policy,
                bus: EventBus::new(),
                live: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.cfg
    }

    pub fn tools(&self) -> Vec<ToolMetadata> {
        self.inner.tools.metas()
    }

    pub fn store(&self) -> Arc<dyn MemoryStore> {
        self.inner.store.clone()
    }

    /// Submit a task; returns its id immediately. The task runs in the background.
    pub async fn submit(&self, mut request: TaskRequest) -> Result<String, String> {
        request.goal = request.goal.trim().to_string();
        if request.goal.is_empty() {
            return Err("goal must not be empty".into());
        }
        if request.goal.chars().count() > 8000 {
            return Err("goal is too long (max 8000 characters)".into());
        }
        if request.auto_approve && !self.inner.cfg.allow_auto_approve {
            return Err("auto_approve is disabled on this server (set DFAE_ALLOW_AUTO_APPROVE=true)".into());
        }
        if self.inner.dispatcher.saturated() {
            return Err(format!("the dispatch queue is full ({} tasks waiting); try again shortly", self.inner.cfg.team.max_queued_tasks));
        }
        let rec = TaskRecord::new(request, self.inner.cfg.budget.clone());
        let id = rec.task_id.clone();
        let goal = rec.goal.clone();
        let ctx = TaskCtx::new(rec, CancellationToken::new(), 0);
        self.inner.live.lock().insert(id.clone(), ctx.clone());
        self.inner.persist(&ctx).await;
        self.inner.emit(&ctx, EventKind::TaskCreated { goal }).await;
        let inner = self.inner.clone();
        tokio::spawn(async move { inner.run_task(ctx).await });
        Ok(id)
    }

    /// Resume tasks left unfinished by a previous process. Returns their ids.
    pub async fn resume_unfinished(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for mut rec in self.inner.store.unfinished_tasks().await.unwrap_or_default() {
            let id = rec.task_id.clone();
            if self.inner.live.lock().contains_key(&id) {
                continue;
            }
            // A subagent cannot be re-attached to the coordinator that was awaiting it.
            // Its parent adopts the result if it had finished, or marks the step failed.
            if rec.team.as_ref().and_then(|t| t.link.as_ref()).is_some() {
                rec.status = TaskStatus::Cancelled;
                rec.error = Some("orphaned by a restart".into());
                let _ = self.inner.store.put_task(&rec).await;
                continue;
            }
            let ctx = TaskCtx::new(rec, CancellationToken::new(), 0);
            self.inner.live.lock().insert(id.clone(), ctx.clone());
            let inner = self.inner.clone();
            tokio::spawn(async move { inner.resume_task(ctx).await });
            ids.push(id);
        }
        ids
    }

    pub async fn get_task(&self, id: &str) -> Option<TaskRecord> {
        let live = self.inner.live.lock().get(id).cloned();
        if let Some(ctx) = live {
            return Some(ctx.rec.lock().await.clone());
        }
        self.inner.store.get_task(id).await.ok().flatten()
    }

    pub async fn list_tasks(&self, limit: usize) -> Vec<TaskRecord> {
        let mut out = self.inner.store.list_tasks(limit).await.unwrap_or_default();
        // overlay live state (more current than the last persisted copy)
        let live: Vec<Arc<TaskCtx>> = self.inner.live.lock().values().cloned().collect();
        for ctx in live {
            let r = ctx.rec.lock().await.clone();
            match out.iter_mut().find(|t| t.task_id == r.task_id) {
                Some(slot) => *slot = r,
                None => out.insert(0, r),
            }
        }
        out.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        out.truncate(limit);
        out
    }

    pub async fn cancel(&self, id: &str) -> bool {
        let ctx = self.inner.live.lock().get(id).cloned();
        match ctx {
            Some(c) => {
                c.cancel.cancel();
                // unblock any approval waits
                let waiters: Vec<_> = c.waiters.lock().drain().collect();
                for (_, tx) in waiters {
                    let _ = tx.send(false);
                }
                true
            }
            None => false,
        }
    }

    /// Resolve a pending approval. The decision is bound to the exact action
    /// recorded when the approval was requested.
    pub async fn decide_approval(&self, task_id: &str, approval_id: &str, approve: bool, by: &str) -> Result<(), String> {
        let ctx = self.inner.live.lock().get(task_id).cloned().ok_or("task is not running")?;
        {
            let mut rec = ctx.rec.lock().await;
            let a = rec.approvals.iter_mut().find(|a| a.id == approval_id).ok_or("unknown approval")?;
            if a.status != ApprovalStatus::Pending {
                return Err(format!("approval already {:?}", a.status).to_lowercase());
            }
            if Utc::now() > a.expires_at {
                a.status = ApprovalStatus::Expired;
                return Err("approval expired".into());
            }
            a.status = if approve { ApprovalStatus::Approved } else { ApprovalStatus::Denied };
            a.decided_by = Some(by.to_string());
        }
        let tx = ctx.waiters.lock().remove(approval_id);
        match tx {
            Some(tx) => {
                let _ = tx.send(approve);
                Ok(())
            }
            None => Err("no step is waiting on this approval".into()),
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.inner.bus.subscribe()
    }

    pub fn backlog(&self, task_id: &str) -> Vec<Event> {
        self.inner.bus.backlog(task_id)
    }

    /// Wait until a task reaches a terminal state (tests, CLI helpers).
    pub async fn wait(&self, id: &str, timeout: Duration) -> Option<TaskRecord> {
        let start = std::time::Instant::now();
        loop {
            if let Some(t) = self.get_task(id).await
                && t.status.is_terminal()
                && !self.inner.live.lock().contains_key(id)
            {
                return Some(t);
            }
            if start.elapsed() > timeout {
                return self.get_task(id).await;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    pub async fn health(&self) -> HealthReport {
        let i = &self.inner;
        let mut comps = Vec::new();
        let db = i.store.health().await;
        comps.push(ComponentHealth { name: "memory".into(), status: if db { "ok" } else { "down" }.into(), detail: i.store.backend().into() });
        let ex = i.extractor.health().await;
        comps.push(ComponentHealth { name: "extractor".into(), status: if ex { "ok" } else { "down" }.into(), detail: i.extractor.name().into() });
        comps.push(ComponentHealth {
            name: "decision".into(),
            status: "ok".into(),
            detail: if i.decider.provider_name() == "jev" { "jev (hosted) + local fallback".into() } else { "local rules".into() },
        });
        for (name, tier) in [("slm", Tier::Slm), ("strong", Tier::Strong)] {
            let (status, detail) = match i.models.health(tier).await {
                Some(true) => ("ok", i.models.model_name(tier).unwrap_or_default()),
                Some(false) => ("down", i.models.model_name(tier).unwrap_or_default()),
                None => ("off", "not configured".into()),
            };
            comps.push(ComponentHealth { name: name.into(), status: status.into(), detail });
        }
        let ok = db;
        HealthReport { ok, components: comps }
    }

    pub async fn stats(&self) -> StatsReport {
        let tasks = self.list_tasks(500).await;
        let mut s = StatsReport { tasks: tasks.len(), decision_provider: self.inner.decider.provider_name().into(), ..Default::default() };
        let mut totals = LedgerSummary::default();
        for t in &tasks {
            let (is_coordinator, is_agent) = match &t.team {
                Some(i) => (i.link.is_none(), i.link.is_some()),
                None => (false, false),
            };
            if is_coordinator {
                s.teams += 1;
                s.team_cache_hits += t.team.as_ref().map(|i| i.cache_hits).unwrap_or(0);
            }
            if is_agent {
                s.agents += 1;
            }
            match t.status {
                TaskStatus::Completed => s.completed += 1,
                TaskStatus::Failed => s.failed += 1,
                _ => {}
            }
            if t.verification.as_ref().map(|v| v.status == VerificationStatus::Verified).unwrap_or(false) {
                s.verified += 1;
            }
            // a coordinator's mode is the max of its agents' (already counted)
            match t.mode.filter(|_| !is_coordinator) {
                Some(ExecMode::Direct) => s.direct += 1,
                Some(ExecMode::Assisted) => s.assisted += 1,
                Some(ExecMode::Escalated) => s.escalated += 1,
                None => {}
            }
            // own calls only: rolled-up agent entries are counted on the agent's record;
            // an agent's baseline is excluded because the coordinator's models the whole request
            let mut l = t.ledger.summary_own();
            if is_agent {
                l.baseline_tokens = 0;
            }
            totals.slm_tokens += l.slm_tokens;
            totals.strong_tokens += l.strong_tokens;
            totals.decision_tokens += l.decision_tokens;
            totals.generative_tokens += l.generative_tokens;
            totals.baseline_tokens += l.baseline_tokens;
            totals.slm_calls += l.slm_calls;
            totals.strong_calls += l.strong_calls;
            totals.jev_calls += l.jev_calls;
            totals.gliner_calls += l.gliner_calls;
            totals.cache_hits += l.cache_hits;
        }
        totals.saved_tokens = totals.baseline_tokens.saturating_sub(totals.generative_tokens);
        totals.saved_pct = if totals.baseline_tokens > 0 { 100.0 * totals.saved_tokens as f32 / totals.baseline_tokens as f32 } else { 0.0 };
        s.totals = totals;
        s.memory = self.inner.store.stats().await.unwrap_or_default();
        s
    }

    /// Dispatcher state: admission queue and per-role worker pools.
    pub fn dispatch(&self) -> DispatchSnapshot {
        self.inner.dispatcher.snapshot()
    }

    pub fn calibration(&self) -> Value {
        self.inner.decider.calibration_snapshot()
    }
}

// ------------------------------------------------------------- helpers -----

impl Inner {
    pub(crate) async fn emit(&self, ctx: &Arc<TaskCtx>, kind: EventKind) {
        let seq = ctx.seq.fetch_add(1, Ordering::SeqCst);
        let audit = audit_worthy(&kind);
        let event = Event { task_id: ctx.id.clone(), seq, ts: Utc::now(), kind };
        if let Some((k, payload)) = audit
            && let Err(e) = self.store.audit_append(&ctx.id, k, payload).await
        {
            tracing::warn!(error = %e, "audit append failed");
        }
        self.bus.publish(event);
    }

    pub(crate) async fn persist(&self, ctx: &Arc<TaskCtx>) {
        let snapshot = {
            let mut r = ctx.rec.lock().await;
            r.updated_at = Utc::now();
            r.clone()
        };
        if let Err(e) = self.store.put_task(&snapshot).await {
            tracing::warn!(error = %e, task = %ctx.id, "persisting task failed");
        }
    }

    pub(crate) async fn set_status(&self, ctx: &Arc<TaskCtx>, next: TaskStatus) {
        let changed = {
            let mut r = ctx.rec.lock().await;
            if r.status == next {
                false
            } else if r.status.can_transition_to(next) {
                r.status = next;
                true
            } else {
                tracing::warn!(from = ?r.status, to = ?next, "invalid task status transition ignored");
                false
            }
        };
        if changed {
            self.emit(ctx, EventKind::Status { status: next }).await;
            self.persist(ctx).await;
        }
    }

    pub(crate) async fn add_ledger(&self, ctx: &Arc<TaskCtx>, entry: LedgerEntry) {
        if matches!(entry.component, Component::Slm | Component::Strong) {
            self.emit(
                ctx,
                EventKind::Model {
                    component: entry.component,
                    model: entry.model.clone(),
                    purpose: entry.purpose.clone(),
                    prompt_tokens: entry.prompt_tokens,
                    completion_tokens: entry.completion_tokens,
                    latency_ms: entry.latency_ms,
                },
            )
            .await;
        }
        ctx.rec.lock().await.ledger.record(entry);
    }

    /// Record a decision (and its ledger entry) and hand back the value.
    pub(crate) async fn note<T>(&self, ctx: &Arc<TaskCtx>, d: Decided<T>) -> T {
        if let Some(e) = d.entry {
            self.add_ledger(ctx, e).await;
        }
        {
            ctx.rec.lock().await.decisions.push(d.record.clone());
        }
        self.emit(ctx, EventKind::Decision { record: d.record }).await;
        d.value
    }

    pub(crate) async fn take_model_call(&self, ctx: &Arc<TaskCtx>, n: u32) -> Result<(), String> {
        let mut r = ctx.rec.lock().await;
        if r.usage.model_calls + n > r.budget.max_model_calls {
            return Err(format!("model-call budget exceeded ({} allowed)", r.budget.max_model_calls));
        }
        r.usage.model_calls += n;
        Ok(())
    }

    pub(crate) async fn take_tool_call(&self, ctx: &Arc<TaskCtx>) -> Result<(), String> {
        let mut r = ctx.rec.lock().await;
        if r.usage.tool_calls + 1 > r.budget.max_tool_calls {
            return Err(format!("tool-call budget exceeded ({} allowed)", r.budget.max_tool_calls));
        }
        r.usage.tool_calls += 1;
        Ok(())
    }

    /// Generate text on a tier with budget + ledger accounting.
    pub(crate) async fn generate_text(&self, ctx: &Arc<TaskCtx>, tier: Tier, req: GenerationRequest) -> Result<String, String> {
        self.take_model_call(ctx, 1).await?;
        let g: Generated = self.models.generate(tier, req).await.map_err(|e| e.to_string())?;
        self.add_ledger(ctx, g.entry.clone()).await;
        Ok(g.text)
    }

    pub(crate) async fn gen_json(&self, ctx: &Arc<TaskCtx>, tier: Tier, req: GenerationRequest) -> Result<Value, String> {
        self.take_model_call(ctx, 1).await?;
        match self.models.generate_json(tier, req).await {
            Ok((v, calls)) => {
                if calls.len() > 1 {
                    self.take_model_call(ctx, (calls.len() - 1) as u32).await.ok();
                }
                for c in calls {
                    self.add_ledger(ctx, c.entry).await;
                }
                Ok(v)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    pub(crate) fn pick_tier(&self, mode: ExecMode) -> Option<Tier> {
        let (first, second) = if mode == ExecMode::Escalated { (Tier::Strong, Tier::Slm) } else { (Tier::Slm, Tier::Strong) };
        if self.models.has(first) {
            Some(first)
        } else if self.models.has(second) {
            Some(second)
        } else {
            None
        }
    }

    pub(crate) fn gen_request(&self, tier: Tier, system: &str, user: String, purpose: &str) -> GenerationRequest {
        let max = if tier == Tier::Slm { self.cfg.slm.max_tokens } else { self.cfg.strong.max_tokens };
        GenerationRequest {
            system: system.into(),
            messages: vec![Message::user(user)],
            max_tokens: max,
            temperature: 0.1,
            json: false,
            purpose: purpose.into(),
        }
    }

    pub(crate) async fn is_cancelled(&self, ctx: &Arc<TaskCtx>) -> bool {
        ctx.cancel.is_cancelled()
    }

    pub(crate) async fn check_deadline(&self, ctx: &Arc<TaskCtx>) -> Result<(), String> {
        let r = ctx.rec.lock().await;
        let elapsed = (Utc::now() - r.created_at).num_seconds().max(0) as u64;
        if elapsed > r.budget.max_seconds {
            return Err(format!("time budget exceeded ({}s)", r.budget.max_seconds));
        }
        Ok(())
    }
}

/// Which events are written to the tamper-evident audit log.
fn audit_worthy(kind: &EventKind) -> Option<(&'static str, Value)> {
    match kind {
        EventKind::TaskCreated { goal } => Some(("task_created", json!({"goal": goal}))),
        EventKind::Mode { mode, reason, skill_id } => Some(("mode", json!({"mode": mode, "reason": reason, "skill_id": skill_id}))),
        EventKind::PlanCreated { plan } => Some((
            "plan_created",
            json!({"source": plan.source, "steps": plan.steps.iter().map(|s| json!({"id": s.id, "tool": s.tool, "args": s.args, "after": s.depends_on})).collect::<Vec<_>>()}),
        )),
        EventKind::PlanRevised { plan, reason } => Some(("plan_revised", json!({"reason": reason, "revision": plan.revision, "steps": plan.steps.len()}))),
        EventKind::StepUpdate { step_id, status, tool, error, attempts, .. } if status.is_done() => {
            Some(("step", json!({"step": step_id, "status": status, "tool": tool, "error": error, "attempts": attempts})))
        }
        EventKind::ApprovalRequested { approval } => {
            Some(("approval_requested", json!({"id": approval.id, "tool": approval.tool, "args_hash": approval.args_hash, "reason": approval.reason})))
        }
        EventKind::ApprovalResolved { approval_id, status } => Some(("approval_resolved", json!({"id": approval_id, "status": status}))),
        EventKind::Verification { result } => {
            Some(("verification", json!({"status": result.status, "failed": result.failed_checks, "unverified": result.unverified_claims})))
        }
        EventKind::Final { status, .. } => Some(("final", json!({"status": status}))),
        EventKind::Warning { message } => Some(("warning", json!({"message": message}))),
        _ => None,
    }
}
