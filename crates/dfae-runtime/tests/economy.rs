//! The token-economy layer end to end: deterministic repair, the pre-execution
//! budget gate, learned prices, output contracts and lazy context expansion.
#![allow(clippy::field_reassign_with_default)]

use async_trait::async_trait;
use dfae_core::*;
use dfae_decision::{Calibrator, Decider};
use dfae_extract::HybridExtractor;
use dfae_memory::InMemoryStore;
use dfae_models::{ModelGateway, ScriptedModel};
use dfae_runtime::{Engine, EngineDeps};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

struct Rig {
    engine: Engine,
    slm: ScriptedModel,
    ws: tempfile::TempDir,
    store: Arc<InMemoryStore>,
}

fn rig(with_slm: bool, extra: Vec<Arc<dyn Tool>>, tweak: impl FnOnce(&mut Config)) -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.allow_auto_approve = true;
    tweak(&mut cfg);
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    for t in extra {
        reg.register(t);
    }
    let slm = ScriptedModel::new("scripted-slm", vec![]);
    let models = ModelGateway::new(with_slm.then(|| Arc::new(slm.clone()) as Arc<dyn LanguageModel>), None);
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store: store.clone(),
        models,
        tools: Arc::new(reg),
        policy,
    });
    Rig { engine, slm, ws, store }
}

async fn run(r: &Rig, goal: &str) -> TaskRecord {
    let id = r.engine.submit(TaskRequest { goal: goal.into(), ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(20)).await.unwrap()
}

fn step<'a>(t: &'a TaskRecord, id: &str) -> &'a Step {
    t.plan.as_ref().unwrap().step(id).unwrap_or_else(|| panic!("no step {id}"))
}

fn kinds(r: &Rig, id: &str) -> Vec<String> {
    r.engine.backlog(id).iter().map(|e| serde_json::to_value(&e.kind).unwrap()["type"].as_str().unwrap().to_string()).collect()
}

fn plan(steps: Value) -> String {
    json!({ "steps": steps }).to_string()
}

// ------------------------------------------------------------------ repair ----

#[tokio::test]
async fn a_misplaced_file_is_found_by_code_not_by_a_model() {
    let r = rig(true, vec![], |_| {});
    std::fs::create_dir_all(r.ws.path().join("notes")).unwrap();
    std::fs::write(r.ws.path().join("notes/todo.md"), "- [ ] docs").unwrap();
    let t = run(&r, "read todo.md").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert!(t.final_answer.as_deref().unwrap().contains("docs"));
    let s = step(&t, "s1");
    assert_eq!((s.repairs, s.attempts), (1, 1), "the repair did not consume the retry budget");
    assert_eq!(s.args, json!({"path": "notes/todo.md"}));
    assert_eq!(t.ledger.summary().generative_tokens, 0);
    assert_eq!(r.slm.calls(), 0);
    assert!(kinds(&r, &t.task_id).contains(&"repaired".to_string()));
    assert!(!t.decisions.iter().any(|d| d.kind == "assess_recovery"), "no decision was even needed");
}

#[tokio::test]
async fn ambiguous_and_forbidden_paths_are_never_guessed() {
    let r = rig(false, vec![], |_| {});
    std::fs::create_dir_all(r.ws.path().join("a")).unwrap();
    std::fs::create_dir_all(r.ws.path().join("b")).unwrap();
    std::fs::write(r.ws.path().join("a/todo.md"), "x").unwrap();
    std::fs::write(r.ws.path().join("b/todo.md"), "y").unwrap();
    let t = run(&r, "read todo.md").await;
    assert_eq!(t.status, TaskStatus::Failed);
    assert_eq!(step(&t, "s1").repairs, 0, "two candidates: ambiguous");

    let t = run(&r, "read ../../etc/passwd.txt").await;
    assert_eq!(t.status, TaskStatus::Failed);
    let s = step(&t, "s1");
    assert_eq!(s.repairs, 0, "a policy denial is never repaired or worked around");
    assert!(s.error.as_ref().unwrap().contains("outside the allowed roots"));
}

