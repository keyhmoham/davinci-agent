//! End-to-end tests of automatic delegation: decomposition, roles, dispatch,
//! handoff, supervision, isolation, shared cache, approvals, cancellation,
//! admission control and resume. Language models are *content-routed* fakes,
//! because parallel agents make strictly ordered scripts non-deterministic.
#![allow(clippy::field_reassign_with_default)]

use async_trait::async_trait;
use dfae_core::*;
use dfae_decision::{Calibrator, Decider};
use dfae_extract::HybridExtractor;
use dfae_memory::InMemoryStore;
use dfae_models::ModelGateway;
use dfae_runtime::{Engine, EngineDeps};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------- fakes -----

type Rule = Box<dyn Fn(&GenerationRequest) -> bool + Send + Sync>;

/// Answers by *content* of the request, so it is safe under concurrency.
#[derive(Clone, Default)]
struct Routed {
    rules: Arc<Mutex<Vec<(Rule, String)>>>,
    seen: Arc<Mutex<Vec<GenerationRequest>>>,
}

impl Routed {
    fn when(&self, f: impl Fn(&GenerationRequest) -> bool + Send + Sync + 'static, reply: &str) -> &Self {
        self.rules.lock().push((Box::new(f), reply.to_string()));
        self
    }
    /// A planner call whose context contains `needle` gets a reasoning-only plan with step text `step`.
    fn plan(&self, needle: &'static str, step: &str) -> &Self {
        let plan = json!({"steps": [{"id": "s1", "tool": null, "do": step}]}).to_string();
        self.when(move |r| r.system.starts_with("You are the planner") && last(r).contains(needle), &plan)
    }
    /// A reasoning step whose description is `step` gets `reply`.
    fn step(&self, step: &'static str, reply: &str) -> &Self {
        self.when(move |r| r.system == dfae_runtime::prompts::STEP_SYSTEM && last(r).contains(&format!("STEP: {step}")), reply)
    }
    fn calls(&self) -> usize {
        self.seen.lock().len()
    }
    fn prompts(&self, system_prefix: &str) -> Vec<String> {
        self.seen.lock().iter().filter(|r| r.system.starts_with(system_prefix)).map(last).collect()
    }
}

fn last(r: &GenerationRequest) -> String {
    r.messages.last().map(|m| m.content.clone()).unwrap_or_default()
}

#[async_trait]
impl LanguageModel for Routed {
    fn name(&self) -> &str {
        "routed-fake"
    }
    async fn generate(&self, req: GenerationRequest) -> Result<GenerationResponse, ModelError> {
        self.seen.lock().push(req.clone());
        let reply = self.rules.lock().iter().find(|(f, _)| f(&req)).map(|(_, t)| t.clone());
        match reply {
            Some(text) => Ok(GenerationResponse { text, usage: Usage::default(), model: "routed-fake".into() }),
            None => Err(ModelError::Request(format!("no rule for {} / {}", req.purpose, last(&req).chars().take(80).collect::<String>()))),
        }
    }
}

/// Read-only or side-effecting tool that records how many ran at once.
struct Probe {
    name: &'static str,
    pattern: &'static str,
    side_effects: bool,
    delay_ms: u64,
    running: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    /// fail this many calls with a transient error first
    fails: Mutex<u32>,
    calls: Arc<AtomicU32>,
}

impl Probe {
    fn new(name: &'static str, pattern: &'static str, side_effects: bool, delay_ms: u64) -> Self {
        Self { name, pattern, side_effects, delay_ms, running: Arc::default(), peak: Arc::default(), fails: Mutex::new(0), calls: Arc::default() }
    }
}

#[async_trait]
impl Tool for Probe {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name.into(),
            description: format!("probe {}", self.name),
            category: "probe".into(),
            risk: Risk::Low,
            requires_approval: false,
            side_effects: self.side_effects,
            reversible: true,
            cost_class: "free".into(),
            params: vec![ParamSpec::new("name", ParamType::String, true, "what to probe")],
            keywords: vec![],
            direct_patterns: vec![self.pattern.into()],
            cacheable: false,
            contract: vec![],
        }
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        {
            let mut f = self.fails.lock();
            if *f > 0 {
                *f -= 1;
                return Err(ToolError::new(ErrorKind::Transient, "upstream hiccup"));
            }
        }
        Ok(ToolOutput::ok(format!("{} ok: {}", self.name, input["name"].as_str().unwrap_or("?"))))
    }
}

