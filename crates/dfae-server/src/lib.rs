//! HTTP + SSE API over the engine.

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dfae_core::*;
use dfae_runtime::Engine;
use futures_util::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub token: Option<Arc<String>>,
}

pub fn router(engine: Engine, token: Option<String>) -> Router {
    let state = AppState { engine, token: token.map(Arc::new) };
    let api = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/info", get(info))
        .route("/v1/tools", get(tools))
        .route("/v1/dispatch", get(dispatch))
        .route("/v1/cost", get(cost))
        .route("/v1/mcp/servers", get(mcp_servers).post(mcp_add))
        .route("/v1/mcp/servers/{name}", axum::routing::delete(mcp_remove))
        .route("/v1/mcp/search", get(mcp_search))
        .route("/v1/board", get(board))
        .route("/v1/agents", get(list_agents).post(define_agent))
        .route("/v1/agents/{name}", axum::routing::delete(remove_agent))
        .route("/v1/routines", get(list_routines).post(add_routine))
        .route("/v1/routines/{id}", get(get_routine).patch(update_routine).delete(remove_routine))
        .route("/v1/routines/{id}/run", post(run_routine))
        .route("/v1/tasks", post(create_task).get(list_tasks))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/tasks/{id}/cancel", post(cancel_task))
        .route("/v1/tasks/{id}/approvals/{aid}", post(decide))
        .route("/v1/tasks/{id}/events", get(task_events))
        .route("/v1/tasks/{id}/audit", get(task_audit))
        .route("/v1/events", get(all_events))
        .route("/v1/memory/search", get(memory_search))
        .route("/v1/memory", post(memory_add))
        .route("/v1/stats", get(stats))
        .route("/v1/calibration", get(calibration))
        .route("/metrics", get(metrics))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .merge(api)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

// ---------------------------------------------------------------- auth -----

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(s): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(tok) = &s.token {
        let given = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        if !ct_eq(given.as_bytes(), tok.as_bytes()) {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "missing or invalid bearer token"}))).into_response();
        }
    }
    next.run(req).await
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<Value>)>;

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (code, Json(json!({"error": msg.into()})))
}

// ------------------------------------------------------------ handlers -----

async fn health(State(s): State<AppState>) -> impl IntoResponse {
    let h = s.engine.health().await;
    (if h.ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE }, Json(h))
}

async fn info(State(s): State<AppState>) -> Json<Value> {
    let cfg = s.engine.config();
    Json(json!({
        "name": "dfae",
        "version": env!("CARGO_PKG_VERSION"),
        "config": {
            "decision": if cfg.decision.base_url.is_some() { "open-model" } else { "local" },
            "decision_model": cfg.decision.model,
            "worker": { "provider": cfg.worker.provider, "model": cfg.worker.model },
            "planner": { "provider": cfg.planner.provider, "model": cfg.planner.model },
            "gliner": cfg.gliner_url.is_some(),
            "database": cfg.database_url.is_some(),
            "budget": cfg.budget,
            "team": cfg.team,
            "economy": cfg.economy,
            "thresholds": cfg.thresholds,
            "allow_auto_approve": cfg.allow_auto_approve,
            "approvals": cfg.approvals,
        },
        "tools": s.engine.tools().iter().map(|t| json!({"name": t.name, "description": t.description, "risk": t.risk, "side_effects": t.side_effects, "signature": t.signature()})).collect::<Vec<_>>(),
    }))
}

/// What each kind of model call costs on this deployment (priors, observations, estimate in use).
async fn cost(State(s): State<AppState>) -> Json<Value> {
    Json(s.engine.cost())
}

/// Admission queue and per-role worker pools.
async fn dispatch(State(s): State<AppState>) -> Json<dfae_runtime::DispatchSnapshot> {
    Json(s.engine.dispatch())
}

async fn tools(State(s): State<AppState>) -> Json<Vec<ToolMetadata>> {
    Json(s.engine.tools())
}

// MCP servers: what is connected, what the directory has, and adding and removing by hand.

async fn mcp_servers(State(s): State<AppState>) -> Json<Value> {
    match s.engine.mcp() {
        Some(h) => Json(json!({"servers": h.servers(), "discovery": h.discovery_enabled(), "credit": dfae_mcp::CREDIT})),
        None => Json(json!({"servers": [], "discovery": false})),
    }
}

