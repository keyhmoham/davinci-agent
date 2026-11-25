//! Memory types and the `MemoryStore` / `Embedder` traits.

use crate::{Entity, MemoryError, TaskRecord};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Previous tasks, interactions, outcomes, failures.
    Episodic,
    /// Facts, concepts, documents.
    Semantic,
    /// Skills and reusable workflows.
    Procedural,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Episodic => "episodic",
            Self::Semantic => "semantic",
            Self::Procedural => "procedural",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "episodic" => Some(Self::Episodic),
            "semantic" => Some(Self::Semantic),
            "procedural" => Some(Self::Procedural),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    pub kind: MemoryKind,
    pub text: String,
    #[serde(default)]
    pub entities: Vec<Entity>,
    /// 0..1, how much this memory matters (verified facts and skills rank higher).
    pub importance: f32,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl MemoryItem {
    pub fn new(kind: MemoryKind, text: impl Into<String>, importance: f32) -> Self {
        Self {
            id: crate::new_id(),
            kind,
            text: text.into(),
            entities: vec![],
            importance,
            metadata: Value::Null,
            session_id: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub text: String,
    pub kinds: Vec<MemoryKind>,
    pub limit: usize,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredMemory {
    pub item: MemoryItem,
    /// Cosine similarity of the embeddings.
    pub similarity: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub id: String,
    pub task_id: String,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub payload: Value,
    /// Hash chain: sha256(prev_hash || canonical payload) — tamper evidence.
    pub hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryStats {
    pub backend: String,
    pub tasks: usize,
    pub episodic: usize,
    pub semantic: usize,
    pub procedural: usize,
}

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    fn backend(&self) -> &str;
    async fn health(&self) -> bool;

    // working state
    async fn put_task(&self, task: &TaskRecord) -> Result<(), MemoryError>;
    async fn get_task(&self, id: &str) -> Result<Option<TaskRecord>, MemoryError>;
    async fn list_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>, MemoryError>;
    /// Tasks in a non-terminal state (for resumption after a restart).
    async fn unfinished_tasks(&self) -> Result<Vec<TaskRecord>, MemoryError>;

    // long-term memory
    async fn store(&self, item: MemoryItem) -> Result<(), MemoryError>;
    async fn retrieve(&self, query: MemoryQuery) -> Result<Vec<ScoredMemory>, MemoryError>;
    async fn update_metadata(&self, id: &str, metadata: Value) -> Result<(), MemoryError>;
    async fn delete(&self, id: &str) -> Result<(), MemoryError>;

    // small durable key/value (calibration statistics etc.)
    async fn kv_get(&self, key: &str) -> Result<Option<Value>, MemoryError>;
    async fn kv_set(&self, key: &str, value: Value) -> Result<(), MemoryError>;

    // append-only audit log with hash chaining
    async fn audit_append(&self, task_id: &str, kind: &str, payload: Value) -> Result<(), MemoryError>;
    async fn audit_list(&self, task_id: &str) -> Result<Vec<AuditRecord>, MemoryError>;

    async fn stats(&self) -> Result<MemoryStats, MemoryError>;
}
