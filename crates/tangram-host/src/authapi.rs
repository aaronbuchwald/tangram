//! The host's session + PAT HTTP API for the shell UI (docs/design/auth.md §9
//! C5, §14). Multi-tenant only — in self-hosted mode the shell shows no auth UI
//! and these routes report `mode = "self-hosted"` / 404, so nothing changes for
//! the loopback-trusted default.
//!
//! Endpoints (mounted at the host root in `routes::root_router`):
//!
//! - `GET  /api/auth`         → `{ mode, principal }` — the UI branches on this
//! - `POST /api/auth/login`   → validate a pasted PAT, mint a session, set the
//!   HttpOnly `tangram_session` cookie (SameSite=Lax, 30-day TTL)
//! - `POST /api/auth/logout`  → revoke the session + clear the cookie
//! - `GET  /api/auth/pats`    → list the caller's own PATs (metadata only)
//! - `POST /api/auth/pats`    → mint a PAT for the caller (token shown ONCE)
//! - `DELETE /api/auth/pats/{id}` → revoke one of the caller's PATs
//!
//! Every PAT route is SELF-scoped: the principal is resolved from the request's
//! own credential, and the store's `*_by_id` / `list_pats` are keyed by that
//! principal's `user_id`, so a caller can only ever touch their own tokens.
//! Minting/listing/revoking a PAT requires a SESSION (the interactive
//! authority), not a PAT — a PAT is a programmatic credential, not a key to
//! manufacture more keys from a web box.

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::accounts::{AccountStore, CredentialKind};
use crate::auth::{Principal, Scope, ScopeSet};
use crate::multitenant::{self, SESSION_COOKIE, now_ms};

/// 30 days in milliseconds — the UI session TTL (auth.md §4, §9 C5).
const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// `GET /api/auth` — the mode + current principal so the shell can branch
/// (self-hosted → no auth UI; multi-tenant → login view / principal chip). In
/// self-hosted mode `principal` is `null` and the UI shows nothing; in
/// multi-tenant mode it is the resolved user or `null` when unauthenticated.
pub async fn auth_state(
    store: Option<&Arc<AccountStore>>,
    oauth_enabled: bool,
    headers: &HeaderMap,
) -> Response {
    let Some(store) = store else {
        // Self-hosted: loopback-trusted, no auth UI.
        return axum::Json(serde_json::json!({
            "mode": "self-hosted",
            "principal": serde_json::Value::Null,
            "oauth": false,
        }))
        .into_response();
    };
    let principal = multitenant::resolve_user(store, headers, now_ms());
    let principal_json = match principal {
        Some(Principal::User {
            user_id,
            email,
            groups,
            scopes,
        }) => serde_json::json!({
            "user_id": user_id,
            "email": email,
            "groups": groups,
            "scopes": scopes.to_db_string().split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        }),
        _ => serde_json::Value::Null,
    };
    axum::Json(serde_json::json!({
        "mode": "multi-tenant",
        "principal": principal_json,
        // Whether OAuth sign-in is available (the UI shows the GitHub button).
        "oauth": oauth_enabled,
    }))
    .into_response()
}

/// `POST /api/auth/login` — exchange a pasted PAT (`{ "token": "tgp_…" }`) for
/// an HttpOnly session cookie (auth.md §14 shape B). The PAT must validate; the
/// session inherits the account it belongs to and gets a 30-day TTL. Uniform
/// 401 on any bad/expired/revoked token (no existence oracle).
pub async fn login(store: &Arc<AccountStore>, body: &serde_json::Value) -> Response {
    let Some(token) = body.get("token").and_then(serde_json::Value::as_str) else {
        return multitenant::unauthorized();
    };
    let now = now_ms();
    // The pasted credential must validate. A session is the interactive
    // authority; we only mint one from a credential that already resolves.
    let Some(cred) = store.validate(token, now) else {
        return multitenant::unauthorized();
    };
    let session = match store.create_session(&cred.user_id, now, Some(now + SESSION_TTL_MS)) {
        Ok(token) => token,
        Err(e) => {
            tracing::error!(
                "login: failed to create session for {}: {e:#}",
                cred.user_id
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "login failed").into_response();
        }
    };
    let cookie = format!(
        "{SESSION_COOKIE}={session}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        SESSION_TTL_MS / 1000
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        axum::Json(serde_json::json!({
            "user_id": cred.user_id,
            "email": cred.email,
        })),
    )
        .into_response()
}

/// `POST /api/auth/logout` — revoke the session named by the cookie and clear
/// it. Idempotent: clearing the cookie always succeeds even if the row was
/// already gone (sign-out is best-effort revoke + always-clear).
pub async fn logout(store: &Arc<AccountStore>, headers: &HeaderMap) -> Response {
    if let Some(token) = multitenant::session_cookie(headers) {
        let hash = AccountStore::token_hash(&token);
        if let Err(e) = store.revoke_session_by_hash(&hash) {
            tracing::warn!("logout: failed to revoke session: {e:#}");
        }
    }
    // Clear the cookie regardless (Max-Age=0 expires it immediately).
    let cleared = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cleared)],
        axum::Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// `GET /api/auth/pats` — list the caller's own PATs (metadata only, never the
