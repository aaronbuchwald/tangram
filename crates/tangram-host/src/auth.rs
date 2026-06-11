//! Bearer-token auth for mutating surfaces (RUNTIME_PLAN Phase 3).
//!
//! When the host has a `TANGRAM_AUTH_TOKEN`, apps flagged `registry = true`
//! (and any app with `require_auth = true`) get two guards layered onto
//! their router:
//!
//! - [`bearer_guard`] on `POST /api/actions/{name}` — every action POST
//!   requires `Authorization: Bearer <token>`;
//! - [`mcp_guard`] on `/mcp` — JSON-RPC `tools/call` of a MUTATING tool
//!   requires the same header. Reads (initialize, tools/list, non-mutating
//!   tools, the SSE stream) stay open, so agents can browse before they are
//!   trusted to write.
//!
//! Everything else (UI, state, events, sync) is read-or-CRDT surface and
//! stays open; without a token nothing is gated, but the host then refuses
//! to run a registry app on a non-loopback bind (see `main.rs`).

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Largest MCP POST body the guard will buffer to inspect. Matches the
/// scale of action arguments; anything bigger is suspicious for a JSON-RPC
/// message anyway.
const MCP_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// One app's auth gate: the host token plus the app's mutating tool names
/// (from its `describe()` manifest).
pub struct AuthGate {
    token: String,
    mutating_tools: HashSet<String>,
}

impl AuthGate {
    pub fn new(token: String, mutating_tools: impl IntoIterator<Item = String>) -> Self {
        Self {
            token,
            mutating_tools: mutating_tools.into_iter().collect(),
        }
    }

    /// Does the request carry `Authorization: Bearer <token>`?
    pub fn authorized(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|presented| ct_eq(presented.as_bytes(), self.token.as_bytes()))
    }

    fn tool_mutates(&self, name: &str) -> bool {
        self.mutating_tools.contains(name)
    }
}

/// Constant-time byte comparison (length leaks, contents don't).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── the request principal (RUNTIME_PLAN Phase 5 → 6 seam) ───────────────────

/// Who a request acts as. Today there are two kinds: the implicit
/// single-tenant principal (top-level routes, trusted-localhost model) and a
/// tenant authenticated by its bearer token. Phase 6 swaps the token lookup
/// in [`resolve_principal`] for OAuth claims without touching call sites —
/// everything downstream consumes a `Principal`, never a raw header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A top-level request — the single-tenant surface, exactly as before.
    /// (Not minted anywhere yet: top-level routes skip principal resolution
    /// entirely today, preserving byte-identical behavior; Phase 6 starts
    /// constructing it when per-user identity arrives.)
    #[allow(dead_code)]
    Local,
    /// A request authenticated as this tenant.
    Tenant(String),
}

impl Principal {
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Tenant(name) => Some(name.as_str()),
        }
    }
}

/// Resolve a request under `/t/<tenant>/` to a [`Principal`]: the request
/// must carry `Authorization: Bearer <that tenant's token>` (constant-time
/// compare). `expected_token` is the tenant's resolved token — `None` for an
/// unknown tenant or one whose token didn't resolve, which fails exactly
/// like a wrong token: the caller answers a uniform 401 either way, so an
/// unauthenticated probe cannot distinguish "tenant exists, wrong token"
/// from "no such tenant".
pub fn resolve_principal(
    headers: &HeaderMap,
    tenant: &str,
    expected_token: Option<&str>,
) -> Option<Principal> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?;
    let expected = expected_token?;
    ct_eq(presented.as_bytes(), expected.as_bytes()).then(|| Principal::Tenant(tenant.to_string()))
}

/// The uniform 401 for the tenant namespace — same body for a missing
/// header, a wrong token, another tenant's token, and a nonexistent tenant
/// (no existence oracle).
pub fn tenant_unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(serde_json::json!({
            "error": "missing or invalid bearer token for this tenant namespace \
                      (send Authorization: Bearer <the tenant's token>)"
        })),
    )
        .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(serde_json::json!({
            "error": "missing or invalid bearer token \
                      (send Authorization: Bearer <TANGRAM_AUTH_TOKEN>)"
        })),
    )
        .into_response()
}

