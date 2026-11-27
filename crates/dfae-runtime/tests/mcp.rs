#![allow(clippy::field_reassign_with_default)]
//! A request about an outside service finds its MCP server in the directory, connects it, and the plan uses its tools; a request
//! about something else never sees them.

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{a}")
}

#[derive(Clone, Default)]
struct Weather {
    calls: Arc<AtomicUsize>,
}

async fn mcp(State(w): State<Weather>, Json(m): Json<Value>) -> Response {
    let id = m.get("id").cloned();
    let result = match m["method"].as_str().unwrap_or_default() {
        "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "Weather", "version": "1"}}),
        "tools/list" => json!({"tools": [{"name": "get_forecast", "description": "Weather forecast for a city", "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}, "days": {"type": "integer"}}, "required": ["city"]}}]}),
        "tools/call" => {
            w.calls.fetch_add(1, Ordering::SeqCst);
            let city = m["params"]["arguments"]["city"].as_str().unwrap_or("?");
            // one city answers with a data blob, as real services do
            let text = if city == "Oslo" {
                json!({"city": city, "temp_c": -3, "sky": "snow", "days": [{"d": 1, "t": -3}, {"d": 2, "t": -5}]}).to_string()
            } else {
                format!("Sunny in {city}")
            };
            json!({"content": [{"type": "text", "text": text}]})
        }
        _ => return StatusCode::ACCEPTED.into_response(),
    };
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

async fn directory(endpoint: String) -> String {
    serve(Router::new().route(
        "/v1/connectors",
        get(move || {
            let endpoint = endpoint.clone();
            async move {
                Json(json!({"connectors": [{"id": "c000000001", "name": "Open Weather", "namespace": "ex.test", "slug": "open-weather", "description": "Forecasts",
                    "healthy": true, "isBoosted": false, "toolCount": 1, "deprecatedAt": null, "attributes": [],
                    "connection": {"authType": "none", "transport": "streamable_http", "url": endpoint},
                    "url": "https://glama.ai/mcp/connectors/ex.test/open-weather"}]}))
            }
        }),
    ))
    .await
}

/// A decision model that knows "weather" is about an outside service.
struct Oracle(LocalDecider);

#[async_trait]
impl DecisionProvider for Oracle {
    fn name(&self) -> &str {
        "open-model"
    }
    async fn decide(&self, req: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let mut r = self.0.decide(req.clone()).await?;
        if req.kind == DecisionKind::RouteTask && (req.state.request.contains("weather") || req.state.request.contains("rain")) {
            r.answers.insert("external_service".into(), Answer::noul(0.95));
        }
        // does a known server fit the request? only a weather server fits a weather request
        for (k, q) in &req.questions {
            if let (true, Question::Noul { instructions, .. }) = (k.starts_with("server_"), q) {
                let fits = instructions.contains("weather") && req.state.request.contains("weather");
                r.answers.insert(k.clone(), Answer::noul(if fits { 0.9 } else { 0.1 }));
            }
        }
        Ok(r)
    }
}

struct Rig {
    engine: Engine,
    worker: ScriptedModel,
    planner: ScriptedModel,
    weather: Weather,
    _ws: tempfile::TempDir,
}

async fn rig(with_directory: bool) -> Rig {
    let ws = tempfile::tempdir().unwrap();
    let weather = Weather::default();
    let mcp_url = serve(Router::new().route("/mcp", post(mcp)).with_state(weather.clone())).await;
    let mut cfg = Config::default();
    cfg.workspace = ws.path().to_path_buf();
    cfg.approvals = ApprovalMode::Never;
    let store = Arc::new(InMemoryStore::new());
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), store.clone());
    let registry = Arc::new(reg);
    let glama = if with_directory { Some(Glama::new(&directory(format!("{mcp_url}/mcp")).await, "key")) } else { None };
    let hub = McpHub::new((*registry).clone(), HubConfig::default(), glama);
    let (worker, planner) = (ScriptedModel::new("worker", vec![]), ScriptedModel::new("planner", vec![]));
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
    engine.set_mcp(hub);
    Rig { engine, worker, planner, weather, _ws: ws }
}

async fn run(r: &Rig, goal: &str) -> TaskRecord {
    let id = r.engine.submit(TaskRequest { goal: goal.into(), delegate: Some(false), mode: ModeRequest::Escalated, ..Default::default() }).await.unwrap();
    r.engine.wait(&id, Duration::from_secs(30)).await.unwrap()
}

const PLAN: &str =
    r#"{"steps":[{"id":"s1","tool":"mcp__open_weather__get_forecast","do":"get the weather forecast for Paris","after":[],"expect":null,"planner":false}]}"#;

#[tokio::test]
async fn a_request_about_an_outside_service_finds_connects_and_uses_its_mcp_server() {
    let r = rig(true).await;
    r.worker.push("weather forecast"); // the search query for the directory
    r.planner.push(PLAN);
    r.worker.push(r#"{"city": "Paris"}"#);
    let t = run(&r, "what is the weather forecast for Paris").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    assert!(t.final_answer.as_deref().unwrap().contains("Sunny in Paris"), "{:?}", t.final_answer);
    assert_eq!(r.weather.calls.load(Ordering::SeqCst), 1);
    let connected = r.engine.backlog(&t.task_id).into_iter().find_map(|e| match e.kind {
        EventKind::McpConnected { server, tools, source, listing } => Some((server, tools, source, listing)),
        _ => None,
    });
    let (server, tools, source, listing) = connected.expect("the task is told which server was connected");
    assert_eq!((server.as_str(), source.as_str()), ("open_weather", "glama"));
    assert_eq!(tools, vec!["mcp__open_weather__get_forecast"]);
    assert_eq!(listing.as_deref(), Some("https://glama.ai/mcp/connectors/ex.test/open-weather"));
    // the planner was shown the new tool
    let plan_req = r.planner.requests.lock().iter().find(|q| q.purpose == "plan").cloned().unwrap();
    // (in the request, not in the system prompt: that is the cached prefix every task shares)
    let user = &plan_req.messages[0].content;
    assert!(user.contains("EXTRA TOOLS") && user.contains("mcp__open_weather__get_forecast"), "the planner is told of the MCP tool: {user}");
    assert!(!plan_req.system.contains("mcp__"), "the shared system prompt stays the same");
    assert!(plan_req.schema.as_ref().unwrap().to_string().contains("mcp__open_weather__get_forecast"), "and may plan with it");
    // the directory was asked with the few words the worker wrote, not with the request
    let query = r.worker.requests.lock().iter().find(|q| q.purpose == "mcp_query").cloned().unwrap();
    assert_eq!(query.max_tokens, 40);
}

#[tokio::test]
async fn a_server_that_is_connected_is_used_again_without_asking_the_directory() {
    let r = rig(true).await;
    r.worker.push("weather forecast");
    r.planner.push(PLAN);
    r.worker.push(r#"{"city": "Paris"}"#);
    run(&r, "what is the weather forecast for Paris").await;
    // the second request fits a connected server: no search query is written
    r.planner.push(PLAN);
    r.worker.push(r#"{"city": "Rome"}"#);
    let t = run(&r, "and the weather forecast for Rome").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    assert_eq!(r.worker.requests.lock().iter().filter(|q| q.purpose == "mcp_query").count(), 1);
    assert_eq!(r.weather.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_request_about_something_else_never_sees_mcp_tools() {
    let r = rig(true).await;
    r.worker.push("weather forecast");
    r.planner.push(PLAN);
    r.worker.push(r#"{"city": "Paris"}"#);
    run(&r, "what is the weather forecast for Paris").await;
    r.planner.push(r#"{"steps":[{"id":"s1","tool":"calc","do":"calculate 2 + 2","after":[],"expect":null,"planner":false}]}"#);
    r.worker.push(r#"{"expression": "2 + 2"}"#);
    let t = run(&r, "calculate 2 + 2 and tell me the sum").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    let plans: Vec<GenerationRequest> = r.planner.requests.lock().iter().filter(|q| q.purpose == "plan").cloned().collect();
    assert_eq!(plans.len(), 2);
    let shown = |q: &GenerationRequest| q.messages[0].content.contains("mcp__open_weather__");
    assert!(shown(&plans[0]) && !shown(&plans[1]), "an unrelated task is not told of MCP tools");
    assert!(!plans[1].schema.as_ref().unwrap().to_string().contains("mcp__"), "nor may it plan with them");
    assert_eq!(plans[0].system, plans[1].system, "both share one cached system prompt");
}

#[tokio::test]
async fn without_a_directory_key_nothing_is_searched_and_the_task_goes_on() {
    let r = rig(false).await;
    r.planner.push(r#"{"steps":[{"id":"s1","tool":null,"do":"explain that no weather source is available","after":[],"expect":null,"planner":false}]}"#);
    r.worker.push("I have no weather source.");
    let t = run(&r, "what is the weather forecast for Paris").await;
    assert!(r.engine.backlog(&t.task_id).iter().all(|e| !matches!(e.kind, EventKind::McpConnected { .. })));
    assert!(r.worker.requests.lock().iter().all(|q| q.purpose != "mcp_query"));
}

#[tokio::test]
async fn data_an_mcp_tool_returns_is_turned_into_an_answer_not_shown_raw() {
    let r = rig(true).await;
    r.worker.push("weather forecast");
    r.planner.push(PLAN);
    r.worker.push(r#"{"city": "Oslo"}"#);
    r.worker.push("It is -3 °C with snow in Oslo.");
    let t = run(&r, "what is the weather forecast for Oslo").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?} / {:?}", t.error, t.final_answer);
    let answer = t.final_answer.unwrap();
    assert!(answer.contains("snow in Oslo") && !answer.contains("temp_c"), "{answer}");
}

#[tokio::test]
async fn a_known_server_that_only_shares_words_with_a_request_is_not_attached() {
    let r = rig(true).await;
    r.worker.push("weather forecast");
    r.planner.push(PLAN);
    r.worker.push(r#"{"city": "Paris"}"#);
    run(&r, "what is the weather forecast for Paris").await;
    // the weather server is known now; this request shares its words ("forecast", "city") but is about something else
    r.planner.push(r#"{"steps":[{"id":"s1","tool":"calc","do":"calculate 1200 * 1.1","after":[],"expect":null,"planner":false}]}"#);
    r.worker.push(r#"{"expression": "1200 * 1.1"}"#);
    let t = run(&r, "forecast the revenue of each city: calculate 1200 * 1.1").await;
    assert_eq!(t.status, TaskStatus::Completed, "{:?}", t.error);
    let attached = r.engine.backlog(&t.task_id).into_iter().any(|e| matches!(e.kind, EventKind::McpConnected { .. }));
    assert!(!attached, "the decision model said the weather server does not fit");
    let plan_req = r.planner.requests.lock().iter().rfind(|q| q.purpose == "plan").cloned().unwrap();
    assert!(!plan_req.messages[0].content.contains("mcp__open_weather"), "the planner is not offered its tools");
}
