#![allow(clippy::field_reassign_with_default)]
//! Planning and acting are separate phases: the planner model writes a brief plan, and the local system
//! (rules + GLiNER, the decision model, the worker model) implements each step, escalating only when it has to.

use async_trait::async_trait;
use dfae_core::*;
use dfae_decision::{Calibrator, Decider, LocalDecider};
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
    planner: ScriptedModel,
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
            && req.state.request.starts_with("STEP:")
            && req.state.completion_facts.as_ref().map(|f| f.evidence.contains("BAD")).unwrap_or(false)
        {
            r.answers.insert("complete".into(), Answer::noul(0.05));
        }
        Ok(r)
    }
}

fn rig(picky: bool, tweak: impl FnOnce(&mut Config)) -> Rig {
    rig_with(picky, true, tweak)
}

/// A deployment with no planner model: the self-hosted one has to plan as well.
fn rig_local(picky: bool, tweak: impl FnOnce(&mut Config)) -> Rig {
    rig_with(picky, false, tweak)
}

fn rig_with(picky: bool, with_strong: bool, tweak: impl FnOnce(&mut Config)) -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    cfg.economy.plan_prefix_always = true;
    tweak(&mut cfg);
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let (worker, planner) = (ScriptedModel::new("worker", vec![]), ScriptedModel::new("planner", vec![]).caching());
    let provider: Option<Arc<dyn DecisionProvider>> = picky.then(|| Arc::new(Picky(LocalDecider::new(cfg.thresholds.clone()))) as Arc<dyn DecisionProvider>);
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(provider, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(worker.clone())), with_strong.then(|| Arc::new(planner.clone()) as Arc<dyn LanguageModel>)),
        tools: Arc::new(reg),
        policy,
    });
    Rig { engine, worker, planner, ws }
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
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "do": "read config.toml", "tool": "fs_read", "expect": "shows the server port"},
            {"id": "s2", "do": "extract the port number from the content of config.toml", "tool": null, "after": ["s1"]},
            {"id": "s3", "do": "write the port number to ports.txt", "tool": "fs_write", "after": ["s2"]}
        ]})
        .to_string(),
    );
    // acting phase: the worker model writes the reasoning step and fills in the one argument rules cannot (the content)
    r.worker.push("8080");
    r.worker.push("8080");
    // the answer is written locally and checked against the evidence; the planner model is not asked for it
    r.worker.push("Wrote 8080 to ports.txt.");
    let t = run(&r, GOAL).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(t.final_answer.as_deref(), Some("Wrote 8080 to ports.txt."));
    assert_eq!(t.mode, Some(ExecMode::Escalated), "a medium task goes to the planner model by default: {:?}", t.mode_reason);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("ports.txt")).unwrap(), "8080");

    // the planner saw the typed-plan prompt: the whole tool catalogue, no arguments requested
    let plan_req = r.planner.requests.lock()[0].clone();
    assert!(
        plan_req.system.contains("ONE JSON object") && plan_req.cache && plan_req.schema_strict && plan_req.schema.is_some(),
        "the stable rules, catalogue and examples are the cacheable system prompt"
    );
    // the planner sees EVERY tool (a decision model's per-tool judgement must not hide fs_write from it) and the typed schema names them
    let all: Vec<String> = r.engine.tools().into_iter().map(|m| m.name).collect();
    assert!(all.iter().all(|n| plan_req.system.contains(&format!("{n}("))), "every tool is in the system prompt");
    let schema = plan_req.schema.as_ref().unwrap().to_string();
    assert!(all.iter().all(|n| schema.contains(&format!("\"{n}\""))), "and in the schema's tool enum");
    let user = &plan_req.messages[0].content;
    assert!(!user.contains("LIKELY TOOLS") && !user.contains("TOOLS:"), "no per-task tool list to hide anything: {user}");
    // the plan is intents the program implemented
    let plan = t.plan.as_ref().unwrap();
    assert_eq!(plan.source, PlanSource::Planner);
    let s = |id: &str| plan.step(id).unwrap();
    assert!(s("s1").how.as_deref().unwrap().starts_with("rules"), "path taken from the sentence: {:?}", s("s1").how);
    assert_eq!(s("s1").args, json!({"path": "config.toml"}));
    assert_eq!(s("s2").how.as_deref(), Some("reasoning"));
    assert_eq!(s("s3").how.as_deref(), Some("rules+worker"), "path by rules, content by the worker model");
    assert!(s("s1").expect.is_some() && !s("s1").brief);
    // the planner model was used for the plan only; every step and the answer were done locally
    assert_eq!(t.usage.model_calls, 1);
    assert_eq!(r.worker.calls(), 3);
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
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "do": "read config.toml", "tool": "fs_read"},
            {"id": "s2", "do": "write the port number found in config.toml to ports.txt", "tool": "fs_write", "after": ["s1"]}
        ]})
        .to_string(),
    );
    r.worker.push("sorry, I cannot do that"); // the worker model's two attempts are not JSON arguments
    r.worker.push("{}");
    r.planner.push(&json!({"path": "ports.txt", "content": "8080"}).to_string()); // the planner model fills in the arguments (two paths in the sentence: which is which is not guessed)
    r.planner.push("Done.");
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
    assert_eq!(t.plan.as_ref().unwrap().step("s2").unwrap().how.as_deref(), Some("planner"));
    let bind = r.planner.requests.lock().iter().find(|q| q.purpose == "bind").unwrap().clone();
    assert!(bind.messages[0].content.contains("fs_write(") && !bind.messages[0].content.contains("params —"), "a lean prompt for the paid model");
}

