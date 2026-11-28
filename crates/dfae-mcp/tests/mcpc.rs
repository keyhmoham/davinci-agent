//! The hub with a real mcpc as its broker: remote servers are mcpc sessions, and the engine reaches them through the sessions'
//! proxies. Skipped (with a note) when mcpc is not installed: `npm install -g @apify/mcpc@0.8.0`, or set `DFAE_MCPC_BIN`.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use dfae_core::ToolContext;
use dfae_mcp::{Broker, HubConfig, McpHub, McpcBroker, McpcConfig, Source, Spec};
use dfae_tools::ToolRegistry;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

fn mcpc() -> Option<(Arc<McpcBroker>, std::path::PathBuf)> {
    let explicit = std::env::var_os("DFAE_MCPC_BIN").map(std::path::PathBuf::from);
    let Some((bin, version)) = dfae_mcp::find_mcpc(explicit.as_deref()) else {
        eprintln!("SKIPPED: mcpc is not installed (npm install -g @apify/mcpc@{}, or DFAE_MCPC_BIN)", dfae_mcp::mcpc::MCPC_VERSION);
        return None;
    };
    eprintln!("mcpc {version} at {}", bin.display());
    let home = tempfile::tempdir().unwrap().keep().join("mcpc");
    Some((McpcBroker::new(McpcConfig::new(bin, home.clone())), home))
}

fn ctx() -> ToolContext {
    ToolContext { task_id: "t".into(), step_id: "s".into(), workspace: std::env::temp_dir(), cancel: CancellationToken::new() }
}

/// What the server saw: the `Authorization` header of every request.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Option<String>>>>);

async fn weather(State(seen): State<Seen>, h: HeaderMap, Json(m): Json<Value>) -> Response {
    seen.0.lock().unwrap().push(h.get("authorization").and_then(|v| v.to_str().ok()).map(String::from));
    let id = m.get("id").cloned();
    let result = match m["method"].as_str().unwrap_or_default() {
        "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "Weather", "version": "1"}}),
        "tools/list" => json!({"tools": [{"name": "get-forecast", "description": "Weather forecast for a city", "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}]}),
        "tools/call" => json!({"content": [{"type": "text", "text": format!("Sunny in {}", m["params"]["arguments"]["city"].as_str().unwrap_or("?"))}]}),
        "ping" => json!({}),
        _ if id.is_none() => return StatusCode::ACCEPTED.into_response(),
        _ => return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})).into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

async fn server() -> (String, Seen) {
    let seen = Seen::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    let app = Router::new().route("/mcp", post(weather)).with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{a}/mcp"), seen)
}

async fn sessions(broker: &McpcBroker) -> Vec<(String, String)> {
    let l = broker.run(&[]).await.unwrap();
    l["sessions"].as_array().unwrap().iter().map(|s| (s["name"].as_str().unwrap().to_string(), s["status"].as_str().unwrap_or("").to_string())).collect()
}

#[tokio::test]
async fn a_remote_server_is_an_mcpc_session_reached_through_its_proxy() {
    let Some((broker, home)) = mcpc() else { return };
    let (url, seen) = server().await;
    let registry = ToolRegistry::new();
    let hub = McpHub::with_broker(registry.clone(), HubConfig::default(), None, broker.clone());
    assert_eq!(hub.broker_kind(), "mcpc");
    let mut headers = BTreeMap::new();
    headers.insert("Authorization".to_string(), "Bearer server-key-123".to_string());
    hub.add("weather", Spec::Http { url: url.clone(), headers }, Source::Api, None, "", false).await.unwrap();

    // the server's tools are the engine's, and a call goes engine → proxy → mcpc → server
    let tool = registry.get("mcp__weather__get_forecast").expect("registered");
    let out = tool.execute(json!({"city": "Paris"}), ctx()).await.unwrap();
    assert_eq!(out.content, "Sunny in Paris");
    assert!(sessions(&broker).await.contains(&("@dfae-weather".to_string(), "live".to_string())));
    // the key reached the server through mcpc; it is not in mcpc's session records, nor left in a file of the connect
    assert!(seen.0.lock().unwrap().iter().all(|a| a.as_deref() == Some("Bearer server-key-123")), "{:?}", seen.0.lock().unwrap());
    assert!(!std::fs::read_to_string(home.join("sessions.json")).unwrap().contains("server-key-123"));
    assert!(std::fs::read_dir(&home).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("connect-")));
    // the view shows the server, not the proxy
    assert_eq!(hub.server("weather").unwrap().endpoint, url);

    // the session dies behind the engine's back: the next call opens it again
    broker.run(&["close", "@dfae-weather"]).await.unwrap();
    let out = tool.execute(json!({"city": "Rome"}), ctx()).await.unwrap();
    assert_eq!(out.content, "Sunny in Rome");

    // removing the server closes its session
    assert!(hub.remove("weather").await);
    assert!(!sessions(&broker).await.iter().any(|(n, _)| n == "@dfae-weather"));
}

#[tokio::test]
async fn the_proxy_wants_the_token_only_the_engine_holds() {
    let Some((broker, _)) = mcpc() else { return };
    let (url, _) = server().await;
    let Spec::Http { url: proxy, headers } = broker.open("guarded", &url, &BTreeMap::new()).await.unwrap() else { panic!() };
    assert!(proxy.starts_with("http://127.0.0.1:") && proxy != url, "{proxy}");
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "x", "version": "1"}}});
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let post = |auth: Option<String>| {
        let mut r = http.post(&proxy).header("accept", "application/json, text/event-stream").json(&init);
        if let Some(a) = auth {
            r = r.header("authorization", a);
        }
        r.send()
    };
    // no token: 401; a wrong one: 403
    assert_eq!(post(None).await.unwrap().status(), 401);
    assert!(matches!(post(Some("Bearer guessed".into())).await.unwrap().status().as_u16(), 401 | 403));
    assert!(post(Some(headers["Authorization"].clone())).await.unwrap().status().is_success());
    // opening it again while it is alive is the same session and proxy
    let Spec::Http { url: again, .. } = broker.open("guarded", &url, &BTreeMap::new()).await.unwrap() else { panic!() };
    assert_eq!(again, proxy);
    broker.close("guarded").await;
}