#[derive(Deserialize)]
struct McpSearchQ {
    q: String,
    #[serde(default)]
    limit: Option<u32>,
}

async fn mcp_search(State(s): State<AppState>, Query(q): Query<McpSearchQ>) -> ApiResult<Value> {
    let hub = s.engine.mcp().ok_or_else(|| err(StatusCode::NOT_FOUND, "MCP is not configured"))?;
    let found = hub.search(&q.q, q.limit.unwrap_or(10).min(50)).await.map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let results: Vec<Value> = found.iter().map(|c| json!({"connector": c, "usable": c.open()})).collect();
    Ok(Json(json!({"results": results, "credit": dfae_mcp::CREDIT})))
}

#[derive(Deserialize)]
struct McpAdd {
    name: String,
    url: String,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
}

async fn mcp_add(State(s): State<AppState>, Json(a): Json<McpAdd>) -> ApiResult<Value> {
    s.engine.mcp_add_remote(&a.name, &a.url, a.headers).await.map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let hub = s.engine.mcp().ok_or_else(|| err(StatusCode::NOT_FOUND, "MCP is not configured"))?;
    Ok(Json(json!(hub.server(&dfae_mcp::server_name(&a.name)))))
}

async fn mcp_remove(State(s): State<AppState>, Path(name): Path<String>) -> ApiResult<Value> {
    let hub = s.engine.mcp().ok_or_else(|| err(StatusCode::NOT_FOUND, "MCP is not configured"))?;
    if hub.remove(&name).await { Ok(Json(json!({"removed": name}))) } else { Err(err(StatusCode::NOT_FOUND, format!("no MCP server '{name}'"))) }
}

// Routines and the board. What the API asks for is a person's request: routines it adds are enabled (an agent's may be proposals).

async fn list_agents(State(s): State<AppState>) -> Json<Vec<AgentProfile>> {
    Json(s.engine.agent_profiles().await)
}