#[tokio::test]
async fn a_directory_the_worker_made_up_is_corrected_while_binding_not_discovered_by_a_failed_step() {
    let r = rig(false, |_| {});
    std::fs::create_dir(r.ws.path().join("site")).unwrap();
    r.planner
        .push(&json!({"steps": [{"id": "s1", "do": "run a shell command in the site directory that shows what it contains", "tool": "shell"}]}).to_string());
    // the first answer names a directory that is nowhere (and outside the workspace); the correction names a real one
    r.worker.push(&json!({"command": "ls", "cwd": "/workspace/nowhere"}).to_string());
    r.worker.push(&json!({"command": "ls", "cwd": "site"}).to_string());
    r.planner.push("Done.");
    let t = run(&r, "show me what the site directory contains using the shell, and tell me what is there please").await;
    let step = t.plan.as_ref().unwrap().step("s1").unwrap().clone();
    assert_eq!((step.status, step.attempts), (StepStatus::Succeeded, 1), "{:?} {:?}", step.error, t.error);
    let implemented = events(&r, &t.task_id).into_iter().find_map(|e| match e {
        EventKind::StepImplemented { args, .. } => Some(args),
        _ => None,
    });
    assert!(!implemented.unwrap().contains("nowhere"), "the made-up directory never reached the tool");
}

#[tokio::test]
async fn a_file_is_written_knowing_the_files_of_its_project_that_were_written_before() {
    let r = rig(false, |_| {});
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "do": "write calc/mathlib.py with a function add(a, b)", "tool": "fs_write"},
            {"id": "s2", "do": "write calc/test_mathlib.py with pytest tests of mathlib.add", "tool": "fs_write", "after": ["s1"]}
        ]})
        .to_string(),
    );
    r.worker.push("def add(a, b):\n    return a + b\n");
    r.worker.push("from mathlib import add\n\ndef test_add():\n    assert add(1, 2) == 3\n");
    let t = run(&r, "create calc/mathlib.py with an add function and calc/test_mathlib.py with tests for it").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} {:?}", t.error, t.final_answer);
    let contents: Vec<GenerationRequest> = r.worker.requests.lock().iter().filter(|q| q.purpose == "content").cloned().collect();
    assert_eq!(contents.len(), 2);
    assert!(!contents[0].messages[0].content.contains("RELATED FILES"));
    let second = &contents[1].messages[0].content;
    assert!(second.contains("RELATED FILES") && second.contains("def add(a, b)") && second.contains("calc/mathlib.py"), "{second}");
}

#[tokio::test]
async fn a_reply_that_hands_the_work_back_is_run_as_a_task_instead() {
    // the decision model took the request for a conversation; the reply says it needs a command
    let r = oracle_rig_with_strong(vec![(DecisionKind::RouteTask, "needs_tools", Answer::noul(0.0), "zorblax")]);
    std::fs::write(r.ws.path().join("zorblax-notes.txt"), "remember the milk\n").unwrap();
    r.worker.push("That needs a look at your files, which has to be run as a task.");
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "list the files in the workspace", "tool": "fs_list"}]}).to_string());
    r.worker.push(&json!({"path": "."}).to_string());
    let t = run(&r, "give me an overview of my zorblax situation").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} {:?}", t.error, t.final_answer);
    let answer = t.final_answer.unwrap_or_default();
    assert!(answer.contains("zorblax-notes.txt") && !answer.contains("as a task"), "{answer}");
    assert_eq!(t.plan.as_ref().unwrap().source, PlanSource::Planner);
    assert!(events(&r, &t.task_id).iter().any(|e| matches!(e, EventKind::Warning { message } if message.contains("running the request as a task"))));
}

#[tokio::test]
async fn the_decision_model_and_rules_turn_a_thinking_step_into_a_tool_call_when_it_binds_outright() {
    let r = rig(false, |_| {});
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "run `echo planned`", "tool": null}]}).to_string());
    let t = run_as(
        &r,
        ModeRequest::Escalated,
        "Run the shell command `echo planned`, then verify that its output is exactly the word planned and report the result in a sentence",
    )
    .await;
    let plan = t.plan.as_ref().unwrap_or_else(|| panic!("{:?} / {:?}", t.error, t.final_answer));
    assert_eq!((plan.steps[0].tool.as_deref(), plan.steps[0].how.as_deref()), (Some("shell"), Some("rules")), "{:?}", plan.steps[0]);
    assert_eq!(r.worker.calls(), 0);
}

