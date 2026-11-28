//! The engine and the mcpc broker over their real protocol: the engine's `RemoteBroker` against `dfae-server mcpc-broker`'s API
//! and relay, with a real mcpc behind them. Skipped when mcpc is not installed (`npm install -g @apify/mcpc@0.8.0`, `DFAE_MCPC_BIN`).

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use dfae_mcp::{Broker, BrokerError, McpClient, McpcBroker, McpcConfig, RemoteBroker, Spec};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

async fn serve(app: axum::Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

async fn open_server(Json(m): Json<Value>) -> Response {
    let id = m.get("id").cloned();
    let result = match m["method"].as_str().unwrap_or_default() {
        "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "Echo", "version": "1"}}),
        "tools/list" => json!({"tools": [{"name": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}),
        "tools/call" => json!({"content": [{"type": "text", "text": m["params"]["arguments"]["text"].clone()}]}),
        "ping" => json!({}),
        _ if id.is_none() => return StatusCode::ACCEPTED.into_response(),
        _ => return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})).into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

async fn walled(_: Json<Value>) -> Response {
    (StatusCode::UNAUTHORIZED, [("www-authenticate", "Bearer")], "sign in first").into_response()
}

#[tokio::test]
async fn the_engine_opens_servers_through_the_broker_and_the_relay_only_takes_known_sign_ins() {
    let explicit = std::env::var_os("DFAE_MCPC_BIN").map(std::path::PathBuf::from);
    let Some((bin, _)) = dfae_mcp::find_mcpc(explicit.as_deref()) else {
        eprintln!("SKIPPED: mcpc is not installed");
        return;
    };
    let home = tempfile::tempdir().unwrap().keep().join("mcpc");
    let inner: Arc<dyn Broker> = McpcBroker::new(McpcConfig::new(bin, home));
    let token = "t0ken-shared-by-engine-and-broker-0123456789".to_string();
    let api = serve(dfae_server::broker::api(inner.clone(), token.clone())).await;
    let relay = serve(dfae_server::broker::relay(inner.clone())).await;
    let open = format!("{}/mcp", serve(axum::Router::new().route("/mcp", post(open_server))).await);
    let wall = format!("{}/mcp", serve(axum::Router::new().route("/mcp", post(walled))).await);

    // the engine's side
    let remote = RemoteBroker::new(&api, &token);
    let spec = remote.open("echo", &open, &BTreeMap::new()).await.unwrap();
    let Spec::Http { url, headers } = &spec else { panic!() };
    assert!(url != &open && headers["Authorization"].starts_with("Bearer "), "the proxy, with its token");
    let c = McpClient::connect(&spec, Duration::from_secs(20)).await.unwrap();
    assert_eq!(c.list_tools(Duration::from_secs(20)).await.unwrap()[0].name, "echo");
    assert_eq!(c.call_tool("echo", json!({"text": "through the broker"}), Duration::from_secs(20)).await.unwrap().text, "through the broker");

    // a server that wants a sign-in says so through the broker, as the hub expects it
    assert!(matches!(remote.open("walled", &wall, &BTreeMap::new()).await, Err(BrokerError::NeedsLogin(_))));
    assert!(!remote.signed_in(&wall).await);

    // without the shared token the broker does nothing
    let wrong = RemoteBroker::new(&api, "not-the-token-not-the-token-000");
    assert!(matches!(wrong.open("echo2", &open, &BTreeMap::new()).await, Err(BrokerError::Failed(m)) if m.contains("refused the engine's token")));

    // the relay hands on only answers to sign-ins it started
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let r = http.get(format!("{relay}/callback?code=abc&state=nobody-started-this")).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("unknown or has expired"));
    let r = http.get(format!("{relay}/callback?code=abc")).send().await.unwrap();
    assert_eq!(r.status(), 400);

    remote.close("echo").await;
}
