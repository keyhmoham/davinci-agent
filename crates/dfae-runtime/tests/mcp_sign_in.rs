#![allow(clippy::field_reassign_with_default)]
//! A request needs a server behind a sign-in: the task says so in the chat with a link, waits, and goes on by itself once the user has
//! signed in (the provider sends the browser back to the engine's callback). The provider here is a fake that does what the MCP
//! authorization spec asks: a 401 with `resource_metadata`, protected resource and authorization server metadata, dynamic client
//! registration, PKCE (S256) and resource indicators.

use async_trait::async_trait;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dfae_core::*;
use dfae_decision::{Calibrator, Decider, LocalDecider};
use dfae_extract::HybridExtractor;
use dfae_mcp::{Glama, HubConfig, McpHub};
use dfae_memory::InMemoryStore;
use dfae_models::{ModelGateway, ScriptedModel};
use dfae_runtime::{Engine, EngineDeps};
use dfae_tools::{PolicyConfig, PolicyEngine, ToolRegistry, register_native};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

/// A code the provider issued: (client_id, redirect_uri, code_challenge, resource).
type Issued = (String, String, String, String);

/// What the provider saw.
#[derive(Clone, Default)]
struct Provider {
    base: Arc<Mutex<String>>,
    /// code → (client_id, redirect_uri, code_challenge, resource)
    codes: Arc<Mutex<HashMap<String, Issued>>>,
    registered: Arc<Mutex<Vec<Value>>>,
    grants: Arc<Mutex<Vec<String>>>,
    /// bearer tokens the MCP server was called with
    seen: Arc<Mutex<Vec<String>>>,
}

fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..=c.len() {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

async fn mcp(State(p): State<Provider>, h: HeaderMap, Json(m): Json<Value>) -> Response {
    let base = p.base.lock().unwrap().clone();
    let token = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(String::from);
    if !token.as_deref().is_some_and(|t| t.starts_with("access-")) {
        return (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp", scope="weather:read""#))],
            "sign in first",
        )
            .into_response();
    }
    p.seen.lock().unwrap().push(token.unwrap());
    let id = m.get("id").cloned();
    let result = match m["method"].as_str().unwrap_or_default() {
        "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "Secure Weather", "version": "1"}}),
        "tools/list" => json!({"tools": [{"name": "get_forecast", "description": "Weather forecast for a city", "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}]}),
        "tools/call" => json!({"content": [{"type": "text", "text": format!("Rain in {}", m["params"]["arguments"]["city"].as_str().unwrap_or("?"))}]}),
        _ => return StatusCode::ACCEPTED.into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

async fn provider() -> (String, Provider) {
    let p = Provider::default();
    let app = Router::new()
        .route("/mcp", post(mcp))
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(|State(p): State<Provider>| async move {
                let base = p.base.lock().unwrap().clone();
                Json(json!({"resource": format!("{base}/mcp"), "authorization_servers": [format!("{base}/auth")], "scopes_supported": ["weather:read"]}))
            }),
        )
        .route(
            "/.well-known/oauth-authorization-server/auth",
            get(|State(p): State<Provider>| async move {
                let base = p.base.lock().unwrap().clone();
                Json(json!({"issuer": format!("{base}/auth"), "authorization_endpoint": format!("{base}/auth/authorize"),
                    "token_endpoint": format!("{base}/auth/token"), "registration_endpoint": format!("{base}/auth/register"),
                    "code_challenge_methods_supported": ["S256"]}))
            }),
        )
        .route(
            "/auth/register",
            post(|State(p): State<Provider>, Json(body): Json<Value>| async move {
                p.registered.lock().unwrap().push(body.clone());
                (StatusCode::CREATED, Json(json!({"client_id": "client-1", "redirect_uris": body["redirect_uris"]})))
            }),
        )
        .route(
            "/auth/authorize",
            get(|State(p): State<Provider>, Query(q): Query<HashMap<String, String>>| async move {
                // the user signs in and agrees: the browser is sent back with a code
                assert_eq!(q["response_type"], "code");
                assert_eq!(q["code_challenge_method"], "S256");
                assert_eq!(q["scope"], "weather:read", "the scope the 401 named");
                let code = format!("code-{}", p.codes.lock().unwrap().len());
                p.codes
                    .lock()
                    .unwrap()
                    .insert(code.clone(), (q["client_id"].clone(), q["redirect_uri"].clone(), q["code_challenge"].clone(), q["resource"].clone()));
                let back = format!("{}?code={code}&state={}", q["redirect_uri"], q["state"]);
                (StatusCode::FOUND, [("location", back)]).into_response()
            }),
        )
        .route(
            "/auth/token",
            post(|State(p): State<Provider>, body: String| async move {
                let f: HashMap<String, String> = reqwest::Url::parse(&format!("http://x/?{body}")).unwrap().query_pairs().into_owned().collect();
                let base = p.base.lock().unwrap().clone();
                assert_eq!(f["resource"], format!("{base}/mcp"), "the token is asked for this MCP server");
                assert_eq!(f["client_id"], "client-1");
                p.grants.lock().unwrap().push(f["grant_type"].clone());
                match f["grant_type"].as_str() {
                    "authorization_code" => {
                        let Some((client, redirect, challenge, resource)) = p.codes.lock().unwrap().remove(&f["code"]) else {
                            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
                        };
                        assert_eq!((client.as_str(), redirect.as_str(), resource.as_str()), ("client-1", f["redirect_uri"].as_str(), f["resource"].as_str()));
                        if b64url(&Sha256::digest(f["code_verifier"].as_bytes())) != challenge {
                            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant", "error_description": "PKCE"}))).into_response();
                        }
                        // a short-lived token: it is refreshed before it is used
                        Json(json!({"access_token": "access-1", "refresh_token": "refresh-1", "expires_in": 10, "token_type": "Bearer"})).into_response()
                    }
                    "refresh_token" if f["refresh_token"] == "refresh-1" => {
                        Json(json!({"access_token": "access-2", "expires_in": 3600, "token_type": "Bearer"})).into_response()
                    }
                    _ => (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response(),
                }
            }),
        )
        .with_state(p.clone());
    let base = serve(app).await;
    *p.base.lock().unwrap() = base.clone();
    (base, p)
}

async fn directory(endpoint: String) -> String {
    serve(Router::new().route(
        "/v1/connectors",
        get(move || {
            let endpoint = endpoint.clone();
            async move {
                Json(json!({"connectors": [{"id": "s000000001", "name": "Secure Weather", "namespace": "ex.test", "slug": "secure-weather",
                    "description": "Forecasts for your account", "healthy": true, "isBoosted": false, "toolCount": 1, "deprecatedAt": null,
                    "attributes": ["auth:oauth2", "oauth:hosted"],
                    "connection": {"authType": "oauth2", "transport": "streamable_http", "url": endpoint},
                    "url": "https://glama.ai/mcp/connectors/ex.test/secure-weather"}]}))
            }
        }),
    ))
    .await
}

struct Oracle(LocalDecider);

#[async_trait]
impl DecisionProvider for Oracle {
    fn name(&self) -> &str {
        "open-model"
    }
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let mut r = self.0.decide(req.clone()).await?;
        if req.kind == DecisionKind::RouteTask {
            r.answers.insert("external_service".into(), Answer::noul(0.95));
        }
        for k in req.questions.keys().filter(|k| k.starts_with("server_")) {
            r.answers.insert(k.clone(), Answer::noul(0.9));
        }
        Ok(r)
    }
}

#[tokio::test]
async fn a_server_behind_a_sign_in_is_offered_with_a_link_and_the_task_goes_on_once_signed_in() {
    let (base, prov) = provider().await;
    let dir = directory(format!("{base}/mcp")).await;
    let ws = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    cfg.mcp.sign_in_wait_secs = 30;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let registry = Arc::new(reg);
    let state = ws.path().join(".state/mcp.json");
    let redirect = "http://localhost:8080/v1/mcp/oauth/callback".to_string();
    let hub = McpHub::new(
        (*registry).clone(),
        HubConfig { redirect_uri: Some(redirect.clone()), state_path: Some(state.clone()), secret: Some("test secret".into()), ..Default::default() },
        Some(Glama::new(&dir, "key")),
    );
    let (worker, planner) = (ScriptedModel::new("worker", vec![]), ScriptedModel::new("planner", vec![]));
    worker.push("weather forecast");
    planner.push(r#"{"steps":[{"id":"s1","tool":"mcp__secure_weather__get_forecast","do":"get the weather forecast for Paris","after":[],"expect":null,"planner":false}]}"#);
    worker.push(r#"{"city": "Paris"}"#);
    let decider = Decider::new(Some(Arc::new(Oracle(LocalDecider::new(cfg.thresholds.clone())))), Arc::new(Calibrator::new()), cfg.thresholds.clone());
    let engine = Engine::new(EngineDeps {
        decider: Arc::new(decider),
        extractor: Arc::new(HybridExtractor::new(None)),
        config: cfg,
        store,
        models: ModelGateway::new(Some(Arc::new(worker.clone())), Some(Arc::new(planner.clone()))),
        tools: registry,
        policy,
    });
    engine.set_mcp(hub.clone());

    // the user: watches the chat for a sign-in link, opens it, and the provider sends the browser back to the engine's callback
    let mut rx = engine.subscribe();
    let user_hub = hub.clone();
    let user = tokio::spawn(async move {
        loop {
            let e = rx.recv().await.unwrap();
            if let EventKind::McpSignInRequired { url, title, .. } = e.kind {
                assert_eq!(title, "Secure Weather");
                let browser = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
                let r = browser.get(&url).send().await.unwrap();
                let back = r.headers()["location"].to_str().unwrap().to_string();
                assert!(back.starts_with("http://localhost:8080/v1/mcp/oauth/callback?"), "{back}");
                let q: HashMap<String, String> = reqwest::Url::parse(&back).unwrap().query_pairs().into_owned().collect();
                // what the callback handler does with the browser's request
                return user_hub.finish_sign_in(&q["state"], q.get("code").map(String::as_str), None).await;
            }
        }
    });

    let id = engine
        .submit(TaskRequest {
            goal: "what is the weather forecast for Paris".into(),
            delegate: Some(false),
            mode: ModeRequest::Escalated,
            ..Default::default()
        })
        .await
        .unwrap();
    let t = engine.wait(&id, Duration::from_secs(60)).await.unwrap();
    assert_eq!(user.await.unwrap().unwrap(), "Secure Weather");
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("Rain in Paris"), "{:?}", t.final_answer);

    let kinds: Vec<String> = engine.backlog(&id).iter().map(|e| serde_json::to_value(&e.kind).unwrap()["type"].as_str().unwrap().to_string()).collect();
    let pos = |k: &str| kinds.iter().position(|x| x == k).unwrap_or_else(|| panic!("no {k} in {kinds:?}"));
    assert!(pos("mcp_sign_in_required") < pos("mcp_signed_in") && pos("mcp_signed_in") < pos("mcp_connected") && pos("mcp_connected") < pos("plan_created"));
    // registered once, as a public client with the engine's callback; the short-lived token was refreshed before use
    let reg = prov.registered.lock().unwrap().clone();
    assert_eq!(reg.len(), 1);
    assert_eq!(reg[0]["redirect_uris"], json!([redirect]));
    assert_eq!(reg[0]["token_endpoint_auth_method"], "none");
    assert_eq!(*prov.grants.lock().unwrap(), vec!["authorization_code", "refresh_token"]);
    assert!(prov.seen.lock().unwrap().iter().all(|t| t == "access-2"), "{:?}", prov.seen.lock().unwrap());
    // the sign-in is kept, sealed: the next run opens the server without asking
    let on_disk = std::fs::read_to_string(ws.path().join(".state/mcp-oauth.json")).unwrap();
    assert!(!on_disk.contains("access-2") && !on_disk.contains("refresh-1"));
    let registry2 = ToolRegistry::new();
    let again = McpHub::new(
        registry2.clone(),
        HubConfig { redirect_uri: Some(redirect), state_path: Some(state), secret: Some("test secret".into()), ..Default::default() },
        None,
    );
    again.restore();
    for _ in 0..50 {
        if registry2.meta("mcp__secure_weather__get_forecast").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(registry2.meta("mcp__secure_weather__get_forecast").is_some(), "reopened with the kept sign-in");
}