/// secret or its hash). Requires a resolved principal (session OR PAT).
pub async fn list_pats(store: &Arc<AccountStore>, headers: &HeaderMap) -> Response {
    let Some(Principal::User { user_id, .. }) = multitenant::resolve_user(store, headers, now_ms())
    else {
        return multitenant::unauthorized();
    };
    match store.list_pats(&user_id) {
        Ok(pats) => {
            let out: Vec<_> = pats
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "label": p.label,
                        "scopes": p.scopes.to_db_string().split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
                        "created_ms": p.created_ms,
                        "expires_ms": p.expires_ms,
                    })
                })
                .collect();
            axum::Json(serde_json::json!({ "pats": out })).into_response()
        }
        Err(e) => {
            tracing::error!("list_pats failed for {user_id}: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "list failed").into_response()
        }
    }
}

/// `POST /api/auth/pats` — mint a PAT for the caller (token shown ONCE in the
/// response). Body: `{ "label": "...", "scopes": ["registry:write", ...] }`.
/// Requires a SESSION credential (the interactive authority) — a programmatic
/// PAT cannot manufacture more PATs from the web box. Scopes default to the
/// caller's own scope set when omitted, and are always INTERSECTED with it so a
/// PAT can never exceed the minting principal's authority. No expiry by default
/// (auth.md §11).
pub async fn mint_pat(
    store: &Arc<AccountStore>,
    headers: &HeaderMap,
    body: &serde_json::Value,
) -> Response {
    // Resolve and require a SESSION (not a PAT) — minting keys is interactive.
    let Some(token) = multitenant::session_cookie(headers) else {
        return multitenant::unauthorized();
    };
    let now = now_ms();
    let Some(cred) = store
        .validate(&token, now)
        .filter(|c| c.kind == CredentialKind::Session)
    else {
        return multitenant::unauthorized();
    };
    let label = body
        .get("label")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("device")
        .to_string();
    // Requested scopes default to the caller's own; always intersect with the
    // caller's set so a mint can never widen authority.
    let requested = match body.get("scopes").and_then(serde_json::Value::as_array) {
        Some(arr) => {
            let joined = arr
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(",");
            ScopeSet::from_db_string(&joined)
        }
        None => cred.scopes.clone(),
    };
    let scopes = intersect_scopes(&requested, &cred.scopes);
    match store.mint_pat(&cred.user_id, scopes, &label, now, None) {
        Ok(minted) => (
            StatusCode::CREATED,
            axum::Json(serde_json::json!({
                "token": minted.token,
                "id": minted.info.id,
                "label": minted.info.label,
                "scopes": minted.info.scopes.to_db_string().split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("mint_pat failed for {}: {e:#}", cred.user_id);
            (StatusCode::INTERNAL_SERVER_ERROR, "mint failed").into_response()
        }
    }
}

/// `DELETE /api/auth/pats/{id}` — revoke one of the caller's own PATs. Scoped
/// to the resolved principal's `user_id`, so a caller can never revoke
/// another's token by guessing an id. 204 on success, 404 when no such PAT
/// belongs to the caller. Requires a resolved principal.
pub async fn revoke_pat(store: &Arc<AccountStore>, headers: &HeaderMap, id: &str) -> Response {
    let Some(Principal::User { user_id, .. }) = multitenant::resolve_user(store, headers, now_ms())
    else {
        return multitenant::unauthorized();
    };
    match store.revoke_pat_by_id(&user_id, id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such PAT").into_response(),
        Err(e) => {
            tracing::error!("revoke_pat failed for {user_id}/{id}: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "revoke failed").into_response()
        }
    }
}

/// The intersection of two scope sets — a minted PAT carries at most the
/// minting principal's own scopes (never wider).
fn intersect_scopes(requested: &ScopeSet, ceiling: &ScopeSet) -> ScopeSet {
    ScopeSet::from_scopes(
        [Scope::RegistryRead, Scope::RegistryWrite, Scope::Admin]
            .into_iter()
            .filter(|s| requested.contains(*s) && ceiling.contains(*s)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_intersection_never_widens() {
        let read_only = ScopeSet::from_scopes([Scope::RegistryRead]);
        let full = ScopeSet::all();
        // Requesting full but bounded by a read-only ceiling → read-only.
        let got = intersect_scopes(&full, &read_only);
        assert!(got.contains(Scope::RegistryRead));
        assert!(!got.contains(Scope::RegistryWrite));
        assert!(!got.contains(Scope::Admin));
        // Requesting a subset within a full ceiling → the subset.
        let got = intersect_scopes(&ScopeSet::from_scopes([Scope::RegistryWrite]), &full);
        assert!(got.contains(Scope::RegistryWrite));
        assert!(!got.contains(Scope::Admin));
    }
}
