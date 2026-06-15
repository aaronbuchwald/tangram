//! Multi-tenant request resolution + scope gating (docs/design/auth.md §4, C3).
//!
//! Activates ONLY under `[auth] mode = "multi-tenant"`. The self-hosted /
//! loopback-trusted default never reaches this module — top-level routes keep
//! their byte-identical behavior. Here the host:
//!
//! 1. resolves a request to a [`Principal::User`] from `Authorization: Bearer
//!    tgp_…` OR a `tgs_…` session cookie, via the host-local [`AccountStore`]
//!    (revocation is immediate — a hash lookup per request, no cache);
//! 2. gates MUTATIONS behind a resolved principal carrying the action's
//!    required scope (`registry:write` for install/remove/enable, `admin` for
//!    admin ops); reads stay open unless `reads_gated`;
//! 3. answers a UNIFORM 401 for every failure (missing / wrong / expired /
//!    revoked / unknown — no existence oracle, auth.md §12).
//!
//! Per-principal data isolation is structural: a `User`'s data path derives
//! from [`Principal::data_dir`] (confined by `tenant::validate_tenant_data_dir`),
//! never from a request parameter, so `User(a)` can never name `User(b)`'s tree.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::accounts::AccountStore;
use crate::auth::{Principal, Scope, ScopeSet};

/// The session cookie name (auth.md §9 C5). HttpOnly + SameSite=Lax are set by
/// the login endpoint; here we only READ it (cookie parsing only, C3).
pub const SESSION_COOKIE: &str = "tangram_session";

/// The `user_id` of the auto-minted local admin (auth.md §7 PAT bootstrap).
pub const LOCAL_ADMIN_USER_ID: &str = "local-admin";

/// Wall-clock milliseconds — the single ambient-clock read for request-time
/// validation (the store itself takes `now_ms`, stays clock-free for tests).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Extract the presented credential plaintext: `Authorization: Bearer <tok>`
/// wins; otherwise the `tangram_session` cookie value. `None` when neither is
/// present.
pub fn presented_credential(headers: &HeaderMap) -> Option<String> {
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        return Some(bearer.to_string());
    }
    session_cookie(headers)
}

/// Parse the `tangram_session` value out of the `Cookie` header (a
/// `name=value; name2=value2` list). Cookie parsing only — no Set-Cookie here.
pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == SESSION_COOKIE).then(|| value.trim().to_string())
    })
}

/// Resolve a multi-tenant request to a [`Principal::User`], or `None` for any
/// failure (the uniform-401 input). Validates the presented credential against
/// the account store by hash lookup at `now_ms`.
pub fn resolve_user(store: &AccountStore, headers: &HeaderMap, now_ms: i64) -> Option<Principal> {
    let token = presented_credential(headers)?;
    let cred = store.validate(&token, now_ms)?;
    Some(Principal::User {
        user_id: cred.user_id,
        email: cred.email,
        groups: cred.groups,
        scopes: cred.scopes,
    })
}

/// The uniform 401 for the multi-tenant top-level surface — identical body for
/// every failure so an unauthenticated probe learns nothing (auth.md §12).
pub fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(serde_json::json!({
            "error": "missing or invalid credential \
                      (send Authorization: Bearer <PAT> or sign in for a session cookie)"
        })),
    )
        .into_response()
}

/// The 403 for an authenticated principal that lacks the required scope —
/// distinct from 401 (you ARE someone, just not allowed). Naming the missing
/// scope is safe: the caller is already authenticated.
pub fn forbidden(scope: Scope) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": format!("this credential lacks the required scope ({})", scope.as_str())
        })),
    )
        .into_response()
}

/// The largest MCP POST body the guard buffers to inspect (mirrors
/// `auth::MCP_BODY_LIMIT`).
const MCP_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// One app's multi-tenant gate: the shared account store, the app's mutating
/// tool names (from `describe()`), and whether reads are gated. Built per app
/// in `AppEntry::new` when the host is multi-tenant.
pub struct PrincipalGate {
    store: Arc<AccountStore>,
    mutating_tools: std::collections::HashSet<String>,
    reads_gated: bool,
}

