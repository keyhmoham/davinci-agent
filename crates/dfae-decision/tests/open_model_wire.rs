//! The self-hosted decision model against a fake OpenAI-compatible server that returns
//! token log-probabilities, the way Ollama, vLLM and llama.cpp do.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use dfae_core::*;
use dfae_decision::{Calibrator, Decider, OpenModelProvider};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}/v1")
}

fn top(alts: &[(&str, f64)]) -> Value {
    json!(alts.iter().map(|(t, p)| json!({"token": t, "logprob": p.ln()})).collect::<Vec<_>>())
}

/// A pretend model: calculation-looking requests get task_type=calculation, everything else "yes".
async fn chat(State(seen): State<Seen>, h: HeaderMap, Json(b): Json<Value>) -> Json<Value> {
    seen.lock().unwrap().push((h.get("authorization").map(|v| v.to_str().unwrap().to_string()), b.clone()));
    let prompt = b["messages"][1]["content"].as_str().unwrap().to_string();
    let alts = if prompt.ends_with("Allowed answers: yes, no") {
        top(&[("Yes", 0.8), ("No", 0.15), ("The", 0.05)])
    } else if prompt.ends_with("Allowed answers: 1, 2, 3") {
        top(&[("1", 0.2), ("2", 0.7), ("3", 0.1)])
    } else {
        // a lettered choice: pick the line that mentions arithmetic, else A
        let pick = prompt.lines().find(|l| l.contains("calculation") || l.contains("direct")).and_then(|l| l.chars().next()).unwrap_or('A');
        let other = if pick == 'A' { "B" } else { "A" };
        top(&[(&pick.to_string(), 0.9), (other, 0.1)])
    };
    Json(json!({
        "model": "fake-qwen",
        "choices": [{"message": {"role": "assistant", "content": "x"}, "logprobs": {"content": [{"token": "x", "logprob": -0.1, "top_logprobs": alts}]}}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 1}
    }))
}

fn cfg(base: String) -> DecisionConfig {
    DecisionConfig { base_url: Some(base), model: "qwen3:4b-instruct-2507-q8_0".into(), api_key: Some("k1".into()), timeout_ms: 3000, parallel: 3 }
}

fn decider(p: OpenModelProvider) -> Decider {
    Decider::new(Some(Arc::new(p)), Arc::new(Calibrator::new()), Config::default().thresholds)
}

#[tokio::test]
async fn typed_answers_come_from_the_probability_mass_of_single_tokens() {
    let seen: Seen = Arc::default();
    let base = serve(Router::new().route("/v1/chat/completions", post(chat)).with_state(seen.clone())).await;
    let d = decider(OpenModelProvider::new(&cfg(base)).unwrap());
    let st = DecisionState { request: "calculate 2 + 2".into(), ..Default::default() };
    let out = d.route_task(st).await;
    assert_eq!(out.record.provider, "open-model");
    assert_eq!(out.value.task_type, "calculation");
    assert!(out.value.needs_tools, "P(yes) = 0.8 / 0.95");
    assert!((out.value.complexity - 0.5).abs() < 0.2, "expected level ~1.0 of 0..2 → {}", out.value.complexity);

    // one single-token call per question, with the documented request shape
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 4, "task_type, complexity, needs_tools, risk");
    let (auth, body) = &calls[0];
    assert_eq!(auth.as_deref(), Some("Bearer k1"));
    assert_eq!(body["model"], "qwen3:4b-instruct-2507-q8_0");
    assert_eq!((body["max_tokens"].clone(), body["temperature"].clone(), body["logprobs"].clone()), (json!(1), json!(0), json!(true)));
    assert!(body["top_logprobs"].as_u64().unwrap() >= 10);
    // all questions share the same state prefix (prompt caching) and end with the question
    assert!(calls.iter().all(|(_, b)| b["messages"][1]["content"].as_str().unwrap().starts_with("REQUEST: calculate 2 + 2\n\nQUESTION:")));
    // the decision is accounted as decision-model tokens
    let e = out.entry.expect("a ledger entry");
    assert_eq!((e.component, e.prompt_tokens, e.completion_tokens), (Component::Decision, 200, 4));
    assert_eq!(e.model, "qwen3:4b-instruct-2507-q8_0");
}

#[tokio::test]
async fn a_server_without_logprobs_falls_back_to_the_local_rules() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Json(json!({"choices": [{"message": {"role": "assistant", "content": "yes"}}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}}))
        }),
    );
    let base = serve(app).await;
    let d = decider(OpenModelProvider::new(&cfg(base)).unwrap());
    let out = d.route_task(DecisionState { request: "calculate 1+1".into(), ..Default::default() }).await;
    assert_eq!(out.record.provider, "local-fallback");
    assert!(out.record.note.unwrap().contains("log-probabilities"));
    assert_eq!(out.value.task_type, "calculation", "the local rules still decide");
}

#[tokio::test]
async fn an_unreachable_model_never_blocks_the_engine() {
    let d = decider(OpenModelProvider::new(&cfg("http://127.0.0.1:1/v1".into())).unwrap());
    let out = d.route_task(DecisionState { request: "list files".into(), ..Default::default() }).await;
    assert_eq!(out.record.provider, "local-fallback");
    assert_eq!(d.health().await, Some(false));
}

#[tokio::test]
async fn health_requires_the_model_to_be_pulled() {
    let app = Router::new().route("/v1/models", get(|| async { Json(json!({"data": [{"id": "qwen3:4b-instruct-2507-q8_0"}, {"id": "other:latest"}]})) }));
    let base = serve(app).await;
    let d = decider(OpenModelProvider::new(&cfg(base.clone())).unwrap());
    assert_eq!(d.health().await, Some(true));
    let mut c = cfg(base);
    c.model = "missing:1b".into();
    assert_eq!(decider(OpenModelProvider::new(&c).unwrap()).health().await, Some(false));
    assert_eq!(Decider::new(None, Arc::new(Calibrator::new()), Config::default().thresholds).health().await, None, "local rules need no health check");
}

#[tokio::test]
async fn tool_selection_asks_one_yes_no_question_per_tool() {
    let seen: Seen = Arc::default();
    let base = serve(Router::new().route("/v1/chat/completions", post(chat)).with_state(seen.clone())).await;
    let d = decider(OpenModelProvider::new(&cfg(base)).unwrap());
    let tool =
        |n: &str| ToolBrief { name: n.into(), description: format!("{n} tool"), category: "x".into(), risk: Risk::Low, side_effects: false, keywords: vec![] };
    let mut st = DecisionState { request: "read a.txt".into(), ..Default::default() };
    st.tools = vec![tool("fs_read"), tool("calc"), tool("http_get")];
    let picked = d.select_tools(st).await;
    assert_eq!(picked.value.len(), 3, "the fake model says yes (0.84) to every tool");
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert!(seen.lock().unwrap().iter().any(|(_, b)| b["messages"][1]["content"].as_str().unwrap().contains("Is the tool `calc`")));
}
