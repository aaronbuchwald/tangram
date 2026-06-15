//! Per-principal audit log (docs/design/auth.md §6, C4).
//!
//! Every MUTATING action POST and MCP `tools/call` that PASSES the multi-tenant
//! guard writes one append-only record attributing the change to its principal:
//! `{ user_id, email, action, app, args_digest, outcome, ts }`. The "who
//! changed what when" layer all three umbrella issues asked for.
//!
//! **Storage choice.** Records live in the accounts sqlite (an `audit` table on
//! the same [`crate::accounts::AccountStore`] connection), NOT a separate store.
//! The audit log shares the credential store's lifecycle and host-local /
//! never-replicated discipline exactly (a replicated audit trail leaks both the
//! principal set and their activity), so one DB keeps the invariant in one
//! place. The CRDT `ActorId` (random per process) is unchanged — this is the
//! HUMAN attribution layer, separate from the Automerge merge actor.
//!
//! **Args are a digest, never plaintext.** [`digest_args`] hashes the action's
//! JSON arguments to `hex(sha256(canonical-json))` so an injected secret in an
//! argument is never written to the log (auth.md §6, §12). The digest is stable
//! for equal arguments (serde_json serializes a `Value` deterministically by
//! map key order) so two identical calls produce the same digest.
//!
//! **Mode.** Multi-tenant: mandatory (the guard writes on every pass).
//! Self-hosted: the guard never runs, so nothing is written — a `LocalUser`
//! audit log is low-value (implicit single user) and stays off, matching §6.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;

use crate::accounts::AccountStore;
use crate::auth::Scope;
use crate::multitenant::{self, now_ms};

/// The default page size for `GET /api/audit` and the in-process cap so a read
/// can never pull an unbounded result set into memory.
pub const DEFAULT_AUDIT_LIMIT: usize = 200;
const MAX_AUDIT_LIMIT: usize = 1000;

/// One audit record (auth.md §6). `args_digest` is a sha-256 hex digest of the
/// action arguments — NEVER the plaintext (no injected secret is ever logged).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuditRecord {
    pub user_id: String,
    pub email: String,
    /// The action / tool name that mutated.
    pub action: String,
    /// The app the action ran against (its route prefix's app segment).
    pub app: String,
    /// hex(sha256(canonical-json(args))) — the digest, never plaintext.
    pub args_digest: String,
    /// `passed` for a guard that authorized the mutation (the only thing
    /// written today; the column is here so a future "denied" tier slots in).
    pub outcome: String,
    pub ts_ms: i64,
}

