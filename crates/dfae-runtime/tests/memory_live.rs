#![allow(clippy::field_reassign_with_default)]
//! Memory with real semantic embeddings and scripted models: a verified plan becomes a skill; a later request that says the same
//! thing in other words cannot replay it (another shape) but its planner is shown the plan as a worked example. Ignored by default
//! (it calls the gateway's embeddings endpoint).
//!
//! ```bash
//! PLANNER_API_KEY=… cargo test -p dfae-runtime --test memory_live -- --ignored --nocapture
//! ```

use dfae_core::*;
use dfae_decision::{Calibrator, Decider};
use dfae_extract::HybridExtractor;
use dfae_memory::{InMemoryStore, RemoteEmbedder};
use dfae_models::{ModelGateway, ScriptedModel};
use dfae_runtime::{Engine, EngineDeps};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
#[ignore = "calls the gateway's embeddings endpoint"]
async fn a_paraphrased_request_gets_the_earlier_plan_as_a_worked_example() {
    let key = std::env::var("PLANNER_API_KEY").expect("PLANNER_API_KEY");
    let embed = RemoteEmbedder::new("https://ai-gateway.vercel.sh/v1", "voyage/voyage-4-lite", Some(key), 1024, Duration::from_secs(20)).unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    let store = Arc::new(InMemoryStore::new().with_semantic(Some(embed)));
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
    let run = |goal: &str| {
        let e = &engine;
        let goal = goal.to_string();
        async move {
            let id = e.submit(TaskRequest { goal, delegate: Some(false), mode: ModeRequest::Escalated, ..Default::default() }).await.unwrap();
            e.wait(&id, Duration::from_secs(60)).await.unwrap()
        }
    };
    std::fs::write(ws.path().join("notes.txt"), "alpha beta\ngamma\n").unwrap();
    // A: planned, carried out, verified: it becomes a skill
    planner.push(&json!({"steps": [{"id": "s1", "tool": "fs_read", "do": "read notes.txt", "after": [], "expect": null, "planner": false}, {"id": "s2", "tool": "fs_mkdir", "do": "create the directory archive", "after": ["s1"], "expect": "archive exists", "planner": false}]}).to_string());
    let a = run("Read notes.txt and then create a directory called archive").await;
    assert_eq!(a.status, TaskStatus::Completed, "{:?}", a.error);
    tokio::time::sleep(Duration::from_millis(300)).await;
    // B: the same job in other words — another shape, so no replay: a plan request, which shows A's plan
    planner.push(
        &json!({"steps": [{"id": "s1", "tool": "fs_mkdir", "do": "create the directory backup", "after": [], "expect": null, "planner": false}]}).to_string(),
    );
    let b = run("Have a look at notes.txt, after that make a folder named backup").await;
    assert_eq!(b.status, TaskStatus::Completed, "{:?}", b.error);
    let reqs = planner.requests.lock().clone();
    let plan_b = reqs.iter().rev().find(|q| q.purpose.starts_with("plan")).unwrap();
    let user = &plan_b.messages[0].content;
    eprintln!("B's plan request:\n{user}");
    assert!(user.contains("A PLAN THAT WORKED FOR A SIMILAR REQUEST") && user.contains("fs_mkdir"), "the planner sees A's plan");
    assert!(!user.contains("{{slot:") && user.contains("<file_path>"), "slots are shown as <name>, never as a value reference");
    // C: an unrelated request sees no template
    planner.push(&json!({"steps": [{"id": "s1", "tool": "calc", "do": "calculate 6 * 7", "after": [], "expect": null, "planner": false}]}).to_string());
    worker.push(r#"{"expression": "6 * 7"}"#);
    run("calculate 6 * 7 and tell me the result").await;
    let reqs = planner.requests.lock().clone();
    let plan_c = reqs.iter().rev().find(|q| q.purpose.starts_with("plan")).unwrap();
    assert!(!plan_c.messages[0].content.contains("A PLAN THAT WORKED"), "{}", plan_c.messages[0].content);
}