// ------------------------------------------------------------------ rig -----

struct Rig {
    engine: Engine,
    worker: Routed,
    ws: tempfile::TempDir,
    store: Arc<InMemoryStore>,
}

fn rig(with_slm: bool, extra: Vec<Arc<dyn Tool>>, tweak: impl FnOnce(&mut Config)) -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.allow_auto_approve = true;
    cfg.approval_ttl_secs = 30;
    tweak(&mut cfg);
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    for t in extra {
        reg.register(t);
    }
    let worker = Routed::default();
    let models = ModelGateway::new(with_slm.then(|| Arc::new(worker.clone()) as Arc<dyn LanguageModel>), None);
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store: store.clone(),
        models,
        tools: Arc::new(reg),
        policy,
    });
    Rig { engine, worker, ws, store }
}

async fn run(r: &Rig, goal: &str) -> TaskRecord {
    run_req(r, TaskRequest { goal: goal.into(), ..Default::default() }).await
}

async fn run_req(r: &Rig, req: TaskRequest) -> TaskRecord {
    let id = r.engine.submit(req).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(30)).await.unwrap()
}

async fn children(r: &Rig, parent: &TaskRecord) -> Vec<TaskRecord> {
    let mut out = Vec::new();
    for c in &parent.team.as_ref().expect("a coordinator").children {
        out.push(r.store.get_task(&c.task_id).await.unwrap().expect("child persisted"));
    }
    out
}

fn kinds(r: &Rig, id: &str) -> Vec<String> {
    r.engine.backlog(id).iter().map(|e| serde_json::to_value(&e.kind).unwrap()["type"].as_str().unwrap().to_string()).collect()
}

fn step<'a>(t: &'a TaskRecord, id: &str) -> &'a Step {
    t.plan.as_ref().unwrap().step(id).unwrap()
}

fn write_files(r: &Rig) {
    for (n, c) in [("a.txt", "alpha"), ("b.txt", "beta"), ("c.txt", "gamma")] {
        std::fs::write(r.ws.path().join(n), c).unwrap();
    }
}

// ---------------------------------------------------------------- tests -----

#[tokio::test]
async fn fan_out_runs_free_agents_and_pays_for_one_join() {
    let r = rig(true, vec![], |_| {});
    write_files(&r);
    r.worker.step("summarize them", "The files hold alpha, beta and gamma.");
    let t = run(&r, "read a.txt, b.txt and c.txt and summarize them").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);

    // the coordinator split the request without any command from the user
    let plan = t.plan.as_ref().unwrap();
    assert_eq!(plan.source, PlanSource::Team);
    assert_eq!(plan.steps.iter().filter(|s| s.kind == StepKind::Agent).count(), 3);
    assert!(plan.steps.iter().all(|s| s.status == StepStatus::Succeeded));
    assert_eq!(t.team.as_ref().unwrap().role, AgentRole::Coordinator);
    assert_eq!(t.final_answer.as_deref(), Some("The files hold alpha, beta and gamma."));
    assert_eq!(t.verification.as_ref().unwrap().status, VerificationStatus::Verified);

    // every agent is an ordinary child task: deterministic, read-only, depth 1, no model
    let kids = children(&r, &t).await;
    assert_eq!(kids.len(), 3);
    for (k, expect) in kids.iter().zip(["alpha", "beta", "gamma"]) {
        let team = k.team.as_ref().unwrap();
        assert_eq!(team.role, AgentRole::Researcher);
        assert_eq!(team.link.as_ref().unwrap().depth, 1);
        assert_eq!(team.link.as_ref().unwrap().parent_id, t.task_id);
        assert!(team.children.is_empty(), "agents do not spawn agents");
        assert_eq!(k.mode, Some(ExecMode::Direct));
        assert_eq!(k.ledger.summary_own().generative_tokens, 0);
        assert_eq!(k.status, TaskStatus::Completed);
        assert!(k.final_answer.as_deref().unwrap().contains(expect));
        assert_ne!(k.plan.as_ref().unwrap().source, PlanSource::Team);
        assert_eq!(k.entities.iter().filter(|e| e.label == "file path").count(), 1, "entities are inherited, not re-extracted");
    }
    // the only model call in the whole request is the join
    assert_eq!(r.worker.calls(), 1);
    let join = &r.worker.prompts("You do one step of the user")[0];
    for expect in ["<data src=\"c1 agent\">", "alpha", "beta", "gamma"] {
        assert!(join.contains(expect), "{join}");
    }
    assert_eq!(t.mode, Some(ExecMode::Assisted));

    // events: team formed, agents dispatched and finished; child announces its role
    let k = kinds(&r, &t.task_id);
    assert_eq!(k.iter().filter(|x| *x == "team_formed").count(), 1);
    assert_eq!(k.iter().filter(|x| *x == "agent_spawned").count(), 3);
    assert_eq!(k.iter().filter(|x| *x == "agent_done").count(), 3);
    assert!(kinds(&r, &kids[0].task_id).contains(&"role_assigned".to_string()));

    // cost accounting: rolled up for the team, but never double counted globally
    let team_total = t.ledger.summary();
    assert_eq!(team_total.worker_calls, 1);
    let stats = r.engine.stats().await;
    assert_eq!((stats.teams, stats.agents), (1, 3));
    assert_eq!(stats.totals.generative_tokens, t.ledger.summary_own().generative_tokens);
    assert_eq!(stats.totals.baseline_tokens, t.ledger.baseline_tokens, "one baseline: a single naive agent doing all of the work");
    assert!(t.ledger.baseline_tokens > 0 && stats.totals.saved_pct > 50.0, "{:?}", stats.totals);
}

