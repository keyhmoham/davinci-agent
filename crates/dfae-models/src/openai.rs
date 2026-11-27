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
    /// Cleared the first time the server rejects `response_format: json_schema`: later calls ask for plain JSON.
    schema_ok: std::sync::atomic::AtomicBool,
    /// When a test request last proved an unlisted model usable: health is polled often, and every ping is a billed request.
    /// Cleared the first time the server rejects the non-standard request fields (`reasoning`, `frequency_penalty`): later calls omit them.
    extras_ok: std::sync::atomic::AtomicBool,
    pinged_ok: std::sync::Mutex<Option<std::time::Instant>>,
    /// Vercel AI Gateway: the upstream providers allowed to serve the model, in order (`providerOptions.gateway`).
    providers: Vec<String>,
}

impl OpenAiCompat {
    pub fn new(base: &str, model: &str, key: Option<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder().timeout(timeout).build().expect("http client");
        // OpenAI's own API and Vercel AI Gateway (which passes on the providers' prefix caching: measured cache hits on the planner)
        // cache a repeated prompt prefix by themselves; other servers say so with `PLANNER_PROMPT_CACHE=on`. A server that does not
        // cache costs nothing extra: after three misses the cacheable prefix is no longer sent (`prefix_misses`).
        let caches = base.contains("api.openai.com") || base.contains("ai-gateway.vercel.sh");
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            model: model.into(),
            key,
            name: format!("openai:{model}"),
            caches,
            schema_ok: true.into(),
            extras_ok: true.into(),
            pinged_ok: Default::default(),
            providers: Vec::new(),
        }
    }

    /// On Vercel AI Gateway, serve the model only from these upstream providers, in this order. Ignored by other servers.
    pub fn with_providers(mut self, providers: &[String]) -> Self {
        if self.base.contains("ai-gateway.vercel.sh") {
            self.providers = providers.to_vec();
        }
        self
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
        if !self.providers.is_empty() {
            body["providerOptions"] = json!({"gateway": {"order": self.providers, "only": self.providers}});
        }
        let extras = self.extras_ok.load(std::sync::atomic::Ordering::Relaxed);
        if extras && req.frequency_penalty > 0.0 {
            body["frequency_penalty"] = json!(req.frequency_penalty);
        }
        // thinking is a cost and a latency: `off` switches it off, a level sets the effort (gateways and OpenAI-compatible servers
        // take `reasoning` / `reasoning_effort`; a server that rejects them is asked without, from then on)
        let has_reasoning = extras && req.reasoning.is_some();
        match req.reasoning.as_deref().filter(|_| extras) {
            Some("off" | "none") => body["reasoning"] = json!({"enabled": false}),
            Some(level) => body["reasoning_effort"] = json!(level),
            None => {}
        }
        let schema = req.schema.as_ref().filter(|_| self.schema_ok.load(std::sync::atomic::Ordering::Relaxed));
        if let Some(schema) = schema {
            body["response_format"] = json!({"type": "json_schema", "json_schema": {"name": "reply", "strict": req.schema_strict, "schema": schema}});
        } else if req.json || req.schema.is_some() {
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
                let err_body: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
                last = format!("HTTP {status}: {err_body}");
                if status == reqwest::StatusCode::NOT_FOUND && err_body.contains("not found") {
                    last.push_str(&format!(
                        " — the model server does not have '{}'. With the bundled Ollama it may still be downloading or the pull failed: \
                         `docker compose logs -f ollama-pull` (retry: `docker compose run --rm ollama-pull`)",
                        self.model
                    ));
                }
                if is_retryable(status) {
                    continue;
                }
                // a server that does not know json_schema is asked for plain JSON from now on, and this call is repeated; so with the
                // non-standard fields (reasoning, penalties)
                if status == reqwest::StatusCode::BAD_REQUEST && schema.is_some() && self.schema_ok.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    body["response_format"] = json!({"type": "json_object"});
                    continue;
                }
                if status == reqwest::StatusCode::BAD_REQUEST
                    && (has_reasoning || req.frequency_penalty > 0.0)
                    && self.extras_ok.swap(false, std::sync::atomic::Ordering::Relaxed)
                {
                    if let Some(b) = body.as_object_mut() {
                        for k in ["reasoning", "reasoning_effort", "frequency_penalty"] {
                            b.remove(k);
                        }
                    }
                    continue;
                }
                return Err(ModelError::Request(last));
            }
            let v: Value = resp.json().await.map_err(|e| ModelError::InvalidOutput(e.to_string()))?;
            let text = v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();
            if text.trim().is_empty() {
                let reason = v["choices"][0]["finish_reason"].as_str().unwrap_or("unknown");
                return Err(ModelError::InvalidOutput(if reason == "length" {
                    format!(
                        "the model used all {} output tokens before writing any text (a reasoning model spends them thinking): raise PLANNER_MAX_TOKENS / WORKER_MAX_TOKENS",
                        req.max_tokens
                    )
                } else {
                    format!("the model returned an empty reply (finish reason: {reason})")
                }));
            }
            let usage = Usage {
                prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                cached_tokens: v["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
                cached_reported: v["usage"]["prompt_tokens_details"]["cached_tokens"].is_u64(),
                // Vercel AI Gateway reports cache writes, reasoning and the call's cost in USD; OpenAI reports reasoning
                cache_write_tokens: v["usage"]["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                reasoning_tokens: v["usage"]["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
                cost_usd: v["usage"]["cost"].as_f64(),
            };
            return Ok(GenerationResponse { text, usage, model: v["model"].as_str().unwrap_or(&self.model).to_string() });
        }
        Err(ModelError::Request(last))
    }

    async fn health(&self) -> bool {
        self.diagnose().await.is_ok()
    }

    async fn diagnose(&self) -> Result<(), String> {
        let mut rb = self.http.get(format!("{}/models", self.base)).timeout(Duration::from_secs(5));
        if let Some(k) = &self.key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await.map_err(|e| format!("cannot reach {}: {e}", self.base))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!("{} answered HTTP {status}: the API key was refused", self.base));
        }
        if !status.is_success() {
            // many gateways have no model listing; the chat endpoint is what matters, and a real call will say what is wrong
            return Ok(());
        }
        // A server that is up but lacks the configured model (Ollama before the pull finishes) is not usable.
        match resp.json::<Value>().await.ok().and_then(|v| v["data"].as_array().cloned()) {
            Some(models) => {
                let norm = |m: &str| m.strip_suffix(":latest").unwrap_or(m).to_string();
                let want = norm(&self.model);
                let ids: Vec<String> = models.iter().filter_map(|m| m["id"].as_str()).map(String::from).collect();
                // gateways list ids with the provider in front (`openai/gpt-x`) and accept the short form too
                let listed = |id: &String| norm(id) == want || id.rsplit('/').next().map(norm).as_deref() == Some(want.as_str());
                if ids.iter().any(listed) {
                    return Ok(());
                }
                if self.pinged_ok.lock().ok().and_then(|g| *g).is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
                    return Ok(());
                }
                // A listing is a hint, not the truth (aliases, models that are routed but not listed): ask the model itself.
                // One token. A server that really lacks the model (Ollama before the pull) answers 404 at once.
                let ping = json!({"model": self.model, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 1, "stream": false});
                let mut rb = self.http.post(format!("{}/chat/completions", self.base)).timeout(Duration::from_secs(20)).json(&ping);
                if let Some(k) = &self.key {
                    rb = rb.bearer_auth(k);
                }
                match rb.send().await {
                    Ok(r) if r.status().is_success() => {
                        if let Ok(mut g) = self.pinged_ok.lock() {
                            *g = Some(std::time::Instant::now());
                        }
                        Ok(())
                    }
                    other => {
                        let why = match other {
                            Ok(r) => {
                                let s = r.status();
                                format!("HTTP {s}: {}", r.text().await.unwrap_or_default().chars().take(160).collect::<String>())
                            }
                            Err(e) => e.to_string(),
                        };
                        let near: Vec<&String> = ids.iter().filter(|id| id.contains(want.split('-').next().unwrap_or(&want))).take(6).collect();
                        Err(format!(
                            "{} does not list the model '{}' and a test request to it failed ({why}); {}",
                            self.base,
                            self.model,
                            if near.is_empty() {
                                format!("it lists {} models", ids.len())
                            } else {
                                format!("similar ids: {}", near.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
                            }
                        ))
                    }
                }
            }
            None => Ok(()), // not a listing we understand: do not guess
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_pinning_is_sent_only_to_vercel_ai_gateway() {
        let p = vec!["baseten".to_string(), "nebius".to_string()];
        let gw = OpenAiCompat::new("https://ai-gateway.vercel.sh/v1", "glm-5.3-flash", None, Duration::from_secs(5)).with_providers(&p);
        assert_eq!(gw.providers, p);
        let other = OpenAiCompat::new("http://localhost:11434/v1", "qwen3", None, Duration::from_secs(5)).with_providers(&p);
        assert!(other.providers.is_empty(), "another server would reject the unknown field");
    }
}
