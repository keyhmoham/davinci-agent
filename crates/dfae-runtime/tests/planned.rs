#![allow(clippy::field_reassign_with_default)]
//! Planning and acting are separate phases: the external model writes a brief plan, and the local system
//! (rules + GLiNER, the decision model, the SLM) implements each step, escalating only when it has to.

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
            && req.state.request.starts_with("STEP:")
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
    cfg.economy.plan_prefix_always = true;
    tweak(&mut cfg);
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let (slm, strong) = (ScriptedModel::new("slm", vec![]), ScriptedModel::new("strong", vec![]).caching());
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
    // the answer is written locally and checked against the evidence; the external model is not asked for it
    r.slm.push("Wrote 8080 to ports.txt.");
    let t = run(&r, GOAL).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(t.final_answer.as_deref(), Some("Wrote 8080 to ports.txt."));
    assert_eq!(t.mode, Some(ExecMode::Escalated), "a medium task goes to the external model by default: {:?}", t.mode_reason);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("ports.txt")).unwrap(), "8080");

    // the planner saw a lean brief-plan prompt: one line per tool, no signatures, no arguments requested
    let plan_req = r.strong.requests.lock()[0].clone();
    assert!(plan_req.system.contains("plan lines") && plan_req.cache, "the stable rules, catalogue and examples are the cacheable system prompt");
    assert!(plan_req.system.contains("fs_read(path") && plan_req.system.contains("fs_edit("), "the whole catalogue is in the prefix");
    let tools = &plan_req.messages[0].content;
    assert!(tools.contains("LIKELY TOOLS:") && tools.contains("fs_read") && !tools.contains("path:str*"), "{tools}");
    // the plan is intents the program implemented
    let plan = t.plan.as_ref().unwrap();
    assert_eq!(plan.source, PlanSource::Strong);
    let s = |id: &str| plan.step(id).unwrap();
    assert!(s("s1").how.as_deref().unwrap().starts_with("rules"), "path taken from the sentence: {:?}", s("s1").how);
    assert_eq!(s("s1").args, json!({"path": "config.toml"}));
    assert_eq!(s("s2").how.as_deref(), Some("reasoning"));
    assert_eq!(s("s3").how.as_deref(), Some("rules+slm"), "path by rules, content by the SLM");
    assert!(s("s1").expect.is_some() && !s("s1").brief);
    // the external model was used for the plan only; every step and the answer were done locally
    assert_eq!(t.usage.model_calls, 1);
    assert_eq!(r.slm.calls(), 3);
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
    let problem = &replan.messages[0].content;
    assert!(problem.contains("Re-plan:"), "the re-plan rules are in the message, so one cached system prompt serves both: {problem}");
    assert!(problem.contains("does not meet what was expected") && problem.contains("status = BAD"), "the planner is told what was actually found: {problem}");
    assert_eq!(t.plan.as_ref().unwrap().step("s1").unwrap().status, StepStatus::Succeeded, "the result is kept, not thrown away");
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
    let r = rig(false, |c| c.thresholds.external_min_complexity = 2.0);
    r.slm.push(&format!("Here is a page:\n```html\n{}```", page()));
    let t = say(&r, "lp", "what does a minimal html page look like").await;
    assert_eq!(t.plan.as_ref().unwrap().source, PlanSource::Chat, "{:?}", t.route);

    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_write", "args": {"path": "index.html", "content": "{{prev_code}}"}}]}).to_string());
    let t = say(&r, "lp", "save that code to index.html").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("index.html")).unwrap(), page(), "the code block itself, verbatim");
    let prompt = r.slm.requests.lock().iter().find(|q| q.purpose == "plan").unwrap().messages[0].content.clone();
    assert!(
        prompt.contains("VALUES from the conversation") && prompt.contains("prev_code = html code block"),
        "the planner knows it can name the code: {prompt}"
    );
    assert!(!prompt.contains("Section 119"), "and the page itself is not in the prompt to be copied: {prompt}");
}