#[tokio::test]
async fn plan_argument_problems_are_fixed_without_a_repair_call() {
    let bad_plan = plan(json!([
        {"id": "s1", "tool": "fs_read", "args": {"path": "a.txt", "max_bytes": "100", "encoding": "utf8"}},
        {"id": "s2", "tool": null, "do": "say what it holds", "after": ["s1"]}
    ]));
    let r = rig(true, vec![], |_| {});
    std::fs::write(r.ws.path().join("a.txt"), "alpha").unwrap();
    r.slm.push(&bad_plan);
    r.slm.push("It holds alpha.");
    let t = run(&r, "Read a.txt and tell me what it holds").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(r.slm.calls(), 2, "plan + step: the model was not asked to repair its own plan");
    assert_eq!(step(&t, "s1").args, json!({"path": "a.txt", "max_bytes": 100}));
    assert_eq!(step(&t, "s1").repairs, 1);
    assert!(kinds(&r, &t.task_id).contains(&"repaired".to_string()));

    // the same plan with repair switched off needs a model round trip
    let r = rig(true, vec![], |c| c.economy.repair = false);
    std::fs::write(r.ws.path().join("a.txt"), "alpha").unwrap();
    r.slm.push(&bad_plan);
    r.slm.push(&plan(
        json!([{"id": "s1", "tool": "fs_read", "args": {"path": "a.txt"}}, {"id": "s2", "tool": null, "do": "say what it holds", "after": ["s1"]}]),
    ));
    r.slm.push("It holds alpha.");
    let t = run(&r, "Read a.txt and tell me what it holds").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(r.slm.calls(), 3);
    let repair_prompt = r.slm.requests.lock()[1].messages.last().unwrap().content.clone();
    assert!(repair_prompt.contains("max_bytes") || repair_prompt.contains("unknown argument"), "{repair_prompt}");
}

// -------------------------------------------------------------- budget gate ----

#[tokio::test]
async fn an_unaffordable_plan_is_rejected_before_any_tool_runs() {
    let r = rig(true, vec![], |c| c.budget.max_model_calls = 4);
    std::fs::write(r.ws.path().join("a.txt"), "alpha").unwrap();
    // three independent reasoning leaves need 3 steps + an answer call = 4, but only 3 calls remain after planning
    r.slm.push(&plan(json!([
        {"id": "s1", "tool": "fs_read", "args": {"path": "a.txt"}},
        {"id": "s2", "tool": null, "do": "angle one", "after": ["s1"]},
        {"id": "s3", "tool": null, "do": "angle two", "after": ["s1"]},
        {"id": "s4", "tool": null, "do": "angle three", "after": ["s1"]}
    ])));
    r.slm.push(&plan(json!([{"id": "s1", "tool": "fs_read", "args": {"path": "a.txt"}}, {"id": "s2", "tool": null, "do": "explain it", "after": ["s1"]}])));
    r.slm.push("It holds alpha.");
    let t = run(&r, "Read a.txt and explain it from several angles").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    let repair = r.slm.requests.lock()[1].messages.last().unwrap().content.clone();
    assert!(repair.contains("needs about 4 model calls but only 3 remain"), "{repair}");
    assert_eq!(t.usage.tool_calls, 1, "the rejected plan's tool never ran");
    assert_eq!(t.plan.as_ref().unwrap().steps.len(), 2);
    assert_eq!(t.usage.model_calls, 3);
}

// ----------------------------------------------------------------- pricing ----

