//! The hub against a fake MCP server (JSON and event-stream answers, a session id) and a fake Glama directory, and against a local
//! stdio server.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dfae_core::{ErrorKind, ToolContext};
use dfae_mcp::{DiscoverError, Glama, HubConfig, McpHub, Source, Spec};
use dfae_tools::ToolRegistry;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

fn ctx() -> ToolContext {
    ToolContext { task_id: "t".into(), step_id: "s".into(), workspace: std::env::temp_dir(), cancel: CancellationToken::new() }
}

#[derive(Clone, Default)]
struct Fake {
    calls: Arc<AtomicUsize>,
    inits: Arc<AtomicUsize>,
    /// The session the server currently knows; `None` after a "restart".
    session: Arc<std::sync::Mutex<Option<String>>>,
}

fn sse(v: Value) -> Response {
    ([("content-type", "text/event-stream")], format!("event: message\ndata: {v}\n\n")).into_response()
}

/// A tiny MCP server: `initialize` (JSON, with a session id), `tools/list` (as an event stream, after a notification), `tools/call`.
async fn mcp(State(f): State<Fake>, h: HeaderMap, Json(m): Json<Value>) -> Response {
    let id = m.get("id").cloned();
    match m["method"].as_str().unwrap_or_default() {
        "initialize" => {
            let n = f.inits.fetch_add(1, Ordering::SeqCst) + 1;
            *f.session.lock().unwrap() = Some(format!("sess-{n}"));
            (
                [("mcp-session-id", format!("sess-{n}"))],
                Json(json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "Weather", "version": "1"}, "instructions": "Cities in English."}})),
            )
                .into_response()
        }
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        other => {
            // every call after `initialize` carries the session
            if h.get("mcp-session-id").and_then(|v| v.to_str().ok()) != f.session.lock().unwrap().as_deref() {
                return (StatusCode::NOT_FOUND, "no session").into_response();
            }
            match other {
                "tools/list" => sse(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                    {"name": "get-forecast", "description": "Weather forecast for a city", "annotations": {"readOnlyHint": true},
                     "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}, "days": {"type": "integer"}, "units": {"type": "object"}}, "required": ["city"]}},
                    {"name": "set_alert", "description": "Create a weather alert", "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}
                ]}})),
                "tools/call" => {
                    f.calls.fetch_add(1, Ordering::SeqCst);
                    let a = &m["params"]["arguments"];
                    if m["params"]["name"] == "set_alert" {
                        return Json(json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": "quota exceeded"}], "isError": true}}))
                            .into_response();
                    }
                    // progress first, then the response
                    ([("content-type", "text/event-stream")], format!(
                        "data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{}}}}\n\ndata: {}\n\n",
                        json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": format!("Sunny in {} for {} days, units {}", a["city"].as_str().unwrap_or("?"), a["days"], a["units"])}]}})
                    )).into_response()
                }
                _ => Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})).into_response(),
            }
        }
    }
}

async fn fake_mcp() -> (String, Fake) {
    let f = Fake::default();
    let base = serve(Router::new().route("/mcp", post(mcp)).with_state(f.clone())).await;
    (format!("{base}/mcp"), f)
}

fn hub(registry: &ToolRegistry, glama: Option<Glama>) -> Arc<McpHub> {
    McpHub::new(registry.clone(), HubConfig { max_auto: 2, ..Default::default() }, glama)
}

/// The directory: an OAuth server, a server that is down, one with an API key, and the one that works.
async fn fake_directory(endpoint: &str) -> String {
    let endpoint = endpoint.to_string();
    let app = Router::new().route(
        "/v1/connectors",
        get(move |h: HeaderMap, q: axum::extract::Query<std::collections::HashMap<String, String>>| {
            let endpoint = endpoint.clone();
            async move {
                if h.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer glm_good") {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"code": "unauthorized", "message": "Invalid or missing API key"}}))).into_response();
                }
                assert_eq!(q.get("sort").map(String::as_str), Some("search-relevance:desc"));
                let c = |id: &str, name: &str, slug: &str, auth: &str, healthy: Value, url: &str| {
                    json!({"id": id, "name": name, "namespace": "ex.test", "slug": slug, "description": format!("{name} server"), "healthy": healthy, "isBoosted": false,
                           "qualityScore": 4.0, "toolCount": 2, "deprecatedAt": null, "attributes": [],
                           "connection": {"authType": auth, "transport": "streamable_http", "url": url},
                           "url": format!("https://glama.ai/mcp/connectors/ex.test/{slug}")})
                };
                Json(json!({"connectors": [
                    c("a000000001", "Oauth Weather", "oauth-weather", "oauth2", Value::Null, "https://oauth.example/mcp"),
                    c("a000000002", "Down Weather", "down-weather", "none", json!(false), "http://127.0.0.1:1/mcp"),
                    c("a000000003", "Keyed Weather", "keyed-weather", "api_key", Value::Null, "https://keyed.example/mcp"),
                    c("a000000004", "Open Weather", "open-weather", "none", json!(true), &endpoint),
                ], "facets": [], "pageInfo": {"endCursor": null, "hasNextPage": false}}))
                .into_response()
            }
        }),
    );
    serve(app).await
}