#[tokio::test]
async fn a_result_that_misses_the_planners_expectation_goes_back_to_the_external_model() {
    let r = rig(true, |_| {});
    std::fs::write(r.ws.path().join("config.toml"), "status = BAD\n").unwrap();
    std::fs::write(r.ws.path().join("backup.toml"), "status = fine\n").unwrap();
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "read config.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    // the re-plan: the same goal with a different file
    r.planner.push(&json!({"steps": [{"id": "r1_1", "do": "read backup.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    r.planner.push("The status is fine (from backup.toml).");
    let t = run(&r, "Read config.toml and check that the status is fine, using backup.toml if the first one is not, and report which file you used").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let replan = r.planner.requests.lock().iter().find(|q| q.purpose == "replan").cloned().expect("the planner was asked to re-plan");
    let problem = &replan.messages[0].content;
    assert!(problem.contains("Re-plan with a patch"), "the re-plan rules are in the message, so one cached system prompt serves both: {problem}");
    assert!(problem.contains("does not meet what was expected") && problem.contains("status = BAD"), "the planner is told what was actually found: {problem}");
    assert_eq!(t.plan.as_ref().unwrap().step("s1").unwrap().status, StepStatus::Succeeded, "the result is kept, not thrown away");
    // the request asks for a report: the answer is written from the evidence, not the raw file
    let a = t.final_answer.as_deref().unwrap();
    assert!(a.contains("fine") && a.contains("backup.toml"), "{a:?}");
}

#[tokio::test]
async fn without_an_external_model_the_self_hosted_ones_plan_with_a_concrete_plan() {
    let r = rig_local(false, |_| {});
    std::fs::write(r.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    r.worker.push(&json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "config.toml"}}, {"id": "s2", "tool": null, "do": "say the port", "after": ["s1"]}]}).to_string());
    r.worker.push("8080");
    let t = run(&r, "Read config.toml and tell me which port the service listens on").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(t.mode, Some(ExecMode::Assisted));
    assert_eq!(r.planner.calls(), 0);
}

#[tokio::test]
async fn a_reply_needs_no_plan_so_the_local_models_answer_it_whatever_its_complexity() {
    let r = rig(false, |_| {});
    let t = run(&r, "hi").await;
    assert_eq!((t.status, r.planner.calls(), r.worker.calls()), (TaskStatus::Completed, 0, 0), "small talk costs nothing");
    r.worker.push("A mutex admits one thread; a semaphore admits N.");
    let t = run(
        &r,
        "Explain in detail how a mutex differs from a semaphore, when to use each one, and which pitfalls such as deadlock and priority inversion to avoid",
    )
    .await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!((r.planner.calls(), r.worker.calls()), (0, 1), "planning is the planner model's job; a reply is not planned");
    assert!(t.final_answer.as_deref().unwrap().contains("semaphore admits N"));
}

#[tokio::test]
async fn a_quick_action_is_one_step_the_local_models_carry_out_without_a_plan_from_the_external_model() {
    let oracle = |a: &str| (DecisionKind::SelectMode, "mode", Answer::choice(a, 0.95, &[(a, 0.95)]), "tell me the first line of notes.txt");
    let r = oracle_rig_with_strong(vec![oracle("assisted")]);
    std::fs::write(r.ws.path().join("notes.txt"), "first line\nsecond line\n").unwrap();
    r.worker.push("The first line is: first line.");
    let t = say(&r, "q", "tell me the first line of notes.txt").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(t.plan.as_ref().unwrap().source, PlanSource::Quick);
    assert_eq!(t.plan.as_ref().unwrap().steps.len(), 2, "the lookup, and the worker model's answer from it");
    assert_eq!(r.planner.calls(), 0, "no plan was asked of the planner model");
    assert!(t.final_answer.as_deref().unwrap().contains("first line"), "{:?}", t.final_answer);
}

#[tokio::test]
async fn when_the_decision_model_says_it_needs_planning_the_external_model_plans_it() {
    let oracle = |a: &str| (DecisionKind::SelectMode, "mode", Answer::choice(a, 0.95, &[(a, 0.95)]), "tell me the first line of notes.txt");
    let r = oracle_rig_with_strong(vec![oracle("escalated")]);
    std::fs::write(r.ws.path().join("notes.txt"), "first line\nsecond line\n").unwrap();
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "read notes.txt", "tool": "fs_read"}]}).to_string());
    // the plan reads the file but has no step that states the answer: the worker writes it from the evidence
    r.worker.push("The first line is: first line");
    let t = say(&r, "p", "tell me the first line of notes.txt").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(t.plan.as_ref().unwrap().source, PlanSource::Planner);
    assert_eq!(r.planner.calls(), 1);
    assert_eq!(t.final_answer.as_deref(), Some("The first line is: first line"));
}

#[tokio::test]
async fn planning_is_never_handed_to_the_local_model_when_the_external_one_cannot_be_reached() {
    let r = rig(false, |c| c.thresholds.plan_min_complexity = 0.01);
    std::fs::write(r.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    r.planner.push_err("HTTP 502: bad gateway");
    r.planner.push_err("HTTP 502: bad gateway");
    r.worker.push(&json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "config.toml"}}]}).to_string());
    let t = run_as(&r, ModeRequest::Escalated, "Read config.toml and tell me which port the service listens on, then explain what to change to use 9090").await;
    assert_eq!(t.status, TaskStatus::Failed, "{:?}", t.final_answer);
    let e = t.error.unwrap_or_default();
    assert!(e.contains("502") && e.contains("DFAE_PLAN_FALLBACK=worker"), "{e}");
    assert_eq!(r.worker.calls(), 0, "the local model did not plan");
}

async fn say(r: &Rig, session: &str, goal: &str) -> TaskRecord {
    let id = r.engine.submit(TaskRequest { goal: goal.into(), session_id: Some(session.into()), delegate: Some(false), ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(30)).await.unwrap()
}

/// A page too big to be copied into a prompt (and certainly not through a small model's output).
fn page() -> String {
    let words: Vec<String> = (0..90).map(|i| format!("word{i}")).collect();
    let body: String = (1..=120)
        .map(|i| format!("<p>Section {i}: {} {} {} {}</p>\n", words[i % 90], words[(i * 7) % 90], words[(i * 13) % 90], words[(i * 29) % 90]))
        .collect();
    format!("<!doctype html>\n<html><body><h1>Brew & Co</h1>\n{body}</body></html>\n")
}

#[tokio::test]
async fn code_shown_in_the_chat_can_be_saved_to_a_file_by_name_without_copying_it() {
    // the page was written into the chat; "save that code to index.html" must not need it re-typed
    let r = rig_local(false, |_| {});
    r.worker.push(&format!("Here is a page:\n```html\n{}```", page()));
    let t = say(&r, "lp", "what does a minimal html page look like").await;
    assert_eq!(t.plan.as_ref().unwrap().source, PlanSource::Chat, "{:?}", t.route);

    r.worker.push(&json!({"steps": [{"id": "s1", "tool": "fs_write", "args": {"path": "index.html", "content": "{{prev_code}}"}}]}).to_string());
    let t = say(&r, "lp", "save that code to index.html").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("index.html")).unwrap(), page(), "the code block itself, verbatim");
    let prompt = r.worker.requests.lock().iter().find(|q| q.purpose == "plan").unwrap().messages[0].content.clone();
    assert!(
        prompt.contains("VALUES from the conversation") && prompt.contains("prev_code = html code block"),
        "the planner knows it can name the code: {prompt}"
    );
    assert!(!prompt.contains("Section 119"), "and the page itself is not in the prompt to be copied: {prompt}");
}

#[tokio::test]
async fn the_external_planner_and_the_local_binder_use_earlier_code_by_name_too() {
    let r = rig(false, |_| {});
    r.worker.push(&format!("Here is a page:\n```html\n{}```", page()));
    say(&r, "lp2", "what does a minimal html page look like").await;
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "write {{prev_code}} to index.html", "tool": "fs_write"}]}).to_string());
    r.worker.push(&json!({"content": "{{prev_code}}"}).to_string()); // the local binder names the value, it does not copy it
    r.planner.push("Saved to index.html.");
    let t = say(&r, "lp2", "Save that html code into index.html so the site has a real front page, and check it is there").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("index.html")).unwrap(), page());
    let bind = r.worker.requests.lock().iter().find(|q| q.purpose == "bind").unwrap().messages[0].content.clone();
    assert!(bind.contains("{{prev_code}} = html code block") && bind.contains("STILL MISSING") && !bind.contains("REJECTED"), "{bind}");
    let plan_prompt = r.planner.requests.lock()[0].messages[0].content.clone();
    assert!(plan_prompt.contains("VALUES from the conversation"), "{plan_prompt}");
}

