//! Auth for mutating surfaces — two deployment modes (docs/design/auth.md).
//!
//! **Self-hosted (the default, `[auth]` omitted): loopback-trusted.** One
//! trusted user (or trusted LAN); a local connection may use every route and
//! no token is required. The host still refuses to run a registry app on a
//! *non*-loopback bind without a credential (see `main.rs`) — loopback trust
//! is the model, internet exposure is not.
//!
//! **Bearer-token gating** (the guards below) applies when a token is
//! configured — an *exposed* self-host that opts into `TANGRAM_AUTH_TOKEN` —
//! and to every `/t/<tenant>/` request in multi-tenant mode. Apps flagged
//! `registry = true` (and any app with `require_auth = true`) get two guards
//! layered onto their router:
//!
//! - [`bearer_guard`] on `POST /api/actions/{name}` — every action POST
//!   requires `Authorization: Bearer <token>`;
//! - [`mcp_guard`] on `/mcp` — JSON-RPC `tools/call` of a MUTATING tool
//!   requires the same header. Reads (initialize, tools/list, non-mutating
//!   tools, the SSE stream) stay open, so agents can browse before they are
//!   trusted to write.
//!
//! Everything else (UI, state, events, sync) is read-or-CRDT surface and
//! stays open. Richer identity — hashed PATs, HttpOnly sessions, OAuth/OIDC —
//! is forthcoming (auth.md checkpoints C2–C6) and slots into the same
//! [`Principal`] seam.

use std::collections::{BTreeSet, HashSet};
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

/// Does the request carry `Authorization: Bearer <token>` (constant-time)?
/// Used by host-level routes that gate on the host token without building a
/// per-app [`AuthGate`] — currently the artifact upload route (Phase S2b).
pub fn bearer_matches(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| ct_eq(presented.as_bytes(), token.as_bytes()))
}

/// The 401 for the host-level artifact upload route — same shape as the
/// per-app bearer guard (point the uploader at `TANGRAM_AUTH_TOKEN`).
pub fn artifact_unauthorized() -> Response {
    unauthorized()
}

// ── scopes (auth.md §4 step 3, #20 §3.4) ────────────────────────────────────

/// An authorization scope — what a credential may do, checked against the
/// action's required scope (not all-or-nothing). Sourced from the app's
/// mutating-tools manifest in C3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    /// Read the registry / app list / state when reads are gated.
    RegistryRead,
    /// Install / remove / enable apps (the mutating registry surface).
    RegistryWrite,
    /// Administrative operations (audit read, account management).
    Admin,
}

impl Scope {
    /// The on-the-wire / at-rest token for this scope (stable; stored in the
    /// account DB and parsed back by [`ScopeSet::from_db_string`]).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RegistryRead => "registry:read",
            Self::RegistryWrite => "registry:write",
            Self::Admin => "admin",
        }
    }

    /// Parse a single scope token; unknown tokens are dropped by the caller.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "registry:read" => Some(Self::RegistryRead),
            "registry:write" => Some(Self::RegistryWrite),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// Every scope, in a stable order — the basis of [`ScopeSet::all`].
    const ALL: [Self; 3] = [Self::RegistryRead, Self::RegistryWrite, Self::Admin];
}

/// A set of [`Scope`]s carried by a credential / principal. Stored in the
/// account DB as a comma-joined token string ([`Self::to_db_string`] /
/// [`Self::from_db_string`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScopeSet {
    scopes: BTreeSet<Scope>,
}

impl ScopeSet {
    /// The empty scope set (no authority).
    #[allow(dead_code)] // used by C3 callers / tests
    pub fn empty() -> Self {
        Self::default()
    }

    /// Every scope — the local-admin / interactive-session authority.
    pub fn all() -> Self {
        Self {
            scopes: Scope::ALL.into_iter().collect(),
        }
    }

    /// A set from an explicit list of scopes.
    #[allow(dead_code)] // used by C3 callers / tests
    pub fn from_scopes(scopes: impl IntoIterator<Item = Scope>) -> Self {
        Self {
            scopes: scopes.into_iter().collect(),
        }
    }