#[tokio::test]
async fn the_external_planner_and_the_local_binder_use_earlier_code_by_name_too() {
    let r = rig(false, |_| {});
    r.slm.push(&format!("Here is a page:\n```html\n{}```", page()));
    say(&r, "lp2", "what does a minimal html page look like").await;
    r.strong.push(&json!({"steps": [{"id": "s1", "do": "write {{prev_code}} to index.html", "tool": "fs_write"}]}).to_string());
    r.slm.push(&json!({"content": "{{prev_code}}"}).to_string()); // the local binder names the value, it does not copy it
    r.strong.push("Saved to index.html.");
    let t = say(&r, "lp2", "Save that html code into index.html so the site has a real front page, and check it is there").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("index.html")).unwrap(), page());
    let bind = r.slm.requests.lock().iter().find(|q| q.purpose == "bind").unwrap().messages[0].content.clone();
    assert!(bind.contains("{{prev_code}} = html code block") && bind.contains("STILL MISSING") && !bind.contains("REJECTED"), "{bind}");
    let plan_prompt = r.strong.requests.lock()[0].messages[0].content.clone();
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
    let mut r = rig(false, |c| c.thresholds.external_min_complexity = 2.0);
    let local = LocalDecider::new(Config::default().thresholds);
    let decider = Decider::new(Some(Arc::new(Oracle { local, answers })), Arc::new(Calibrator::new()), Config::default().thresholds);
    // same engine, different decision model
    let ws = r.ws.path().to_path_buf();
    let mut cfg = Config::default();
    cfg.workspace = ws.clone();
    cfg.approvals = ApprovalMode::Never;
    cfg.thresholds.external_min_complexity = 2.0;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    r.engine = Engine::new(EngineDeps {
        decider: Arc::new(decider),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(r.slm.clone())), Some(Arc::new(r.strong.clone()))),
        tools: Arc::new(reg),
        policy,
    });
    r
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
    assert_eq!((r.slm.calls(), r.strong.calls()), (0, 0));
}

#[tokio::test]
async fn the_decision_model_says_a_request_changes_something_even_when_it_uses_no_change_verb() {
    let r = oracle_rig(vec![(DecisionKind::RouteTask, "changes_state", Answer::noul(0.95), "turn every occurrence")]);
    std::fs::write(r.ws.path().join("test.txt"), "hello world !").unwrap();
    // the planner only reads; the routing says the request changes something, so the plan is sent back naming the tool
    r.slm.push(
        &json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "test.txt"}}, {"id": "s2", "tool": null, "do": "confirm", "after": ["s1"]}]})
            .to_string(),
    );
    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_edit", "args": {"path": "test.txt", "old_string": "world", "new_string": "keyhan"}}]}).to_string());
    let t = say(&r, "chg", "turn every occurrence of world in test.txt into keyhan").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("test.txt")).unwrap(), "hello keyhan !");
    assert!(t.route.as_ref().unwrap().changes);
}

#[tokio::test]
async fn an_expectation_about_files_is_checked_by_code_and_a_miss_goes_back_to_the_planner() {
    let r = rig(false, |_| {});
    // the planner expects b.txt, the step writes a.txt: no model is needed to see that
    r.strong.push(&json!({"steps": [{"id": "s1", "do": "write hello into a.txt", "tool": "fs_write", "expect": "b.txt exists"}]}).to_string());
    r.strong.push(
        &json!({"steps": [{"id": "r1_1", "do": "write hello into b.txt", "tool": "fs_write", "expect": "b.txt exists and the output is not empty"}]})
            .to_string(),
    );
    r.slm.push(&json!({"path": "a.txt", "content": "hello"}).to_string());
    r.slm.push(&json!({"path": "b.txt", "content": "hello"}).to_string());
    r.slm.push("Wrote hello into b.txt.");
    let t = run(&r, "Write hello into a file and make sure it is called b.txt").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let replan = r.strong.requests.lock().iter().find(|q| q.purpose == "replan").cloned().expect("the miss sent the planner back to the plan");
    assert!(replan.messages[0].content.contains("b.txt exists") || replan.messages[0].content.contains("file exists: b.txt"), "{}", replan.messages[0].content);
    assert!(r.ws.path().join("b.txt").exists());
    // no decision model, and the SLM was not asked to judge anything
    assert!(r.slm.requests.lock().iter().all(|q| q.purpose != "judge"));
}