/// A decision model that understands what the fallback rules cannot: answers given for requests containing a phrase.
struct Oracle {
    local: LocalDecider,
    answers: Vec<(DecisionKind, &'static str, Answer, &'static str)>,
}

#[async_trait]
impl DecisionProvider for Oracle {
    fn name(&self) -> &str {
        "open-model"
    }
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let mut r = self.local.decide(req.clone()).await?;
        for (kind, key, answer, phrase) in &self.answers {
            if *kind == req.kind && req.state.request.contains(phrase) {
                r.answers.insert((*key).into(), answer.clone());
            }
        }
        Ok(r)
    }
}

fn oracle_rig(answers: Vec<(DecisionKind, &'static str, Answer, &'static str)>) -> Rig {
    let mut r = rig_local(false, |_| {});
    let local = LocalDecider::new(Config::default().thresholds);
    let decider = Decider::new(Some(Arc::new(Oracle { local, answers })), Arc::new(Calibrator::new()), Config::default().thresholds);
    // same engine, different decision model
    let ws = r.ws.path().to_path_buf();
    let mut cfg = Config::default();
    cfg.workspace = ws.clone();
    cfg.approvals = ApprovalMode::Never;
    cfg.thresholds.plan_min_complexity = 2.0;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    r.engine = Engine::new(EngineDeps {
        decider: Arc::new(decider),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(r.worker.clone())), None),
        tools: Arc::new(reg),
        policy,
    });
    r
}

/// The oracle rig, with an planner model configured.
fn oracle_rig_with_strong(answers: Vec<(DecisionKind, &'static str, Answer, &'static str)>) -> Rig {
    let r = oracle_rig(answers.clone());
    let local = LocalDecider::new(Config::default().thresholds);
    let decider = Decider::new(Some(Arc::new(Oracle { local, answers })), Arc::new(Calibrator::new()), Config::default().thresholds);
    let ws = r.ws.path().to_path_buf();
    let mut cfg = Config::default();
    cfg.workspace = ws.clone();
    cfg.approvals = ApprovalMode::Never;
    cfg.economy.plan_prefix_always = true;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let planner = r.planner.clone().caching();
    Rig {
        engine: Engine::new(EngineDeps {
            decider: Arc::new(decider),
            extractor: Arc::new(HybridExtractor::new(None)),
            config: cfg,
            store,
            models: ModelGateway::new(Some(Arc::new(r.worker.clone())), Some(Arc::new(planner.clone()))),
            tools: Arc::new(reg),
            policy,
        }),
        worker: r.worker,
        planner,
        ws: r.ws,
    }
}

#[tokio::test]
async fn the_decision_model_not_a_phrase_list_decides_what_a_request_refers_to() {
    let r = oracle_rig(vec![(DecisionKind::ResolveReference, "refers_to", Answer::choice("file", 0.95, &[("file", 0.95)]), "the thing we just made")]);
    std::fs::write(r.ws.path().join("test.txt"), "hello world !").unwrap();
    say(&r, "ref", "create a file named test.txt with content \"hello world !\"").await;
    // no "it", no "that file": only a model can tell that "the thing we just made" is the file
    let t = say(&r, "ref", "show me what is inside the thing we just made").await;
    assert_eq!(t.goal, "show me what is inside the thing we just made (test.txt)");
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("hello world !"));
    let decisions: Vec<String> = t.decisions.iter().map(|d| d.kind.clone()).collect();
    assert!(decisions.contains(&"resolve_reference".to_string()), "{decisions:?}");
    // with nothing earlier to refer to the model is not even asked
    let fresh = say(&r, "other", "show me what is inside the thing we just made").await;
    assert!(!fresh.decisions.iter().any(|d| d.kind == "resolve_reference"));
}

#[tokio::test]
async fn the_decision_model_recognises_small_talk_in_words_no_list_contains() {
    let r = oracle_rig(vec![(DecisionKind::RouteTask, "smalltalk", Answer::choice("greeting", 0.95, &[("greeting", 0.95)]), "howdy partner")]);
    let t = say(&r, "talk", "howdy partner, lovely day isn't it").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert!(t.final_answer.as_deref().unwrap().starts_with("Hi!"), "{:?}", t.final_answer);
    assert_eq!((r.worker.calls(), r.planner.calls()), (0, 0));
}

#[tokio::test]
async fn the_decision_model_says_a_request_changes_something_even_when_it_uses_no_change_verb() {
    let r = oracle_rig(vec![(DecisionKind::RouteTask, "changes_state", Answer::noul(0.95), "turn every occurrence")]);
    std::fs::write(r.ws.path().join("test.txt"), "hello world !").unwrap();
    // the planner only reads; the routing says the request changes something, so the plan is sent back naming the tool
    r.worker.push(
        &json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "test.txt"}}, {"id": "s2", "tool": null, "do": "confirm", "after": ["s1"]}]})
            .to_string(),
    );
    r.worker
        .push(&json!({"steps": [{"id": "s1", "tool": "fs_edit", "args": {"path": "test.txt", "old_string": "world", "new_string": "keyhan"}}]}).to_string());
    let t = say(&r, "chg", "turn every occurrence of world in test.txt into keyhan").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("test.txt")).unwrap(), "hello keyhan !");
    assert!(t.route.as_ref().unwrap().changes);
}

