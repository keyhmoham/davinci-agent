//! The Jev adapter against a fake `/v1/decisions` server that follows the
//! documented contract (typed `noul` / `choice` / `score` answers).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use dfae_core::*;
use dfae_decision::{Calibrator, Decider, JevProvider};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}/v1/decisions")
}

type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

/// Answers every question with a valid answer of the right type.
async fn good(State((seen, hits)): State<(Seen, Arc<AtomicUsize>)>, h: HeaderMap, Json(b): Json<Value>) -> Json<Value> {
    hits.fetch_add(1, Ordering::SeqCst);
    seen.lock().unwrap().push((h.get("authorization").map(|v| v.to_str().unwrap().to_string()), b.clone()));
    let mut answers = serde_json::Map::new();
    for (k, q) in b["questions"].as_object().unwrap() {
        let a = match q["type"].as_str().unwrap() {
            "noul" => json!({"type": "noul", "noul": 0.9}),
            "choice" => {
                let first = q["criteria"].as_object().unwrap().keys().next().unwrap().clone();
                let preferred = if q["criteria"].get("direct").is_some() {
                    "direct".to_string()
                } else if q["criteria"].get("calculation").is_some() {
                    "calculation".to_string()
                } else {
                    first
                };
                json!({"type": "choice", "choice": preferred, "confidence": 0.97, "probabilities": {preferred.clone(): 0.97}})
            }
            _ => {
                json!({"type": "score", "score": 1.2, "confidence": 0.7, "legend": {"0": "a", "1": "b", "2": "c"}, "probabilities": {"0": 0.1, "1": 0.7, "2": 0.2}})
            }
        };
        answers.insert(k.clone(), a);
    }
    Json(json!({"model": "jev-1.14.0", "answers": answers, "usage": {"input_tokens": 406, "output_tokens": 73}}))
}

fn cfg(url: String) -> JevConfig {
    JevConfig { api_key: Some("jev-key".into()), url, model: "typesafe/jev-1.14".into(), timeout_ms: 2000 }
}

#[tokio::test]
async fn sends_the_documented_request_and_parses_typed_answers() {
    let seen: Seen = Arc::default();
    let hits = Arc::new(AtomicUsize::new(0));
    let url = serve(Router::new().route("/v1/decisions", post(good)).with_state((seen.clone(), hits.clone()))).await;
    let jev = JevProvider::new(&cfg(url)).unwrap();
    let decider = Decider::new(Some(Arc::new(jev)), Arc::new(Calibrator::new()), Config::default().thresholds);
    let out = decider.route_task(DecisionState { request: "calculate 2+2".into(), ..Default::default() }).await;
    assert_eq!(out.record.provider, "jev");
    assert_eq!(out.value.task_type, "calculation");
    assert!((out.value.complexity - 0.6).abs() < 0.01, "score 1.2 of 0..2 → 0.6, got {}", out.value.complexity);
    assert!(out.value.needs_tools);
    let e = out.entry.expect("a hosted call is recorded in the ledger");
    assert_eq!((e.component, e.prompt_tokens, e.completion_tokens), (Component::Jev, 406, 73));

    let (auth, body) = seen.lock().unwrap()[0].clone();
    assert_eq!(auth.as_deref(), Some("Bearer jev-key"));
    assert_eq!(body["model"], "typesafe/jev-1.14");
    assert_eq!(body["state"]["request"], "calculate 2+2");
    for (k, ty) in [("task_type", "choice"), ("complexity", "score"), ("needs_tools", "noul"), ("risk", "choice")] {
        assert_eq!(body["questions"][k]["type"], ty, "{k}");
        assert!(body["questions"][k]["instructions"].is_string());
    }
    assert_eq!(body["questions"]["complexity"]["criteria"].as_array().unwrap().len(), 3);
    // tool keywords are local-only and must not leak into the hosted request
    assert!(body["state"].get("tools").is_none());
}

#[tokio::test]
async fn identical_decisions_are_cached_and_cost_nothing() {
    let seen: Seen = Arc::default();
    let hits = Arc::new(AtomicUsize::new(0));
    let url = serve(Router::new().route("/v1/decisions", post(good)).with_state((seen, hits.clone()))).await;
    let decider = Decider::new(Some(Arc::new(JevProvider::new(&cfg(url)).unwrap())), Arc::new(Calibrator::new()), Config::default().thresholds);
    let st = DecisionState { request: "what time is it".into(), ..Default::default() };
    decider.route_task(st.clone()).await;
    let second = decider.route_task(st).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let e = second.entry.unwrap();
    assert!(e.cached && e.prompt_tokens == 0);
}