#[tokio::test]
async fn a_file_cut_off_by_the_token_limit_is_caught_by_a_parser_and_rewritten_locally() {
    let r = rig(false, |_| {});
    r.strong.push(&json!({"steps": [{"id": "s1", "do": "write data.json with the settings", "tool": "fs_write"}]}).to_string());
    r.slm.push(&json!({"path": "data.json", "content": "{\"settings\": {\"port\": 80"}).to_string());
    // asked again with the parser's complaint, the local model gets it right
    r.slm.push(&json!({"path": "data.json", "content": "{\"settings\": {\"port\": 80}}"}).to_string());
    let t = run(&r, "Write the settings into data.json").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("data.json")).unwrap(), "{\"settings\": {\"port\": 80}}");
    // the second bind request carried the complaint; the external model was asked for the plan only
    let binds: Vec<String> = r.slm.requests.lock().iter().filter(|q| q.purpose == "bind").map(|q| q.messages[0].content.clone()).collect();
    assert!(binds.len() >= 2 && binds[1].contains("malformed") && binds[1].contains("invalid JSON"), "{binds:?}");
    assert_eq!(r.strong.calls(), 1, "no re-plan: a parser error is the local model's to fix");
}

#[tokio::test]
async fn work_the_external_model_plans_is_one_plan_not_a_team_of_planners() {
    let r = rig(false, |c| c.thresholds.external_min_complexity = 0.01);
    r.strong.push(
        &json!({"steps": [
            {"id": "s1", "do": "explain what a mutex is in two sentences", "tool": null},
            {"id": "s2", "do": "explain what a semaphore is in two sentences", "tool": null}
        ]})
        .to_string(),
    );
    r.slm.push("A mutex is a lock.");
    r.slm.push("A semaphore counts permits.");
    r.slm.push("A mutex is a lock; a semaphore counts permits.");
    // delegation is left on (the default): the decomposition exists, but each agent would pay for a plan of its own
    let id = r.engine.submit(TaskRequest { goal: "Explain what a mutex is, and explain what a semaphore is".into(), ..Default::default() }).await.unwrap();
    let t = r.engine.wait(&id, Duration::from_secs(30)).await.unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.team.is_none(), "no team: {:?}", t.team);
    assert_eq!(r.strong.calls(), 1, "one planner call for the whole request");
}

#[tokio::test]
async fn a_reasoning_step_that_misses_its_expectation_is_rewritten_locally_before_the_planner_hears_of_it() {
    let r = rig(true, |_| {});
    std::fs::write(r.ws.path().join("brief.txt"), "A small coffee roaster that sells beans by subscription.").unwrap();
    r.strong.push(
        &json!({"steps": [
            {"id": "s1", "do": "read brief.txt", "tool": "fs_read"},
            {"id": "s2", "do": "come up with a short product tagline from the brief", "tool": null, "after": ["s1"], "expect": "a tagline"}
        ]})
        .to_string(),
    );
    r.slm.push("BAD tagline: coffee");
    r.slm.push("Roasted slow, poured fast.");
    let t = run_as(&r, ModeRequest::Escalated, "Read brief.txt and come up with a short product tagline from it").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("Roasted slow"), "{:?}", t.final_answer);
    let steps: Vec<String> = r.slm.requests.lock().iter().filter(|q| q.purpose == "step").map(|q| q.messages[0].content.clone()).collect();
    assert_eq!(steps.len(), 2, "the step was written twice");
    assert!(steps[1].contains("PREVIOUS ANSWER WAS REJECTED"), "{}", steps[1]);
    assert_eq!(r.strong.calls(), 1, "the planner was not bothered");
}

