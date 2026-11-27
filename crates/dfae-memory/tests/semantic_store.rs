//! The stores with semantic embeddings, for real: paraphrases are found, the shape similarity stays the hash one, rows stored before
//! the model was configured are backfilled. Ignored by default (it calls the gateway; the Postgres part needs a disposable database).
//!
//! ```bash
//! PLANNER_API_KEY=… DFAE_TEST_DATABASE_URL=postgres://… cargo test -p dfae-memory --test semantic_store -- --ignored --nocapture
//! ```

use dfae_core::*;
use dfae_memory::{InMemoryStore, PgStore, RemoteEmbedder};
use std::time::Duration;

fn model() -> RemoteEmbedder {
    let key = std::env::var("PLANNER_API_KEY").expect("PLANNER_API_KEY");
    RemoteEmbedder::new("https://ai-gateway.vercel.sh/v1", "voyage/voyage-4-lite", Some(key), 1024, Duration::from_secs(20)).unwrap()
}

const STORED: &[&str] = &[
    "Lesson: for \"rename calcTotal across src\": fs_edit on one file left the others — what worked: fs_edit with path=src",
    "Lesson: for \"serve the site\": the server ran in the workspace root — what worked: cwd site",
    "server: db01.internal",
    "the user prefers TypeScript for new projects",
];

async fn check(store: &dyn MemoryStore, label: &str) {
    let q = |t: &str| MemoryQuery { text: t.into(), kinds: vec![], limit: 4, session_id: None };
    for (query, want) in [
        ("refactor: give a function a new name in every file of the project", 0),
        ("host the website folder over http", 1),
        ("which machine runs our database?", 2),
        ("start a new web app for me", 3),
    ] {
        let r = store.retrieve(q(query)).await.unwrap();
        let top = &r[0];
        eprintln!(
            "{label}: {query:?} -> {:?} (sim {:.2}, shape {:.2}, semantic {})",
            top.item.text.chars().take(40).collect::<String>(),
            top.similarity,
            top.shape_similarity,
            top.semantic
        );
        assert_eq!(top.item.text, STORED[want], "{label}: {query}");
        assert!(top.semantic, "ranked by meaning");
    }
}

#[tokio::test]
#[ignore = "calls the gateway's embeddings endpoint"]
async fn the_in_memory_store_finds_paraphrases() {
    let s = InMemoryStore::new().with_semantic(Some(model()));
    for t in STORED {
        s.store(MemoryItem::new(MemoryKind::Semantic, *t, 0.6)).await.unwrap();
    }
    check(&s, "memory").await;
}

#[tokio::test]
#[ignore = "calls the gateway; needs DFAE_TEST_DATABASE_URL"]
async fn postgres_adds_the_column_backfills_old_rows_and_finds_paraphrases() {
    let Ok(url) = std::env::var("DFAE_TEST_DATABASE_URL") else {
        eprintln!("skipping: DFAE_TEST_DATABASE_URL not set");
        return;
    };
    // rows stored by a deployment that had no semantic model
    let old = PgStore::connect(&url).await.unwrap();
    for id in ["sem-test-0", "sem-test-1", "sem-test-2", "sem-test-3"] {
        let _ = old.delete(id).await;
    }
    for (i, t) in STORED.iter().enumerate() {
        let mut it = MemoryItem::new(MemoryKind::Semantic, *t, 0.6);
        it.id = format!("sem-test-{i}");
        old.store(it).await.unwrap();
    }
    // the same database, now with the model: the column is added and the old rows are embedded
    let s = PgStore::connect(&url).await.unwrap().with_semantic(Some(model())).await.unwrap();
    let n = s.backfill_semantic().await.unwrap();
    assert!(n >= 4, "backfilled {n}");
    assert_eq!(s.backfill_semantic().await.unwrap(), 0, "nothing left");
    check(&s, "postgres").await;
    for id in ["sem-test-0", "sem-test-1", "sem-test-2", "sem-test-3"] {
        let _ = s.delete(id).await;
    }
}
