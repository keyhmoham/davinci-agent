#![allow(clippy::field_reassign_with_default)]
//! Regressions from real sessions: a landing page that was written but never served, replans that broke on duplicate ids,
//! placeholders written into files, and an external model that was down.

use dfae_core::*;
use dfae_decision::{Calibrator, Decider};
use dfae_extract::HybridExtractor;
use dfae_memory::InMemoryStore;
use dfae_models::{ModelGateway, ScriptedModel};
use dfae_runtime::{Engine, EngineDeps};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

struct Rig {
    engine: Engine,
    slm: ScriptedModel,
    strong: ScriptedModel,
    ws: tempfile::TempDir,
}

fn rig(strong: bool, tweak: impl FnOnce(&mut Config)) -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    cfg.thresholds.external_min_complexity = 2.0; // these are about the self-hosted path unless a test turns the external model on
    tweak(&mut cfg);
    let mut pc = PolicyConfig::from_toml("[shell]\nmode = \"sandbox\"\n").unwrap();
    pc.approvals = ApprovalMode::Never;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(pc, ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let (slm, strong_m) = (ScriptedModel::new("slm", vec![]), ScriptedModel::new("strong", vec![]));
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(slm.clone())), strong.then(|| Arc::new(strong_m.clone()) as Arc<dyn LanguageModel>)),
        tools: Arc::new(reg),
        policy,
    });
    Rig { engine, slm, strong: strong_m, ws }
}

async fn run(r: &Rig, goal: &str) -> TaskRecord {
    let id = r.engine.submit(TaskRequest { goal: goal.into(), delegate: Some(false), ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(30)).await.unwrap()
}

const GOAL: &str = "Create a landing page about earth in public/earth.html and then run a shell command to list that directory to prove it is there";

#[tokio::test]
async fn a_step_that_needs_a_directory_waits_for_the_step_that_creates_it() {
    let r = rig(false, |_| {});
    // the model forgot the "after": the listing would have run in parallel, in a directory that did not exist yet
    r.slm.push(
        &json!({"steps": [
            {"id": "s1", "tool": "fs_write", "args": {"path": "public/earth.html", "content": "<html><body><h1>Earth</h1></body></html>"}},
            {"id": "s2", "tool": "shell", "args": {"command": "ls", "cwd": "public"}}
        ]})
        .to_string(),
    );
    let t = run(&r, GOAL).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let plan = t.plan.as_ref().unwrap();
    assert_eq!(plan.step("s2").unwrap().depends_on, vec!["s1"], "ordered by code");
    assert!(t.final_answer.as_deref().unwrap().contains("earth.html"), "{:?}", t.final_answer);
    assert!(r.engine.backlog(&t.task_id).iter().any(|e| matches!(&e.kind, EventKind::Repaired { note, .. } if note.contains("s2 now waits for s1"))));
}

#[tokio::test]
async fn a_replan_that_repeats_ids_and_forgets_an_argument_gets_one_repair_round() {
    let r = rig(false, |_| {});
    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "missing.txt"}}, {"id": "s2", "tool": null, "do": "say what it holds", "after": ["s1"]}]}).to_string());
    // the replan from the screenshot: ids repeated and mixed, and a write without a path
    r.slm.push(
        &json!({"steps": [
            {"id": "r1_s1", "tool": "fs_search", "args": {}},
            {"id": "r1_s2", "tool": null, "do": "report", "after": ["r1_s1"]},
            {"id": "r1_s2", "tool": null, "do": "report again", "after": ["r1_s1"]}
        ]})
        .to_string(),
    );
    r.slm.push(&json!({"steps": [{"id": "r1_1", "tool": "fs_list", "args": {"path": "."}}, {"id": "r1_2", "tool": null, "do": "say that the folder is empty", "after": ["r1_1"]}]}).to_string());
    r.slm.push("missing.txt does not exist and the folder is empty.");
    let t = run(&r, "Read missing.txt and then explain in your own words what it holds").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("folder is empty"));
    let repair =
        r.slm.requests.lock().iter().find(|q| q.purpose == "replan_repair").map(|q| q.messages.last().unwrap().content.clone()).expect("a repair round");
    assert!(repair.contains("missing required argument 'pattern'"), "{repair}");
    let ids: Vec<String> = t.plan.as_ref().unwrap().steps.iter().map(|s| s.id.clone()).collect();
    assert!(ids.contains(&"r1_1".to_string()) && ids.contains(&"r1_2".to_string()), "{ids:?}");
}

#[tokio::test]
async fn placeholders_a_small_model_invents_are_sent_back_not_written_into_the_page() {
    let r = rig(false, |_| {});
    r.slm.push(
        &json!({"steps": [{"id": "s1", "tool": "fs_write", "args": {"path": "p.html", "content": "<p>{{ENT.location}} and {{ENT.person}} are here</p>"}}]})
            .to_string(),
    );
    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_write", "args": {"path": "p.html", "content": "<p>Earth is here</p>"}}]}).to_string());
    let t = run(&r, "Create the file p.html with a short page about the earth").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("p.html")).unwrap(), "<p>Earth is here</p>");
    let repair = r.slm.requests.lock().iter().find(|q| q.purpose == "plan_repair").unwrap().messages.last().unwrap().content.clone();
    assert!(repair.contains("{{ENT.location}}") && repair.contains("real text"), "{repair}");
}

#[tokio::test]
async fn when_the_external_model_is_down_the_self_hosted_one_plans_and_answers() {
    let r = rig(true, |c| c.thresholds.external_min_complexity = 0.01);
    r.strong.push_err("connection refused");
    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_write", "args": {"path": "a.txt", "content": "hi"}}]}).to_string());
    let t = run(&r, "Create a file a.txt that says hi, and keep it tidy and short").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("a.txt")).unwrap(), "hi");
    let warnings: Vec<String> =
        r.engine.backlog(&t.task_id).into_iter().filter_map(|e| if let EventKind::Warning { message } = e.kind { Some(message) } else { None }).collect();
    assert!(warnings.iter().any(|m| m.contains("self-hosted model plans instead")), "{warnings:?}");
}

#[tokio::test]
async fn a_conversation_the_external_model_cannot_answer_is_answered_locally() {
    let r = rig(true, |c| c.thresholds.external_min_complexity = 0.01);
    r.strong.push_err("HTTP 503 overloaded");
    r.slm.push("A mutex admits one thread at a time.");
    let t = run(
        &r,
        "Explain in detail how a mutex differs from a semaphore, when to use each one, and which pitfalls such as deadlock and priority inversion to avoid",
    )
    .await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert!(t.final_answer.as_deref().unwrap().contains("one thread"), "{:?}", t.final_answer);
}