/// Guard for the action route: every request must present the token.
pub async fn bearer_guard(State(gate): State<Arc<AuthGate>>, req: Request, next: Next) -> Response {
    if gate.authorized(req.headers()) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// Guard for the MCP endpoint: only a `tools/call` of a MUTATING tool needs
/// the token, so the guard buffers POST bodies just far enough to look at
/// the JSON-RPC method + tool name, then forwards the request untouched.
pub async fn mcp_guard(State(gate): State<Arc<AuthGate>>, req: Request, next: Next) -> Response {
    if req.method() != Method::POST || gate.authorized(req.headers()) {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MCP_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "mcp body too large").into_response(),
    };
    if calls_mutating_tool(&gate, &bytes) {
        return unauthorized();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// Does this JSON-RPC payload (single message or batch) call a mutating
/// tool? Unparseable bodies pass through — rmcp rejects them with a proper
/// JSON-RPC error itself.
fn calls_mutating_tool(gate: &AuthGate, body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let messages: &[serde_json::Value] = match &value {
        serde_json::Value::Array(batch) => batch,
        single => std::slice::from_ref(single),
    };
    messages.iter().any(|msg| {
        msg.get("method").and_then(serde_json::Value::as_str) == Some("tools/call")
            && msg
                .pointer("/params/name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| gate.tool_mutates(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::post;
    use tower::ServiceExt as _;

    fn gate() -> Arc<AuthGate> {
        Arc::new(AuthGate::new(
            "sesame".into(),
            ["install_app".to_string(), "remove_app".to_string()],
        ))
    }

    fn actions_router() -> Router {
        Router::new()
            .route("/api/actions/{name}", post(|| async { "ran" }))
            .layer(axum::middleware::from_fn_with_state(gate(), bearer_guard))
    }

    fn mcp_router() -> Router {
        Router::new()
            .route("/mcp", post(|| async { "rpc" }).get(|| async { "sse" }))
            .layer(axum::middleware::from_fn_with_state(gate(), mcp_guard))
    }

    fn req(method: &str, uri: &str, auth: Option<&str>, body: &str) -> Request {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(auth) = auth {
            builder = builder.header(header::AUTHORIZATION, auth);
        }
        builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn action_post_requires_bearer() {
        let cases = [
            (None, StatusCode::UNAUTHORIZED),                 // missing header
            (Some("Bearer wrong"), StatusCode::UNAUTHORIZED), // wrong token
            (Some("Basic sesame"), StatusCode::UNAUTHORIZED), // wrong scheme
            (Some("Bearer sesame"), StatusCode::OK),          // correct
        ];
        for (auth, expected) in cases {
            let res = actions_router()
                .oneshot(req("POST", "/api/actions/install_app", auth, "{}"))
                .await
                .unwrap();
            assert_eq!(res.status(), expected, "auth header {auth:?}");
        }
    }

    fn tools_call(name: &str) -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": {} }
        })
        .to_string()
    }

    #[tokio::test]
    async fn mcp_gates_only_mutating_tools_calls() {
        // mutating tool without token → 401
        let res = mcp_router()
            .oneshot(req("POST", "/mcp", None, &tools_call("install_app")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // mutating tool with token → passes through
        let res = mcp_router()
            .oneshot(req(
                "POST",
                "/mcp",
                Some("Bearer sesame"),
                &tools_call("install_app"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // non-mutating tool without token → passes through
        let res = mcp_router()
            .oneshot(req("POST", "/mcp", None, &tools_call("list_apps")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // reads (initialize / tools/list) without token → pass through
        for method in ["initialize", "tools/list"] {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": method, "params": {}
            })
            .to_string();
            let res = mcp_router()
                .oneshot(req("POST", "/mcp", None, &body))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "method {method}");
        }

        // the GET SSE stream stays open
        let res = mcp_router()
            .oneshot(req("GET", "/mcp", None, ""))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // a batch containing one mutating call is gated
        let batch = format!("[{},{}]", tools_call("list_apps"), tools_call("remove_app"));
        let res = mcp_router()
            .oneshot(req("POST", "/mcp", None, &batch))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // unparseable bodies pass through (rmcp answers with a JSON-RPC error)
        let res = mcp_router()
            .oneshot(req("POST", "/mcp", None, "not json"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn principal_resolution_is_per_tenant_and_uniform_on_failure() {
        let bearer = |token: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            headers
        };
        let alice = Some("alice-token");

        // The right token resolves to that tenant's principal.
        assert_eq!(
            resolve_principal(&bearer("alice-token"), "alice", alice),
            Some(Principal::Tenant("alice".into()))
        );
        // Missing header, wrong token, another tenant's token, wrong scheme,
        // and an unknown tenant (expected_token = None) all fail identically.
        assert_eq!(resolve_principal(&HeaderMap::new(), "alice", alice), None);
        assert_eq!(resolve_principal(&bearer("wrong"), "alice", alice), None);
        assert_eq!(
            resolve_principal(&bearer("bob-token"), "alice", alice),
            None
        );
        assert_eq!(
            resolve_principal(&bearer("alice-token"), "ghost", None),
            None
        );
        let mut basic = HeaderMap::new();
        basic.insert(header::AUTHORIZATION, "Basic alice-token".parse().unwrap());
        assert_eq!(resolve_principal(&basic, "alice", alice), None);

        assert_eq!(Principal::Tenant("alice".into()).tenant(), Some("alice"));
        assert_eq!(Principal::Local.tenant(), None);
    }

    #[test]
    fn ct_eq_compares_exactly() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
        assert!(ct_eq(b"", b""));
    }
}