#[tokio::test]
async fn an_expectation_about_files_is_checked_by_code_and_a_miss_goes_back_to_the_planner() {
    let r = rig(false, |_| {});
    // the planner expects b.txt, the step writes a.txt: no model is needed to see that
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "write hello into a.txt", "tool": "fs_write", "expect": "b.txt exists"}]}).to_string());
    r.planner.push(
        &json!({"steps": [{"id": "r1_1", "do": "write hello into b.txt", "tool": "fs_write", "expect": "b.txt exists and the output is not empty"}]})
            .to_string(),
    );
    r.worker.push(&json!({"path": "a.txt", "content": "hello"}).to_string());
    r.worker.push(&json!({"path": "b.txt", "content": "hello"}).to_string());
    r.worker.push("Wrote hello into b.txt.");
    let t = run(&r, "Write hello into a file and make sure it is called b.txt").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let replan = r.planner.requests.lock().iter().find(|q| q.purpose == "replan").cloned().expect("the miss sent the planner back to the plan");
    assert!(replan.messages[0].content.contains("b.txt exists") || replan.messages[0].content.contains("file exists: b.txt"), "{}", replan.messages[0].content);
    assert!(r.ws.path().join("b.txt").exists());
    // no decision model, and the worker model was not asked to judge anything
    assert!(r.worker.requests.lock().iter().all(|q| q.purpose != "judge"));
}

#[tokio::test]
async fn a_file_cut_off_by_the_token_limit_is_caught_by_a_parser_and_rewritten_locally() {
    let r = rig(false, |_| {});
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "write data.json with the settings", "tool": "fs_write"}]}).to_string());
    // the file's content is written as plain text; the first version is cut off (a code fence around it is tolerated)
    r.worker.push("```json\n{\"settings\": {\"port\": 80\n```");
    // asked again with the parser's complaint, the local model gets it right
    r.worker.push("{\"settings\": {\"port\": 80}}");
    let t = run(&r, "Write the settings into data.json").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("data.json")).unwrap(), "{\"settings\": {\"port\": 80}}");
    // the second bind request carried the complaint; the planner model was asked for the plan only
    let binds: Vec<String> = r.worker.requests.lock().iter().filter(|q| q.purpose == "content").map(|q| q.messages[0].content.clone()).collect();
    assert!(binds.len() == 2 && binds[1].contains("REJECTED") && binds[1].contains("invalid JSON"), "{binds:?}");
    assert_eq!(r.planner.calls(), 1, "no re-plan: a parser error is the local model's to fix");
}

#[tokio::test]
async fn work_the_external_model_plans_is_one_plan_not_a_team_of_planners() {
    let r = rig(false, |c| c.thresholds.plan_min_complexity = 0.01);
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "do": "read a.txt", "tool": "fs_read"},
            {"id": "s2", "do": "summarise what a.txt says", "tool": null, "after": ["s1"]},
            {"id": "s3", "do": "read b.txt", "tool": "fs_read"},
            {"id": "s4", "do": "summarise what b.txt says", "tool": null, "after": ["s3"]}
        ]})
        .to_string(),
    );
    std::fs::write(r.ws.path().join("a.txt"), "a mutex is a lock").unwrap();
    std::fs::write(r.ws.path().join("b.txt"), "a semaphore counts permits").unwrap();
    r.worker.push("A mutex is a lock.");
    r.worker.push("A semaphore counts permits.");
    r.worker.push("A mutex is a lock; a semaphore counts permits.");
    // delegation is left on (the default): the decomposition exists, but each agent would pay for a plan of its own
    let id = r.engine.submit(TaskRequest { goal: "Read a.txt and summarise it, and read b.txt and summarise it".into(), ..Default::default() }).await.unwrap();
    let t = r.engine.wait(&id, Duration::from_secs(30)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.team.is_none(), "no team: {:?}", t.team);
    assert_eq!(r.planner.calls(), 1, "one planner call for the whole request");
}

#[tokio::test]
async fn a_reasoning_step_that_misses_its_expectation_is_rewritten_locally_before_the_planner_hears_of_it() {
    let r = rig(true, |_| {});
    std::fs::write(r.ws.path().join("brief.txt"), "A small coffee roaster that sells beans by subscription.").unwrap();
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "do": "read brief.txt", "tool": "fs_read"},
            {"id": "s2", "do": "come up with a short product tagline from the brief", "tool": null, "after": ["s1"], "expect": "a tagline"}
        ]})
        .to_string(),
    );
    r.worker.push("BAD tagline: coffee");
    r.worker.push("Roasted slow, poured fast.");
    let t = run_as(&r, ModeRequest::Escalated, "Read brief.txt and come up with a short product tagline from it").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("Roasted slow"), "{:?}", t.final_answer);
    let steps: Vec<String> = r.worker.requests.lock().iter().filter(|q| q.purpose == "step").map(|q| q.messages[0].content.clone()).collect();
    assert_eq!(steps.len(), 2, "the step was written twice");
    assert!(steps[1].contains("PREVIOUS ANSWER WAS REJECTED"), "{}", steps[1]);
    assert_eq!(r.planner.calls(), 1, "the planner was not bothered");
}

#[tokio::test]
async fn a_plan_of_intents_becomes_a_skill_that_costs_no_planner_tokens_the_next_time() {
    let r = rig(false, |c| c.thresholds.plan_min_complexity = 0.01);
    std::fs::create_dir_all(r.ws.path().join("notes")).unwrap();
    std::fs::write(r.ws.path().join("notes/alpha.md"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(r.ws.path().join("notes/beta.md"), "one\ntwo\n").unwrap();
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "do": "read notes/alpha.md", "tool": "fs_read"},
            {"id": "s2", "do": "count the lines of the file and state the number", "tool": null, "after": ["s1"]}
        ]})
        .to_string(),
    );
    r.worker.push("The file has 3 lines.");
    let t1 = run(&r, "Read notes/alpha.md and tell me how many lines it has").await;
    assert_eq!(t1.status, TaskStatus::Completed, "{:?} / {:?}", t1.error, t1.final_answer);
    assert_eq!(r.planner.calls(), 1);
    // the same request about another file: the skill is replayed as intents, the local system writes the content afresh
    r.worker.push("The file has 2 lines.");
    let t2 = run(&r, "Read notes/beta.md and tell me how many lines it has").await;
    assert_eq!(t2.status, TaskStatus::Completed, "{:?} / {:?}", t2.error, t2.final_answer);
    assert_eq!(t2.plan.as_ref().unwrap().source, PlanSource::Skill, "{:?}", t2.mode_reason);
    assert_eq!(r.planner.calls(), 1, "no external call for the replay");
    assert!(t2.final_answer.as_deref().unwrap().contains("2 lines"), "{:?}", t2.final_answer);
    assert_eq!(t2.plan.as_ref().unwrap().step("s1").unwrap().args["path"], "notes/beta.md");
}