    /// Does this set contain `scope`?
    pub fn contains(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    /// The at-rest form: scope tokens joined by commas in a stable order.
    pub fn to_db_string(&self) -> String {
        self.scopes
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Parse the at-rest form; unknown / empty tokens are dropped (forward
    /// compatible — a scope this build doesn't know is simply not granted).
    pub fn from_db_string(raw: &str) -> Self {
        Self {
            scopes: raw.split(',').filter_map(Scope::parse).collect(),
        }
    }
}

// ── per-call principal identity for gateway telemetry (observability O2) ────

/// The host-asserted identity header injected on every LLM/MCP request the host
/// proxies to agentgateway (observability O2;
/// `docs/design/gateway-observability-identity.md` §5). agentgateway's generated
/// config copies it into the access-log + OTLP-trace fields (see
/// `gateway::render_config`), so every call lands in telemetry attributed to a
/// specific principal — `local` vs `user:alice/nutrition` (rendered "Aaron ·
/// nutrition" in the O3 UI) — not just "some traffic".
///
/// The value is host-asserted at the trusted proxy boundary: the proxy STRIPS
/// any inbound header of this name before injecting (a sandboxed component / a
/// loopback client cannot forge its own identity), then sets the value computed
/// from the resolved [`Principal`] + the dispatching app/component. This is the
/// smallest-correct O2 slice of the design's eventual signed-JWT plane (§9 open
/// decision): the gateway *labels* on it but does not yet *authorize* on it —
/// the host's existing `mcp_guard` / loopback rule remain the enforcement.
pub const PRINCIPAL_HEADER: &str = "x-tangram-principal";

/// The separator between the principal id and the component in a
/// [`principal_identity`] value. ASCII (HTTP header values must be visible ASCII;
/// the human-facing "Aaron · nutrition" rendering happens in the UI/telemetry
/// layer, O3 — the wire value stays low-cardinality + ASCII).
const COMPONENT_SEP: &str = "/";

/// The structured, low-cardinality-friendly identity value injected as
/// [`PRINCIPAL_HEADER`] (observability O2). Shape: the principal id, optionally
/// suffixed with `/`-joined dispatching component:
///
/// - `user:alice/nutrition` — multi-tenant user `alice`, call from `nutrition`
/// - `tenant:acme/notes`    — a tenant's call from `notes`
/// - `local`                — the self-hosted single user, direct (no component)
/// - `local/guided-learning`— the self-hosted user, call from a component
///
/// Both fields are low-cardinality (a principal id + an app name), so the value
/// is safe as a telemetry dimension — and ASCII, so it is a valid HTTP header
/// value (a `·`-rendered "Aaron · nutrition" is the O3 UI surface, not the wire).
/// App names are already validated path-safe (`config::validate_name`), so they
/// carry no separator/control characters.
pub fn principal_identity(principal: &Principal, component: Option<&str>) -> String {
    let id = principal.telemetry_id();
    match component {
        Some(component) if !component.is_empty() => format!("{id}{COMPONENT_SEP}{component}"),
        _ => id,
    }
}

// ── the request principal (RUNTIME_PLAN Phase 5 → 6 seam) ───────────────────

/// Who a request acts as, in the two-mode model (docs/design/auth.md). Three
/// kinds: the self-hosted single user / loopback-trusted principal
/// ([`Principal::LocalUser`]); a tenant authenticated by its bearer token
/// ([`Principal::Tenant`]); and a multi-tenant user authenticated by a hashed
/// PAT or session ([`Principal::User`], carrying identity + scopes — the
/// credential layer ports the Cloudflare model, auth.md §4). Everything
/// downstream consumes a `Principal`, never a raw header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// The self-hosted single user — loopback-trusted, no token needed.
    /// (Not minted anywhere yet: self-hosted top-level routes skip principal
    /// resolution entirely today, preserving byte-identical behavior; the
    /// loopback-trust middleware that mints it lands in a later checkpoint.)
    #[allow(dead_code)]
    LocalUser,
    /// A request authenticated as this tenant.
    Tenant(String),
    /// A multi-tenant user, authenticated by a hashed PAT or session, carrying
    /// the identity + scopes the credential validated to (auth.md §3).
    #[allow(dead_code)] // minted in C3
    User {
        user_id: String,
        email: String,
        groups: Vec<String>,
        scopes: ScopeSet,
    },
}

impl Principal {
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Self::LocalUser | Self::User { .. } => None,
            Self::Tenant(name) => Some(name.as_str()),
        }
    }

    /// A stable, low-cardinality identifier for telemetry attribution
    /// (observability O2): `local` for the self-hosted single user,
    /// `tenant:<name>` for a tenant, `user:<id>` for a multi-tenant user. Carries
    /// no secret — it is the principal *identity*, never its credential — and is
    /// safe as a trace/log/metric dimension (see [`principal_identity`]).
    pub fn telemetry_id(&self) -> String {
        match self {
            Self::LocalUser => "local".to_string(),
            Self::Tenant(name) => format!("tenant:{name}"),
            Self::User { user_id, .. } => format!("user:{user_id}"),
        }
    }

    /// Does this principal carry `scope`? `LocalUser` and `Tenant` are full
    /// authority within their surface (loopback trust / the tenant's own
    /// namespace), so they have every scope; a `User` is checked against the
    /// scopes its credential validated to.
    pub fn has_scope(&self, scope: Scope) -> bool {
        match self {
            Self::LocalUser | Self::Tenant(_) => true,
            Self::User { scopes, .. } => scopes.contains(scope),
        }
    }