#[tokio::test]
async fn plans_carry_a_price_and_the_price_is_learned() {
    let r = rig(true, vec![], |_| {});
    std::fs::write(r.ws.path().join("a.txt"), "alpha").unwrap();
    r.slm.push(&plan(json!([{"id": "s1", "tool": "fs_read", "args": {"path": "a.txt"}}, {"id": "s2", "tool": null, "do": "explain it", "after": ["s1"]}])));
    r.slm.push("It holds alpha.");
    assert_eq!(r.engine.cost()["step"]["samples"].as_u64(), Some(0));
    assert_eq!(r.engine.cost()["step"]["estimate"].as_f64(), Some(150.0), "a prior until something is observed");
    let t = run(&r, "Read a.txt and explain it").await;
    let est = t.plan.as_ref().unwrap().estimate.expect("every accepted plan is priced");
    assert_eq!((est.model_calls, est.tokens), (1, 150), "one reasoning leaf: no extra answer call");
    assert!(kinds(&r, &t.task_id).contains(&"plan_created".to_string()));
    // finishing the task taught the model what a step and a plan really cost here
    let c = r.engine.cost();
    assert_eq!((c["step"]["samples"].as_u64(), c["plan"]["samples"].as_u64()), (Some(1), Some(1)));
    assert!(r.store.kv_get("costmodel").await.unwrap().is_some(), "and it was persisted");
    // direct plans are free and say so
    let t = run(&r, "what is 6 * 7").await;
    let est = t.plan.as_ref().unwrap().estimate.unwrap();
    assert_eq!((est.model_calls, est.tokens), (0, 0));
}

// --------------------------------------------------------------- contracts ----

#[tokio::test]
async fn junk_model_output_is_stopped_at_the_step_that_produced_it() {
    let r = rig(true, vec![], |_| {});
    std::fs::write(r.ws.path().join("a.txt"), "alpha").unwrap();
    r.slm.push(&plan(json!([{"id": "s1", "tool": "fs_read", "args": {"path": "a.txt"}}, {"id": "s2", "tool": null, "do": "explain it", "after": ["s1"]}])));
    r.slm.push("   "); // empty
    r.slm.push(&"the the the ".repeat(30)); // degenerate
    r.slm.push("It holds alpha.");
    let t = run(&r, "Read a.txt and explain it").await;
    assert_eq!(t.status, TaskStatus::Failed, "two rejected outputs exhaust the step's attempts: {:?}", t.error);
    assert!(step(&t, "s2").error.as_ref().unwrap().contains("model output rejected"));
    assert!(!t.final_answer.as_deref().unwrap_or("").contains("the the"), "junk never becomes the answer");

    let r = rig(true, vec![], |_| {});
    std::fs::write(r.ws.path().join("a.txt"), "alpha").unwrap();
    r.slm.push(&plan(json!([{"id": "s1", "tool": "fs_read", "args": {"path": "a.txt"}}, {"id": "s2", "tool": null, "do": "explain it", "after": ["s1"]}])));
    r.slm.push("");
    r.slm.push("It holds alpha.");
    let t = run(&r, "Read a.txt and explain it").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(step(&t, "s2").attempts, 2, "one bad output is retried");
    assert_eq!(t.final_answer.as_deref(), Some("It holds alpha."));
}

#[tokio::test]
async fn a_step_does_not_run_on_empty_upstream_input() {
    let r = rig(true, vec![], |_| {});
    std::fs::write(r.ws.path().join("empty.txt"), "").unwrap();
    r.slm.push(&plan(json!([
        {"id": "s1", "tool": "fs_read", "args": {"path": "empty.txt"}},
        {"id": "s2", "tool": "fs_read", "args": {"path": "{{s1}}"}}
    ])));
    let t = run(&r, "Read empty.txt then read the file whose name it contains").await;
    assert_eq!(t.status, TaskStatus::Failed);
    let s = step(&t, "s2");
    assert!(s.error.as_ref().unwrap().contains("s1 produced no output") && s.error.as_ref().unwrap().contains("not run"), "{:?}", s.error);
    assert_eq!(s.error_kind, Some(ErrorKind::InvalidArgs));
    assert_eq!(t.usage.tool_calls, 1, "the second read never ran");
}

