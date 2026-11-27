use crate::{GENESIS, HashEmbedder, RemoteEmbedder, chain_hash, cosine};
use async_trait::async_trait;
use chrono::Utc;
use dfae_core::*;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, TaskRecord>,
    /// (item, hash embedding, semantic embedding if the model gave one)
    items: Vec<(MemoryItem, Vec<f32>, Option<Vec<f32>>)>,
    kv: HashMap<String, Value>,
    audit: Vec<AuditRecord>,
}

/// Volatile store for tests and database-less development.
pub struct InMemoryStore {
    inner: Mutex<Inner>,
    embedder: HashEmbedder,
    semantic: Option<RemoteEmbedder>,
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self { inner: Mutex::new(Inner::default()), embedder: HashEmbedder::default(), semantic: None }
    }
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Also give every item a semantic embedding from `model` (retrieval then ranks by meaning).
    pub fn with_semantic(mut self, model: Option<RemoteEmbedder>) -> Self {
        self.semantic = model;
        self
    }
}

#[async_trait]
impl MemoryStore for InMemoryStore {
    fn backend(&self) -> &str {
        "memory"
    }
    async fn health(&self) -> bool {
        true
    }

    async fn put_task(&self, task: &TaskRecord) -> Result<(), MemoryError> {
        self.inner.lock().tasks.insert(task.task_id.clone(), task.clone());
        Ok(())
    }
    async fn get_task(&self, id: &str) -> Result<Option<TaskRecord>, MemoryError> {
        Ok(self.inner.lock().tasks.get(id).cloned())
    }
    async fn list_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>, MemoryError> {
        let mut v: Vec<TaskRecord> = self.inner.lock().tasks.values().cloned().collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        v.truncate(limit);
        Ok(v)
    }
    async fn unfinished_tasks(&self) -> Result<Vec<TaskRecord>, MemoryError> {
        Ok(self.inner.lock().tasks.values().filter(|t| !t.status.is_terminal()).cloned().collect())
    }

    async fn store(&self, item: MemoryItem) -> Result<(), MemoryError> {
        let emb = {
            use dfae_core::Embedder;
            self.embedder.embed(&item.text)
        };
        let sem = match &self.semantic {
            Some(m) => m.embed_one(&item.text).await.map_err(|e| tracing::warn!(error = %e, "semantic embedding failed")).ok(),
            None => None,
        };
        let mut g = self.inner.lock();
        g.items.retain(|(i, _, _)| i.id != item.id);
        g.items.push((item, emb, sem));
        Ok(())
    }

    async fn retrieve(&self, q: MemoryQuery) -> Result<Vec<ScoredMemory>, MemoryError> {
        use dfae_core::Embedder;
        let qe = self.embedder.embed(&q.text);
        let qs = match &self.semantic {
            Some(m) => m.embed_one(&q.text).await.ok(),
            None => None,
        };
        let g = self.inner.lock();
        let mut out: Vec<ScoredMemory> = g
            .items
            .iter()
            .filter(|(i, _, _)| q.kinds.is_empty() || q.kinds.contains(&i.kind))
            .filter(|(i, _, _)| match (&q.session_id, &i.session_id) {
                (Some(s), Some(is)) => s == is,
                _ => true,
            })
            .map(|(i, e, s)| {
                let shape = cosine(&qe, e);
                match (&qs, s) {
                    (Some(qs), Some(s)) => ScoredMemory { item: i.clone(), similarity: cosine(qs, s), shape_similarity: shape, semantic: true },
                    _ => ScoredMemory::by_shape(i.clone(), shape),
                }
            })
            .collect();
        out.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
        out.truncate(q.limit);
        Ok(out)
    }

    async fn update_metadata(&self, id: &str, metadata: Value) -> Result<(), MemoryError> {
        let mut g = self.inner.lock();
        match g.items.iter_mut().find(|(i, _, _)| i.id == id) {
            Some((i, _, _)) => {
                i.metadata = metadata;
                Ok(())
            }
            None => Err(MemoryError::NotFound(id.into())),
        }
    }
    async fn delete(&self, id: &str) -> Result<(), MemoryError> {
        self.inner.lock().items.retain(|(i, _, _)| i.id != id);
        Ok(())
    }

    async fn kv_get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
        Ok(self.inner.lock().kv.get(key).cloned())
    }
    async fn kv_set(&self, key: &str, value: Value) -> Result<(), MemoryError> {
        self.inner.lock().kv.insert(key.into(), value);
        Ok(())
    }

    async fn audit_append(&self, task_id: &str, kind: &str, payload: Value) -> Result<(), MemoryError> {
        let mut g = self.inner.lock();
        let prev = g.audit.iter().rev().find(|r| r.task_id == task_id).map(|r| r.hash.clone()).unwrap_or_else(|| GENESIS.to_string());
        let hash = chain_hash(&prev, kind, &payload);
        g.audit.push(AuditRecord { id: new_id(), task_id: task_id.into(), ts: Utc::now(), kind: kind.into(), payload, hash });
        Ok(())
    }
    async fn audit_list(&self, task_id: &str) -> Result<Vec<AuditRecord>, MemoryError> {
        Ok(self.inner.lock().audit.iter().filter(|r| r.task_id == task_id).cloned().collect())
    }

    async fn stats(&self) -> Result<MemoryStats, MemoryError> {
        let g = self.inner.lock();
        let c = |k: MemoryKind| g.items.iter().filter(|(i, _, _)| i.kind == k).count();
        Ok(MemoryStats {
            backend: "memory".into(),
            tasks: g.tasks.len(),
            episodic: c(MemoryKind::Episodic),
            semantic: c(MemoryKind::Semantic),
            procedural: c(MemoryKind::Procedural),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify_audit_chain;

    #[tokio::test]
    async fn stores_and_retrieves_by_similarity() {
        let s = InMemoryStore::new();
        s.store(MemoryItem::new(MemoryKind::Semantic, "the staging database lives on host db-staging", 0.8)).await.unwrap();
        s.store(MemoryItem::new(MemoryKind::Semantic, "lunch is at noon on fridays", 0.5)).await.unwrap();
        let r = s
            .retrieve(MemoryQuery { text: "where is the staging database host".into(), kinds: vec![MemoryKind::Semantic], limit: 1, session_id: None })
            .await
            .unwrap();
        assert!(r[0].item.text.contains("staging database"));
    }

    #[tokio::test]
    async fn audit_chain_is_tamper_evident() {
        let s = InMemoryStore::new();
        for i in 0..3 {
            s.audit_append("t1", "step", serde_json::json!({"i": i})).await.unwrap();
        }
        let mut recs = s.audit_list("t1").await.unwrap();
        assert!(verify_audit_chain(&recs));
        recs[1].payload = serde_json::json!({"i": 99});
        assert!(!verify_audit_chain(&recs));
    }
}