#[tokio::test]
async fn a_pipeline_hands_results_down_and_separates_read_only_from_state_changing_work() {
    let r = rig(true, vec![], |_| {});
    r.worker
        .plan("GOAL: Find the latest", "list-vulns")
        .plan("GOAL: check whether my server", "check-server")
        .plan("GOAL: patch it if necessary", "apply-patch");
    r.worker
        .step("list-vulns", "A critical flaw affects the older release")
        .step("check-server", "The server runs the older release and is affected")
        .step("apply-patch", "Patch plan: upgrade to the fixed release");
    let t = run(&r, "Find the latest PostgreSQL vulnerabilities, check whether my server is affected, and patch it if necessary").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}\n{:?}", t.error, t.final_answer);
    let kids = children(&r, &t).await;
    assert_eq!(kids.len(), 3);

    // roles follow what each sub-task does (least privilege)
    let roles: Vec<_> = kids.iter().map(|k| k.team.as_ref().unwrap().role).collect();
    assert_eq!(roles, vec![AgentRole::Researcher, AgentRole::Verifier, AgentRole::Operator], "{roles:?}");

    // strictly sequential: each agent finishes before the next one is dispatched
    let evs = r.engine.backlog(&t.task_id);
    let pos = |pred: &dyn Fn(&EventKind) -> bool| evs.iter().position(|e| pred(&e.kind)).unwrap();
    for i in 0..2 {
        let done = pos(&|k| matches!(k, EventKind::AgentDone { child_id, .. } if *child_id == kids[i].task_id));
        let next = pos(&|k| matches!(k, EventKind::AgentSpawned { child_id, .. } if *child_id == kids[i + 1].task_id));
        assert!(done < next, "agent {i} must finish before agent {} starts", i + 1);
    }

    // handoff: the second agent sees the first one's result (compactly), the first sees none
    let planner_prompts = r.worker.prompts("You are the planner");
    let p2 = planner_prompts.iter().find(|p| p.contains("GOAL: check whether")).unwrap();
    assert!(p2.contains("UPSTREAM:") && p2.contains("critical flaw"), "{p2}");
    assert!(!planner_prompts.iter().find(|p| p.contains("GOAL: Find the latest")).unwrap().contains("UPSTREAM"));

    // least privilege: the researcher's planner was never shown a tool that changes state
    let p1 = planner_prompts.iter().find(|p| p.contains("GOAL: Find the latest")).unwrap();
    assert!(!p1.contains("fs_write(") && !p1.contains("memory_store("), "{p1}");
    assert_eq!(kids[0].team.as_ref().unwrap().link.as_ref().unwrap().upstream.len(), 0);
    assert!(kids[2].team.as_ref().unwrap().link.as_ref().unwrap().upstream[0].summary.contains("affected"));

    let a = t.final_answer.unwrap();
    for part in ["critical flaw", "is affected", "upgrade to the fixed release"] {
        assert!(a.contains(part), "{a}");
    }
    assert_eq!(t.mode, Some(ExecMode::Assisted), "the coordinator's mode is the highest of its agents'");
}

