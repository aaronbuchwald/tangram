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
use crate::auth::{self, AuthGate};
use crate::mcp::McpBridge;
use crate::tenant::AppKey;

/// One running app's routes + runtime, as stored in the host's live table.
pub struct AppEntry {
    pub runtime: Arc<AppRuntime>,
    pub router: Router,
    /// For registry apps: the task that nudges the reconciler on every
    /// document change (action, MCP call, or sync from a replica) — desired
    /// state converges exactly like an `apps.toml` edit.
    pub watch_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for AppEntry {
    fn drop(&mut self) {
        if let Some(task) = &self.watch_task {
            task.abort();
        }
    }
}

impl AppEntry {
    /// Build the app's router. With `auth_token` set (registry apps and
    /// `require_auth` apps when the host has TANGRAM_AUTH_TOKEN), the
    /// mutating surfaces are gated: every `POST /api/actions/*` and every
    /// MCP `tools/call` of a mutating tool requires the bearer token; read
    /// routes stay open.
    pub fn new(runtime: AppRuntime, auth_token: Option<&str>) -> Self {
        let runtime = Arc::new(runtime);
        let mcp_service = StreamableHttpService::new(
            {
                let bridge = McpBridge::new(runtime.clone());
                move || Ok(bridge.clone())
            },
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );

        let mut actions = Router::new()
            .route("/api/actions/{name}", axum::routing::post(run_action))
            .with_state(runtime.clone());
        let mut mcp = Router::new().nest_service("/mcp", mcp_service);
        if let Some(token) = auth_token {
            let gate = Arc::new(AuthGate::new(
                token.to_string(),
                runtime
                    .describe
                    .actions
                    .iter()
                    .filter(|a| a.mutates)
                    .map(|a| a.name.clone()),
            ));
            actions = actions.layer(axum::middleware::from_fn_with_state(
                gate.clone(),
                auth::bearer_guard,
            ));
            mcp = mcp.layer(axum::middleware::from_fn_with_state(gate, auth::mcp_guard));
        }

        let router = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/state", get(state))
            .route("/api/capabilities", get(capabilities))
            .route("/api/actions", get(list_actions))
            .route("/api/events", get(events))
            .route("/sync", axum::routing::post(sync_post))
            .route("/sync/events", get(sync_events))
            .with_state(runtime.clone())
            .merge(actions)
            .merge(mcp)
            // Permissive CORS, same as the SDK's derived surface.
            .layer(CorsLayer::permissive())
            .fallback_service(
                ServeDir::new(&runtime.spec.ui).append_index_html_on_directories(true),
            );
        Self {
            runtime,
            router,
            watch_task: None,
        }
    }
}

/// The root router: index page, fleet status, plus the dynamic
/// `/<app>/...` dispatcher. With `via_gateway` (the PUBLIC listener when the
/// host runs an agentgateway child), MCP paths — per-app `/<app>/mcp` and
/// the aggregate `/mcp` — are reverse-proxied through the gateway; the
/// INTERNAL loopback listener always serves apps directly (it is what the
/// gateway targets, so it must never proxy back — that would loop).
pub fn root_router(host: Arc<Host>, via_gateway: bool) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/fleet", get(fleet))
        .route("/mcp", axum::routing::any(aggregate_mcp))
        .fallback(dispatch_app)
        .with_state((host, via_gateway))
}

/// The aggregate MCP endpoint: every app's tools on ONE session, namespaced
/// `<app>_<tool>` — agentgateway's multiplexing. Without a gateway there is
/// nothing to aggregate and the route 404s (per-app `/<app>/mcp` still
/// serves directly).
async fn aggregate_mcp(
    State((host, via_gateway)): State<(Arc<Host>, bool)>,
    req: Request,
) -> Response {
    match host.gateway.as_ref().filter(|_| via_gateway) {
        Some(gateway) => gateway.proxy(req).await,
        None => (
            StatusCode::NOT_FOUND,
            "no aggregate /mcp endpoint (enable [gateway] in apps.toml); \
             per-app MCP is at /<app>/mcp",
        )
            .into_response(),
    }
}

