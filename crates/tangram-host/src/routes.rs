//! The host's web surface: an index of running apps at `/`, and per app the
//! same derived surface a native Tangram binary serves — static UI, state +
//! actions JSON API, SSE state stream, the HTTP sync protocol, MCP — with
//! routes appearing and disappearing as the reconciler converges (each
//! request resolves the app from the live table, so there is no rebuild step
//! when `apps.toml` changes).

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use futures_util::{Stream, StreamExt};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::json;
use tangram::sync::DocHandle as _;
use tokio_stream::wrappers::WatchStream;
use tower::ServiceExt as _;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::Host;
use crate::app::{AppRuntime, DispatchError};
use crate::mcp::McpBridge;

/// One running app's routes + runtime, as stored in the host's live table.
pub struct AppEntry {
    pub runtime: Arc<AppRuntime>,
    pub router: Router,
}

impl AppEntry {
    pub fn new(runtime: AppRuntime) -> Self {
        let runtime = Arc::new(runtime);
        let mcp_service = StreamableHttpService::new(
            {
                let bridge = McpBridge::new(runtime.clone());
                move || Ok(bridge.clone())
            },
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );
        let router = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/state", get(state))
            .route("/api/capabilities", get(capabilities))
            .route("/api/actions", get(list_actions))
            .route("/api/actions/{name}", axum::routing::post(run_action))
            .route("/api/events", get(events))
            .route("/sync", axum::routing::post(sync_post))
            .route("/sync/events", get(sync_events))
            .nest_service("/mcp", mcp_service)
            // Permissive CORS, same as the SDK's derived surface.
            .layer(CorsLayer::permissive())
            .fallback_service(
                ServeDir::new(&runtime.spec.ui).append_index_html_on_directories(true),
            )
            .with_state(runtime.clone());
        Self { runtime, router }
    }
}

/// The root router: index page plus the dynamic `/<app>/...` dispatcher.
pub fn root_router(host: Arc<Host>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(dispatch_app)
        .with_state(host)
}

/// Route `/<app>` and `/<app>/...` to the app's router, resolved against the
/// LIVE app table — this is what makes apps.toml edits take effect without
/// restarting the host.
async fn dispatch_app(State(host): State<Arc<Host>>, mut req: Request) -> Response {
    let path = req.uri().path().to_string();
    let Some(without_slash) = path.strip_prefix('/') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (name, rest) = match without_slash.split_once('/') {
        Some((name, rest)) => (name.to_string(), format!("/{rest}")),
        None => (without_slash.to_string(), String::new()),
    };

    let router = {
        let apps = host.apps.read().await;
        match apps.get(&name) {
            Some(entry) => entry.router.clone(),
            None => return (StatusCode::NOT_FOUND, "no such app").into_response(),
        }
    };

    if rest.is_empty() {
        // The app UIs fetch relative paths, so the prefix must end with a
        // slash for them to resolve (same redirect the shell serves).
        return Redirect::permanent(&format!("/{name}/")).into_response();
    }

    // Strip the `/<app>` prefix and forward to the app's router.
    let path_and_query = match req.uri().query() {
        Some(query) => format!("{rest}?{query}"),
        None => rest,
    };
    match path_and_query.parse::<Uri>() {
        Ok(uri) => *req.uri_mut() = uri,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    }
    match router.oneshot(req).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

// ── per-app handlers (the SDK's derived surface, component-backed) ──────────

async fn state(State(rt): State<Arc<AppRuntime>>) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        rt.state_json().await,
    )
        .into_response()
}

/// The app's capabilities object from its `describe()` manifest (computed by
/// the component at instantiation from its granted env). Apps that publish
/// none get a 404, matching a native app without the custom probe route.
async fn capabilities(State(rt): State<Arc<AppRuntime>>) -> Response {
    match &rt.describe.capabilities {
        Some(caps) => axum::Json(caps.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn list_actions(State(rt): State<Arc<AppRuntime>>) -> axum::Json<serde_json::Value> {
    let actions: Vec<_> = rt
        .describe
        .actions
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "description": a.description,
                "mutates": a.mutates,
                "input_schema": a.input_schema,
            })
        })
        .collect();
    axum::Json(json!({ "actions": actions }))
}

