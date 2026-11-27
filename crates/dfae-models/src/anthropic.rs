use crate::is_retryable;
use async_trait::async_trait;
use dfae_core::*;
use serde_json::{Value, json};
use std::time::Duration;

/// Smallest stable prefix worth marking for prompt caching.
pub const CACHE_MIN_TOKENS: usize = 1024;

/// Claude via the Messages API. The system prompt is sent as a cacheable
/// block so the stable instruction prefix is billed at the cached rate.
pub struct AnthropicModel {
    http: reqwest::Client,
    base: String,
    model: String,
    key: String,
    name: String,
}

impl AnthropicModel {
    pub fn new(base: &str, model: &str, key: String, timeout: Duration) -> Self {
        let http = reqwest::Client::builder().timeout(timeout).build().expect("http client");
        Self { http, base: base.trim_end_matches('/').to_string(), model: model.into(), key, name: format!("anthropic:{model}") }
    }
}

#[async_trait]
impl LanguageModel for AnthropicModel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn generate(&self, req: GenerationRequest) -> Result<GenerationResponse, ModelError> {
        let system = if req.json { format!("{}\n\nRespond with a single JSON object and nothing else.", req.system) } else { req.system.clone() };
        let messages: Vec<Value> =
            req.messages.iter().map(|m| json!({"role": match m.role { Role::User => "user", Role::Assistant => "assistant" }, "content": m.content})).collect();
        // Prompt caching only applies above a minimum prefix length (historically ~1,024 tokens
        // for Sonnet-class models); below it `cache_control` is dead weight, so it is only sent
        // when the stable prefix is long enough to be worth caching.
        let mut block = json!({"type": "text", "text": system});
        if req.cache || tokens::estimate_tokens(&system) >= CACHE_MIN_TOKENS {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
        let body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "system": [block],
            "messages": messages,
        });
        let mut last = String::new();
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
            }
            let resp = match self
                .http
                .post(format!("{}/v1/messages", self.base))
                .header("x-api-key", &self.key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                last = format!("HTTP {status}: {}", resp.text().await.unwrap_or_default().chars().take(300).collect::<String>());
                if is_retryable(status) {
                    continue;
                }
                return Err(ModelError::Request(last));
            }
            let v: Value = resp.json().await.map_err(|e| ModelError::InvalidOutput(e.to_string()))?;
            let text: String = v["content"].as_array().map(|a| a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("")).unwrap_or_default();
            let u = &v["usage"];
            let usage = Usage {
                prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_creation_input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                completion_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                cached_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cached_reported: u.get("cache_read_input_tokens").is_some(),
                cache_write_tokens: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                ..Default::default()
            };
            return Ok(GenerationResponse { text, usage, model: v["model"].as_str().unwrap_or(&self.model).to_string() });
        }
        Err(ModelError::Request(last))
    }

    fn caches_prompts(&self) -> bool {
        true
    }

    async fn health(&self) -> bool {
        // No free health endpoint; configured with a key means "assumed up".
        true
    }
}
