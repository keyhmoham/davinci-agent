//! Wire-format tests against fake OpenAI-compatible and Anthropic servers.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use dfae_core::*;
use dfae_models::{AnthropicModel, OpenAiCompat};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

fn req(json_mode: bool) -> GenerationRequest {
    GenerationRequest { system: "SYS".into(), messages: vec![Message::user("hello")], max_tokens: 50, temperature: 0.0, json: json_mode, purpose: "t".into() }
}

#[tokio::test]
async fn openai_compat_request_shape_usage_and_retry() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen: Arc<parking_lot_free::Cell> = Arc::default();
    async fn chat(
        State((hits, seen)): State<(Arc<AtomicUsize>, Arc<parking_lot_free::Cell>)>,
        h: HeaderMap,
        Json(b): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        if hits.fetch_add(1, Ordering::SeqCst) == 0 {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "warming up"})));
        }
        *seen.0.lock().unwrap() = Some((b.clone(), h.get("authorization").map(|v| v.to_str().unwrap().to_string())));
        (
            StatusCode::OK,
            Json(
                json!({"model": "qwen-test", "choices": [{"message": {"role": "assistant", "content": "{\"ok\":true}"}}], "usage": {"prompt_tokens": 12, "completion_tokens": 5}}),
            ),
        )
    }
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(|| async { Json(json!({"data": []})) }))
        .with_state((hits.clone(), seen.clone()));
    let base = serve(app).await;
    let m = OpenAiCompat::new(&format!("{base}/v1"), "qwen-test", Some("k123".into()), Duration::from_secs(5));
    let r = m.generate(req(true)).await.unwrap();
    assert_eq!(r.text, "{\"ok\":true}");
    assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (12, 5));
    assert_eq!(hits.load(Ordering::SeqCst), 2, "a 503 is retried");
    let (body, auth) = seen.0.lock().unwrap().clone().unwrap();
    assert_eq!(auth.as_deref(), Some("Bearer k123"));
    assert_eq!(body["model"], "qwen-test");
    assert_eq!(body["messages"][0], json!({"role": "system", "content": "SYS"}));
    assert_eq!(body["messages"][1], json!({"role": "user", "content": "hello"}));
    assert_eq!(body["response_format"], json!({"type": "json_object"}));
    assert!(m.health().await);
}

#[tokio::test]
async fn openai_compat_client_errors_are_not_retried() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h2 = hits.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move || {
            let h = h2.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                (StatusCode::BAD_REQUEST, Json(json!({"error": "bad"})))
            }
        }),
    );
    let base = serve(app).await;
    let m = OpenAiCompat::new(&base, "m", None, Duration::from_secs(5));
    assert!(m.generate(req(false)).await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn anthropic_request_shape_and_cache_control() {
    let seen: Arc<parking_lot_free::Cell> = Arc::default();
    async fn msgs(State(seen): State<Arc<parking_lot_free::Cell>>, h: HeaderMap, Json(b): Json<Value>) -> Json<Value> {
        *seen.0.lock().unwrap() =
            Some((b, h.get("x-api-key").map(|v| format!("{}|{}", v.to_str().unwrap(), h.get("anthropic-version").unwrap().to_str().unwrap()))));
        Json(
            json!({"model": "claude-test", "content": [{"type": "text", "text": "hi "}, {"type": "text", "text": "there"}], "usage": {"input_tokens": 20, "output_tokens": 4, "cache_read_input_tokens": 100}}),
        )
    }
    let base = serve(Router::new().route("/v1/messages", post(msgs)).with_state(seen.clone())).await;
    let m = AnthropicModel::new(&base, "claude-test", "sk-test".into(), Duration::from_secs(5));
    let r = m.generate(req(true)).await.unwrap();
    assert_eq!(r.text, "hi there");
    assert_eq!(r.usage.prompt_tokens, 120, "cached input tokens are counted");
    let (body, key) = seen.0.lock().unwrap().clone().unwrap();
    assert_eq!(key.as_deref(), Some("sk-test|2023-06-01"));
    assert_eq!(body["system"][0]["cache_control"], json!({"type": "ephemeral"}));
    assert!(body["system"][0]["text"].as_str().unwrap().starts_with("SYS"));
    assert!(body["system"][0]["text"].as_str().unwrap().contains("JSON"));
    assert_eq!(body["messages"][0]["role"], "user");
}

mod parking_lot_free {
    use serde_json::Value;
    #[derive(Default)]
    pub struct Cell(pub std::sync::Mutex<Option<(Value, Option<String>)>>);
}
