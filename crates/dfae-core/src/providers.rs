//! Provider traits for language models and entity extraction.

use crate::{Entity, ExtractionError, ModelError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// -------------------------------------------------------------- models -----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn user(c: impl Into<String>) -> Self {
        Self { role: Role::User, content: c.into() }
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: c.into() }
    }
}

#[derive(Debug, Clone)]
pub struct GenerationRequest {
    /// Stable instructions. Keep this byte-identical across calls so servers
    /// with prefix caching (llama.cpp, vLLM, Anthropic) can reuse it.
    pub system: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Ask the backend to constrain output to a JSON object.
    pub json: bool,
    /// What the call is for (`plan`, `step`, `answer`, ...); used by the ledger.
    pub purpose: String,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct GenerationResponse {
    pub text: String,
    pub usage: Usage,
    pub model: String,
}

#[async_trait]
pub trait LanguageModel: Send + Sync {
    fn name(&self) -> &str;
    async fn generate(&self, request: GenerationRequest) -> Result<GenerationResponse, ModelError>;
    async fn health(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------- extraction -----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionRequest {
    pub text: String,
    pub labels: Vec<String>,
    #[serde(default = "default_threshold")]
    pub threshold: f32,
}

fn default_threshold() -> f32 {
    0.4
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExtractionResult {
    pub entities: Vec<Entity>,
    pub backend: String,
    pub latency_ms: u64,
    #[serde(default)]
    pub cached: bool,
}

#[async_trait]
pub trait EntityExtractor: Send + Sync {
    fn name(&self) -> &str;
    async fn extract(&self, request: ExtractionRequest) -> Result<ExtractionResult, ExtractionError>;
    async fn health(&self) -> bool {
        true
    }
}

/// Default label set (task-specific sets are chosen by the runtime).
pub const DEFAULT_LABELS: &[&str] = &[
    "person", "organization", "software", "repository", "server", "database", "api", "tool", "file path",
    "url", "version", "environment", "location", "deadline", "constraint", "action", "document", "risk",
];
