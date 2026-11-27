use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model not configured: {0}")]
    NotConfigured(String),
    #[error("model request failed: {0}")]
    Request(String),
    #[error("model returned invalid output: {0}")]
    InvalidOutput(String),
}

#[derive(Debug, Error)]
pub enum ExtractionError {
    #[error("extractor unavailable: {0}")]
    Unavailable(String),
    #[error("extraction failed: {0}")]
    Failed(String),
}

#[derive(Debug, Error)]
pub enum DecisionError {
    #[error("decision provider unavailable: {0}")]
    Unavailable(String),
    #[error("decision request failed: {0}")]
    Failed(String),
    #[error("decision response invalid: {0}")]
    Invalid(String),
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory backend error: {0}")]
    Backend(String),
    #[error("not found: {0}")]
    NotFound(String),
}

/// Why a tool call failed. Drives recovery decisions (retry vs replan vs fail).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Denied by policy — never retried, never worked around.
    Policy,
    /// Held back for the user (a removal that content from outside asked for): the plan goes on without it, never with it.
    Withheld,
    InvalidArgs,
    NotFound,
    Timeout,
    Transient,
    Io,
    Cancelled,
    Other,
}

#[derive(Debug, Clone, Error, serde::Serialize, serde::Deserialize)]
#[error("{kind:?}: {message}")]
pub struct ToolError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ToolError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn policy(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Policy, m)
    }
    pub fn args(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidArgs, m)
    }
    pub fn io(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, m)
    }
    pub fn other(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, m)
    }
    pub fn is_transient(&self) -> bool {
        matches!(self.kind, ErrorKind::Timeout | ErrorKind::Transient)
    }
}
