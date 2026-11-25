//! The self-hosted decision model against a fake OpenAI-compatible server that returns
//! token log-probabilities, the way Ollama, vLLM and llama.cpp do.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use dfae_core::*;
use dfae_decision::{Calibrator, Decider, OpenModelProvider};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    DecisionConfig {
        base_url: Some(base),
        model: "qwen3:4b-instruct-2507-q4_K_M".into(),
        api_key: Some("k1".into()),
        timeout_ms: 3000,
        parallel: 1,
        kinds: vec![],
    }
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
    assert_eq!(body["model"], "qwen3:4b-instruct-2507-q4_K_M");
    assert_eq!((body["max_tokens"].clone(), body["temperature"].clone(), body["logprobs"].clone()), (json!(1), json!(0), json!(true)));
    assert!(body["top_logprobs"].as_u64().unwrap() >= 10);
    // all questions share the same state prefix (prompt caching) and end with the question
    assert!(calls.iter().all(|(_, b)| b["messages"][1]["content"].as_str().unwrap().starts_with("REQUEST: calculate 2 + 2\n\nQUESTION:")));
    // the decision is accounted as decision-model tokens
    let e = out.entry.expect("a ledger entry");
    assert_eq!((e.component, e.prompt_tokens, e.completion_tokens), (Component::Decision, 200, 4));
    assert_eq!(e.model, "qwen3:4b-instruct-2507-q4_K_M");
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
    let app = Router::new().route("/v1/models", get(|| async { Json(json!({"data": [{"id": "qwen3:4b-instruct-2507-q4_K_M"}, {"id": "other:latest"}]})) }));
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

#[tokio::test]
async fn the_model_is_asked_about_the_best_few_tools_only() {
    let seen: Seen = Arc::default();
    let base = serve(Router::new().route("/v1/chat/completions", post(chat)).with_state(seen.clone())).await;
    let d = decider(OpenModelProvider::new(&cfg(base)).unwrap());
    let tool = |n: &str, desc: &str| ToolBrief {
        name: n.into(),
        description: desc.into(),
        category: "x".into(),
        risk: Risk::Low,
        side_effects: false,
        keywords: vec![],
    };
    let mut st = DecisionState { request: "read the file a.txt".into(), ..Default::default() };
    st.tools = vec![
        tool("fs_read", "read a file"),
        tool("fs_list", "list files in a directory"),
        tool("fs_write", "write a file"),
        tool("fs_search", "search files"),
        tool("calc", "arithmetic"),
        tool("time_now", "current time"),
        tool("system_info", "machine status"),
        tool("memory_store", "remember a fact"),
    ];
    let picked = d.select_tools(st).await;
    assert_eq!(seen.lock().unwrap().len(), 4, "one call per candidate, not per tool");
    assert!(picked.value.iter().any(|(n, _)| n == "fs_read"));
    assert!(!picked.value.iter().any(|(n, _)| n == "time_now" || n == "memory_store"), "tools nobody asked the model about keep their (low) local score");
}

#[tokio::test]
async fn decisions_the_model_does_not_make_never_reach_it() {
    let seen: Seen = Arc::default();
    let base = serve(Router::new().route("/v1/chat/completions", post(chat)).with_state(seen.clone())).await;
    let p: Arc<dyn DecisionProvider> = Arc::new(OpenModelProvider::new(&cfg(base)).unwrap());
    let d = Decider::new(Some(p), Arc::new(Calibrator::new()), Config::default().thresholds).with_model_kinds(&["route_task".to_string()]);
    d.route_task(DecisionState { request: "calculate 2+2".into(), ..Default::default() }).await;
    let before = seen.lock().unwrap().len();
    assert_eq!(before, 4);
    let m = d.select_mode(DecisionState { request: "x".into(), ..Default::default() }, ModeRequest::Auto).await;
    assert_eq!(m.record.provider, "local");
    assert_eq!(seen.lock().unwrap().len(), before);
}

#[tokio::test]
async fn after_a_failure_decisions_skip_the_model_instead_of_waiting_for_it_again() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let h = h.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, "loading")
            }
        }),
    );
    let base = serve(app).await;
    let p = OpenModelProvider::new(&cfg(base)).unwrap().with_timing(Duration::from_millis(300), Duration::from_millis(50));
    let d = decider(p);
    let st = || DecisionState { request: "list files".into(), ..Default::default() };
    assert_eq!(d.route_task(st()).await.record.provider, "local-fallback");
    let after_first = hits.load(Ordering::SeqCst);
    assert!(after_first >= 1);
    let second = d.route_task(DecisionState { request: "list the directory".into(), ..Default::default() }).await;
    assert_eq!(second.record.provider, "local-fallback");
    assert!(second.record.note.unwrap().contains("paused"), "the second decision did not wait for the broken model");
    assert_eq!(hits.load(Ordering::SeqCst), after_first, "and did not call it");
    tokio::time::sleep(Duration::from_millis(350)).await;
    d.route_task(DecisionState { request: "list the folder".into(), ..Default::default() }).await;
    assert!(hits.load(Ordering::SeqCst) > after_first, "the model is tried again once the pause is over");
}

#[tokio::test]
async fn warm_up_keeps_decisions_local_until_the_model_answers() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |State(seen): State<Seen>, hdr: HeaderMap, body: Json<Value>| {
            let n = h.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "model is loading"})));
                }
                (axum::http::StatusCode::OK, chat(State(seen), hdr, body).await)
            }
        }),
    );
    let base = serve(app.with_state(Seen::default())).await;
    let p = Arc::new(OpenModelProvider::new(&cfg(base)).unwrap().with_timing(Duration::from_secs(30), Duration::from_millis(40)));
    p.start_warmup();
    let d = Decider::new(Some(p.clone() as Arc<dyn DecisionProvider>), Arc::new(Calibrator::new()), Config::default().thresholds);
    let early = d.route_task(DecisionState { request: "calculate 3+3".into(), ..Default::default() }).await;
    assert_eq!(early.record.provider, "local-fallback");
    assert!(early.record.note.unwrap().contains("still loading"));
    let mut ready = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        if hits.load(Ordering::SeqCst) >= 3 {
            ready = true;
            break;
        }
    }
    assert!(ready, "the warm-up retried until the server answered");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let late = d.route_task(DecisionState { request: "calculate 4+4".into(), ..Default::default() }).await;
    assert_eq!(late.record.provider, "open-model");
}
