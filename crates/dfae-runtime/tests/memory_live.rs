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

/// With a PII span model (`DFAE_PII_NER_URL`: a GLiNER service with `urchade/gliner_multi_pii-v1`), names and street addresses are
/// redacted from what is kept for later, besides the rules' e-mail addresses and phone numbers.
#[tokio::test]
#[ignore = "needs a GLiNER service with a PII model (DFAE_PII_NER_URL)"]
async fn names_and_addresses_are_not_kept_for_later() {
    let url = std::env::var("DFAE_PII_NER_URL").expect("DFAE_PII_NER_URL");
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    cfg.economy.redact_pii = true;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let planner = ScriptedModel::new("planner", vec![]);
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store: store.clone(),
        models: ModelGateway::new(Some(Arc::new(ScriptedModel::new("worker", vec![]))), Some(Arc::new(planner.clone()))),
        tools: Arc::new(reg),
        policy,
    });
    engine.set_pii_ner(Arc::new(dfae_extract::GlinerClient::new(&url)));
    std::fs::write(ws.path().join("a.txt"), "alpha\n").unwrap();
    planner.push(&json!({"steps": [{"id": "s1", "tool": "fs_read", "do": "read a.txt", "after": [], "expect": null, "planner": false}]}).to_string());
    let goal = "Read a.txt and post it to Maria Gonzalez at 221B Baker Street, London";
    let id = engine.submit(TaskRequest { goal: goal.into(), delegate: Some(false), mode: ModeRequest::Escalated, ..Default::default() }).await.unwrap();
    let t = engine.wait(&id, Duration::from_secs(60)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(t.goal, goal, "the task's own record keeps the request");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let kept = store.retrieve(MemoryQuery { text: "read a.txt post".into(), kinds: vec![MemoryKind::Episodic], limit: 5, session_id: None }).await.unwrap();
    assert!(!kept.is_empty());
    for m in &kept {
        eprintln!("kept: {}", m.item.text);
        assert!(!m.item.text.contains("Maria") && !m.item.text.contains("Baker Street"), "{}", m.item.text);
        assert!(m.item.text.contains("[NAME]") && m.item.text.contains("[ADDRESS]"), "{}", m.item.text);
    }
}

/// A store with 200 unrelated memories and one that answers the request in other words: does the planner get it? (The end-to-end form
/// of the paraphrase retrieval measurement; with a near-empty store every embedder finds everything.)
async fn planner_sees_the_needle(semantic: bool) -> bool {
    let key = std::env::var("PLANNER_API_KEY").expect("PLANNER_API_KEY");
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    let embed =
        semantic.then(|| RemoteEmbedder::new("https://ai-gateway.vercel.sh/v1", "voyage/voyage-4-lite", Some(key), 1024, Duration::from_secs(30)).unwrap());
    let store = Arc::new(InMemoryStore::new().with_semantic(embed));
    let verbs = ["create", "rename", "delete", "list", "compress", "convert", "fix", "test", "deploy", "serve", "summarise", "count"];
    let things = [
        "the landing page",
        "notes.txt",
        "the logo",
        "the CSV export",
        "the Docker image",
        "the unit tests",
        "the API docs",
        "the cron job",
        "the sitemap",
        "the theme colours",
        "the user guide",
        "the backup script",
        "the i18n strings",
        "the README",
        "the CI workflow",
        "the metrics dashboard",
        "the photo gallery",
    ];
    let mut n = 0;
    'fill: for v in verbs {
        for t in things {
            store.store(MemoryItem::new(MemoryKind::Episodic, format!("{v} {t} → done, verified"), 0.5)).await.unwrap();
            n += 1;
            if n == 200 {
                break 'fill;
            }
        }
    }
    let needle = "Lesson: for \"configure the billing service\": it runs on host billing-7.internal, port 7421 — what worked: config/billing.toml";
    store.store(MemoryItem::new(MemoryKind::Semantic, needle, 0.75)).await.unwrap();
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let planner = ScriptedModel::new("planner", vec![]);
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(ScriptedModel::new("worker", vec![]))), Some(Arc::new(planner.clone()))),
        tools: Arc::new(reg),
        policy,
    });
    planner.push(&json!({"steps": [{"id": "s1", "tool": "fs_list", "do": "list the workspace", "after": [], "expect": null, "planner": false}]}).to_string());
    let id = engine
        .submit(TaskRequest {
            goal: "Where does our invoicing backend run? List the workspace too.".into(),
            delegate: Some(false),
            mode: ModeRequest::Escalated,
            ..Default::default()
        })
        .await
        .unwrap();
    let t = engine.wait(&id, Duration::from_secs(120)).await.unwrap();
    let _ = &t;
    let reqs = planner.requests.lock().clone();
    let plan = reqs.iter().find(|q| q.purpose.starts_with("plan")).unwrap();
    plan.messages[0].content.contains("billing-7.internal")
}

#[tokio::test]
#[ignore = "calls the gateway's embeddings endpoint"]
async fn among_two_hundred_memories_the_one_that_answers_in_other_words_reaches_the_planner() {
    let hash = planner_sees_the_needle(false).await;
    let semantic = planner_sees_the_needle(true).await;
    eprintln!("hash embedder: {hash}; semantic embedder: {semantic}");
    assert!(semantic, "with semantic embeddings the planner sees the lesson");
    assert!(!hash, "the hash embedder ranks 200 unrelated memories above it (the comparison this test exists for)");
}