/// `GET /api/fleet` — live host-level status for every desired TOP-LEVEL
/// app: where its spec came from (file vs registry), whether it is enabled,
/// running, healthy (the live instance answers a state probe), and the
/// last converge error if it failed to start. This is observation of THIS
/// host — deliberately not part of the registry's replicated document.
/// Tenant apps are NOT listed here (this route is unauthenticated); each
/// tenant sees their own at `GET /t/<tenant>/api/fleet`.
async fn fleet(State((host, _)): State<(Arc<Host>, bool)>) -> axum::Json<serde_json::Value> {
    let statuses = host.fleet.read().await.clone();
    let apps = host.apps.read().await;
    let mut out = Vec::with_capacity(statuses.len());
    for (key, status) in statuses.iter().filter(|(key, _)| key.tenant.is_none()) {
        let entry = apps.get(key);
        let healthy = match entry {
            Some(entry) => entry.runtime.healthy().await,
            None => false,
        };
        out.push(json!({
            "name": key.app,
            "source": status.source.as_str(),
            "registry": status.registry,
            "require_auth": status.require_auth,
            "enabled": status.enabled,
            "running": entry.is_some(),
            "healthy": healthy,
            "error": status.error,
        }));
    }
    // Host-level MCP gateway observation (null = direct serving).
    let gateway = host.gateway.as_ref().map(|gw| {
        json!({
            "running": gw.running(),
            "pid": gw.child_pid(),
            "port": gw.port,
        })
    });
    axum::Json(json!({ "apps": out, "gateway": gateway }))
}

/// Route `/<app>` and `/<app>/...` to the app's router, resolved against the
/// LIVE app table — this is what makes apps.toml edits take effect without
/// restarting the host. `/t/...` is the tenant namespace (reserved as an app
/// name) and is handled by [`dispatch_tenant`].
async fn dispatch_app(
    State((host, via_gateway)): State<(Arc<Host>, bool)>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    let Some(without_slash) = path.strip_prefix('/') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (name, rest) = match without_slash.split_once('/') {
        Some((name, rest)) => (name.to_string(), format!("/{rest}")),
        None => (without_slash.to_string(), String::new()),
    };

    if name == "t" {
        return dispatch_tenant(host, via_gateway, req, &rest).await;
    }

    let router = {
        let apps = host.apps.read().await;
        match apps.get(&AppKey::top(&name)) {
            Some(entry) => entry.router.clone(),
            None => return (StatusCode::NOT_FOUND, "no such app").into_response(),
        }
    };

    if rest.is_empty() {
        // The app UIs fetch relative paths, so the prefix must end with a
        // slash for them to resolve (same redirect the shell serves).
        return Redirect::permanent(&format!("/{name}/")).into_response();
    }

    // The MCP plane goes through agentgateway when the host runs one: the
    // gateway hairpins to this app's endpoint on the INTERNAL listener
    // (whose router has via_gateway = false), where the bearer gate on
    // mutating tools still applies — the gateway forwards Authorization.
    if via_gateway
        && (rest == "/mcp" || rest.starts_with("/mcp/"))
        && let Some(gateway) = host.gateway.as_ref()
    {
        // Path untouched: agentgateway matches the same /<app>/mcp prefix.
        return gateway.proxy(req).await;
    }

    forward_to_app(router, req, rest).await
}

