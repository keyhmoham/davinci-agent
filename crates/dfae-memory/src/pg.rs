use crate::{GENESIS, HashEmbedder, chain_hash, cosine};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use dfae_core::*;
use serde_json::Value;
use std::time::Duration;
use tokio_postgres::{NoTls, types::Json};

/// PostgreSQL-backed store. Uses pgvector for similarity search when the
/// extension is available and falls back to `real[]` + in-process cosine
/// scoring otherwise.
pub struct PgStore {
    pool: Pool,
    embedder: HashEmbedder,
    has_vector: bool,
}

fn be<E: std::fmt::Display>(e: E) -> MemoryError {
    MemoryError::Backend(e.to_string())
}

fn vector_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x:.6}")).collect();
    format!("[{}]", parts.join(","))
}

impl PgStore {
    pub async fn connect(url: &str) -> Result<Self, MemoryError> {
        let cfg: tokio_postgres::Config = url.parse().map_err(be)?;
        let mgr = Manager::from_config(cfg, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
        let pool = Pool::builder(mgr).max_size(8).build().map_err(be)?;
        // wait for the database (compose starts services concurrently)
        let mut last = String::new();
        for _ in 0..40 {
            match pool.get().await {
                Ok(_) => {
                    last.clear();
                    break;
                }
                Err(e) => {
                    last = e.to_string();
                    tokio::time::sleep(Duration::from_millis(750)).await;
                }
            }
        }
        if !last.is_empty() {
            return Err(MemoryError::Backend(format!("cannot connect to postgres: {last}")));
        }
        let mut store = Self { pool, embedder: HashEmbedder::default(), has_vector: false };
        store.migrate().await?;
        Ok(store)
    }

    pub fn has_vector(&self) -> bool {
        self.has_vector
    }

    async fn migrate(&mut self) -> Result<(), MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        // serialise concurrent migrations (several replicas starting together)
        c.batch_execute("SELECT pg_advisory_lock(727274)").await.map_err(be)?;
        let res = async {
            self.has_vector = c.batch_execute("CREATE EXTENSION IF NOT EXISTS vector").await.is_ok();
            let col = if self.has_vector { "vector(256)" } else { "real[]" };
            c.batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS dfae_tasks (
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, status TEXT NOT NULL, goal TEXT NOT NULL,
                    record JSONB NOT NULL, created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL);
                 CREATE INDEX IF NOT EXISTS dfae_tasks_status ON dfae_tasks(status);
                 CREATE INDEX IF NOT EXISTS dfae_tasks_created ON dfae_tasks(created_at DESC);
                 CREATE TABLE IF NOT EXISTS dfae_memory (
                    id TEXT PRIMARY KEY, kind TEXT NOT NULL, text TEXT NOT NULL, entities JSONB NOT NULL DEFAULT '[]',
                    importance REAL NOT NULL, metadata JSONB NOT NULL DEFAULT 'null', session_id TEXT,
                    created_at TIMESTAMPTZ NOT NULL, embedding {col} NOT NULL);
                 CREATE INDEX IF NOT EXISTS dfae_memory_kind ON dfae_memory(kind, created_at DESC);
                 CREATE TABLE IF NOT EXISTS dfae_kv (key TEXT PRIMARY KEY, value JSONB NOT NULL);
                 CREATE TABLE IF NOT EXISTS dfae_audit (
                    seq BIGSERIAL PRIMARY KEY, id TEXT NOT NULL, task_id TEXT NOT NULL, ts TIMESTAMPTZ NOT NULL,
                    kind TEXT NOT NULL, payload JSONB NOT NULL, hash TEXT NOT NULL);
                 CREATE INDEX IF NOT EXISTS dfae_audit_task ON dfae_audit(task_id, seq);"
            ))
            .await
            .map_err(be)?;
            if self.has_vector {
                // best effort: HNSW needs pgvector >= 0.5
                let _ = c.batch_execute("CREATE INDEX IF NOT EXISTS dfae_memory_vec ON dfae_memory USING hnsw (embedding vector_cosine_ops)").await;
            }
            Ok::<(), MemoryError>(())
        }
        .await;
        let _ = c.batch_execute("SELECT pg_advisory_unlock(727274)").await;
        res
    }

    fn row_to_item(row: &tokio_postgres::Row) -> Result<MemoryItem, MemoryError> {
        let kind: String = row.get("kind");
        let entities: Json<Vec<Entity>> = row.get("entities");
        let metadata: Json<Value> = row.get("metadata");
        Ok(MemoryItem {
            id: row.get("id"),
            kind: MemoryKind::parse(&kind).ok_or_else(|| be(format!("bad kind {kind}")))?,
            text: row.get("text"),
            entities: entities.0,
            importance: row.get("importance"),
            metadata: metadata.0,
            session_id: row.get("session_id"),
            created_at: row.get::<_, DateTime<Utc>>("created_at"),
        })
    }
}