    /// The per-principal data root, RELATIVE to the host data root (auth.md
    /// §3). `LocalUser` owns the whole tree (`.tangram`); a `User` is confined
    /// to its own subtree (`.tangram/<user_id>/`), the same confinement
    /// `tenant.rs`'s `validate_tenant_data_dir` enforces for `/t/<tenant>/`.
    /// `Tenant` has no top-level data root (its data lives under the tenant
    /// tree, routed by the existing tenant machinery).
    #[allow(dead_code)] // consumed by the per-principal registry wiring (C3+)
    pub fn data_dir(&self) -> std::path::PathBuf {
        use std::path::PathBuf;
        match self {
            Self::LocalUser | Self::Tenant(_) => PathBuf::from(".tangram"),
            Self::User { user_id, .. } => PathBuf::from(".tangram").join(user_id),
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
        assert_eq!(Principal::LocalUser.tenant(), None);
    }

    #[test]
    fn scope_set_round_trips_through_db_string() {
        let set = ScopeSet::from_scopes([Scope::RegistryWrite, Scope::Admin]);
        // Stable order, regardless of insertion order.
        assert_eq!(set.to_db_string(), "registry:write,admin");
        let parsed = ScopeSet::from_db_string(&set.to_db_string());
        assert_eq!(parsed, set);
        assert!(parsed.contains(Scope::Admin));
        assert!(!parsed.contains(Scope::RegistryRead));

        // all() carries every scope; empty() none.
        assert!(ScopeSet::all().contains(Scope::RegistryRead));
        assert!(ScopeSet::all().contains(Scope::RegistryWrite));
        assert!(ScopeSet::all().contains(Scope::Admin));
        assert!(!ScopeSet::empty().contains(Scope::Admin));

        // Unknown / empty tokens are dropped (forward compatible).
        let lenient = ScopeSet::from_db_string("admin,,future:scope,registry:read");
        assert_eq!(
            lenient,
            ScopeSet::from_scopes([Scope::Admin, Scope::RegistryRead])
        );
        assert_eq!(ScopeSet::from_db_string("").to_db_string(), "");
    }

    #[test]
    fn principal_scope_authority() {
        // LocalUser and Tenant are full authority within their surface.
        assert!(Principal::LocalUser.has_scope(Scope::Admin));
        assert!(Principal::Tenant("alice".into()).has_scope(Scope::RegistryWrite));
        // A User is checked against its own scope set.
        let user = Principal::User {
            user_id: "u1".into(),
            email: "u@x".into(),
            groups: vec![],
            scopes: ScopeSet::from_scopes([Scope::RegistryRead]),
        };
        assert!(user.has_scope(Scope::RegistryRead));
        assert!(!user.has_scope(Scope::RegistryWrite));
        assert!(!user.has_scope(Scope::Admin));
        assert_eq!(user.tenant(), None);
    }

    #[test]
    fn principal_identity_attributes_by_principal_and_component() {
        // observability O2: the structured, low-cardinality identity value the
        // proxy injects as PRINCIPAL_HEADER.
        let user = Principal::User {
            user_id: "alice".into(),
            email: "a@x".into(),
            groups: vec![],
            scopes: ScopeSet::all(),
        };
        assert_eq!(user.telemetry_id(), "user:alice");
        assert_eq!(
            principal_identity(&user, Some("nutrition")),
            "user:alice/nutrition"
        );
        // No component → bare principal id (a direct/aggregate call).
        assert_eq!(principal_identity(&user, None), "user:alice");
        assert_eq!(principal_identity(&user, Some("")), "user:alice");

        // Tenant + local.
        assert_eq!(
            Principal::Tenant("acme".into()).telemetry_id(),
            "tenant:acme"
        );
        assert_eq!(
            principal_identity(&Principal::Tenant("acme".into()), Some("notes")),
            "tenant:acme/notes"
        );
        assert_eq!(Principal::LocalUser.telemetry_id(), "local");
        assert_eq!(
            principal_identity(&Principal::LocalUser, Some("guided-learning")),
            "local/guided-learning"
        );
        assert_eq!(principal_identity(&Principal::LocalUser, None), "local");

        // The value is a valid HTTP header value (visible ASCII) so the proxy
        // can inject it — the panic-free `from_str` proves no non-ASCII slips in.
        assert!(
            axum::http::HeaderValue::from_str(&principal_identity(&user, Some("nutrition")))
                .is_ok()
        );
        // The identity carries NO secret — it is the principal id, never a
        // token (a leaked label cannot widen authority).
        assert!(!principal_identity(&user, Some("nutrition")).contains("Bearer"));
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
