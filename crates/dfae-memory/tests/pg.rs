//! Runs only when `DFAE_TEST_DATABASE_URL` points at a disposable database.

use dfae_core::*;
use dfae_memory::{verify_audit_chain, PgStore};

fn url() -> Option<String> {
    std::env::var("DFAE_TEST_DATABASE_URL").ok()
}

#[tokio::test]
async fn pg_store_roundtrip() {
    let Some(url) = url() else {
        eprintln!("skipping: DFAE_TEST_DATABASE_URL not set");
        return;
    };
    let s = PgStore::connect(&url).await.expect("connect");
    assert!(s.health().await);

    // tasks
    let mut t = TaskRecord::new(TaskRequest { goal: "pg test".into(), ..Default::default() }, Budget::default());
    t.status = TaskStatus::Running;
    s.put_task(&t).await.unwrap();
    let got = s.get_task(&t.task_id).await.unwrap().unwrap();
    assert_eq!(got.goal, "pg test");
    assert!(s.unfinished_tasks().await.unwrap().iter().any(|x| x.task_id == t.task_id));
    t.status = TaskStatus::Completed;
    s.put_task(&t).await.unwrap();
    assert!(!s.unfinished_tasks().await.unwrap().iter().any(|x| x.task_id == t.task_id));

    // memory
    let marker = format!("zebra-{}", new_id());
    s.store(MemoryItem::new(MemoryKind::Semantic, format!("the {marker} database lives on host db-staging"), 0.9)).await.unwrap();
    s.store(MemoryItem::new(MemoryKind::Semantic, "lunch is at noon on fridays", 0.5)).await.unwrap();
    let r = s
        .retrieve(MemoryQuery { text: format!("where does the {marker} database live"), kinds: vec![MemoryKind::Semantic], limit: 1, session_id: None })
        .await
        .unwrap();
    assert!(r[0].item.text.contains(&marker), "{:?}", r[0].item.text);

    // kv + audit
    s.kv_set("k", serde_json::json!({"a": 1})).await.unwrap();
    assert_eq!(s.kv_get("k").await.unwrap().unwrap()["a"], 1);
    for i in 0..3 {
        s.audit_append(&t.task_id, "step", serde_json::json!({"i": i})).await.unwrap();
    }
    let recs = s.audit_list(&t.task_id).await.unwrap();
    assert_eq!(recs.len(), 3);
    assert!(verify_audit_chain(&recs));
    assert!(s.stats().await.unwrap().tasks >= 1);
    eprintln!("backend = {}", s.backend());
}