#[tokio::test]
async fn a_remote_server_becomes_tools_that_run() {
    let (url, fake) = fake_mcp().await;
    let registry = ToolRegistry::new();
    let hub = hub(&registry, None);
    let server = hub.add("Weather", Spec::Http { url, headers: Default::default() }, Source::Api, None, "", false).await.unwrap();
    assert_eq!(server.name, "weather");
    let names = hub.tools_of("weather");
    assert_eq!(names, vec!["mcp__weather__get_forecast", "mcp__weather__set_alert"]);
    let meta = registry.meta("mcp__weather__get_forecast").unwrap();
    assert!(!meta.side_effects && !meta.requires_approval, "a read-only tool");
    assert!(registry.meta("mcp__weather__set_alert").unwrap().side_effects);
    assert_eq!(meta.params.iter().find(|p| p.name == "days").unwrap().ty, dfae_core::ParamType::Integer);
    assert!(meta.validate_args(&json!({"city": "Paris", "days": 3, "units": "{\"t\":\"c\"}"})).is_ok());
    // the object-typed parameter is JSON text for the model and an object for the server
    let out = registry.get("mcp__weather__get_forecast").unwrap().execute(json!({"city": "Paris", "days": 3, "units": "{\"t\":\"c\"}"}), ctx()).await.unwrap();
    assert_eq!(out.content, "Sunny in Paris for 3 days, units {\"t\":\"c\"}");
    // a server's own error is the tool's error, with its text
    let err = registry.get("mcp__weather__set_alert").unwrap().execute(json!({"city": "Paris"}), ctx()).await.unwrap_err();
    assert_eq!((err.kind, err.message.as_str()), (ErrorKind::Other, "quota exceeded"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert_eq!(hub.instructions("weather").as_deref(), Some("Cities in English."));
    assert_eq!(hub.relevant("what is the weather forecast for Paris").await, vec!["weather"]);
    assert!(hub.relevant("write a poem about autumn").await.is_empty());
    // a server that is removed takes its tools with it
    assert!(hub.remove("weather").await);
    assert!(registry.meta("mcp__weather__get_forecast").is_none() && hub.servers().is_empty());
}

#[tokio::test]
async fn a_connection_that_broke_is_opened_again() {
    let (url, fake) = fake_mcp().await;
    let registry = ToolRegistry::new();
    let hub = hub(&registry, None);
    hub.add("weather", Spec::Http { url, headers: Default::default() }, Source::Api, None, "", false).await.unwrap();
    assert_eq!(fake.inits.load(Ordering::SeqCst), 1);
    // the server restarts and forgets its sessions: the next call is answered 404; the hub opens a new session and repeats it
    *fake.session.lock().unwrap() = None;
    let out = registry.get("mcp__weather__get_forecast").unwrap().execute(json!({"city": "Rome"}), ctx()).await.unwrap();
    assert!(out.content.starts_with("Sunny in Rome"), "{}", out.content);
    assert_eq!(fake.inits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn discovery_connects_only_what_can_be_used_as_it_is() {
    let (mcp_url, _) = fake_mcp().await;
    let dir = fake_directory(&mcp_url).await;
    let registry = ToolRegistry::new();
    let hub = hub(&registry, Some(Glama::new(&dir, "glm_good")));
    let found = hub.discover("weather forecast").await.unwrap();
    // OAuth and key-protected servers are skipped, the one that is down is tried and dropped, the open one is connected
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].server, "open_weather");
    assert_eq!(found[0].listing, "https://glama.ai/mcp/connectors/ex.test/open-weather");
    assert_eq!(found[0].tools, vec!["mcp__open_weather__get_forecast", "mcp__open_weather__set_alert"]);
    assert!(!found[0].known);
    // what came from a directory nobody vetted asks before it changes anything
    assert!(registry.meta("mcp__open_weather__set_alert").unwrap().requires_approval);
    assert!(!registry.meta("mcp__open_weather__get_forecast").unwrap().requires_approval);
    let view = hub.servers();
    assert_eq!((view[0].source, view[0].connected), ("glama", true));
    // the same request again finds what is already connected
    let again = hub.discover("weather forecast").await.unwrap();
    assert!(again[0].known);
    assert_eq!(registry.names().iter().filter(|n| n.starts_with("mcp__open_weather__")).count(), 2);
}

#[tokio::test]
async fn a_key_for_a_server_that_wants_one_makes_it_usable() {
    let (mcp_url, _) = fake_mcp().await;
    // the "keyed" server is the working one this time: the directory lists the endpoint with api_key auth
    let endpoint = mcp_url.clone();
    let app = Router::new().route(
        "/v1/connectors",
        get(move || {
            let endpoint = endpoint.clone();
            async move {
                Json(json!({"connectors": [{"id": "b000000001", "name": "Keyed", "namespace": "ex.test", "slug": "keyed", "description": null, "healthy": true,
                    "isBoosted": false, "toolCount": 2, "deprecatedAt": null, "attributes": [],
                    "connection": {"authType": "api_key", "transport": "streamable_http", "url": endpoint}}]}))
            }
        }),
    );
    let dir = serve(app).await;
    let registry = ToolRegistry::new();
    let without = McpHub::new(registry.clone(), HubConfig::default(), Some(Glama::new(&dir, "k")));
    match without.discover("keyed").await {
        Err(DiscoverError::NoneUsable { skipped, .. }) => assert!(skipped[0].contains("DFAE_MCP_KEY_KEYED"), "{skipped:?}"),
        other => panic!("{other:?}"),
    }
    let keys = [("keyed".to_string(), "secret".to_string())].into();
    let with = McpHub::new(registry.clone(), HubConfig { keys, ..Default::default() }, Some(Glama::new(&dir, "k")));
    assert_eq!(with.discover("keyed").await.unwrap()[0].server, "keyed");
}

#[tokio::test]
async fn a_refused_directory_key_says_so() {
    let (mcp_url, _) = fake_mcp().await;
    let dir = fake_directory(&mcp_url).await;
    let hub = hub(&ToolRegistry::new(), Some(Glama::new(&dir, "wrong")));
    let e = hub.discover("weather").await.unwrap_err().to_string();
    assert!(e.contains("GLAMA_API_KEY"), "{e}");
    let none = McpHub::new(ToolRegistry::new(), HubConfig::default(), None);
    assert!(matches!(none.discover("x").await, Err(DiscoverError::NotConfigured)));
}

#[tokio::test]
async fn servers_found_in_one_run_are_known_in_the_next() {
    let (mcp_url, _) = fake_mcp().await;
    let dir = fake_directory(&mcp_url).await;
    let state = tempfile::tempdir().unwrap().keep().join("mcp.json");
    let cfg = || HubConfig { state_path: Some(state.clone()), ..Default::default() };
    let first = McpHub::new(ToolRegistry::new(), cfg(), Some(Glama::new(&dir, "glm_good")));
    first.discover("weather").await.unwrap();
    assert!(std::fs::read_to_string(&state).unwrap().contains("open_weather"));
    let registry = ToolRegistry::new();
    let second = McpHub::new(registry.clone(), cfg(), None);
    second.restore();
    for _ in 0..50 {
        if registry.meta("mcp__open_weather__get_forecast").is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(registry.meta("mcp__open_weather__get_forecast").is_some(), "reopened from the saved state, without asking the directory");
}

/// A server that speaks JSON-RPC lines on stdio (what `npx` and `uvx` servers do), written for the test.
const STDIO_SERVER: &str = r#"
import sys, json
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m:
        continue
    method = m["method"]
    if method == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "Calc", "version": "1"}}
    elif method == "tools/list":
        r = {"tools": [{"name": "add", "description": "Add two numbers", "annotations": {"readOnlyHint": True},
             "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}}]}
    elif method == "tools/call":
        a = m["params"]["arguments"]
        r = {"content": [{"type": "text", "text": str(a["a"] + a["b"])}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "no"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;

#[tokio::test]
async fn a_local_server_speaking_stdio_is_a_server_like_any_other() {
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return; // no interpreter here
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("calc.py");
    std::fs::write(&script, STDIO_SERVER).unwrap();
    let registry = ToolRegistry::new();
    let hub = hub(&registry, None);
    let spec = Spec::Stdio { command: "python3".into(), args: vec![script.to_string_lossy().into()], env: Default::default() };
    hub.add("calc", spec, Source::Config, None, "arithmetic", false).await.unwrap();
    let out = registry.get("mcp__calc__add").unwrap().execute(json!({"a": 2, "b": 40.5}), ctx()).await.unwrap();
    assert_eq!(out.content, "42.5");
    // a command that does not exist is an error, not a hang
    let bad = Spec::Stdio { command: "/no/such/command".into(), args: vec![], env: Default::default() };
    assert!(hub.add("ghost", bad, Source::Config, None, "", false).await.is_err());
    assert!(hub.server("ghost").is_some_and(|s| !s.connected && s.error.is_some()), "kept, with the reason, for the operator to see");
}