#[tokio::test]
async fn one_failed_agent_does_not_discard_its_siblings_and_policy_is_never_retried() {
    let r = rig(true, vec![], |_| {});
    write_files(&r);
    r.worker.step("summarize them", "never reached");
    let t = run(&r, "read a.txt, ../../etc/passwd.txt and summarize them").await;
    assert_eq!(t.status, TaskStatus::Failed);
    assert_eq!(step(&t, "c1").status, StepStatus::Succeeded, "the independent sibling ran to completion");
    assert_eq!(step(&t, "c2").status, StepStatus::Failed);
    assert_eq!(step(&t, "join").status, StepStatus::Cancelled, "the join is blocked by the failure");
    assert_eq!(t.usage.children, 2, "a policy denial is not retried by the supervisor");
    let a = t.final_answer.unwrap();
    assert!(a.contains("alpha"), "partial results are preserved: {a}");
    assert!(a.contains("outside the allowed roots"), "{a}");
    assert_eq!(r.worker.calls(), 0, "no model is paid for a join that cannot happen");
    assert_eq!(t.verification.unwrap().status, VerificationStatus::Failed);
    // the split itself is judged by its outcome (calibration)
    assert_eq!(r.engine.calibration()["stats"]["assess_decomposition:split"], json!([0.0, 1.0]));
}

#[tokio::test]
async fn supervision_retries_a_transient_agent_failure_with_a_fresh_agent() {
    let probe = Arc::new(Probe::new("widget_probe", r"(?i)\bfetch widget (?P<name>\w+)", false, 1));
    *probe.fails.lock() = 2; // exhausts the first agent's own retries
    let calls = probe.calls.clone();
    let r = rig(true, vec![probe], |_| {});
    r.worker.step("summarize", "both widgets are fine");
    let t = run(&r, "fetch widget alpha, then fetch widget beta, then summarize").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}\n{:?}", t.error, t.final_answer);
    assert_eq!(step(&t, "c1").attempts, 2, "the supervisor re-dispatched the failed sub-task");
    assert_eq!(t.usage.children, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 4, "2 failures + 1 success (alpha) + 1 (beta)");
    let kids = children(&r, &t).await;
    assert_eq!(kids.iter().filter(|k| k.status == TaskStatus::Failed).count(), 1);
    assert!(t.decisions.iter().any(|d| d.kind == "assess_recovery"));
    assert_eq!(r.engine.calibration()["stats"]["assess_decomposition:split"], json!([1.0, 0.0]));
}

#[tokio::test]
async fn operators_are_serialised_while_researchers_run_in_parallel() {
    let scan = Arc::new(Probe::new("zone_scan", r"(?i)\bscan zone (?P<name>\w+)", false, 120));
    let reset = Arc::new(Probe::new("zone_reset", r"(?i)\breset zone (?P<name>\w+)", true, 80));
    let (scan_peak, reset_peak) = (scan.peak.clone(), reset.peak.clone());
    let r = rig(true, vec![scan, reset], |_| {});
    r.worker.step("summarize them", "done");
    let t = run(&r, "scan zone a, scan zone b, scan zone c and summarize them").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert!(scan_peak.load(Ordering::SeqCst) >= 2, "read-only agents fan out in parallel");
    let t = run(&r, "reset zone a, reset zone b, reset zone c and summarize them").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert!(children(&r, &t).await.iter().all(|k| k.team.as_ref().unwrap().role == AgentRole::Operator));
    assert_eq!(reset_peak.load(Ordering::SeqCst), 1, "agents that may change state never run concurrently");
    let d = r.engine.dispatch();
    assert_eq!(d.roles["operator"].peak, 1);
    assert!(d.roles["researcher"].peak >= 2);
    assert_eq!(d.agents_spawned, 6);
    assert_eq!(d.agents.running, 0, "every worker slot was released");
}

