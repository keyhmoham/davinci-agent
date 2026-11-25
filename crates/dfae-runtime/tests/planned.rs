#![allow(clippy::field_reassign_with_default)]
//! Planning and acting are separate phases: the external model writes a brief plan, and the local system
//! (rules + GLiNER, the decision model, the SLM) implements each step, escalating only when it has to.

use async_trait::async_trait;
use dfae_core::*;
use dfae_decision::{Calibrator, Decider, LocalDecider};
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
    slm: ScriptedModel,
    strong: ScriptedModel,
    ws: tempfile::TempDir,
}

/// A decision model that agrees with the local rules except that it finds any step result containing "BAD" unsatisfactory.
struct Picky(LocalDecider);

#[async_trait]
impl DecisionProvider for Picky {
    fn name(&self) -> &str {
        "open-model"
    }
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let mut r = self.0.decide(req.clone()).await?;
        if req.kind == DecisionKind::AssessCompletion
            && req.state.request.starts_with("Step:")
            && req.state.completion_facts.as_ref().map(|f| f.evidence.contains("BAD")).unwrap_or(false)
        {
            r.answers.insert("complete".into(), Answer::noul(0.05));
        }
        Ok(r)
    }
}

fn rig(picky: bool, tweak: impl FnOnce(&mut Config)) -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    tweak(&mut cfg);
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let (slm, strong) = (ScriptedModel::new("slm", vec![]), ScriptedModel::new("strong", vec![]));
    let provider: Option<Arc<dyn DecisionProvider>> = picky.then(|| Arc::new(Picky(LocalDecider::new(cfg.thresholds.clone()))) as Arc<dyn DecisionProvider>);
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(provider, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(slm.clone())), Some(Arc::new(strong.clone()))),
        tools: Arc::new(reg),
        policy,
    });
    Rig { engine, slm, strong, ws }
}

async fn run(r: &Rig, goal: &str) -> TaskRecord {
    // one task: this file tests the plan/act split, not delegation
    let id = r.engine.submit(TaskRequest { goal: goal.into(), delegate: Some(false), ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(30)).await.unwrap()
}

fn events(r: &Rig, id: &str) -> Vec<EventKind> {
    r.engine.backlog(id).into_iter().map(|e| e.kind).collect()
}

async fn run_as(r: &Rig, mode: ModeRequest, goal: &str) -> TaskRecord {
    let id = r.engine.submit(TaskRequest { goal: goal.into(), delegate: Some(false), mode, ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(30)).await.unwrap()
}

const GOAL: &str = "Read config.toml, work out which port the service listens on, and write only that port number into ports.txt";

#[tokio::test]
async fn medium_tasks_are_planned_by_the_external_model_and_carried_out_by_the_local_system() {
    let r = rig(false, |_| {});
    std::fs::write(r.ws.path().join("config.toml"), "[server]\nhost = \"0.0.0.0\"\nport = 8080\n").unwrap();
    // planning phase: intents, no arguments
    r.strong.push(
        &json!({"steps": [
            {"id": "s1", "do": "read config.toml", "tool": "fs_read", "expect": "shows the server port"},
            {"id": "s2", "do": "extract the port number from the content of config.toml", "tool": null, "after": ["s1"]},
            {"id": "s3", "do": "write the port number to ports.txt", "tool": "fs_write", "after": ["s2"]}
        ]})
        .to_string(),
    );
    // acting phase: the SLM writes the reasoning step and fills in the one argument rules cannot (the content)
    r.slm.push("8080");
    r.slm.push(&json!({"content": "{{s2}}"}).to_string());
    r.strong.push("Wrote 8080 to ports.txt.");
    let t = run(&r, GOAL).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(t.mode, Some(ExecMode::Escalated), "a medium task goes to the external model by default: {:?}", t.mode_reason);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("ports.txt")).unwrap(), "8080");

    // the planner saw a lean brief-plan prompt: one line per tool, no signatures, no arguments requested
    let plan_req = r.strong.requests.lock()[0].clone();
    assert_eq!(plan_req.system, prompts::BRIEF_PLAN_SYSTEM);
    let tools = &plan_req.messages[0].content;
    assert!(tools.contains("fs_read:") && !tools.contains("path:str*"), "{tools}");
    // the plan is intents the program implemented
    let plan = t.plan.as_ref().unwrap();
    assert_eq!(plan.source, PlanSource::Strong);
    let s = |id: &str| plan.step(id).unwrap();
    assert!(s("s1").how.as_deref().unwrap().starts_with("rules"), "path taken from the sentence: {:?}", s("s1").how);
    assert_eq!(s("s1").args, json!({"path": "config.toml"}));
    assert_eq!(s("s2").how.as_deref(), Some("reasoning"));
    assert_eq!(s("s3").how.as_deref(), Some("rules+slm"), "path by rules, content by the SLM");
    assert!(s("s1").expect.is_some() && !s("s1").brief);
    // the external model was used for the plan and the final answer only; every step ran locally
    assert_eq!(t.usage.model_calls, 2);
    assert_eq!(r.slm.calls(), 2);
    let kinds: Vec<String> =
        events(&r, &t.task_id).iter().filter_map(|e| serde_json::to_value(e).ok()).filter_map(|v| v["type"].as_str().map(String::from)).collect();
    for expect in ["phase", "step_implemented"] {
        assert!(kinds.iter().any(|k| k == expect), "{expect} in {kinds:?}");
    }
    let phases: Vec<String> =
        events(&r, &t.task_id).into_iter().filter_map(|e| if let EventKind::Phase { phase, .. } = e { Some(phase) } else { None }).collect();
    assert_eq!(phases, ["planning", "acting", "reviewing"]);
}