#[tokio::test]
async fn a_task_that_went_wrong_and_then_worked_leaves_a_lesson_that_is_remembered() {
    let r = rig(true, |_| {});
    std::fs::write(r.ws.path().join("config.toml"), "status = BAD\n").unwrap();
    std::fs::write(r.ws.path().join("backup.toml"), "status = fine\n").unwrap();
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "read config.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    r.planner.push(&json!({"steps": [{"id": "r1_1", "do": "read backup.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    r.planner.push("The status is fine (from backup.toml).");
    let t = run(&r, "Read config.toml and check that the status is fine, using backup.toml if the first one is not, and report which file you used").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let lessons = r
        .engine
        .store()
        .retrieve(MemoryQuery { text: "check that the status is fine in config.toml".into(), kinds: vec![MemoryKind::Semantic], limit: 5, session_id: None })
        .await
        .unwrap();
    let lesson = lessons
        .iter()
        .find(|m| m.item.text.starts_with("Lesson:"))
        .unwrap_or_else(|| panic!("no lesson in {:?}", lessons.iter().map(|m| &m.item.text).collect::<Vec<_>>()));
    assert!(lesson.item.text.contains("backup.toml") && lesson.item.text.contains("status = BAD"), "{}", lesson.item.text);
    // and the next planner sees it: the first plan of a similar request starts from what worked
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "read backup.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    r.planner.push("The status is fine (from backup.toml).");
    let before = r.planner.calls();
    run(&r, "Read config.toml and check that the status is fine, using backup.toml if the first one is not, and report which file you used").await;
    let next_plan = r.planner.requests.lock()[before].clone();
    assert!(next_plan.messages[0].content.contains("Lesson:"), "{}", next_plan.messages[0].content);
}

#[tokio::test]
async fn a_user_who_says_the_result_was_wrong_teaches_the_system_something() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let lp = log.path().to_path_buf();
    let r = rig(false, move |c| c.decision_log = Some(lp));
    std::fs::write(r.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    let first = say(&r, "chat-1", "Read config.toml").await;
    assert_eq!(first.status, TaskStatus::Completed, "{:?}", first.error);
    r.worker.push("Sorry, let me look again.");
    let second = say(&r, "chat-1", "That's wrong, try again").await;
    assert_eq!(second.route.as_ref().and_then(|x| x.feedback.as_deref()), Some("negative"), "{:?}", second.route);
    let lessons = r
        .engine
        .store()
        .retrieve(MemoryQuery { text: "Read config.toml did not satisfy the user".into(), kinds: vec![MemoryKind::Semantic], limit: 5, session_id: None })
        .await
        .unwrap();
    assert!(
        lessons.iter().any(|m| m.item.text.contains("did not satisfy the user") && m.item.text.contains("try again")),
        "{:?}",
        lessons.iter().map(|m| &m.item.text).collect::<Vec<_>>()
    );
    // the decisions behind the first answer are exported with the user's verdict, ready to train or evaluate with
    let logged = std::fs::read_to_string(log.path()).unwrap();
    assert!(logged.lines().any(|l| l.contains("rejected_by_user") && l.contains("Read config.toml")), "{logged}");
    assert!(logged.lines().any(|l| l.contains("\"outcome\":\"completed\"")), "{logged}");
}

#[tokio::test]
async fn shadow_evaluation_measures_the_decision_model_against_the_external_one_when_asked() {
    let r = rig(false, |c| c.shadow_eval = 1.0);
    std::fs::write(r.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    r.planner.push(
        &json!({"task_type": "file_operations", "needs_tools": "yes", "changes_state": "no", "conditional_change": "no", "risk": "low", "smalltalk": "none", "feedback": "none", "role": "researcher"})
            .to_string(),
    );
    r.planner.push(
        &json!({"steps": [{"id": "s1", "do": "read config.toml", "tool": "fs_read"}, {"id": "s2", "do": "state the port", "tool": null, "after": ["s1"]}]})
            .to_string(),
    );
    r.worker.push("The port is 8080.");
    let t = run_as(&r, ModeRequest::Escalated, "Read config.toml and tell me the port").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let s = r.engine.stats().await;
    assert_eq!(s.shadow_requests, 1);
    assert!(s.shadow_compared >= 6 && s.shadow_agree >= 5, "agreed on {} of {}", s.shadow_agree, s.shadow_compared);
    assert!(t.ledger.summary().planner_by_purpose.get("shadow_eval").copied().unwrap_or(0) > 0, "the evaluation is visible as what it is");
    // off by default: the same request costs the planner model nothing for it
    let r2 = rig(false, |_| {});
    std::fs::write(r2.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    r2.planner.push(
        &json!({"steps": [{"id": "s1", "do": "read config.toml", "tool": "fs_read"}, {"id": "s2", "do": "state the port", "tool": null, "after": ["s1"]}]})
            .to_string(),
    );
    r2.worker.push("The port is 8080.");
    run_as(&r2, ModeRequest::Escalated, "Read config.toml and tell me the port").await;
    assert_eq!(r2.engine.stats().await.shadow_requests, 0);
}

#[tokio::test]
async fn a_step_that_comes_back_as_a_question_for_the_user_is_told_to_decide_and_do_the_work() {
    let r = rig(false, |_| {});
    std::fs::write(r.ws.path().join("brief.txt"), "A planet page.").unwrap();
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "do": "read brief.txt", "tool": "fs_read"},
            {"id": "s2", "do": "compose the text of a landing page about Earth from the brief", "tool": null, "after": ["s1"]}
        ]})
        .to_string(),
    );
    r.worker.push("To create a landing page about Earth, I'll need some content and design details. What information do you want to include?");
    r.worker.push("Earth: our home planet. (Assumed a short intro.)");
    let t = run_as(&r, ModeRequest::Escalated, "Read brief.txt and compose the text of a landing page about Earth from it").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("home planet"), "{:?}", t.final_answer);
    let second = r.worker.requests.lock().iter().filter(|q| q.purpose == "step").nth(1).unwrap().messages[0].content.clone();
    assert!(second.contains("ASKED THE USER A QUESTION"), "{second}");
}