/// Strip the route prefix off and forward to the app's router (the prefix is
/// everything before `rest`, which keeps its leading slash).
async fn forward_to_app(router: Router, mut req: Request, rest: String) -> Response {
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

// ── the tenant namespace (RUNTIME_PLAN Phase 5) ─────────────────────────────

/// Split the path after `/t` into (tenant, rest): `/alice` → ("alice", ""),
/// `/alice/` → ("alice", "/"), `/alice/notes/api` → ("alice", "/notes/api").
/// `None` for no tenant segment (`/t`, `/t/`).
fn split_tenant(rest: &str) -> Option<(String, String)> {
    let after = rest.strip_prefix('/')?;
    let (tenant, tail) = match after.split_once('/') {
        Some((tenant, tail)) => (tenant, format!("/{tail}")),
        None => (after, String::new()),
    };
    (!tenant.is_empty()).then(|| (tenant.to_string(), tail))
}

/// Requests under `/t/<tenant>/...`. EVERYTHING here — UI, state, actions,
/// SSE, sync, MCP, the tenant index and fleet — requires `Authorization:
/// Bearer <that tenant's token>`: tenant data is private, unlike the
/// trusted-localhost single-tenant surface where reads stay open. The 401 is
/// uniform across missing header / wrong token / another tenant's token /
/// nonexistent tenant, so the namespace leaks no tenant-existence signal.
async fn dispatch_tenant(host: Arc<Host>, via_gateway: bool, req: Request, rest: &str) -> Response {
    let Some((tenant, rest)) = split_tenant(rest) else {
        return (StatusCode::NOT_FOUND, "no such app").into_response();
    };

    // The Phase-6 seam: request → Principal, resolved exactly once, here
    // (and on the internal listener, which the MCP gateway targets — the
    // gateway hop cannot bypass it). OAuth claims later replace the token
    // table inside resolve_principal without touching anything below.
    let principal = {
        let tokens = host.tenant_tokens.read().await;
        auth::resolve_principal(
            req.headers(),
            &tenant,
            tokens.get(&tenant).map(String::as_str),
        )
    };
    // Everything below acts as `principal` — which must be THIS tenant (a
    // `Local` principal has no business inside a tenant namespace).
    if principal
        .as_ref()
        .and_then(auth::Principal::tenant)
        .is_none_or(|name| name != tenant)
    {
        return auth::tenant_unauthorized();
    }

    match rest.as_str() {
        "" => return Redirect::permanent(&format!("/t/{tenant}/")).into_response(),
        "/" => return tenant_index(&host, &tenant).await,
        "/api/fleet" => return tenant_fleet(&host, &tenant).await,
        _ => {}
    }

    // The per-tenant aggregate MCP endpoint — this tenant's tools only,
    // multiplexed by the gateway (404 without one, like the global /mcp).
    if rest == "/mcp" || rest.starts_with("/mcp/") {
        return match host.gateway.as_ref().filter(|_| via_gateway) {
            Some(gateway) => gateway.proxy(req).await,
            None => (
                StatusCode::NOT_FOUND,
                "no aggregate /t/<tenant>/mcp endpoint (enable [gateway] in apps.toml); \
                 per-app MCP is at /t/<tenant>/<app>/mcp",
            )
                .into_response(),
        };
    }

    let after = &rest[1..];
    let (app, app_rest) = match after.split_once('/') {
        Some((app, more)) => (app.to_string(), format!("/{more}")),
        None => (after.to_string(), String::new()),
    };
    let router = {
        let apps = host.apps.read().await;
        match apps.get(&AppKey::tenant(&tenant, &app)) {
            Some(entry) => entry.router.clone(),
            None => return (StatusCode::NOT_FOUND, "no such app").into_response(),
        }
    };

    if app_rest.is_empty() {
        return Redirect::permanent(&format!("/t/{tenant}/{app}/")).into_response();
    }

    // Per-app MCP through the gateway, same hairpin as top-level apps: the
    // gateway targets this path on the INTERNAL listener, whose tenant
    // dispatch re-checks the bearer (agentgateway forwards Authorization).
    if via_gateway
        && (app_rest == "/mcp" || app_rest.starts_with("/mcp/"))
        && let Some(gateway) = host.gateway.as_ref()
    {
        return gateway.proxy(req).await;
    }

    forward_to_app(router, req, app_rest).await
}

/// `GET /t/<tenant>/api/fleet` — the tenant-scoped twin of `/api/fleet`
/// (bearer-gated by the dispatcher): only this tenant's apps, plus the
/// EFFECTIVE outbound grant (after the ceiling intersection) so a tenant can
/// see what an install actually got.
async fn tenant_fleet(host: &Arc<Host>, tenant: &str) -> Response {
    let statuses = host.fleet.read().await.clone();
    let apps = host.apps.read().await;
    let mut out = Vec::new();
    for (key, status) in statuses
        .iter()
        .filter(|(key, _)| key.tenant.as_deref() == Some(tenant))
    {
        let entry = apps.get(key);
        let healthy = match entry {
            Some(entry) => entry.runtime.healthy().await,
            None => false,
        };
        out.push(json!({
            "name": key.app,
            "source": status.source.as_str(),
            "registry": status.registry,
            "enabled": status.enabled,
            "running": entry.is_some(),
            "healthy": healthy,
            "allow_hosts": status.allow_hosts,
            "error": status.error,
        }));
    }
    axum::Json(json!({ "tenant": tenant, "apps": out })).into_response()
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

/// The root index lists TOP-LEVEL apps only — tenant apps are private to
/// their (authenticated) tenant index at `/t/<tenant>/`.
async fn index(State((host, _)): State<(Arc<Host>, bool)>) -> Html<String> {
    let apps = host.apps.read().await;
    // Deterministic order — alphabetical by app name — so this host, a local
    // replica, and the Cloudflare worker all list apps identically. The live
    // table is a HashMap, whose iteration order is otherwise non-deterministic
    // across processes and even across runs (hash-seed randomization).
    let mut keys: Vec<_> = apps.keys().filter(|key| key.tenant.is_none()).collect();
    keys.sort_by(|a, b| a.app.cmp(&b.app));
    let cards: String = keys
        .into_iter()
        .map(|key| app_card(&key.route_prefix(), &key.app))
        .collect();
    index_page(
        "Tangram host",
        "WASM components running on this host",
        cards,
    )
}

/// `GET /t/<tenant>/` — the tenant's own index (bearer-gated by the
/// dispatcher): just their apps, linked under their namespace.
async fn tenant_index(host: &Arc<Host>, tenant: &str) -> Response {
    let apps = host.apps.read().await;
    // Deterministic order — alphabetical by app name (same rule as `index`).
    let mut keys: Vec<_> = apps
        .keys()
        .filter(|key| key.tenant.as_deref() == Some(tenant))
        .collect();
    keys.sort_by(|a, b| a.app.cmp(&b.app));
    let cards: String = keys
        .into_iter()
        .map(|key| app_card(&key.route_prefix(), &key.app))
        .collect();
    index_page(
        &format!("Tangram — {tenant}"),
        "your apps on this host",
        cards,
    )
    .into_response()
}

fn app_card(prefix: &str, name: &str) -> String {
    format!(
        r#"    <li>
      <a class="app" href="{prefix}/"><strong>{name}</strong><span>WASM component</span></a>
      <div class="endpoints">
        <code>{prefix}/mcp</code>
        <code>{prefix}/sync</code>
      </div>
    </li>
"#
    )
}

fn index_page(title: &str, subtitle: &str, cards: String) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title}</title>
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
    <h1>{title}</h1>
    <p class="sub">{subtitle}</p>
    <ul>
{cards}    </ul>
  </main>
</body>
</html>
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::split_tenant;

    #[test]
    fn tenant_path_parsing() {
        // After the dispatcher strips "/t", `rest` keeps its leading slash.
        assert_eq!(split_tenant("/alice"), Some(("alice".into(), "".into())));
        assert_eq!(split_tenant("/alice/"), Some(("alice".into(), "/".into())));
        assert_eq!(
            split_tenant("/alice/notes/api/state"),
            Some(("alice".into(), "/notes/api/state".into()))
        );
        assert_eq!(
            split_tenant("/alice/api/fleet"),
            Some(("alice".into(), "/api/fleet".into()))
        );
        assert_eq!(
            split_tenant("/alice/mcp"),
            Some(("alice".into(), "/mcp".into()))
        );
        // No tenant segment: "/t" and "/t/" 404 like an unknown app.
        assert_eq!(split_tenant(""), None);
        assert_eq!(split_tenant("/"), None);
    }
}