/// Same error envelope and status mapping as the SDK's action route.
async fn run_action(
    State(rt): State<Arc<AppRuntime>>,
    Path(name): Path<String>,
    body: Option<axum::Json<serde_json::Value>>,
) -> impl IntoResponse {
    let args = body.map(|axum::Json(v)| v).unwrap_or_else(|| json!({}));
    match rt.dispatch(&name, args).await {
        Ok(result) => (StatusCode::OK, axum::Json(json!({ "result": result }))),
        Err(e) => {
            let status = match &e {
                DispatchError::Unknown(_) => StatusCode::NOT_FOUND,
                DispatchError::BadArgs(_) => StatusCode::BAD_REQUEST,
                DispatchError::Failed(_) => StatusCode::UNPROCESSABLE_ENTITY,
                DispatchError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, axum::Json(json!({ "error": e.to_string() })))
        }
    }
}

/// Live state stream: the full state as JSON on connect and on every change
/// (action, MCP tool call, or sync from a peer) — rendered by the component.
async fn events(
    State(rt): State<Arc<AppRuntime>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = WatchStream::new(rt.doc.subscribe()).then(move |_| {
        let rt = rt.clone();
        async move { Ok(Event::default().event("state").data(rt.state_json().await)) }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `POST /sync` — one HTTP sync exchange, via the SDK's shared server core
/// (`docs/SYNC_PROTOCOL.md`).
async fn sync_post(State(rt): State<Arc<AppRuntime>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(session) = headers
        .get("x-tangram-session")
        .and_then(|v| v.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing X-Tangram-Session header").into_response();
    };
    match tangram::sync::handle_post(&*rt.doc, &rt.sessions, session, &body) {
        Ok(frames) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], frames).into_response()
        }
        Err(e) => {
            tracing::warn!("rejecting bad sync message: {e}");
            (StatusCode::BAD_REQUEST, "bad sync message").into_response()
        }
    }
}

/// `GET /sync/events` — SSE poke stream (connect + every document change).
async fn sync_events(
    State(rt): State<Arc<AppRuntime>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream =
        WatchStream::new(rt.doc.subscribe()).map(|_| Ok(Event::default().event("poke").data("")));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── index ────────────────────────────────────────────────────────────────────

async fn index(State(host): State<Arc<Host>>) -> Html<String> {
    let apps = host.apps.read().await;
    let cards: String = apps
        .values()
        .map(|entry| {
            let name = &entry.runtime.name;
            format!(
                r#"    <li>
      <a class="app" href="/{name}/"><strong>{name}</strong><span>WASM component</span></a>
      <div class="endpoints">
        <code>/{name}/mcp</code>
        <code>/{name}/sync</code>
      </div>
    </li>
"#
            )
        })
        .collect();
    Html(format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Tangram host</title>
  <style>
    :root {{ color-scheme: dark; }}
    body {{
      margin: 0; min-height: 100vh; display: grid; place-content: center;
      background: #14161a; color: #e6e8eb;
      font: 16px/1.5 system-ui, -apple-system, sans-serif;
    }}
    main {{ padding: 3rem 1.5rem; max-width: 36rem; }}
    h1 {{ font-size: 1.4rem; margin: 0 0 0.25rem; }}
    p.sub {{ color: #9aa0a8; margin: 0 0 2rem; }}
    ul {{ list-style: none; margin: 0; padding: 0; display: grid; gap: 1rem; }}
    a.app {{
      display: block; padding: 1rem 1.25rem; border-radius: 10px;
      background: #1d2026; border: 1px solid #2a2e36;
      color: inherit; text-decoration: none;
    }}
    a.app:hover {{ border-color: #4a90d9; }}
    a.app strong {{ display: block; font-size: 1.1rem; }}
    a.app span {{ color: #9aa0a8; font-size: 0.9rem; }}
    .endpoints {{ margin: 0.4rem 0.25rem 0; display: flex; gap: 0.75rem; }}
    .endpoints code {{
      font-size: 0.78rem; color: #7d8590; background: #1a1d22;
      padding: 0.1rem 0.45rem; border-radius: 5px;
    }}
  </style>
</head>
<body>
  <main>
    <h1>Tangram host</h1>
    <p class="sub">WASM components running on this host</p>
    <ul>
{cards}    </ul>
  </main>
</body>
</html>
"#
    ))
}
