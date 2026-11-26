#![allow(clippy::field_reassign_with_default)]
//! The agent never waits for a human, knows its coding tools, and answers tersely.

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
    worker: ScriptedModel,
    ws: tempfile::TempDir,
}

/// `approvals`: what the engine is configured with. Shell sandbox mode as in the shipped container policy.
fn rig(approvals: ApprovalMode) -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = approvals;
    cfg.approval_ttl_secs = 2;
    let mut pc = PolicyConfig::from_toml("[shell]\nmode = \"sandbox\"\n").unwrap();
    pc.approvals = approvals;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(pc, ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let worker = ScriptedModel::new("scripted-worker", vec![]);
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(worker.clone())), None),
        tools: Arc::new(reg),
        policy,
    });
    Rig { engine, worker, ws }
}

async fn run(r: &Rig, goal: &str) -> TaskRecord {
    let id = r.engine.submit(TaskRequest { goal: goal.into(), ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(30)).await.unwrap()
}

#[tokio::test]
async fn nothing_waits_for_a_human() {
    let r = rig(ApprovalMode::Never);
    let t = run(&r, r#"create a file named a.txt with content "hello""#).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.approvals.is_empty(), "no approval was ever requested");
    assert_eq!(std::fs::read_to_string(r.ws.path().join("a.txt")).unwrap(), "hello");

    // mutating shell commands, file moves and deletes all just run
    let t = run(&r, "run `mkdir -p out/x && echo hi > out/x/y.txt && mv a.txt out/a.txt`").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.approvals.is_empty());
    assert!(r.ws.path().join("out/a.txt").exists() && r.ws.path().join("out/x/y.txt").exists());
    let t = run(&r, "delete out/x/y.txt").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert!(!r.ws.path().join("out/x/y.txt").exists() && t.approvals.is_empty());

    // what the policy forbids is refused, not asked about
    let t = run(&r, r#"create a file named ../escape.txt with content "x""#).await;
    assert_eq!(t.status, TaskStatus::Failed);
    assert!(t.approvals.is_empty() && t.error.as_deref().unwrap_or("").contains("outside the allowed roots"), "{:?}", t.error);
    assert_eq!(r.worker.calls(), 0);
}

#[tokio::test]
async fn with_approvals_switched_on_a_write_still_waits() {
    let r = rig(ApprovalMode::Ask);
    let t = run(&r, r#"create a file named a.txt with content "hello""#).await;
    assert_eq!(t.approvals.len(), 1, "the machinery is still there for those who want it");
    assert!(!r.ws.path().join("a.txt").exists());
}

#[tokio::test]
async fn the_planner_is_shown_how_to_use_the_coding_tools_it_needs() {
    let r = rig(ApprovalMode::Never);
    std::fs::create_dir_all(r.ws.path().join("src")).unwrap();
    std::fs::write(r.ws.path().join("src/lib.rs"), "fn foo() {}\nfn main() { foo(); }\n").unwrap();
    r.worker.push(
        &json!({"steps": [
            {"id": "s1", "tool": "fs_read", "do": "read the file", "args": {"path": "src/lib.rs"}},
            {"id": "s2", "tool": "fs_edit", "do": "rename foo", "after": ["s1"], "args": {"path": "src/lib.rs", "old_string": "foo", "new_string": "bar", "replace_all": true}}
        ]})
        .to_string(),
    );
    let t = run(&r, "refactor src/lib.rs: rename the function foo to bar everywhere in the file").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("src/lib.rs")).unwrap(), "fn bar() {}\nfn main() { bar(); }\n");
    let prompt = r.worker.requests.lock()[0].messages.last().unwrap().content.clone();
    for expect in ["fs_edit(", "copied exactly from fs_read", "fs_read(", "fs_search("] {
        assert!(prompt.contains(expect), "the planner must see {expect}: {prompt}");
    }
    assert!(!prompt.contains("http_get(") && !prompt.contains("calc("), "unrelated tools stay out of the prompt: {prompt}");
}

#[tokio::test]
async fn shell_job_tools_come_along_with_the_sandbox_shell() {
    let r = rig(ApprovalMode::Never);
    r.worker.push(&json!({"steps": [{"id": "s1", "tool": "shell", "do": "run the tests", "args": {"command": "echo 3 passed"}}]}).to_string());
    let t = run(&r, "run the project's test suite with a bash command and report the result").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let prompt = r.worker.requests.lock()[0].messages.last().unwrap().content.clone();
    assert!(prompt.contains("shell(") && prompt.contains("background=true") && prompt.contains("shell_output(") && prompt.contains("shell_kill("), "{prompt}");
    assert!(t.final_answer.as_deref().unwrap().contains("3 passed"), "{:?}", t.final_answer);
}

#[tokio::test]
async fn answers_are_stripped_of_filler_and_chat_is_short() {
    let r = rig(ApprovalMode::Never);
    r.worker.push("Sure! A mutex lets one thread in at a time. Let me know if you'd like an example.");
    let t = run(&r, "what is a mutex?").await;
    assert_eq!(t.final_answer.as_deref(), Some("A mutex lets one thread in at a time."));
    let req = r.worker.requests.lock()[0].clone();
    assert!(req.system.contains("Never ask for confirmation"));
    assert!(req.max_tokens > 400, "a self-hosted reply is not cut short (only a paid one is capped)");
}
