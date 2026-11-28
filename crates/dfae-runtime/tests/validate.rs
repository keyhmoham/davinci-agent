#![allow(clippy::field_reassign_with_default)]
//! The code a task wrote is validated before the task is judged: here a page calls an API route the backend does not have; the
//! validation finds it with no request made, the planner is told exactly what is broken, the patched plan runs, and the second round
//! passes. Without the fix the task is not verified.

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

fn rig() -> (Engine, ScriptedModel, ScriptedModel, tempfile::TempDir) {
    rig_with(1)
}

fn rig_with(rounds: u32) -> (Engine, ScriptedModel, ScriptedModel, tempfile::TempDir) {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.validate.rounds = rounds;
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    cfg.validate.ui = false;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let (worker, planner) = (ScriptedModel::new("worker", vec![]), ScriptedModel::new("planner", vec![]));
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(worker.clone())), Some(Arc::new(planner.clone()))),
        tools: Arc::new(reg),
        policy,
    });
    (engine, worker, planner, ws)
}

const SERVER: &str = "from fastapi import FastAPI\napp = FastAPI()\n\n@app.get(\"/api/items\")\ndef items():\n    return [{\"name\": \"tea\"}]\n";

#[tokio::test]
async fn a_page_that_calls_a_route_the_backend_lacks_is_repaired_before_the_task_is_done() {
    let (engine, _worker, planner, ws) = rig();
    planner.push(
        &json!({"steps": [
            {"id": "s1", "tool": "fs_write", "args": {"path": "shop/server.py", "content": SERVER}, "do": "write the API", "after": [], "expect": null, "planner": false},
            {"id": "s2", "tool": "fs_write", "args": {"path": "shop/static/app.js", "content": "fetch('/api/itemz').then(r => r.json()).then(show);\n"}, "do": "write the page script", "after": [], "expect": null, "planner": false}
        ]})
        .to_string(),
    );
    // the repair the planner writes when told what failed
    planner.push(
        &json!({"edit": [], "add": [
            {"id": "r1_1", "tool": "fs_write", "args": {"path": "shop/static/app.js", "content": "fetch('/api/items').then(r => r.json()).then(show);\n"}, "do": "call the route the API has", "after": [], "expect": null, "planner": false}
        ], "drop": []})
        .to_string(),
    );
    let id = engine
        .submit(TaskRequest {
            goal: "write a small shop: a FastAPI backend with an items API and a page script that lists them".into(),
            delegate: Some(false),
            mode: ModeRequest::Escalated,
            ..Default::default()
        })
        .await
        .unwrap();
    let t = engine.wait(&id, Duration::from_secs(60)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);

    let rounds: Vec<(u32, Vec<CheckResult>)> = engine
        .backlog(&id)
        .into_iter()
        .filter_map(|e| if let EventKind::Validated { round, checks } = e.kind { Some((round, checks)) } else { None })
        .collect();
    assert_eq!(rounds.len(), 2, "{rounds:?}");
    let failed0: Vec<&CheckResult> = rounds[0].1.iter().filter(|c| c.status == CheckStatus::Failed).collect();
    assert_eq!(failed0.len(), 1, "{:?}", rounds[0].1);
    assert!(failed0[0].evidence.contains("GET /api/itemz but no backend route serves it"), "{}", failed0[0].evidence);
    assert!(rounds[1].1.iter().all(|c| c.status == CheckStatus::Passed) && !rounds[1].1.is_empty(), "{:?}", rounds[1].1);

    // the planner was told what is broken, in so many words
    let reqs = planner.requests.lock().clone();
    let repair = reqs.iter().find(|q| q.messages.iter().any(|m| m.content.contains("VALIDATION FAILED"))).expect("a repair request");
    assert!(repair.messages.iter().any(|m| m.content.contains("/api/itemz")));
    assert_eq!(std::fs::read_to_string(ws.path().join("shop/static/app.js")).unwrap(), "fetch('/api/items').then(r => r.json()).then(show);\n");
    // and the verification carries the validation
    let v = t.verification.unwrap();
    assert!(v.checks.iter().any(|c| c.check_id == "validate:api" && c.status == CheckStatus::Passed), "{:?}", v.checks);
    assert!(!v.checks.iter().any(|c| c.check_id.starts_with("validate:") && c.status == CheckStatus::Failed));
}

#[tokio::test]
async fn a_task_that_wrote_no_code_is_not_validated() {
    let (engine, _worker, planner, _ws) = rig();
    planner.push(&json!({"steps": [{"id": "s1", "tool": "fs_write", "args": {"path": "notes/todo.md", "content": "- buy tea\n"}, "do": "write the note", "after": [], "expect": null, "planner": false}]}).to_string());
    let id = engine
        .submit(TaskRequest {
            goal: "write a note notes/todo.md that says buy tea".into(),
            delegate: Some(false),
            mode: ModeRequest::Escalated,
            ..Default::default()
        })
        .await
        .unwrap();
    let t = engine.wait(&id, Duration::from_secs(60)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert!(engine.backlog(&id).iter().all(|e| !matches!(e.kind, EventKind::Validated { .. })));
}

#[tokio::test]
async fn what_stays_broken_is_said_in_the_answer_whoever_wrote_it() {
    let (engine, _worker, planner, _ws) = rig_with(0);
    planner.push(
        &json!({"steps": [
            {"id": "s1", "tool": "fs_write", "args": {"path": "shop/server.py", "content": SERVER}, "do": "write the API", "after": [], "expect": null, "planner": false},
            {"id": "s2", "tool": "fs_write", "args": {"path": "shop/static/app.js", "content": "fetch('/api/itemz').then(r => r.json());\n"}, "do": "write the page script", "after": [], "expect": null, "planner": false}
        ]})
        .to_string(),
    );
    let id = engine
        .submit(TaskRequest {
            goal: "write a small shop: a FastAPI backend with an items API and a page script that lists them".into(),
            delegate: Some(false),
            mode: ModeRequest::Escalated,
            ..Default::default()
        })
        .await
        .unwrap();
    let t = engine.wait(&id, Duration::from_secs(60)).await.unwrap();
    let answer = t.final_answer.unwrap_or_default();
    assert!(answer.contains("Tested, and still not working") && answer.contains("/api/itemz"), "{answer}");
    assert_eq!(t.verification.unwrap().status, VerificationStatus::Failed);
}
