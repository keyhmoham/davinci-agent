use async_trait::async_trait;
use dfae_core::*;
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};

/// Client for the GLiNER Python service (see `services/gliner-service`).
pub struct GlinerClient {
    http: reqwest::Client,
    base: String,
}

#[derive(Deserialize)]
struct WireEntity {
    text: String,
    label: String,
    #[serde(default)]
    start: Option<usize>,
    #[serde(default)]
    end: Option<usize>,
    #[serde(default)]
    score: f32,
}

#[derive(Deserialize)]
struct WireResponse {
    entities: Vec<WireEntity>,
    #[serde(default)]
    model: String,
}

impl GlinerClient {
    pub fn new(base_url: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("http client");
        Self { http, base: base_url.trim_end_matches('/').to_string() }
    }
}

#[async_trait]
impl EntityExtractor for GlinerClient {
    fn name(&self) -> &str {
        "gliner"
    }

    async fn extract(&self, request: ExtractionRequest) -> Result<ExtractionResult, ExtractionError> {
        let started = Instant::now();
        let resp = self
            .http
            .post(format!("{}/extract", self.base))
            .json(&json!({ "text": request.text, "labels": request.labels, "threshold": request.threshold }))
            .send()
            .await
            .map_err(|e| ExtractionError::Unavailable(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.json::<serde_json::Value>().await.ok().and_then(|v| v["detail"].as_str().map(String::from)).unwrap_or_default();
            return Err(ExtractionError::Unavailable(format!("HTTP {status}{}", if detail.is_empty() { String::new() } else { format!(": {detail}") })));
        }
        let parsed: WireResponse = resp.json().await.map_err(|e| ExtractionError::Failed(e.to_string()))?;
        let entities = parsed
            .entities
            .into_iter()
            .map(|e| Entity { label: e.label, text: e.text, start: e.start, end: e.end, score: e.score, source: "gliner".into() })
            .collect();
        Ok(ExtractionResult {
            entities,
            backend: if parsed.model.is_empty() { "gliner".into() } else { format!("gliner:{}", parsed.model) },
            latency_ms: started.elapsed().as_millis() as u64,
            cached: false,
        })
    }

    async fn health(&self) -> bool {
        match self.http.get(format!("{}/health", self.base)).send().await {
            Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.map(|v| v["model_loaded"].as_bool().unwrap_or(false)).unwrap_or(false),
            _ => false,
        }
    }
}
