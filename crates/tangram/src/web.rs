//! The derived web surface: state + actions JSON API, a live SSE state
//! stream, the sync WebSocket, and the app's static UI.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::Stream;
use futures_util::StreamExt;
use serde_json::json;
use tokio_stream::wrappers::WatchStream;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::Model;
use crate::action::{ActionError, Actions};
use crate::store::Store;
use crate::sync;

pub(crate) fn router<M: Model + Actions>(store: Arc<Store<M>>, ui_dir: PathBuf) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/state", get(state::<M>))
        .route("/api/actions", get(list_actions::<M>))
        .route("/api/actions/{name}", axum::routing::post(run_action::<M>))
        .route("/api/events", get(events::<M>))
        .route("/sync", get(sync_ws::<M>))
        // Permissive CORS so embedding hosts can call the API; tighten for
        // apps holding sensitive data.
        .layer(CorsLayer::permissive())
        .fallback_service(ServeDir::new(ui_dir).append_index_html_on_directories(true))
        .with_state(store)
}

async fn state<M: Model + Actions>(State(store): State<Arc<Store<M>>>) -> Json<serde_json::Value> {
    Json(store.state_json())
}

async fn list_actions<M: Model + Actions>(
    State(store): State<Arc<Store<M>>>,
) -> Json<serde_json::Value> {
    let actions: Vec<_> = store
        .action_defs()
        .map(|a| {
            json!({
                "name": a.name,
                "description": a.description,
                "mutates": a.mutates,
                "input_schema": (a.input_schema)(),
            })
        })
        .collect();
    Json(json!({ "actions": actions }))
}

async fn run_action<M: Model + Actions>(
    State(store): State<Arc<Store<M>>>,
    Path(name): Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    let args = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    match store.dispatch(&name, args).await {
        Ok(result) => (StatusCode::OK, Json(json!({ "result": result }))),
        Err(e) => {
            let status = match &e {
                ActionError::Unknown(_) => StatusCode::NOT_FOUND,
                ActionError::BadArgs(_) => StatusCode::BAD_REQUEST,
                ActionError::Failed(_) => StatusCode::UNPROCESSABLE_ENTITY,
                ActionError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({ "error": e.to_string() })))
        }
    }
}

/// Live state stream: emits the full state as JSON immediately on connect and
/// again on every change (local action, MCP tool call, or sync from a peer).
/// Apps' UIs render straight from this stream, so a change made on another
/// instance shows up as soon as the sync frame lands.
async fn events<M: Model + Actions>(
    State(store): State<Arc<Store<M>>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = WatchStream::new(store.subscribe()).map(move |_| {
        Ok(Event::default()
            .event("state")
            .data(store.state_json().to_string()))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn sync_ws<M: Model + Actions>(
    State(store): State<Arc<Store<M>>>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| sync::serve_peer(socket, store))
}