#[async_trait]
impl MemoryStore for PgStore {
    fn backend(&self) -> &str {
        if self.has_vector { "postgres+pgvector" } else { "postgres" }
    }

    async fn health(&self) -> bool {
        match self.pool.get().await {
            Ok(c) => c.query_one("SELECT 1", &[]).await.is_ok(),
            Err(_) => false,
        }
    }

    async fn put_task(&self, t: &TaskRecord) -> Result<(), MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        c.execute(
            "INSERT INTO dfae_tasks (id, session_id, status, goal, record, created_at, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7)
             ON CONFLICT (id) DO UPDATE SET status=EXCLUDED.status, record=EXCLUDED.record, updated_at=EXCLUDED.updated_at",
            &[&t.task_id, &t.session_id, &t.status.to_string(), &t.goal, &Json(t), &t.created_at, &t.updated_at],
        )
        .await
        .map_err(be)?;
        Ok(())
    }

    async fn get_task(&self, id: &str) -> Result<Option<TaskRecord>, MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        let row = c.query_opt("SELECT record FROM dfae_tasks WHERE id=$1", &[&id]).await.map_err(be)?;
        Ok(row.map(|r| r.get::<_, Json<TaskRecord>>(0).0))
    }

    async fn list_tasks(&self, limit: usize) -> Result<Vec<TaskRecord>, MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        let rows = c.query("SELECT record FROM dfae_tasks ORDER BY created_at DESC LIMIT $1", &[&(limit as i64)]).await.map_err(be)?;
        Ok(rows.iter().map(|r| r.get::<_, Json<TaskRecord>>(0).0).collect())
    }

    async fn unfinished_tasks(&self) -> Result<Vec<TaskRecord>, MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        let rows =
            c.query("SELECT record FROM dfae_tasks WHERE status NOT IN ('completed','failed','cancelled') ORDER BY created_at", &[]).await.map_err(be)?;
        Ok(rows.iter().map(|r| r.get::<_, Json<TaskRecord>>(0).0).collect())
    }

    async fn store(&self, item: MemoryItem) -> Result<(), MemoryError> {
        use dfae_core::Embedder;
        let emb = self.embedder.embed(&item.text);
        let c = self.pool.get().await.map_err(be)?;
        let kind = item.kind.as_str();
        if self.has_vector {
            c.execute(
                "INSERT INTO dfae_memory (id,kind,text,entities,importance,metadata,session_id,created_at,embedding)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9::text::vector)
                 ON CONFLICT (id) DO UPDATE SET text=EXCLUDED.text, entities=EXCLUDED.entities, importance=EXCLUDED.importance,
                   metadata=EXCLUDED.metadata, embedding=EXCLUDED.embedding",
                &[
                    &item.id,
                    &kind,
                    &item.text,
                    &Json(&item.entities),
                    &item.importance,
                    &Json(&item.metadata),
                    &item.session_id,
                    &item.created_at,
                    &vector_literal(&emb),
                ],
            )
            .await
            .map_err(be)?;
        } else {
            c.execute(
                "INSERT INTO dfae_memory (id,kind,text,entities,importance,metadata,session_id,created_at,embedding)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                 ON CONFLICT (id) DO UPDATE SET text=EXCLUDED.text, entities=EXCLUDED.entities, importance=EXCLUDED.importance,
                   metadata=EXCLUDED.metadata, embedding=EXCLUDED.embedding",
                &[&item.id, &kind, &item.text, &Json(&item.entities), &item.importance, &Json(&item.metadata), &item.session_id, &item.created_at, &emb],
            )
            .await
            .map_err(be)?;
        }
        Ok(())
    }

    async fn retrieve(&self, q: MemoryQuery) -> Result<Vec<ScoredMemory>, MemoryError> {
        use dfae_core::Embedder;
        let qe = self.embedder.embed(&q.text);
        let kinds: Vec<String> = if q.kinds.is_empty() {
            vec!["episodic".into(), "semantic".into(), "procedural".into()]
        } else {
            q.kinds.iter().map(|k| k.as_str().to_string()).collect()
        };
        let c = self.pool.get().await.map_err(be)?;
        let session = q.session_id.clone();
        if self.has_vector {
            let rows = c
                .query(
                    "SELECT id,kind,text,entities,importance,metadata,session_id,created_at,
                            1 - (embedding <=> $1::text::vector) AS sim
                     FROM dfae_memory
                     WHERE kind = ANY($2) AND ($4::text IS NULL OR session_id IS NULL OR session_id = $4)
                     ORDER BY embedding <=> $1::text::vector LIMIT $3",
                    &[&vector_literal(&qe), &kinds, &(q.limit as i64), &session],
                )
                .await
                .map_err(be)?;
            rows.iter().map(|r| Ok(ScoredMemory { item: Self::row_to_item(r)?, similarity: r.get::<_, f64>("sim") as f32 })).collect()
        } else {
            let rows = c
                .query(
                    "SELECT id,kind,text,entities,importance,metadata,session_id,created_at,embedding
                     FROM dfae_memory
                     WHERE kind = ANY($1) AND ($2::text IS NULL OR session_id IS NULL OR session_id = $2)
                     ORDER BY created_at DESC LIMIT 1000",
                    &[&kinds, &session],
                )
                .await
                .map_err(be)?;
            let mut out: Vec<ScoredMemory> = rows
                .iter()
                .map(|r| {
                    let e: Vec<f32> = r.get("embedding");
                    Ok(ScoredMemory { item: Self::row_to_item(r)?, similarity: cosine(&qe, &e) })
                })
                .collect::<Result<_, MemoryError>>()?;
            out.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
            out.truncate(q.limit);
            Ok(out)
        }
    }

    async fn update_metadata(&self, id: &str, metadata: Value) -> Result<(), MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        let n = c.execute("UPDATE dfae_memory SET metadata=$2 WHERE id=$1", &[&id, &Json(&metadata)]).await.map_err(be)?;
        if n == 0 {
            return Err(MemoryError::NotFound(id.into()));
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        c.execute("DELETE FROM dfae_memory WHERE id=$1", &[&id]).await.map_err(be)?;
        Ok(())
    }

    async fn kv_get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        let row = c.query_opt("SELECT value FROM dfae_kv WHERE key=$1", &[&key]).await.map_err(be)?;
        Ok(row.map(|r| r.get::<_, Json<Value>>(0).0))
    }

    async fn kv_set(&self, key: &str, value: Value) -> Result<(), MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        c.execute("INSERT INTO dfae_kv (key,value) VALUES ($1,$2) ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value", &[&key, &Json(&value)])
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn audit_append(&self, task_id: &str, kind: &str, payload: Value) -> Result<(), MemoryError> {
        let mut c = self.pool.get().await.map_err(be)?;
        let tx = c.transaction().await.map_err(be)?;
        // serialise appends per task so the hash chain stays linear
        tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&task_id]).await.map_err(be)?;
        let prev: String = tx
            .query_opt("SELECT hash FROM dfae_audit WHERE task_id=$1 ORDER BY seq DESC LIMIT 1", &[&task_id])
            .await
            .map_err(be)?
            .map(|r| r.get(0))
            .unwrap_or_else(|| GENESIS.to_string());
        let hash = chain_hash(&prev, kind, &payload);
        tx.execute(
            "INSERT INTO dfae_audit (id,task_id,ts,kind,payload,hash) VALUES ($1,$2,$3,$4,$5,$6)",
            &[&new_id(), &task_id, &Utc::now(), &kind, &Json(&payload), &hash],
        )
        .await
        .map_err(be)?;
        tx.commit().await.map_err(be)?;
        Ok(())
    }

    async fn audit_list(&self, task_id: &str) -> Result<Vec<AuditRecord>, MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        let rows = c.query("SELECT id,task_id,ts,kind,payload,hash FROM dfae_audit WHERE task_id=$1 ORDER BY seq", &[&task_id]).await.map_err(be)?;
        Ok(rows
            .iter()
            .map(|r| AuditRecord {
                id: r.get("id"),
                task_id: r.get("task_id"),
                ts: r.get("ts"),
                kind: r.get("kind"),
                payload: r.get::<_, Json<Value>>("payload").0,
                hash: r.get("hash"),
            })
            .collect())
    }

    async fn stats(&self) -> Result<MemoryStats, MemoryError> {
        let c = self.pool.get().await.map_err(be)?;
        let tasks: i64 = c.query_one("SELECT count(*) FROM dfae_tasks", &[]).await.map_err(be)?.get(0);
        let rows = c.query("SELECT kind, count(*) FROM dfae_memory GROUP BY kind", &[]).await.map_err(be)?;
        let mut s = MemoryStats { backend: self.backend().into(), tasks: tasks as usize, ..Default::default() };
        for r in rows {
            let k: String = r.get(0);
            let n: i64 = r.get(1);
            match k.as_str() {
                "episodic" => s.episodic = n as usize,
                "semantic" => s.semantic = n as usize,
                "procedural" => s.procedural = n as usize,
                _ => {}
            }
        }
        Ok(s)
    }
}