#[tokio::test]
async fn http_errors_and_malformed_answers_fall_back_to_local_rules() {
    // 500s
    let url = serve(Router::new().route("/v1/decisions", post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }))).await;
    let d = Decider::new(Some(Arc::new(JevProvider::new(&cfg(url)).unwrap())), Arc::new(Calibrator::new()), Config::default().thresholds);
    let out = d.route_task(DecisionState { request: "calculate 1+1".into(), ..Default::default() }).await;
    assert_eq!(out.record.provider, "local-fallback");
    assert!(out.record.note.unwrap().contains("500"));
    assert!(out.entry.is_none());

    // an answer of the wrong type for the question is rejected, not trusted
    let bad = Router::new().route(
        "/v1/decisions",
        post(|Json(b): Json<Value>| async move {
            let answers: serde_json::Map<String, Value> =
                b["questions"].as_object().unwrap().keys().map(|k| (k.clone(), json!({"type": "noul", "noul": 0.5}))).collect();
            Json(json!({"model": "x", "answers": answers, "usage": {"input_tokens": 1, "output_tokens": 1}}))
        }),
    );
    let d = Decider::new(Some(Arc::new(JevProvider::new(&cfg(serve(bad).await)).unwrap())), Arc::new(Calibrator::new()), Config::default().thresholds);
    let out = d.route_task(DecisionState { request: "list files".into(), ..Default::default() }).await;
    assert_eq!(out.record.provider, "local-fallback");

    // a choice outside the offered options is rejected too
    let bad = Router::new().route(
        "/v1/decisions",
        post(|Json(b): Json<Value>| async move {
            let answers: serde_json::Map<String, Value> = b["questions"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, q)| {
                    let a = match q["type"].as_str().unwrap() {
                        "noul" => json!({"type": "noul", "noul": 0.5}),
                        "choice" => json!({"type": "choice", "choice": "made_up", "confidence": 0.9, "probabilities": {}}),
                        _ => json!({"type": "score", "score": 1.0, "confidence": 0.5, "probabilities": {}}),
                    };
                    (k.clone(), a)
                })
                .collect();
            Json(json!({"model": "x", "answers": answers}))
        }),
    );
    let d = Decider::new(Some(Arc::new(JevProvider::new(&cfg(serve(bad).await)).unwrap())), Arc::new(Calibrator::new()), Config::default().thresholds);
    assert_eq!(d.route_task(DecisionState { request: "x".into(), ..Default::default() }).await.record.provider, "local-fallback");
}

#[tokio::test]
async fn unreachable_provider_falls_back() {
    let d = Decider::new(
        Some(Arc::new(JevProvider::new(&cfg("http://127.0.0.1:1/v1/decisions".into())).unwrap())),
        Arc::new(Calibrator::new()),
        Config::default().thresholds,
    );
    let out = d.route_task(DecisionState { request: "calculate 3*3".into(), ..Default::default() }).await;
    assert_eq!(out.record.provider, "local-fallback");
    assert_eq!(out.value.task_type, "calculation");
}

#[tokio::test]
async fn a_provider_cannot_pick_direct_mode_without_a_binding() {
    let seen: Seen = Arc::default();
    let url = serve(Router::new().route("/v1/decisions", post(good)).with_state((seen, Arc::default()))).await;
    let d = Decider::new(Some(Arc::new(JevProvider::new(&cfg(url)).unwrap())), Arc::new(Calibrator::new()), Config::default().thresholds);
    // the fake always answers "direct" (confidence 0.97)
    let mut st = DecisionState { request: "do something creative".into(), ..Default::default() };
    st.mode_facts = Some(ModeFacts { direct_bindable: false, slm_available: true, ..Default::default() });
    let m = d.select_mode(st.clone(), ModeRequest::Auto).await.value;
    assert_eq!(m.raw_choice, "direct");
    assert_eq!(m.mode, ExecMode::Assisted, "hard rule: no binding, no direct mode");
    st.mode_facts = Some(ModeFacts { direct_bindable: true, slm_available: true, ..Default::default() });
    assert_eq!(d.select_mode(st, ModeRequest::Auto).await.value.mode, ExecMode::Direct);
}
