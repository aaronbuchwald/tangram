//! The derived web surface: state + actions JSON API, a live SSE state
//! stream, the HTTP sync endpoints, and the app's static UI.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{FromRef, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
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

/// Router state: the store plus the per-peer sync sessions (which belong to
/// this serving surface, not to the transport-neutral store).
struct AppState<M> {
    store: Arc<Store<M>>,
    sync_sessions: Arc<sync::Sessions>,
}

impl<M> Clone for AppState<M> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            sync_sessions: self.sync_sessions.clone(),
        }
    }
}

impl<M> FromRef<AppState<M>> for Arc<Store<M>> {
    fn from_ref(state: &AppState<M>) -> Self {
        state.store.clone()
    }
}

pub(crate) fn router<M: Model + Actions>(store: Arc<Store<M>>, ui_dir: PathBuf) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/state", get(state::<M>))
        .route("/api/actions", get(list_actions::<M>))
        .route("/api/actions/{name}", axum::routing::post(run_action::<M>))
        .route("/api/events", get(events::<M>))
        .route("/sync", axum::routing::post(sync_post::<M>))
        .route("/sync/events", get(sync_events::<M>))
        // Permissive CORS so embedding hosts can call the API; tighten for
        // apps holding sensitive data.
        .layer(CorsLayer::permissive())
        .fallback_service(ServeDir::new(ui_dir).append_index_html_on_directories(true))
        .with_state(AppState {
            store,
            sync_sessions: Arc::default(),
        })
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
    let args = body.map_or_else(|| json!({}), |Json(v)| v);
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

/// `POST /sync` — one HTTP sync exchange (see `docs/SYNC_PROTOCOL.md`): the
/// body carries zero or one automerge sync message from the peer identified
/// by `X-Tangram-Session`; the response carries every message we owe it,
/// length-framed.
async fn sync_post<M: Model + Actions>(
    State(app): State<AppState<M>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(session) = headers
        .get("x-tangram-session")
        .and_then(|v| v.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing X-Tangram-Session header").into_response();
    };
    match sync::handle_post(&*app.store, &app.sync_sessions, session, &body) {
        Ok(frames) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], frames).into_response()
        }
        Err(e) => {
            tracing::warn!("rejecting bad sync message: {e}");
            (StatusCode::BAD_REQUEST, "bad sync message").into_response()
        }
    }
}

/// `GET /sync/events` — SSE poke stream: an `event: poke` on connect and on
/// every document change, telling the peer to run a `POST /sync` round. The
/// `session` query parameter is accepted for symmetry with the protocol doc
/// but unused here (sessions are tracked by the POST handler).
async fn sync_events<M: Model + Actions>(
    State(store): State<Arc<Store<M>>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream =
        WatchStream::new(store.subscribe()).map(|_| Ok(Event::default().event("poke").data("")));
    Sse::new(stream).keep_alive(KeepAlive::default())
}
