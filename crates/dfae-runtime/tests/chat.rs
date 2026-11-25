#![allow(clippy::field_reassign_with_default)]
//! Conversation, sessions and the first-touch behaviours users hit: greetings, "read it", file
//! creation phrasings, weather in "my city", a planner that names the wrong tool.

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
    ws: tempfile::TempDir,
}

fn rig() -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.allow_auto_approve = true;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let slm = ScriptedModel::new("scripted-slm", vec![]);
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(Decider::new(None, Arc::new(Calibrator::new()), cfg.thresholds.clone())),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(slm.clone())), None),
        tools: Arc::new(reg),
        policy,
    });
    Rig { engine, slm, ws }
}

async fn say(r: &Rig, session: &str, goal: &str) -> TaskRecord {
    let id = r.engine.submit(TaskRequest { goal: goal.into(), session_id: Some(session.into()), auto_approve: true, ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(20)).await.unwrap()
}

#[tokio::test]
async fn greetings_and_small_talk_are_answered_without_any_model() {
    let r = rig();
    for (msg, expect) in [("hi", "Hi!"), ("hi. tell me how are you ?", "oing well"), ("thanks!", "welcome"), ("what can you do?", "tools")] {
        let t = say(&r, "s", msg).await;
        assert_eq!(t.status, TaskStatus::Completed, "{msg}: {:?}", t.error);
        assert!(t.final_answer.as_deref().unwrap().contains(expect), "{msg}: {:?}", t.final_answer);
        assert_eq!(t.plan.as_ref().unwrap().source, PlanSource::Chat);
        assert_eq!(t.ledger.summary().generative_tokens, 0);
    }
    assert_eq!(r.slm.calls(), 0);
}

#[tokio::test]
async fn a_conversation_is_one_model_call_per_turn_with_the_history_attached() {
    let r = rig();
    r.slm.push("A mutex is a lock that lets one thread at a time into a critical section.");
    r.slm.push("Yes: a semaphore generalises it to N holders.");
    let t1 = say(&r, "c", "explain what a mutex is").await;
    assert!(!t1.final_answer.as_deref().unwrap().contains("Not found in the collected evidence"), "chat replies are not evidence-checked");
    assert_eq!(t1.status, TaskStatus::Completed, "{:?}", t1.error);
    assert_eq!(t1.plan.as_ref().unwrap().source, PlanSource::Chat, "no tool is needed, so there is no planning call");
    assert_eq!(r.slm.calls(), 1);
    let t2 = say(&r, "c", "and is a semaphore similar?").await;
    assert_eq!(t2.status, TaskStatus::Completed, "{:?}", t2.error);
    assert_eq!(r.slm.calls(), 2);
    let req = r.slm.requests.lock()[1].clone();
    assert_eq!(req.system, dfae_runtime::prompts::CHAT_SYSTEM);
    assert_eq!(req.messages.len(), 3, "previous question, previous answer, new message");
    assert!(req.messages[0].content.contains("mutex") && req.messages[1].content.contains("critical section"));
    assert!(req.messages[2].content.contains("semaphore"));
    // another session does not see it
    r.slm.push("ok");
    say(&r, "other", "and is a semaphore similar?").await;
    assert_eq!(r.slm.requests.lock()[2].messages.len(), 1);
}

#[tokio::test]
async fn file_creation_phrasings_run_directly_and_keep_the_content_verbatim() {
    let r = rig();
    let t = say(&r, "f", r#"create a file named test.txt with content "hello keyhan !" in ./ path"#).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(t.mode, Some(ExecMode::Direct));
    assert_eq!(std::fs::read_to_string(r.ws.path().join("test.txt")).unwrap(), "hello keyhan !");
    let t = say(&r, "f", r#"hi. create a file named "second.txt" with content 'hello world !' "#).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("second.txt")).unwrap(), "hello world !");
    assert_eq!(r.slm.calls(), 0, "no model was needed for either");
}

#[tokio::test]
async fn it_and_that_file_mean_what_the_conversation_was_about() {
    let r = rig();
    say(&r, "p", r#"create a file named test.txt with content "hello keyhan !""#).await;
    let t = say(&r, "p", "read it").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(t.goal, "read test.txt", "the request was made self-contained before routing");
    assert!(t.final_answer.as_deref().unwrap().contains("hello keyhan !"), "{:?}", t.final_answer);
    assert_eq!(r.slm.calls(), 0);
    // a different session has no idea what "it" is
    let t = say(&r, "q", "read it").await;
    assert_eq!(t.goal, "read it");
}

#[tokio::test]
async fn weather_is_found_through_the_web_tools_with_the_place_from_the_conversation() {
    let r = rig();
    r.slm
        .push(&json!({"steps": [{"id": "s1", "tool": "http_get", "do": "current weather", "args": {"url": "https://wttr.in/new+york?format=3"}}]}).to_string());
    let t = say(&r, "w", "hi. i'm keyhan from new york. tell me weather temperature in my city.").await;
    assert_eq!(t.goal, "hi. i'm keyhan from new york. tell me weather temperature in new york.", "'my city' was resolved before planning");
    let prompt = r.slm.requests.lock()[0].messages.last().unwrap().content.clone();
    assert!(prompt.contains("http_get(") && prompt.contains("wttr.in"), "the planner is told how to get weather from the web: {prompt}");
    assert!(prompt.contains("web_search("), "{prompt}");
    let plan = t.plan.expect("a plan was made");
    assert_eq!(plan.steps[0].tool.as_deref(), Some("http_get"), "{:?}", plan.steps);
    assert_eq!(plan.source, PlanSource::Slm);
}

#[tokio::test]
async fn a_planner_that_forgets_the_write_is_told_which_tool_to_use() {
    let r = rig();
    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_list", "do": "list the directory", "args": {"path": "."}}]}).to_string());
    r.slm.push(
        &json!({"steps": [{"id": "s1", "tool": "fs_write", "do": "write the file", "args": {"path": "note.txt", "content": "hello there"}}]}).to_string(),
    );
    let t = say(&r, "e", "please create note.txt containing hello there").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("note.txt")).unwrap(), "hello there");
    let repair = r.slm.requests.lock()[1].messages.last().unwrap().content.clone();
    assert!(repair.contains("fs_write"), "the repair prompt names the missing tool: {repair}");
}

#[tokio::test]
async fn a_harmless_tool_the_selector_missed_is_admitted_not_fatal() {
    let r = rig();
    std::fs::write(r.ws.path().join("a.txt"), "alpha").unwrap();
    // "read a.txt and then compare it" selects fs_read; the model also lists the directory
    r.slm.push(
        &json!({"steps": [
            {"id": "s1", "tool": "fs_read", "args": {"path": "a.txt"}},
            {"id": "s2", "tool": "fs_list", "args": {"path": "."}},
            {"id": "s3", "tool": null, "do": "say what the file holds", "after": ["s1", "s2"]}
        ]})
        .to_string(),
    );
    r.slm.push("It holds alpha.");
    let t = say(&r, "a", "Read a.txt and tell me what it holds").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(r.slm.calls(), 2, "plan + step: no repair round was needed");
}

#[tokio::test]
async fn create_read_replace_read_changes_the_file_without_any_model() {
    let r = rig();
    let t = say(&r, "e2e", r#"create a file named "test.txt" that has this content : "hello world !" "#).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    let t = say(&r, "e2e", "now read that file").await;
    assert_eq!(t.final_answer.as_deref(), Some("hello world !"));
    let t = say(&r, "e2e", r#"ok. now replace the word "world" with "keyhan" in that file."#).await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let plan = t.plan.unwrap();
    assert_eq!((plan.source, plan.steps[0].tool.as_deref()), (PlanSource::Direct, Some("fs_edit")), "{:?}", plan.steps);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("test.txt")).unwrap(), "hello keyhan !");
    let t = say(&r, "e2e", "read it again").await;
    assert_eq!(t.final_answer.as_deref(), Some("hello keyhan !"));
    assert_eq!(r.slm.calls(), 0);
}

#[tokio::test]
async fn a_plan_that_only_reads_cannot_pass_for_an_edit() {
    let r = rig();
    std::fs::write(r.ws.path().join("test.txt"), "hello world !").unwrap();
    // a phrasing nothing binds directly; the model first "plans" only a read and a reassuring sentence
    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "test.txt"}}, {"id": "s2", "tool": null, "do": "confirm the replacement", "after": ["s1"]}]}).to_string());
    r.slm.push(&json!({"steps": [{"id": "s1", "tool": "fs_edit", "args": {"path": "test.txt", "old_string": "world", "new_string": "keyhan"}}]}).to_string());
    let t = say(&r, "lazy", "could you swap the word world for keyhan wherever it appears inside test.txt").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("test.txt")).unwrap(), "hello keyhan !", "the file really changed");
    let repair = r.slm.requests.lock()[1].messages.last().unwrap().content.clone();
    assert!(repair.contains("no step does it") && repair.contains("fs_edit"), "{repair}");
}

#[tokio::test]
async fn a_reply_cannot_claim_a_change_nothing_made() {
    let r = rig();
    std::fs::write(r.ws.path().join("test.txt"), "hello world !").unwrap();
    // both attempts are read-only: the second is accepted (a plan merely less ambitious is still a plan)...
    let lazy =
        json!({"steps": [{"id": "s1", "tool": "fs_read", "args": {"path": "test.txt"}}, {"id": "s2", "tool": null, "do": "say it is done", "after": ["s1"]}]})
            .to_string();
    r.slm.push(&lazy);
    r.slm.push(&lazy);
    r.slm.push("Done, I replaced it.");
    let t = say(&r, "claim", "could you swap the word world for keyhan wherever it appears inside test.txt").await;
    // ...but the answer says what happened
    assert!(t.final_answer.as_deref().unwrap().contains("Nothing was changed"), "{:?}", t.final_answer);
    assert_eq!(std::fs::read_to_string(r.ws.path().join("test.txt")).unwrap(), "hello world !");
}