/// hex(sha256(...)) of the canonical JSON of an action's arguments. Equal args
/// hash equally (serde_json serializes a `Value` by sorted map key order), and
/// the plaintext — which may carry an injected secret — never lands in the log.
pub fn digest_args(args: &serde_json::Value) -> String {
    let canonical = serde_json::to_vec(args).unwrap_or_default();
    let digest = Sha256::digest(&canonical);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Write one PASSED-mutation audit record. Best-effort: a write failure is
/// logged but never fails the request (the mutation already succeeded; losing
/// an audit row must not 500 a user). Multi-tenant only — the caller holds the
/// store only in that mode.
pub fn record_mutation(
    store: &AccountStore,
    principal: &crate::auth::Principal,
    app: &str,
    action: &str,
    args_digest: String,
) {
    let crate::auth::Principal::User { user_id, email, .. } = principal else {
        return; // self-hosted / tenant principals are not audited (auth.md §6)
    };
    let record = AuditRecord {
        user_id: user_id.clone(),
        email: email.clone(),
        action: action.to_string(),
        app: app.to_string(),
        args_digest,
        outcome: "passed".to_string(),
        ts_ms: now_ms(),
    };
    if let Err(e) = store.append_audit(&record) {
        tracing::warn!("audit: failed to append record for {user_id}/{action}: {e:#}");
    }
}

/// `GET /api/audit` — the admin-scoped read of the audit log (auth.md §6).
/// Resolves the request to a principal via the host-local store and requires
/// the `admin` scope; a non-admin (or unauthenticated) caller gets the SAME
/// uniform 401 the rest of the multi-tenant surface uses for an unresolved
/// principal, and a 403 (naming the scope) for an authenticated non-admin —
/// matching the action guard's 401/403 split. `?limit=N` caps the page
/// (default [`DEFAULT_AUDIT_LIMIT`], hard cap [`MAX_AUDIT_LIMIT`]).
pub async fn get_audit(
    State(store): State<Arc<AccountStore>>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let principal = match multitenant::resolve_user(&store, &headers, now_ms()) {
        Some(p) => p,
        None => return multitenant::unauthorized(),
    };
    if !principal.has_scope(Scope::Admin) {
        return multitenant::forbidden(Scope::Admin);
    }
    let limit = parse_limit(query.as_deref());
    match store.recent_audit(limit) {
        Ok(records) => axum::Json(serde_json::json!({ "records": records })).into_response(),
        Err(e) => {
            tracing::error!("audit: read failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "audit read failed").into_response()
        }
    }
}

/// Parse `limit=N` out of the raw query string, clamped to `[1, MAX]`, default
/// [`DEFAULT_AUDIT_LIMIT`].
fn parse_limit(query: Option<&str>) -> usize {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.split_once('=').filter(|(k, _)| *k == "limit"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .map_or(DEFAULT_AUDIT_LIMIT, |n| n.clamp(1, MAX_AUDIT_LIMIT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ScopeSet;

    fn principal(user_id: &str, scopes: ScopeSet) -> crate::auth::Principal {
        crate::auth::Principal::User {
            user_id: user_id.to_string(),
            email: format!("{user_id}@example.com"),
            groups: vec![],
            scopes,
        }
    }

    #[test]
    fn args_digest_is_stable_and_not_plaintext() {
        let a = serde_json::json!({ "secret": "hunter2", "n": 1 });
        let b = serde_json::json!({ "n": 1, "secret": "hunter2" });
        // Equal args (any key order) → equal digest.
        assert_eq!(digest_args(&a), digest_args(&b));
        // 64 hex chars, and the plaintext secret never appears.
        let d = digest_args(&a);
        assert_eq!(d.len(), 64);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!d.contains("hunter2"));
        // Different args → different digest.
        assert_ne!(digest_args(&a), digest_args(&serde_json::json!({ "n": 2 })));
    }

    #[test]
    fn two_principals_produce_two_attributed_records() {
        let store = AccountStore::open_in_memory().unwrap();
        store.create_account("alice", "alice@x", &[], 1).unwrap();
        store.create_account("bob", "bob@x", &[], 1).unwrap();
        let args = serde_json::json!({ "name": "notes" });
        record_mutation(
            &store,
            &principal("alice", ScopeSet::all()),
            "registry",
            "install_app",
            digest_args(&args),
        );
        record_mutation(
            &store,
            &principal("bob", ScopeSet::all()),
            "registry",
            "remove_app",
            digest_args(&args),
        );
        let records = store.recent_audit(10).unwrap();
        assert_eq!(records.len(), 2);
        // Newest first.
        assert_eq!(records[0].user_id, "bob");
        assert_eq!(records[0].action, "remove_app");
        assert_eq!(records[1].user_id, "alice");
        assert_eq!(records[1].action, "install_app");
        // Args are digested, not plaintext.
        assert_eq!(records[0].args_digest, digest_args(&args));
        assert!(!records[0].args_digest.contains("notes"));
        assert_eq!(records[0].outcome, "passed");
    }

    #[test]
    fn self_hosted_and_tenant_principals_are_not_audited() {
        let store = AccountStore::open_in_memory().unwrap();
        record_mutation(
            &store,
            &crate::auth::Principal::LocalUser,
            "registry",
            "install_app",
            digest_args(&serde_json::json!({})),
        );
        record_mutation(
            &store,
            &crate::auth::Principal::Tenant("t".into()),
            "registry",
            "install_app",
            digest_args(&serde_json::json!({})),
        );
        assert!(store.recent_audit(10).unwrap().is_empty());
    }

    #[test]
    fn limit_parsing_clamps_and_defaults() {
        assert_eq!(parse_limit(None), DEFAULT_AUDIT_LIMIT);
        assert_eq!(parse_limit(Some("")), DEFAULT_AUDIT_LIMIT);
        assert_eq!(parse_limit(Some("limit=50")), 50);
        assert_eq!(parse_limit(Some("other=1&limit=5")), 5);
        assert_eq!(parse_limit(Some("limit=0")), 1); // clamped up
        assert_eq!(parse_limit(Some("limit=99999")), MAX_AUDIT_LIMIT);
        assert_eq!(parse_limit(Some("limit=abc")), DEFAULT_AUDIT_LIMIT);
    }
}