#[tokio::test]
async fn team_members_share_reads_until_somebody_writes() {
    let r = rig(true, vec![], |_| {});
    std::fs::write(r.ws.path().join("a.txt"), "alpha").unwrap();
    r.worker.step("summarize", "ok");
    // two reads of the same file in sequence: the second is served from the team cache
    let t = run(&r, "read a.txt, then read a.txt, then summarize").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(t.team.as_ref().unwrap().cache_hits, 1);
    assert_eq!(t.usage.tool_calls, 1, "the cached read does not spend tool budget");
    assert_eq!(r.engine.stats().await.team_cache_hits, 1);
    let kids = children(&r, &t).await;
    assert!(step(&kids[1], "s1").evidence.iter().any(|e| e.contains("shared cache")));

    // a write in between invalidates it: the later read must see the new content
    let id = r
        .engine
        .submit(TaskRequest {
            goal: "read a.txt, then write 'changed' to a.txt, then read a.txt, then summarize".into(),
            auto_approve: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let t = r.engine.wait(&id, Duration::from_secs(30)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(t.team.as_ref().unwrap().cache_hits, 0);
    let kids = children(&r, &t).await;
    assert!(kids[2].final_answer.as_deref().unwrap().contains("changed"), "{:?}", kids[2].final_answer);
    assert_eq!(kids[1].team.as_ref().unwrap().role, AgentRole::Operator);
    assert!(kids[1].approvals.iter().all(|a| a.status == ApprovalStatus::Approved), "auto-approve is inherited by agents");
}

#[tokio::test]
async fn approvals_raised_by_an_agent_are_answered_on_that_agent() {
    let r = rig(true, vec![], |_| {});
    r.worker.step("summarize", "wrote it");
    let id = r.engine.submit(TaskRequest { goal: "write 'x' to d.txt, then read d.txt, then summarize".into(), ..Default::default() }).await.unwrap();
    // find the agent that is waiting for approval
    let mut found = None;
    for _ in 0..300 {
        let parent = r.engine.get_task(&id).await.unwrap();
        if let Some(c) = parent.team.as_ref().and_then(|t| t.children.first())
            && let Some(child) = r.engine.get_task(&c.task_id).await
            && let Some(a) = child.approvals.iter().find(|a| a.status == ApprovalStatus::Pending)
        {
            found = Some((child.task_id.clone(), a.id.clone(), child.team.as_ref().unwrap().role));
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (child, approval, role) = found.expect("the agent asked for approval");
    assert_eq!(role, AgentRole::Operator);
    assert!(!r.ws.path().join("d.txt").exists());
    r.engine.decide_approval(&child, &approval, true, "tester").await.unwrap();
    let t = r.engine.wait(&id, Duration::from_secs(30)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("d.txt")).unwrap(), "x");
}

#[tokio::test]
async fn cancelling_the_coordinator_cancels_its_agents_and_frees_their_slots() {
    let r = rig(true, vec![], |_| {});
    let id = r.engine.submit(TaskRequest { goal: "write 'x' to e.txt, then read e.txt, then summarize".into(), ..Default::default() }).await.unwrap();
    let mut child_id = None;
    for _ in 0..300 {
        let parent = r.engine.get_task(&id).await.unwrap();
        if let Some(c) = parent.team.as_ref().and_then(|t| t.children.first())
            && r.engine.get_task(&c.task_id).await.map(|t| t.status == TaskStatus::WaitingForApproval).unwrap_or(false)
        {
            child_id = Some(c.task_id.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let child_id = child_id.expect("the agent is blocked on approval");
    assert!(r.engine.cancel(&id).await);
    let t = r.engine.wait(&id, Duration::from_secs(10)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Cancelled);
    let child = r.engine.wait(&child_id, Duration::from_secs(10)).await.unwrap();
    assert_eq!(child.status, TaskStatus::Cancelled);
    assert!(!r.ws.path().join("e.txt").exists());
    assert_eq!(r.engine.dispatch().agents.running, 0);
    assert_eq!(r.engine.dispatch().tasks.running, 0);
}

#[tokio::test]
async fn independent_model_agents_get_disjoint_slices_of_the_budget() {
    let r = rig(true, vec![], |c| c.budget.max_worker_calls = 8);
    r.worker.plan("GOAL: explain what a mutex is", "mutex-text").plan("GOAL: explain what a semaphore is", "sem-text");
    r.worker.step("mutex-text", "A mutex is a lock.").step("sem-text", "A semaphore is a counter.");
    let t = run(&r, "explain what a mutex is and explain what a semaphore is").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}\n{:?}", t.error, t.final_answer);
    let kids = children(&r, &t).await;
    assert_eq!(kids.len(), 2);
    assert!(kids.iter().all(|k| k.team.as_ref().unwrap().role == AgentRole::Analyst));
    let sum: u32 = kids.iter().map(|k| k.budget.max_worker_calls).sum();
    assert!(sum <= 8, "children cannot overspend the coordinator's budget: {sum}");
    assert!(kids.iter().all(|k| k.budget.max_children == 0));
    assert_eq!(t.usage.worker_calls, kids.iter().map(|k| k.usage.worker_calls).sum::<u32>(), "usage rolls up");
    assert_eq!(t.usage.worker_calls, 4);
    assert_eq!(t.usage.model_calls, 0, "no external call was made: the self-hosted model is not metered against the paid budget");
    let a = t.final_answer.unwrap();
    assert!(a.contains("A mutex is a lock.") && a.contains("A semaphore is a counter."), "{a}");
    // agents ran concurrently under the analyst pool
    assert!(r.engine.dispatch().roles["analyst"].peak >= 1);
}

#[tokio::test]
async fn delegation_can_be_switched_off_and_needs_a_model_where_one_is_needed() {
    // the user opts out
    let r = rig(true, vec![], |_| {});
    write_files(&r);
    r.worker.when(
        |q| q.system.starts_with("You are the planner"),
        &json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "a.txt"}}, {"id": "s2", "tool": null, "do": "summarise"}]}).to_string(),
    );
    r.worker.step("summarise", "flat");
    let t = run_req(&r, TaskRequest { goal: "read a.txt, b.txt and c.txt and summarize them".into(), delegate: Some(false), ..Default::default() }).await;
    assert!(t.team.is_none(), "delegate=false keeps it one task");
    assert_ne!(t.plan.as_ref().unwrap().source, PlanSource::Team);

    // no model: a join cannot be written, so no team is formed
    let r2 = rig(false, vec![], |_| {});
    write_files(&r2);
    let t = run(&r2, "read a.txt, b.txt and c.txt and summarize them").await;
    assert!(t.team.is_none());
    assert!(t.error.unwrap().contains("no language model"));

    // server-wide switch
    let r3 = rig(true, vec![], |c| c.team.enabled = false);
    write_files(&r3);
    r3.worker.when(|q| q.system.starts_with("You are the planner"), &json!({"steps": [{"id": "s1", "tool": null, "do": "summarise"}]}).to_string());
    r3.worker.step("summarise", "flat");
    assert!(run(&r3, "read a.txt, b.txt and c.txt and summarize them").await.team.is_none());
}

#[tokio::test]
async fn single_tasks_stay_single() {
    let r = rig(true, vec![], |_| {});
    std::fs::write(r.ws.path().join("todo.md"), "- [ ] docs").unwrap();
    r.worker.plan("GOAL: Read todo.md", "count-open").step("count-open", "1 open task");
    let t = run(&r, "Read todo.md and tell me how many tasks are open").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert!(t.team.is_none(), "an analysis clause that refers to what was just read is not a separate task");
}

#[tokio::test]
async fn admission_control_queues_then_sheds_load() {
    let r = rig(true, vec![], |c| {
        c.team.max_concurrent_tasks = 1;
        c.team.max_queued_tasks = 1;
    });
    // the first task occupies the only slot: it waits for an approval
    let first = r.engine.submit(TaskRequest { goal: "write 'x' to q.txt".into(), ..Default::default() }).await.unwrap();
    for _ in 0..300 {
        if r.engine.get_task(&first).await.map(|t| t.status == TaskStatus::WaitingForApproval).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let second = r.engine.submit(TaskRequest { goal: "what is 2 + 2".into(), ..Default::default() }).await.unwrap();
    for _ in 0..300 {
        if r.engine.dispatch().queued == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(r.engine.dispatch().queued, 1);
    assert_eq!(r.engine.get_task(&second).await.unwrap().status, TaskStatus::Created, "queued tasks have not started");
    assert!(kinds(&r, &second).contains(&"queued".to_string()));
    let err = r.engine.submit(TaskRequest { goal: "what is 3 + 3".into(), ..Default::default() }).await.unwrap_err();
    assert!(err.contains("dispatch queue is full"), "{err}");

    // freeing the slot lets the queued task run
    assert!(r.engine.cancel(&first).await);
    let t = r.engine.wait(&second, Duration::from_secs(10)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Completed);
    assert!(t.final_answer.unwrap().contains("4"));
    assert_eq!(r.engine.dispatch().queued, 0);
}

#[tokio::test]
async fn a_task_cancelled_while_queued_never_runs() {
    let r = rig(true, vec![], |c| c.team.max_concurrent_tasks = 1);
    let first = r.engine.submit(TaskRequest { goal: "write 'x' to z.txt".into(), ..Default::default() }).await.unwrap();
    for _ in 0..300 {
        if r.engine.get_task(&first).await.map(|t| t.status == TaskStatus::WaitingForApproval).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let second = r.engine.submit(TaskRequest { goal: "what is 2 + 2".into(), ..Default::default() }).await.unwrap();
    for _ in 0..300 {
        if r.engine.dispatch().queued == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(r.engine.cancel(&second).await);
    let t = r.engine.wait(&second, Duration::from_secs(10)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Cancelled);
    assert!(t.plan.is_none(), "it never started");
    r.engine.cancel(&first).await;
}

#[tokio::test]
async fn an_interrupted_team_adopts_finished_agents_and_never_reruns_unknown_ones() {
    let r = rig(true, vec![], |_| {});
    let mk_child = |goal: &str, status: TaskStatus, parent: &str, step: &str, answer: Option<&str>| {
        let mut c = TaskRecord::new(TaskRequest { goal: goal.into(), ..Default::default() }, Budget::default());
        c.status = status;
        c.final_answer = answer.map(String::from);
        c.team = Some(TeamInfo {
            role: AgentRole::Researcher,
            link: Some(TeamLink {
                parent_id: parent.into(),
                parent_step: step.into(),
                depth: 1,
                upstream: vec![],
                entities: vec![],
                inherited_taint: false,
                profile: None,
            }),
            children: vec![],
            reason: None,
            cache_hits: 0,
        });
        c
    };
    let mut parent = TaskRecord::new(TaskRequest { goal: "read a.txt, read b.txt and then".into(), ..Default::default() }, Budget::default());
    parent.status = TaskStatus::Running;
    parent.mode = Some(ExecMode::Direct);
    let done = mk_child("read a.txt", TaskStatus::Completed, &parent.task_id, "c1", Some("alpha"));
    let orphan = mk_child("read b.txt", TaskStatus::Running, &parent.task_id, "c2", None);
    let mut s1 = Step::reasoning("c1", "read a.txt");
    s1.kind = StepKind::Agent;
    s1.status = StepStatus::Running;
    s1.args = json!({"child": done.task_id});
    let mut s2 = Step::reasoning("c2", "read b.txt");
    s2.kind = StepKind::Agent;
    s2.status = StepStatus::Running;
    s2.args = json!({"child": orphan.task_id});
    parent.plan = Some(Plan { goal: parent.goal.clone(), steps: vec![s1, s2], source: PlanSource::Team, revision: 0, estimate: None, report: None });
    parent.team = Some(TeamInfo::coordinator("test"));
    for rec in [&parent, &done, &orphan] {
        r.store.put_task(rec).await.unwrap();
    }
    let resumed = r.engine.resume_unfinished().await;
    assert_eq!(resumed, vec![parent.task_id.clone()], "orphaned agents are not resumed on their own");
    let t = r.engine.wait(&parent.task_id, Duration::from_secs(10)).await.unwrap();
    assert_eq!(step(&t, "c1").status, StepStatus::Succeeded);
    assert_eq!(step(&t, "c1").output.as_deref(), Some("alpha"), "the finished agent's result was adopted");
    assert_eq!(step(&t, "c2").status, StepStatus::Failed);
    assert!(step(&t, "c2").error.as_ref().unwrap().contains("outcome is unknown"));
    assert_eq!(r.store.get_task(&orphan.task_id).await.unwrap().unwrap().status, TaskStatus::Cancelled);
    assert_eq!(t.status, TaskStatus::Failed);
    assert!(t.final_answer.unwrap().contains("alpha"));
}