impl PrincipalGate {
    pub fn new(
        store: Arc<AccountStore>,
        mutating_tools: impl IntoIterator<Item = String>,
        reads_gated: bool,
    ) -> Self {
        Self {
            store,
            mutating_tools: mutating_tools.into_iter().collect(),
            reads_gated,
        }
    }

    fn tool_mutates(&self, name: &str) -> bool {
        self.mutating_tools.contains(name)
    }
}

/// The scope a mutating action requires. Admin ops carry `admin`; every other
/// mutation is `registry:write`. (A finer per-action map can layer on later;
/// the manifest's `mutates` flag is the source of which actions are gated.)
fn required_scope(action: &str) -> Scope {
    if is_admin_action(action) {
        Scope::Admin
    } else {
        Scope::RegistryWrite
    }
}

/// Admin-scoped actions (account / audit management). The audit read tool
/// (C4) and any future account ops live here.
fn is_admin_action(action: &str) -> bool {
    matches!(action, "read_audit" | "list_accounts" | "audit")
}

/// Guard for the action route under multi-tenant mode: every `POST
/// /api/actions/{name}` of a MUTATING action requires a resolved principal
/// with the action's scope. Non-mutating actions (reads) require a principal
/// only when `reads_gated`. The action name is the last path segment.
pub async fn action_guard(
    State(gate): State<Arc<PrincipalGate>>,
    req: Request,
    next: Next,
) -> Response {
    let action = req
        .uri()
        .path()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let mutates = gate.tool_mutates(&action);

    // Reads pass through unless gated.
    if !mutates && !gate.reads_gated {
        return next.run(req).await;
    }

    let principal = match resolve_user(&gate.store, req.headers(), now_ms()) {
        Some(p) => p,
        None => return unauthorized(),
    };
    if mutates {
        let scope = required_scope(&action);
        if !principal.has_scope(scope) {
            return forbidden(scope);
        }
    } else {
        // A gated read needs at least registry:read.
        if !principal.has_scope(Scope::RegistryRead) {
            return forbidden(Scope::RegistryRead);
        }
    }
    next.run(req).await
}