#[tokio::test]
async fn two_files_in_one_plan_each_get_their_own_path_and_content_never_a_step_output_as_a_path() {
    let r = rig(false, |_| {});
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "tool": "fs_write", "do": "write README.md, a short README for Brewster, a tool that tracks coffee orders: a title and one paragraph", "after": [], "expect": null, "planner": false},
            {"id": "s2", "tool": "fs_write", "do": "write .gitignore for a Python project (pycache, virtualenvs, build output)", "after": [], "expect": null, "planner": false}
        ]})
        .to_string(),
    );
    r.worker.push("# Brewster\n\nBrewster tracks coffee orders.");
    r.worker.push("__pycache__/\n.venv/\nbuild/\n");
    let t = run_as(
        &r,
        ModeRequest::Escalated,
        "Create README.md with a short description of Brewster, a tool that tracks coffee orders, and a .gitignore for a Python project",
    )
    .await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(std::fs::read_to_string(r.ws.path().join("README.md")).unwrap().contains("Brewster"));
    assert!(std::fs::read_to_string(r.ws.path().join(".gitignore")).unwrap().contains("__pycache__"));
    // both contents were written as plain text, from the step's own sentence
    let asks: Vec<String> = r.worker.requests.lock().iter().filter(|q| q.purpose == "content").map(|q| q.messages[0].content.clone()).collect();
    assert_eq!(asks.len(), 2, "{asks:?}");
    assert!(asks[0].starts_with("FILE TO WRITE (write only this one): README.md") && asks[1].contains(": .gitignore"), "{asks:?}");
    assert!(!asks[1].contains("BACKGROUND"), "a step with a full sentence needs no help from the whole request");
}

#[tokio::test]
async fn a_page_the_model_cuts_off_is_written_again_shorter() {
    let r = rig(false, |_| {});
    r.planner.push(&json!({"steps": [{"id": "s1", "tool": "fs_write", "do": "write index.html, a landing page about Earth with a title and three sections", "after": [], "expect": null, "planner": false}]}).to_string());
    r.worker.push("<!doctype html><html><head><style>body{margin:0}</style></head><body><div class=\"hero\"><h1>Earth</h1>");
    r.worker.push("<!doctype html><html><head><style>body{margin:0}</style></head><body><h1>Earth</h1><p>Our home.</p></body></html>");
    let t = run_as(&r, ModeRequest::Escalated, "Create index.html, a landing page about Earth with a title and three sections").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(std::fs::read_to_string(r.ws.path().join("index.html")).unwrap().ends_with("</html>"));
    let asks: Vec<String> = r.worker.requests.lock().iter().filter(|q| q.purpose == "content").map(|q| q.messages[0].content.clone()).collect();
    assert_eq!(asks.len(), 2);
    assert!(asks[1].contains("REJECTED") && asks[1].contains("cut off"), "{}", asks[1]);
}

#[tokio::test]
async fn a_step_the_worker_cannot_carry_out_is_edited_by_the_planner_and_only_that_step_runs_again() {
    let r = rig(false, |_| {});
    std::fs::write(r.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "tool": "fs_write", "do": "write notes.txt with the single line: checked", "after": [], "expect": null, "planner": false},
            {"id": "s2", "tool": "fs_read", "do": "read settings.toml", "after": [], "expect": null, "planner": false},
            {"id": "s3", "tool": null, "do": "say which port the file sets", "after": ["s2"], "expect": null, "planner": false}
        ]})
        .to_string(),
    );
    // the patch: only step s2 changes (a different file); s1 is done and s3 is untouched
    r.planner.push(
        &json!({"edit": [{"id": "s2", "tool": "fs_read", "do": "read config.toml", "after": [], "expect": null, "planner": false}], "add": [], "drop": []})
            .to_string(),
    );
    r.worker.push("checked");
    r.worker.push("The file sets port 8080.");
    let t = run_as(&r, ModeRequest::Escalated, "Write checked into notes.txt, then read settings.toml and tell me which port it sets").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let plan = t.plan.as_ref().unwrap();
    assert_eq!(plan.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["s1", "s2", "s3"], "no step was replaced, one was edited");
    assert_eq!(plan.step("s2").unwrap().description, "read config.toml");
    assert_eq!(plan.revision, 1);
    assert!(plan.steps.iter().all(|s| s.status == StepStatus::Succeeded));
    // s1 ran once; the planner was asked twice (plan, patch) and only for the patch's problem
    assert_eq!(r.worker.requests.lock().iter().filter(|q| q.purpose == "content").count(), 1);
    let patch_req = r.planner.requests.lock().iter().find(|q| q.purpose == "replan").cloned().unwrap();
    let ask = &patch_req.messages[0].content;
    assert!(ask.contains("s1 DONE") && ask.contains("s2 FAILED") && ask.contains("s3 TODO") && ask.contains("settings.toml"), "{ask}");
    assert_eq!(patch_req.schema.as_ref().unwrap()["required"], json!(["edit", "add", "drop"]));
}

