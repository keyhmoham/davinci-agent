//! Live checks against the real services. Ignored by default:
//! `cargo test -p dfae-tools --test live -- --ignored --nocapture`.
use dfae_core::*;
use dfae_memory::InMemoryStore;
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use serde_json::json;
use std::sync::Arc;

fn setup() -> (ToolRegistry, ToolContext) {
    let d = std::env::temp_dir();
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), d.clone()));
    let mut r = ToolRegistry::new();
    register_native(&mut r, policy, Arc::new(InMemoryStore::new()));
    (r, ToolContext { task_id: "t".into(), step_id: "s".into(), workspace: d, cancel: tokio_util::sync::CancellationToken::new() })
}

#[tokio::test]
#[ignore]
async fn weather_and_search_live() {
    let (r, ctx) = setup();
    let w = r.get("weather").unwrap().execute(json!({"location": "new york"}), ctx.clone()).await;
    println!("WEATHER: {w:?}");
    let s = r.get("web_search").unwrap().execute(json!({"query": "rust async book", "fetch_top": 1}), ctx.clone()).await;
    println!("SEARCH: {s:?}");
}