/// Guard for the MCP endpoint under multi-tenant mode: a `tools/call` of a
/// mutating tool requires the resolved principal + scope; reads pass through
/// (unless `reads_gated`, in which case any POST requires a principal). Mirrors
/// `auth::mcp_guard`'s body-buffering shape.
pub async fn mcp_guard(
    State(gate): State<Arc<PrincipalGate>>,
    req: Request,
    next: Next,
) -> Response {
    if req.method() != Method::POST {
        // The SSE GET stream and other reads stay open unless gated.
        if gate.reads_gated && resolve_user(&gate.store, req.headers(), now_ms()).is_none() {
            return unauthorized();
        }
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MCP_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "mcp body too large").into_response(),
    };
    let mutating = calls_mutating_tool(&gate, &bytes);
    if mutating || gate.reads_gated {
        let principal = match resolve_user(&gate.store, &parts.headers, now_ms()) {
            Some(p) => p,
            None => return unauthorized(),
        };
        let needed = if mutating {
            // We do not know the exact tool's admin-ness cheaply here; mutating
            // MCP tools require registry:write (admin tools are exposed via the
            // action route, which carries the finer check). A read under
            // reads_gated needs registry:read.
            Scope::RegistryWrite
        } else {
            Scope::RegistryRead
        };
        if !principal.has_scope(needed) {
            return forbidden(needed);
        }
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// Does this JSON-RPC payload call a mutating tool? (Same logic as
/// `auth::calls_mutating_tool`; duplicated to keep the gate self-contained.)
fn calls_mutating_tool(gate: &PrincipalGate, body: &[u8]) -> bool {
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

/// Zero-accounts boot: mint a local-admin PAT (Admin + RegistryWrite +
/// RegistryRead) and print the plaintext ONCE (auth.md §7). Idempotent — does
/// nothing once any account exists. Returns the plaintext when one was minted
/// (for tests / first-run UX).
pub fn bootstrap_admin(store: &AccountStore) -> anyhow::Result<Option<String>> {
    if !store.is_empty()? {
        return Ok(None);
    }
    let now = now_ms();
    store.create_account(LOCAL_ADMIN_USER_ID, "admin@localhost", &[], now)?;
    let minted = store.mint_pat(
        LOCAL_ADMIN_USER_ID,
        ScopeSet::all(),
        "local-admin bootstrap",
        now,
        None,
    )?;
    Ok(Some(minted.token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Scope;

    fn store_with_admin() -> (Arc<AccountStore>, String) {
        let store = Arc::new(AccountStore::open_in_memory().unwrap());
        let token = bootstrap_admin(&store).unwrap().expect("minted on empty");
        (store, token)
    }

    fn headers_with(auth: Option<&str>, cookie: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(a) = auth {
            h.insert(header::AUTHORIZATION, a.parse().unwrap());
        }
        if let Some(c) = cookie {
            h.insert(header::COOKIE, c.parse().unwrap());
        }
        h
    }

    #[test]
    fn bootstrap_mints_once_with_full_scope() {
        let (store, token) = store_with_admin();
        assert!(token.starts_with("tgp_"));
        // Re-running is a no-op.
        assert!(bootstrap_admin(&store).unwrap().is_none());
        // The minted PAT validates with full scope.
        let p = resolve_user(
            &store,
            &headers_with(Some(&format!("Bearer {token}")), None),
            1,
        )
        .expect("admin resolves");
        assert!(p.has_scope(Scope::Admin));
        assert!(p.has_scope(Scope::RegistryWrite));
        assert!(p.has_scope(Scope::RegistryRead));
    }

    #[test]
    fn resolves_from_bearer_or_cookie_and_uniform_none() {
        let (store, token) = store_with_admin();
        // Bearer.
        assert!(
            resolve_user(
                &store,
                &headers_with(Some(&format!("Bearer {token}")), None),
                1
            )
            .is_some()
        );
        // Session cookie.
        let session = store.create_session(LOCAL_ADMIN_USER_ID, 1, None).unwrap();
        let cookie = format!("other=1; {SESSION_COOKIE}={session}; x=2");
        assert!(resolve_user(&store, &headers_with(None, Some(&cookie)), 1).is_some());
        // Bearer takes precedence over a (bogus) cookie.
        assert!(
            resolve_user(
                &store,
                &headers_with(
                    Some(&format!("Bearer {token}")),
                    Some("tangram_session=garbage")
                ),
                1
            )
            .is_some()
        );
        // Every failure → None.
        assert!(resolve_user(&store, &HeaderMap::new(), 1).is_none());
        assert!(resolve_user(&store, &headers_with(Some("Bearer tgp_wrong"), None), 1).is_none());
        assert!(resolve_user(&store, &headers_with(Some("Basic xyz"), None), 1).is_none());
        assert!(
            resolve_user(
                &store,
                &headers_with(None, Some("tangram_session=tgs_nope")),
                1
            )
            .is_none()
        );
    }

    #[test]
    fn required_scope_routing() {
        assert_eq!(required_scope("install_app"), Scope::RegistryWrite);
        assert_eq!(required_scope("remove_app"), Scope::RegistryWrite);
        assert_eq!(required_scope("read_audit"), Scope::Admin);
    }

    #[test]
    fn revoked_credential_resolves_to_none_immediately() {
        let (store, token) = store_with_admin();
        let pats = store.list_pats(LOCAL_ADMIN_USER_ID).unwrap();
        assert_eq!(pats.len(), 1);
        assert!(
            resolve_user(
                &store,
                &headers_with(Some(&format!("Bearer {token}")), None),
                1
            )
            .is_some()
        );
        assert!(
            store
                .revoke_pat_by_id(LOCAL_ADMIN_USER_ID, &pats[0].id)
                .unwrap()
        );
        assert!(
            resolve_user(
                &store,
                &headers_with(Some(&format!("Bearer {token}")), None),
                2
            )
            .is_none()
        );
    }
}
