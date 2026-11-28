//! `dfae-server mcpc-broker`: mcpc in a container of its own, for an engine elsewhere.
//!
//! The engine's agent runs commands as the engine's user, in the engine's container; mcpc keeps the user's sign-ins in files there
//! when there is no keyring (any container). So mcpc runs here instead, as another user with its own volume, and the engine reaches
//! it through two doors:
//!
//! * the **API** (`POST /v1/<op>`, [`dfae_mcp::remote`]'s protocol), with the token the two share (`DFAE_MCPC_BROKER_TOKEN`);
//! * the sessions' **proxies** (`--proxy 0.0.0.0:<port>`), each with a bearer token only the engine holds.
//!
//! The **relay** is the third, for the user's browser: a provider sends it back to mcpc's loopback callback
//! (`http://127.0.0.1:13316/callback`), which in a container nobody outside can reach. Publish those ports on the host's loopback to
//! the relay, and the browser's answer is handed to the sign-in it belongs to (matched, and checked, by its `state`).

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dfae_mcp::{Broker, BrokerError};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct Api {
    broker: Arc<dyn Broker>,
    token: Arc<String>,
}

/// The broker's API, for the engine.
pub fn api(broker: Arc<dyn Broker>, token: String) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/v1/{op}", post(op))
        .with_state(Api { broker, token: Arc::new(token) })
}

async fn op(State(a): State<Api>, Path(op): Path<String>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    let given = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    if !crate::ct_eq(given.as_bytes(), a.token.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    match dfae_mcp::remote::dispatch(a.broker.as_ref(), &op, &body).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            let status = match e {
                BrokerError::NeedsLogin(_) => StatusCode::UNAUTHORIZED,
                BrokerError::Unsupported(_) => StatusCode::BAD_REQUEST,
                BrokerError::Failed(_) => StatusCode::BAD_GATEWAY,
            };
            // 401 is the engine's token refused: a sign-in the server wants is told apart by the body
            let status = if status == StatusCode::UNAUTHORIZED { StatusCode::CONFLICT } else { status };
            (status, Json(serde_json::to_value(&e).unwrap_or_default())).into_response()
        }
    }
}

/// The relay: where the user's browser lands after signing in (the callback ports, published to it).
pub fn relay(broker: Arc<dyn Broker>) -> Router {
    Router::new().route("/callback", get(callback)).with_state(broker)
}

async fn callback(State(broker): State<Arc<dyn Broker>>, RawQuery(query): RawQuery) -> Response {
    let query = query.unwrap_or_default();
    let Some(state) = dfae_mcp::broker::callback_state(&query) else {
        return crate::sign_in_page(StatusCode::BAD_REQUEST, "Sign-in not recognised", "The provider sent no state back.");
    };
    match broker.deliver(&state, &query).await {
        Ok(()) => crate::sign_in_page(
            StatusCode::OK,
            "Signing you in",
            "You can close this tab and go back to the chat: it says when the sign-in is done, and the task goes on by itself.",
        ),
        Err(e) => crate::sign_in_page(StatusCode::BAD_REQUEST, "Sign-in not completed", &e.to_string()),
    }
}