#[tokio::test]
async fn a_plan_with_slips_a_model_makes_is_repaired_by_code_before_anyone_is_asked_again() {
    let r = rig(false, |_| {});
    std::fs::write(r.ws.path().join("a.txt"), "hello").unwrap();
    // a misspelt tool, a numeric id, a dependency on a step that does not exist, the same id twice
    r.planner.push(
        &json!({"plan": [
            {"id": 1, "tool": "FS-Read", "do": "read a.txt", "after": ["7"]},
            {"id": 2, "tool": "none", "intent": "say what the file says", "depends_on": "1"},
            {"id": 2, "tool": "none", "do": "say it again, shorter", "after": ["2"]}
        ]})
        .to_string(),
    );
    r.worker.push("It says hello.");
    r.worker.push("hello");
    let t = run_as(&r, ModeRequest::Escalated, "Read a.txt and say what it says, then say it again shorter").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(r.planner.calls(), 1, "no repair round: code repaired it");
    let notes: Vec<String> =
        events(&r, &t.task_id).into_iter().filter_map(|e| if let EventKind::Repaired { note, .. } = e { Some(note) } else { None }).collect();
    assert!(
        notes.iter().any(|n| n.contains("fs_read")) && notes.iter().any(|n| n.contains("used twice")) && notes.iter().any(|n| n.contains("does not exist")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_worker_model_that_cannot_be_reached_does_not_stop_the_task_the_planner_does_the_call() {
    let r = rig(false, |_| {});
    std::fs::write(r.ws.path().join("a.txt"), "hello").unwrap();
    r.planner.push(
        &json!({"steps": [
            {"id": "s1", "tool": "fs_read", "do": "read a.txt", "after": [], "expect": null, "planner": false},
            {"id": "s2", "tool": null, "do": "say what the file says", "after": ["s1"], "expect": null, "planner": false}
        ]})
        .to_string(),
    );
    r.worker.push_err("connection refused");
    r.planner.push("It says hello.");
    let t = run_as(&r, ModeRequest::Escalated, "Read a.txt and say what it says").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("hello"));
    let warns: Vec<String> =
        events(&r, &t.task_id).into_iter().filter_map(|e| if let EventKind::Warning { message } = e { Some(message) } else { None }).collect();
    assert!(warns.iter().any(|m| m.contains("worker model is unavailable")), "{warns:?}");
}

#[tokio::test]
async fn a_request_the_routing_calls_a_conversation_but_a_tool_plainly_serves_is_handled_as_a_task() {
    // the decision model says "no tools needed" for a request about a file's content: the tool-relevance question disagrees
    let r = oracle_rig_with_strong(vec![
        (DecisionKind::RouteTask, "needs_tools", Answer::noul(0.05), "show me what is inside notes.txt"),
        (DecisionKind::SelectMode, "mode", Answer::choice("assisted", 0.95, &[("assisted", 0.95)]), "show me what is inside notes.txt"),
    ]);
    std::fs::write(r.ws.path().join("notes.txt"), "first line\nsecond line\n").unwrap();
    r.worker.push("It starts with: first line.");
    let t = say(&r, "c", "show me what is inside notes.txt").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(t.plan.as_ref().unwrap().source, PlanSource::Quick, "not a chat reply: {:?}", t.plan.as_ref().map(|p| p.source));
    assert!(t.plan.as_ref().unwrap().steps[0].tool.is_some(), "a tool step, not a reply");
}

#[tokio::test]
async fn a_plain_reply_that_is_a_tool_call_in_disguise_is_never_shown_to_the_user() {
    let r = rig(false, |_| {});
    r.worker.push("<think>they want the price</think></think><tool_call>Tool: web.search\n{\"query\": \"bitcoin price\"}");
    r.worker.push("I can't look that up from here.");
    let t = say(&r, "tc", "tell me a short fact about the moon").await;
    let a = t.final_answer.clone().unwrap_or_default();
    assert!(!a.contains("tool_call") && !a.contains("Tool:") && !a.contains("think"), "{a}");
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {a}", t.error);
    assert!(a.contains("can't look that up"), "{a}");
}

#[tokio::test]
async fn an_answer_that_depends_on_what_the_world_looks_like_now_is_looked_up_never_chatted() {
    // "find the price of bitcoin for now": the decision model says the answer changes over time, so it is a task whatever else it says
    let r = oracle_rig_with_strong(vec![
        (DecisionKind::RouteTask, "needs_tools", Answer::noul(0.05), "find the price of bitcoin for now"),
        (DecisionKind::RouteTask, "fresh_data", Answer::noul(0.95), "find the price of bitcoin for now"),
        (DecisionKind::SelectMode, "mode", Answer::choice("assisted", 0.95, &[("assisted", 0.95)]), "find the price of bitcoin for now"),
    ]);
    r.worker.push(r#"{"query": "bitcoin price usd"}"#);
    let t = say(&r, "fresh", "find the price of bitcoin for now").await;
    let route = t.route.clone().unwrap();
    assert!(route.fresh_data && route.needs_tools, "{route:?}");
    let plan = t.plan.as_ref().expect("a plan");
    assert_eq!(plan.source, PlanSource::Quick, "not a chat reply");
    assert_eq!(plan.steps[0].tool.as_deref(), Some("web_search"), "{:?}", plan.steps);
}

#[tokio::test]
async fn every_plan_request_shares_one_cacheable_prefix() {
    // the system prompt and the schema are the provider's cached prefix: they must not change from task to task (a tool more or less,
    // a step limit in a description, ... each is a full cache miss)
    let r = rig(true, |_| {});
    std::fs::write(r.ws.path().join("a.txt"), "alpha\n").unwrap();
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "read a.txt", "tool": "fs_read"}]}).to_string());
    r.planner.push(&json!({"steps": [{"id": "s1", "do": "create the directory out", "tool": "fs_mkdir"}]}).to_string());
    run_as(&r, ModeRequest::Escalated, "Read a.txt").await;
    run_as(&r, ModeRequest::Escalated, "Create a directory called out").await;
    let reqs: Vec<GenerationRequest> = r.planner.requests.lock().iter().filter(|q| q.purpose.starts_with("plan")).cloned().collect();
    assert!(reqs.len() >= 2, "{} plan requests", reqs.len());
    assert_eq!(reqs[0].system, reqs[1].system, "one system prompt for every task");
    assert_eq!(reqs[0].schema, reqs[1].schema, "one schema for every task");
    assert!(!reqs[0].schema.as_ref().unwrap().to_string().contains("at most 12"), "no per-task limit inside the schema");
}