async fn define_agent(State(s): State<AppState>, Json(v): Json<Value>) -> Result<(StatusCode, Json<AgentProfile>), (StatusCode, Json<Value>)> {
    let p = AgentProfile::from_value(&v).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    s.engine.define_agent(p).await.map(|p| (StatusCode::CREATED, Json(p))).map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

async fn remove_agent(State(s): State<AppState>, Path(name): Path<String>) -> ApiResult<Value> {
    s.engine.remove_agent(&name).await.map(|p| Json(json!({"removed": p.name}))).map_err(|e| err(StatusCode::NOT_FOUND, e))
}

async fn board(State(s): State<AppState>) -> Json<dfae_runtime::routines::Board> {
    Json(s.engine.board().await)
}

async fn list_routines(State(s): State<AppState>) -> Json<Vec<dfae_core::routine::Routine>> {
    Json(s.engine.routines())
}

async fn get_routine(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<dfae_core::routine::Routine> {
    s.engine.routine(&id).map(Json).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no routine '{id}'")))
}

async fn add_routine(
    State(s): State<AppState>,
    Json(spec): Json<dfae_runtime::routines::RoutineSpec>,
) -> Result<(StatusCode, Json<dfae_core::routine::Routine>), (StatusCode, Json<Value>)> {
    s.engine.add_routine(spec, "user").await.map(|r| (StatusCode::CREATED, Json(r))).map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

async fn update_routine(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(spec): Json<dfae_runtime::routines::RoutineSpec>,
) -> ApiResult<dfae_core::routine::Routine> {
    s.engine
        .update_routine(&id, spec)
        .await
        .map(Json)
        .map_err(|e| err(if e.starts_with("no routine") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST }, e))
}

async fn remove_routine(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    s.engine.remove_routine(&id).await.map(|r| Json(json!({"removed": r.id}))).map_err(|e| err(StatusCode::NOT_FOUND, e))
}

#[derive(Deserialize)]
struct RunQ {
    /// `true`: still only if the routine's gates pass.
    #[serde(default)]
    gates: bool,
}

async fn run_routine(State(s): State<AppState>, Path(id): Path<String>, Query(q): Query<RunQ>) -> ApiResult<Value> {
    s.engine.run_routine(&id, q.gates).await.map(|t| Json(json!({"task_id": t}))).map_err(|e| err(StatusCode::BAD_REQUEST, e))
}

#[derive(Serialize)]
struct Created {
    task_id: String,
}

async fn create_task(State(s): State<AppState>, Json(req): Json<TaskRequest>) -> Result<(StatusCode, Json<Created>), (StatusCode, Json<Value>)> {
    match s.engine.submit(req).await {
        Ok(id) => Ok((StatusCode::ACCEPTED, Json(Created { task_id: id }))),
        Err(e) => Err(err(StatusCode::BAD_REQUEST, e)),
    }
}

#[derive(Serialize)]
struct TaskSummary {
    task_id: String,
    session_id: String,
    goal: String,
    /// What the user typed (`goal` may have been made self-contained by the conversation resolver).
    asked: String,
    /// The reply (bounded: the task view has the full text).
    answer: Option<String>,
    error: Option<String>,
    status: TaskStatus,
    mode: Option<ExecMode>,
    verification: Option<VerificationStatus>,
    pending_approvals: usize,
    ledger: LedgerSummary,
    created_at: chrono::DateTime<chrono::Utc>,
    /// Team view: `coordinator` for a task that delegated, a worker role for an agent.
    role: Option<AgentRole>,
    parent_id: Option<String>,
    agents: usize,
}

#[derive(Deserialize)]
struct ListQ {
    limit: Option<usize>,
}

async fn list_tasks(State(s): State<AppState>, Query(q): Query<ListQ>) -> Json<Vec<TaskSummary>> {
    let tasks = s.engine.list_tasks(q.limit.unwrap_or(50).min(500)).await;
    Json(
        tasks
            .into_iter()
            .map(|t| TaskSummary {
                pending_approvals: t.approvals.iter().filter(|a| a.status == ApprovalStatus::Pending).count(),
                ledger: t.ledger.summary(),
                role: t.team.as_ref().map(|i| i.role),
                parent_id: t.team.as_ref().and_then(|i| i.link.as_ref()).map(|l| l.parent_id.clone()),
                agents: t.team.as_ref().map(|i| i.children.len()).unwrap_or(0),
                verification: t.verification.as_ref().map(|v| v.status),
                asked: t.request.goal.clone(),
                answer: t.final_answer.as_deref().map(|a| a.chars().take(2000).collect()),
                error: t.error.as_deref().map(|a| a.chars().take(400).collect()),
                task_id: t.task_id,
                session_id: t.session_id,
                goal: t.goal,
                status: t.status,
                mode: t.mode,
                created_at: t.created_at,
            })
            .collect(),
    )
}

async fn get_task(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<TaskRecord> {
    s.engine.get_task(&id).await.map(Json).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such task"))
}

async fn cancel_task(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    if s.engine.cancel(&id).await { Ok(Json(json!({"cancelled": true}))) } else { Err(err(StatusCode::NOT_FOUND, "task is not running")) }
}

#[derive(Deserialize)]
struct Decision {
    approve: bool,
    #[serde(default)]
    by: Option<String>,
}

async fn decide(State(s): State<AppState>, Path((id, aid)): Path<(String, String)>, Json(d): Json<Decision>) -> ApiResult<Value> {
    s.engine
        .decide_approval(&id, &aid, d.approve, d.by.as_deref().unwrap_or("api"))
        .await
        .map(|_| Json(json!({"approved": d.approve})))
        .map_err(|e| err(StatusCode::CONFLICT, e))
}

fn to_sse(e: &Event) -> SseEvent {
    let kind = serde_json::to_value(&e.kind).ok().and_then(|v| v["type"].as_str().map(String::from)).unwrap_or_else(|| "event".into());
    SseEvent::default().id(e.seq.to_string()).event(kind).json_data(e).unwrap_or_else(|_| SseEvent::default().data("{}"))
}

async fn task_events(State(s): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // subscribe first, then read the backlog, so nothing falls in the gap
    let rx = s.engine.subscribe();
    let after: Option<u64> = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    let backlog: Vec<Event> = s.engine.backlog(&id).into_iter().filter(|e| after.map(|a| e.seq > a).unwrap_or(true)).collect();
    let high = backlog.last().map(|e| e.seq).or(after);
    let tid = id.clone();
    let live = BroadcastStream::new(rx).filter_map(move |r| {
        let tid = tid.clone();
        async move { r.ok().filter(|e| e.task_id == tid && high.map(|h| e.seq > h).unwrap_or(true)) }
    });
    let stream = stream::iter(backlog).chain(live).map(|e| Ok(to_sse(&e)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn all_events(State(s): State<AppState>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(s.engine.subscribe()).filter_map(|r| async move { r.ok() }).map(|e| Ok(to_sse(&e)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn task_audit(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    let recs = s.engine.store().audit_list(&id).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let valid = dfae_memory::verify_audit_chain(&recs);
    Ok(Json(json!({"chain_valid": valid, "records": recs})))
}

#[derive(Deserialize)]
struct MemQ {
    q: String,
    limit: Option<usize>,
}

async fn memory_search(State(s): State<AppState>, Query(q): Query<MemQ>) -> ApiResult<Vec<ScoredMemory>> {
    s.engine
        .store()
        .retrieve(MemoryQuery { text: q.q, kinds: vec![], limit: q.limit.unwrap_or(8).min(50), session_id: None })
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Deserialize)]
struct MemAdd {
    text: String,
    #[serde(default)]
    kind: Option<String>,
}

async fn memory_add(State(s): State<AppState>, Json(m): Json<MemAdd>) -> ApiResult<Value> {
    if m.text.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "text must not be empty"));
    }
    let kind = m.kind.as_deref().and_then(MemoryKind::parse).unwrap_or(MemoryKind::Semantic);
    if kind == MemoryKind::Procedural {
        return Err(err(StatusCode::BAD_REQUEST, "procedural memories are created by the engine from verified plans"));
    }
    let mut item = MemoryItem::new(kind, m.text, 0.6);
    item.metadata = json!({"source": "api"});
    let id = item.id.clone();
    s.engine.store().store(item).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({"id": id})))
}

async fn stats(State(s): State<AppState>) -> Json<dfae_runtime::StatsReport> {
    Json(s.engine.stats().await)
}

async fn calibration(State(s): State<AppState>) -> Json<Value> {
    Json(s.engine.calibration())
}

async fn metrics(State(s): State<AppState>) -> impl IntoResponse {
    let st = s.engine.stats().await;
    let d = s.engine.dispatch();
    let t = &st.totals;
    let body = format!(
        "# TYPE dfae_tasks gauge\ndfae_tasks {}\ndfae_tasks_completed {}\ndfae_tasks_failed {}\ndfae_tasks_verified {}\n\
         dfae_mode_tasks{{mode=\"direct\"}} {}\ndfae_mode_tasks{{mode=\"assisted\"}} {}\ndfae_mode_tasks{{mode=\"escalated\"}} {}\n\
         dfae_tokens{{kind=\"worker\"}} {}\ndfae_tokens{{kind=\"planner\"}} {}\ndfae_tokens{{kind=\"decision\"}} {}\n\
         dfae_teams {}\ndfae_agents {}\ndfae_team_cache_hits {}\n\
         dfae_dispatch_tasks{{state=\"running\"}} {}\ndfae_dispatch_tasks{{state=\"queued\"}} {}\ndfae_dispatch_agents{{state=\"running\"}} {}\ndfae_dispatch_agents_spawned {}\n\
         dfae_tokens_baseline {}\ndfae_tokens_saved {}\ndfae_model_calls{{kind=\"worker\"}} {}\ndfae_model_calls{{kind=\"planner\"}} {}\ndfae_model_calls{{kind=\"decision\"}} {}\n",
        st.tasks,
        st.completed,
        st.failed,
        st.verified,
        st.direct,
        st.assisted,
        st.escalated,
        t.worker_tokens,
        t.planner_tokens,
        t.decision_tokens,
        st.teams,
        st.agents,
        st.team_cache_hits,
        d.tasks.running,
        d.queued,
        d.agents.running,
        d.agents_spawned,
        t.baseline_tokens,
        t.saved_tokens,
        t.worker_calls,
        t.planner_calls,
        t.decision_calls
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}