#[tokio::test]
async fn a_plan_of_intents_becomes_a_skill_that_costs_no_external_tokens_the_next_time() {
    let r = rig(false, |c| c.thresholds.external_min_complexity = 0.01);
    std::fs::create_dir_all(r.ws.path().join("notes")).unwrap();
    std::fs::write(r.ws.path().join("notes/alpha.md"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(r.ws.path().join("notes/beta.md"), "one\ntwo\n").unwrap();
    r.strong.push(
        &json!({"steps": [
            {"id": "s1", "do": "read notes/alpha.md", "tool": "fs_read"},
            {"id": "s2", "do": "count the lines of the file and state the number", "tool": null, "after": ["s1"]}
        ]})
        .to_string(),
    );
    r.slm.push("The file has 3 lines.");
    let t1 = run(&r, "Read notes/alpha.md and tell me how many lines it has").await;
    assert_eq!(t1.status, TaskStatus::Completed, "{:?} / {:?}", t1.error, t1.final_answer);
    assert_eq!(r.strong.calls(), 1);
    // the same request about another file: the skill is replayed as intents, the local system writes the content afresh
    r.slm.push("The file has 2 lines.");
    let t2 = run(&r, "Read notes/beta.md and tell me how many lines it has").await;
    assert_eq!(t2.status, TaskStatus::Completed, "{:?} / {:?}", t2.error, t2.final_answer);
    assert_eq!(t2.plan.as_ref().unwrap().source, PlanSource::Skill, "{:?}", t2.mode_reason);
    assert_eq!(r.strong.calls(), 1, "no external call for the replay");
    assert!(t2.final_answer.as_deref().unwrap().contains("2 lines"), "{:?}", t2.final_answer);
    assert_eq!(t2.plan.as_ref().unwrap().step("s1").unwrap().args["path"], "notes/beta.md");
}

#[tokio::test]
async fn a_task_that_went_wrong_and_then_worked_leaves_a_lesson_that_is_remembered() {
    let r = rig(true, |_| {});
    std::fs::write(r.ws.path().join("config.toml"), "status = BAD\n").unwrap();
    std::fs::write(r.ws.path().join("backup.toml"), "status = fine\n").unwrap();
    r.strong.push(&json!({"steps": [{"id": "s1", "do": "read config.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    r.strong.push(&json!({"steps": [{"id": "r1_1", "do": "read backup.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    r.strong.push("The status is fine (from backup.toml).");
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
    r.strong.push(&json!({"steps": [{"id": "s1", "do": "read backup.toml", "tool": "fs_read", "expect": "the status is fine"}]}).to_string());
    r.strong.push("The status is fine (from backup.toml).");
    let before = r.strong.calls();
    run(&r, "Read config.toml and check that the status is fine, using backup.toml if the first one is not, and report which file you used").await;
    let next_plan = r.strong.requests.lock()[before].clone();
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
    r.slm.push("Sorry, let me look again.");
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
    r.strong.push(
        &json!({"task_type": "file_operations", "needs_tools": "yes", "changes_state": "no", "conditional_change": "no", "risk": "low", "smalltalk": "none", "feedback": "none", "role": "researcher"})
            .to_string(),
    );
    r.strong.push(
        &json!({"steps": [{"id": "s1", "do": "read config.toml", "tool": "fs_read"}, {"id": "s2", "do": "state the port", "tool": null, "after": ["s1"]}]})
            .to_string(),
    );
    r.slm.push("The port is 8080.");
    let t = run_as(&r, ModeRequest::Escalated, "Read config.toml and tell me the port").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let s = r.engine.stats().await;
    assert_eq!(s.shadow_requests, 1);
    assert!(s.shadow_compared >= 6 && s.shadow_agree >= 5, "agreed on {} of {}", s.shadow_agree, s.shadow_compared);
    assert!(t.ledger.summary().external_by_purpose.get("shadow_eval").copied().unwrap_or(0) > 0, "the evaluation is visible as what it is");
    // off by default: the same request costs the external model nothing for it
    let r2 = rig(false, |_| {});
    std::fs::write(r2.ws.path().join("config.toml"), "port = 8080\n").unwrap();
    r2.strong.push(
        &json!({"steps": [{"id": "s1", "do": "read config.toml", "tool": "fs_read"}, {"id": "s2", "do": "state the port", "tool": null, "after": ["s1"]}]})
            .to_string(),
    );
    r2.slm.push("The port is 8080.");
    run_as(&r2, ModeRequest::Escalated, "Read config.toml and tell me the port").await;
    assert_eq!(r2.engine.stats().await.shadow_requests, 0);
}

#[tokio::test]
async fn a_step_that_comes_back_as_a_question_for_the_user_is_told_to_decide_and_do_the_work() {
    let r = rig(false, |_| {});
    std::fs::write(r.ws.path().join("brief.txt"), "A planet page.").unwrap();
    r.strong.push(
        &json!({"steps": [
            {"id": "s1", "do": "read brief.txt", "tool": "fs_read"},
            {"id": "s2", "do": "compose the text of a landing page about Earth from the brief", "tool": null, "after": ["s1"]}
        ]})
        .to_string(),
    );
    r.slm.push("To create a landing page about Earth, I'll need some content and design details. What information do you want to include?");
    r.slm.push("Earth: our home planet. (Assumed a short intro.)");
    let t = run_as(&r, ModeRequest::Escalated, "Read brief.txt and compose the text of a landing page about Earth from it").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("home planet"), "{:?}", t.final_answer);
    let second = r.slm.requests.lock().iter().filter(|q| q.purpose == "step").nth(1).unwrap().messages[0].content.clone();
    assert!(second.contains("ASKED THE USER A QUESTION"), "{second}");
}