#[tokio::test]
async fn when_the_slm_cannot_bind_a_step_the_external_model_is_asked_for_the_arguments() {
    let r = rig(false, |_| {});
    std::fs::write(r.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    r.strong.push(
        &json!({"steps": [
            {"id": "s1", "do": "read config.toml", "tool": "fs_read"},
            {"id": "s2", "do": "write the port number found in config.toml to ports.txt", "tool": "fs_write", "after": ["s1"]}
        ]})
        .to_string(),
    );
    r.slm.push("sorry, I cannot do that"); // the SLM's two attempts are not JSON arguments
    r.slm.push("{}");
    r.strong.push(&json!({"path": "ports.txt", "content": "8080"}).to_string()); // the external model fills in the arguments (two paths in the sentence: which is which is not guessed)
    r.strong.push("Done.");
    let t = run(&r, GOAL).await;
    assert_eq!(
        t.status,
        TaskStatus::Completed,
        "{:?} / {:?} / {:?}",
        t.error,
        t.final_answer,
        t.plan.as_ref().map(|p| p.steps.iter().map(|s| (s.id.clone(), s.how.clone(), s.error.clone())).collect::<Vec<_>>())
    );
    assert_eq!(std::fs::read_to_string(r.ws.path().join("ports.txt")).unwrap(), "8080");
    assert_eq!(t.plan.as_ref().unwrap().step("s2").unwrap().how.as_deref(), Some("strong"));
    let bind = r.strong.requests.lock().iter().find(|q| q.purpose == "bind").unwrap().clone();
    assert!(bind.messages[0].content.contains("fs_write(") && !bind.messages[0].content.contains("params —"), "a lean prompt for the paid model");
}

#[tokio::test]
async fn the_decision_model_and_rules_turn_a_thinking_step_into_a_tool_call_when_it_binds_outright() {
    let r = rig(false, |_| {});
    r.strong.push(&json!({"steps": [{"id": "s1", "do": "run `echo planned`", "tool": null}]}).to_string());
    let t = run_as(
        &r,
        ModeRequest::Escalated,
        "Run the shell command `echo planned`, then verify that its output is exactly the word planned and report the result in a sentence",
    )
    .await;
    let plan = t.plan.as_ref().unwrap_or_else(|| panic!("{:?} / {:?}", t.error, t.final_answer));
    assert_eq!((plan.steps[0].tool.as_deref(), plan.steps[0].how.as_deref()), (Some("shell"), Some("rules")), "{:?}", plan.steps[0]);
    assert_eq!(r.slm.calls(), 0);
}

#[tokio::test]
async fn a_result_that_misses_the_planners_expectation_goes_back_to_the_external_model() {
    let r = rig(true, |_| {});
    std::fs::write(r.ws.path().join("config.toml"), "status = BAD\n").unwrap();
    std::fs::write(r.ws.path().join("backup.toml"), "status = fine\n").unwrap();
    r.strong.push(&json!({"steps": [{"id": "s1", "do": "read config.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    // the re-plan: the same goal with a different file
    r.strong.push(&json!({"steps": [{"id": "r1_1", "do": "read backup.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    r.strong.push("The status is fine (from backup.toml).");
    let t = run(&r, "Read config.toml and check that the status is fine, using backup.toml if the first one is not, and report which file you used").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let replan = r.strong.requests.lock().iter().find(|q| q.purpose == "replan").cloned().expect("the planner was asked to re-plan");
    assert_eq!(replan.system, prompts::BRIEF_REPLAN_SYSTEM);
    assert!(replan.messages[0].content.contains("does not meet the expectation"), "{}", replan.messages[0].content);
    assert!(t.final_answer.as_deref().unwrap().contains("status = fine"), "{:?}", t.final_answer);
}

#[tokio::test]
async fn without_an_external_model_nothing_changes_and_below_the_threshold_neither() {
    // threshold above any complexity: the self-hosted models handle it alone, with their own concrete plan
    let r = rig(false, |c| c.thresholds.external_min_complexity = 2.0);
    std::fs::write(r.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "config.toml"}}, {"id": "s2", "tool": null, "do": "say the port", "after": ["s1"]}]}).to_string());
    r.slm.push("8080");
    let t = run(&r, "Read config.toml and tell me which port the service listens on").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(t.mode, Some(ExecMode::Assisted));
    assert_eq!(r.strong.calls(), 0);
}

#[tokio::test]
async fn a_conversation_of_medium_complexity_is_answered_by_the_external_model_and_small_talk_is_not() {
    let r = rig(false, |_| {});
    let t = run(&r, "hi").await;
    assert_eq!((t.status, r.strong.calls(), r.slm.calls()), (TaskStatus::Completed, 0, 0), "small talk costs nothing");
    r.strong.push("A mutex admits one thread; a semaphore admits N. Use a mutex for exclusive access, a semaphore for pools.");
    let t = run(
        &r,
        "Explain in detail how a mutex differs from a semaphore, when to use each one, and which pitfalls such as deadlock and priority inversion to avoid",
    )
    .await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(t.mode, Some(ExecMode::Escalated), "{:?}", t.route);
    assert_eq!((r.strong.calls(), r.slm.calls()), (1, 0));
    assert!(t.final_answer.as_deref().unwrap().contains("semaphore admits N"));
}
