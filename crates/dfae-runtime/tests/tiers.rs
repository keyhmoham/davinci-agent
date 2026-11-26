#![allow(clippy::field_reassign_with_default)]
//! Self-hosted models cost nothing, the external one is paid for: prompts are generous for the former and lean for the latter.

use dfae_core::*;
use dfae_decision::{Calibrator, Decider};
use dfae_extract::HybridExtractor;
use dfae_memory::InMemoryStore;
use dfae_models::{ModelGateway, ScriptedModel};
use dfae_runtime::{Engine, EngineDeps, prompts};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

struct Rig {
    engine: Engine,
    worker: ScriptedModel,
    planner: ScriptedModel,
    ws: tempfile::TempDir,
}

fn rig() -> Rig {
    rig_full(false, true)
}

/// No planner model: the self-hosted one plans as well.
fn rig_local() -> Rig {
    rig_full(false, false)
}

fn rig_with(condense: bool) -> Rig {
    rig_full(condense, true)
}

fn rig_full(condense: bool, with_strong: bool) -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    cfg.economy.plan_prefix_always = true;
    cfg.economy.condense = condense;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let (worker, planner) = (ScriptedModel::new("worker", vec![]), ScriptedModel::new("planner", vec![]).caching());
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(worker.clone())), with_strong.then(|| Arc::new(planner.clone()) as Arc<dyn LanguageModel>)),
        tools: Arc::new(reg),
        policy,
    });
    Rig { engine, worker, planner, ws }
}

async fn run(r: &Rig, goal: &str, mode: ModeRequest) -> TaskRecord {
    let id = r.engine.submit(TaskRequest { goal: goal.into(), mode, ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(20)).await.unwrap()
}

fn plan(read: &str) -> String {
    json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": read}}, {"id": "s2", "tool": null, "do": "summarise the file", "after": ["s1"]}]})
        .to_string()
}

/// What the planner model writes: intents, the reasoning step done by itself (`planner`) so its prompt can be compared.
fn brief(read: &str) -> String {
    json!({"steps": [{"id": "s1", "do": format!("read {read}"), "tool": "fs_read"}, {"id": "s2", "do": "summarise the file", "tool": null, "after": ["s1"], "planner": true}]}).to_string()
}

fn big_file(r: &Rig) {
    let text: String = (1..=600).map(|i| format!("line number {i} of the log with some words in it\n")).collect();
    std::fs::write(r.ws.path().join("big.log"), text).unwrap();
}

#[tokio::test]
async fn the_planner_prompt_is_generous_for_the_self_hosted_model_and_lean_for_the_paid_one() {
    let rl = rig_local();
    big_file(&rl);
    rl.worker.push(&plan("big.log"));
    rl.worker.push("summary");
    let t = run(&rl, "Read big.log and summarise it", ModeRequest::Assisted).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    let local = rl.worker.requests.lock()[0].clone();
    let r = rig();
    big_file(&r);
    assert_eq!(local.system, prompts::PLAN_SYSTEM_LOCAL);
    assert!(
        local.system.contains("Examples") && local.messages[0].content.contains("params —"),
        "worked examples and parameter help: {}",
        local.messages[0].content
    );

    r.planner.push(&brief("big.log"));
    r.planner.push("summary");
    let t = run(&r, "Read big.log and summarise it", ModeRequest::Escalated).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    let paid = r.planner.requests.lock()[0].clone();
    // the paid prompt's stable part (rules, whole catalogue, examples) is one cacheable prefix; what changes per task is short
    assert!(paid.cache && paid.system.contains("fs_read(path") && !paid.messages[0].content.contains("params —"));
    assert!(paid.messages[0].content.len() < local.messages[0].content.len(), "the per-task part of the paid prompt is the shorter one");
    assert_eq!(t.usage.model_calls, 2, "the brief plan and the one step the planner kept for itself are the paid calls");
}

#[tokio::test]
async fn the_worker_reads_a_little_more_of_an_observation_than_the_planner_and_neither_the_whole_of_it() {
    let rl = rig_local();
    big_file(&rl);
    rl.worker.push(&plan("big.log"));
    rl.worker.push("ok");
    run(&rl, "Read big.log and summarise it", ModeRequest::Assisted).await;
    let local_step = rl.worker.requests.lock().iter().find(|q| q.purpose == "step").unwrap().messages[0].content.clone();
    let r = rig();
    big_file(&r);
    r.planner.push(&brief("big.log"));
    r.planner.push("ok");
    run(&r, "Read big.log and summarise it", ModeRequest::Escalated).await;
    let paid_step = r.planner.requests.lock().iter().find(|q| q.purpose == "step").unwrap().messages[0].content.clone();
    let lines = |s: &str| s.matches("line number").count();
    assert!(lines(&local_step) > lines(&paid_step), "the worker sees {} lines, the planner {}", lines(&local_step), lines(&paid_step));
    assert!(lines(&local_step) < 100, "both are billed by the token: the worker sees {} of the 600 lines", lines(&local_step));
}

#[tokio::test]
async fn worker_calls_do_not_count_against_the_planner_budget_but_their_tokens_are_billed_tokens() {
    let r = rig_local();
    big_file(&r);
    r.worker.push(&plan("big.log"));
    r.worker.push("summary");
    let t = run(&r, "Read big.log and summarise it", ModeRequest::Assisted).await;
    assert_eq!((t.usage.model_calls, t.usage.worker_calls), (0, 2), "the planner budget meters planner calls only");
    let s = t.ledger.summary();
    assert_eq!(s.planner_tokens, 0);
    assert!(s.worker_tokens > 0);
    assert_eq!(s.saved_tokens, s.baseline_tokens - s.generative_tokens, "the worker's tokens are hosted tokens: they count against the naive baseline");
}

#[tokio::test]
async fn a_big_output_is_read_locally_and_the_external_model_gets_what_matters() {
    let r = rig_with(true);
    big_file(&r);
    r.planner.push(&brief("big.log"));
    // the self-hosted model condenses the 600-line log for the planner's later steps
    r.worker.push("line number 3 of the log with some words in it");
    r.planner.push("done");
    let t = run(&r, "Read big.log and summarise it", ModeRequest::Escalated).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    let step = t.plan.as_ref().unwrap().step("s1").unwrap().clone();
    assert!(step.summary.as_deref().unwrap().starts_with("[condensed]"), "{:?}", step.summary);
    assert!(r.worker.requests.lock().iter().any(|q| q.purpose == "condense"));
    // the paid reasoning step read the condensed text, not 600 lines
    let paid = r.planner.requests.lock().iter().find(|q| q.purpose == "step").unwrap().messages[0].content.clone();
    assert!(paid.matches("line number").count() <= 2, "{paid}");
    // and a made-up number would have been refused: an answer that invents values is not a summary
    assert_eq!(t.ledger.summary().worker_calls, 1);
}
