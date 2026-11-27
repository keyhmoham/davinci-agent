//! Language-model providers and the gateway that routes between them.
//!
//! * [`OpenAiCompat`] — any OpenAI-compatible server (Ollama, llama.cpp,
//!   vLLM, SGLang, hosted APIs).
//! * [`AnthropicModel`] — Claude via the Messages API, with prompt caching on
//!   the stable system prefix.
//! * [`OfflineStub`] / [`ScriptedModel`] — deterministic models for smoke
//!   tests and unit tests; they are not intelligence.
//! * [`ModelGateway`] — worker model / planner-model tiers, token accounting, JSON
//!   extraction with a single repair retry.

mod anthropic;
mod gateway;
mod mock;
mod openai;

pub use anthropic::AnthropicModel;
pub use gateway::*;
pub use mock::{MIN_CACHE_TOKENS, MeteredModel, OfflineStub, ScriptedModel};
pub use openai::OpenAiCompat;

use dfae_core::*;
use std::sync::Arc;
use std::time::Duration;

/// Build the model for a configured tier, or `None` when it is disabled.
pub fn build_model(cfg: &ModelConfig) -> Option<Arc<dyn LanguageModel>> {
    let timeout = Duration::from_secs(cfg.timeout_secs);
    match cfg.provider.as_str() {
        "openai" => {
            let mut m = OpenAiCompat::new(&cfg.base_url, &cfg.model, cfg.api_key.clone(), timeout).with_providers(&cfg.providers);
            if let Some(on) = cfg.prompt_cache {
                m = m.with_prompt_cache(on);
            }
            Some(Arc::new(m))
        }
        "anthropic" => cfg.api_key.clone().map(|k| Arc::new(AnthropicModel::new(&cfg.base_url, &cfg.model, k, timeout)) as Arc<dyn LanguageModel>),
        "mock" => Some(Arc::new(OfflineStub)),
        _ => None,
    }
}

pub(crate) fn is_retryable(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}
