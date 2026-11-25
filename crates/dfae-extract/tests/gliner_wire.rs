use axum::routing::{get, post};
use axum::{Json, Router};
use dfae_core::*;
use dfae_extract::{GlinerClient, HybridExtractor};
use serde_json::{Value, json};
use std::sync::Arc;

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

fn app() -> Router {
    Router::new()
        .route(
            "/extract",
            post(|Json(b): Json<Value>| async move {
                assert!(b["labels"].as_array().unwrap().iter().any(|l| l == "software"));
                assert!(b["threshold"].as_f64().is_some());
                Json(json!({"model": "gliner-test", "entities": [
                    {"text": "Redis", "label": "software", "start": 7, "end": 12, "score": 0.93},
                    {"text": "Frankfurt", "label": "location", "start": 45, "end": 54, "score": 0.88}]}))
            }),
        )
        .route("/health", get(|| async { Json(json!({"status": "ok", "model_loaded": true})) }))
}

#[tokio::test]
async fn hybrid_merges_gliner_and_rules() {
    let base = serve(app()).await;
    let g = GlinerClient::new(&base);
    assert!(g.health().await);
    let h = HybridExtractor::new(Some(Arc::new(g)));
    let text = "Deploy Redis 8 on the production server in Frankfurt, but don't restart it until Friday.";
    let r = h.extract(ExtractionRequest { text: text.into(), labels: vec!["software".into(), "location".into()], threshold: 0.4 }).await.unwrap();
    assert!(r.backend.contains("gliner:gliner-test") && r.backend.contains("rules"), "{}", r.backend);
    let has = |l: &str, t: &str| r.entities.iter().any(|e| e.label == l && e.text == t);
    assert!(has("software", "Redis"));
    assert!(has("location", "Frankfurt"));
    assert!(has("environment", "production"), "rule entities survive the merge");
    assert!(r.entities.iter().any(|e| e.label == "constraint"));
    assert_eq!(r.entities.iter().filter(|e| e.text == "Redis").count(), 1, "overlapping duplicates are resolved");
}

#[tokio::test]
async fn a_model_that_is_still_loading_reports_unhealthy_and_extraction_degrades() {
    let loading = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "loading", "model_loaded": false})) }))
        .route("/extract", post(|| async { (axum::http::StatusCode::SERVICE_UNAVAILABLE, "model loading") }));
    let base = serve(loading).await;
    let g = GlinerClient::new(&base);
    assert!(!g.health().await);
    let h = HybridExtractor::new(Some(Arc::new(g)));
    let r = h.extract(ExtractionRequest { text: "read /workspace/a.txt".into(), labels: vec![], threshold: 0.4 }).await.unwrap();
    assert_eq!(r.backend, "rules(fallback)");
    assert!(r.entities.iter().any(|e| e.label == "file path"));
}
