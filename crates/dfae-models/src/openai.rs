use crate::is_retryable;
use async_trait::async_trait;
use dfae_core::*;
use serde_json::{Value, json};
use std::time::Duration;

pub struct OpenAiCompat {
    http: reqwest::Client,
    base: String,
    model: String,
    key: Option<String>,
    name: String,
    caches: bool,
}

impl OpenAiCompat {
    pub fn new(base: &str, model: &str, key: Option<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder().timeout(timeout).build().expect("http client");
        // OpenAI's own API caches a repeated prompt prefix by itself; other servers say so with `STRONG_PROMPT_CACHE=on`.
        // Self-hosted ones are free anyway, and a request that is billed in full is no loss there.
        let caches = base.contains("api.openai.com");
        Self { http, base: base.trim_end_matches('/').to_string(), model: model.into(), key, name: format!("openai:{model}"), caches }
    }

    /// Declare whether this server bills a repeated prompt prefix at a fraction.
    pub fn with_prompt_cache(mut self, on: bool) -> Self {
        self.caches = on;
        self
    }
}

#[async_trait]
impl LanguageModel for OpenAiCompat {
    fn name(&self) -> &str {
        &self.name
    }

    fn caches_prompts(&self) -> bool {
        self.caches
    }

    async fn generate(&self, req: GenerationRequest) -> Result<GenerationResponse, ModelError> {
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        for m in &req.messages {
            messages.push(json!({"role": match m.role { Role::User => "user", Role::Assistant => "assistant" }, "content": m.content}));
        }
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "stream": false,
        });
        if let Some(schema) = &req.schema {
            body["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "reply", "strict": false, "schema": schema}});
        } else if req.json {
            body["response_format"] = json!({"type": "json_object"});
        }
        let mut last = String::new();
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(attempt))).await;
            }
            let mut rb = self.http.post(format!("{}/chat/completions", self.base)).json(&body);
            if let Some(k) = &self.key {
                rb = rb.bearer_auth(k);
            }
            let resp = match rb.send().await {
                Ok(r) => r,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let body: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
                last = format!("HTTP {status}: {body}");
                if status == reqwest::StatusCode::NOT_FOUND && body.contains("not found") {
                    last.push_str(&format!(
                        " — the model server does not have '{}'. With the bundled Ollama it may still be downloading or the pull failed: \
                         `docker compose logs -f ollama-pull` (retry: `docker compose run --rm ollama-pull`)",
                        self.model
                    ));
                }
                if is_retryable(status) {
                    continue;
                }
                return Err(ModelError::Request(last));
            }
            let v: Value = resp.json().await.map_err(|e| ModelError::InvalidOutput(e.to_string()))?;
            let text = v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();
            let usage = Usage {
                prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                cached_tokens: v["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
            };
            return Ok(GenerationResponse { text, usage, model: v["model"].as_str().unwrap_or(&self.model).to_string() });
        }
        Err(ModelError::Request(last))
    }

    async fn health(&self) -> bool {
        let mut rb = self.http.get(format!("{}/models", self.base)).timeout(Duration::from_secs(3));
        if let Some(k) = &self.key {
            rb = rb.bearer_auth(k);
        }
        let Ok(resp) = rb.send().await else { return false };
        if !resp.status().is_success() {
            return false;
        }
        // A server that is up but lacks the configured model (Ollama before the pull finishes) is not healthy.
        match resp.json::<Value>().await.ok().and_then(|v| v["data"].as_array().cloned()) {
            Some(models) => {
                let norm = |m: &str| m.strip_suffix(":latest").unwrap_or(m).to_string();
                let want = norm(&self.model);
                models.iter().filter_map(|m| m["id"].as_str()).any(|id| norm(id) == want)
            }
            None => true, // not a listing we understand: do not guess
        }
    }
}