struct Garbling;

#[async_trait]
impl Tool for Garbling {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "status_probe".into(),
            description: "probe status".into(),
            category: "system".into(),
            risk: Risk::Low,
            requires_approval: false,
            side_effects: false,
            reversible: true,
            cost_class: "free".into(),
            params: vec![],
            keywords: vec![],
            direct_patterns: vec![r"(?i)\bprobe the status\b".into()],
            cacheable: false,
            contract: vec![Check::Contains { value: "status: ".into() }],
        }
    }
    async fn execute(&self, _i: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("<html>502 Bad Gateway</html>"))
    }
}

#[tokio::test]
async fn a_tools_declared_contract_catches_a_malformed_result() {
    let r = rig(false, vec![Arc::new(Garbling)], |_| {});
    let t = run(&r, "probe the status").await;
    assert_eq!(t.status, TaskStatus::Failed);
    let e = step(&t, "s1").error.clone().unwrap();
    assert!(e.contains("broke its contract") && e.contains("status: "), "{e}");
    // the built-in tools carry contracts too, and honest output passes them
    let t = run(&r, "what is 6 * 7").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
}

// ------------------------------------------------------------ lazy context ----

fn long_file(r: &Rig) {
    let body: String = (0..120).map(|i| format!("line {i} of the long document\n")).collect::<String>() + "ZZTAIL-MARKER";
    std::fs::write(r.ws.path().join("long.txt"), body).unwrap();
}

fn two_step_plan() -> String {
    plan(json!([{"id": "s1", "tool": "fs_read", "args": {"path": "long.txt"}}, {"id": "s2", "tool": null, "do": "find the marker", "after": ["s1"]}]))
}

#[tokio::test]
async fn lazy_context_sends_short_inputs_and_expands_only_when_asked() {
    // the model asks for more: one extra call, full text on the retry
    let r = rig(true, vec![], |c| c.economy.lazy_context = true);
    long_file(&r);
    r.slm.push(&two_step_plan());
    r.slm.push("NEED s1");
    r.slm.push("The marker is ZZTAIL-MARKER.");
    let t = run(&r, "Read long.txt and find the marker").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    let reqs = r.slm.requests.lock().clone();
    assert_eq!(reqs.len(), 3);
    let (first, second) = (reqs[1].messages.last().unwrap().content.clone(), reqs[2].messages.last().unwrap().content.clone());
    assert!(first.contains("reply with exactly `NEED <id>`") && !first.contains("ZZTAIL-MARKER"), "short inputs first:\n{first}");
    assert!(second.contains("ZZTAIL-MARKER") && !second.contains("NEED <id>"), "full text on request:\n{second}");
    assert_eq!(reqs[2].purpose, "step_expand");
    assert!(step(&t, "s2").evidence.iter().any(|e| e.contains("expanded on request: s1")));
    assert!(first.len() < second.len() / 2, "the default prompt is much smaller");

    // the model is content with the short summary: no extra call
    let r = rig(true, vec![], |c| c.economy.lazy_context = true);
    long_file(&r);
    r.slm.push(&two_step_plan());
    r.slm.push("It is a long document.");
    let t = run(&r, "Read long.txt and find the marker").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(r.slm.calls(), 2);
}

#[tokio::test]
async fn lazy_context_is_off_by_default_and_changes_nothing_then() {
    let r = rig(true, vec![], |_| {});
    long_file(&r);
    r.slm.push(&two_step_plan());
    r.slm.push("NEED s1"); // an answer, as far as a non-lazy runtime is concerned
    let t = run(&r, "Read long.txt and find the marker").await;
    assert_eq!(r.slm.calls(), 2, "no expansion round trip");
    let prompt = r.slm.requests.lock()[1].messages.last().unwrap().content.clone();
    assert!(!prompt.contains("NEED <id>"));
    assert_eq!(t.final_answer.as_deref(), Some("NEED s1"));
}
