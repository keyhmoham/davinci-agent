//! Against real servers on the internet. `cargo test -p dfae-mcp --test live -- --ignored --nocapture`

use dfae_mcp::{McpClient, Spec};
use std::time::Duration;

#[tokio::test]
#[ignore = "needs the internet"]
async fn a_public_streamable_http_server_lists_and_runs_its_tools() {
    let spec = Spec::Http { url: "https://mcp.deepwiki.com/mcp".into(), headers: Default::default() };
    let client = McpClient::connect(&spec, Duration::from_secs(30)).await.expect("initialize");
    assert_eq!(client.info.name, "DeepWiki");
    let tools = client.list_tools(Duration::from_secs(30)).await.expect("tools/list");
    println!("{} tools: {:?}", tools.len(), tools.iter().map(|t| &t.name).collect::<Vec<_>>());
    assert!(tools.iter().any(|t| t.name == "read_wiki_structure"));
    let r = client
        .call_tool("read_wiki_structure", serde_json::json!({"repoName": "modelcontextprotocol/rust-sdk"}), Duration::from_secs(60))
        .await
        .expect("tools/call");
    println!("{}", r.text.chars().take(300).collect::<String>());
    assert!(!r.is_error && !r.text.is_empty());
    client.close().await;
}
