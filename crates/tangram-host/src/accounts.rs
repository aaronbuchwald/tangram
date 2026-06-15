//! Host-local credential store (docs/design/auth.md §4, C2; ADR-0011).
//!
//! The Cloudflare `TangramAccounts` model (ADR-0003) ported to Rust over
//! embedded rusqlite: accounts, IdP identity links, hashed Personal Access
//! Tokens (PATs), and browser sessions. It is the lookup [`crate::auth`]'s
//! multi-tenant `resolve_principal` consults — **host-local and per-host,
//! NEVER replicated into an Automerge document** (a replicated credential is a
//! leaked credential).
//!
//! Token discipline (the security invariant, auth.md §12):
//!
//! - A PAT is `tgp_` + base64url(20 CSPRNG bytes); a session is `tgs_` +
//!   base64url(20). 20 bytes = 160 bits of entropy.
//! - We store **only** `hex(sha256(plaintext))`. The plaintext is returned to
//!   the caller exactly once (at mint time) and is unrecoverable thereafter.
//! - Validation hashes the presented token and looks the row up by hash, so a
//!   deleted row 401s on the very next request (revocation immediacy — no
//!   cache between delete and effect).
//! - All time is the caller's `now_ms` (no ambient clock), so expiry is
//!   deterministically testable.
//!
//! This module is pure storage + crypto; wiring into request handling lands in
//! C3.

#![allow(dead_code)] // wired in C3

use std::path::Path;

use anyhow::Context as _;
use base64::Engine as _;
use rand::RngCore as _;
use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

use crate::auth::ScopeSet;

/// The `tgp_` prefix marks a Personal Access Token (replicas / MCP / CLI).
const PAT_PREFIX: &str = "tgp_";
/// The `tgs_` prefix marks a browser session token (the UI cookie).
const SESSION_PREFIX: &str = "tgs_";
/// CSPRNG entropy per token, in bytes (160 bits).
const TOKEN_BYTES: usize = 20;

/// hex(sha256(token)) — the at-rest form of every credential. The plaintext is
/// never stored; this is the only thing the DB ever sees.
fn hash_token(plaintext: &str) -> String {
    let digest = Sha256::digest(plaintext.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Mint a fresh token plaintext with the given prefix: `<prefix>` +
/// base64url(20 CSPRNG bytes), no padding.
fn mint_token(prefix: &str) -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!("{prefix}{body}")
}

/// A stored account (no secrets — credentials live in `pats` / `sessions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub user_id: String,
    pub email: String,
    pub groups: Vec<String>,
    pub created_ms: i64,
}

/// One PAT's metadata, surfaced to its owner — **never** the secret or its
/// hash. `id` is the opaque handle used to revoke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatInfo {
    pub id: String,
    pub user_id: String,
    pub scopes: ScopeSet,
    pub label: String,
    pub created_ms: i64,
    pub expires_ms: Option<i64>,
}

/// A freshly minted PAT: the plaintext (shown ONCE) plus its metadata.
#[derive(Debug, Clone)]
pub struct MintedPat {
    /// The `tgp_…` plaintext — return to the caller once, then it is gone.
    pub token: String,
    pub info: PatInfo,
}

/// The result of [`AccountStore::validate`] — who a presented credential
/// authenticates as, with the scopes it carries. Carries everything
/// `Principal::User` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCredential {
    pub user_id: String,
    pub email: String,
    pub groups: Vec<String>,
    pub scopes: ScopeSet,
    /// How the credential was presented — a PAT or a session cookie.
    pub kind: CredentialKind,
}

/// Which credential class validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Pat,
    Session,
}

/// Groups serialize as a comma-joined string in the `accounts.groups` column.
/// Group names cannot contain commas (IdP groups are simple identifiers); we
/// drop empties so a round-trip of `[]` is stable.
fn groups_to_db(groups: &[String]) -> String {
    groups.join(",")
}

fn groups_from_db(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The host-local account / credential store (auth.md §4).
pub struct AccountStore {
    conn: Connection,
}

impl AccountStore {
    /// Open (creating + migrating) the store at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating account store dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening account store {}", path.display()))?;
        Self::from_conn(conn)
    }

    /// An in-memory store — for tests and ephemeral hosts.
    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::from_conn(Connection::open_in_memory().context("opening in-memory account store")?)
    }

    fn from_conn(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)
            .context("enabling foreign_keys")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS accounts (
                user_id    TEXT PRIMARY KEY,
                email      TEXT NOT NULL,
                groups     TEXT NOT NULL,
                created_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS idents (
                provider_id TEXT PRIMARY KEY,
                user_id     TEXT NOT NULL REFERENCES accounts(user_id) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS pats (
                token_hash TEXT PRIMARY KEY,
                id         TEXT NOT NULL UNIQUE,
                user_id    TEXT NOT NULL REFERENCES accounts(user_id) ON DELETE CASCADE,
                scopes     TEXT NOT NULL,
                label      TEXT NOT NULL,
                created_ms INTEGER NOT NULL,
                expires_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS sessions (
                token_hash TEXT PRIMARY KEY,
                user_id    TEXT NOT NULL REFERENCES accounts(user_id) ON DELETE CASCADE,
                created_ms INTEGER NOT NULL,
                expires_ms INTEGER
            );
            "#,
        )
        .context("creating account store schema")?;
        Ok(Self { conn })
    }

    /// True when no account exists yet — the zero-accounts boot that mints the
    /// local-admin PAT (auth.md §7, C3).
    pub fn is_empty(&self) -> anyhow::Result<bool> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))?;
        Ok(n == 0)
    }

    // ── accounts ───────────────────────────────────────────────────────────

    /// Create an account. Errors if the `user_id` already exists.
    pub fn create_account(
        &self,
        user_id: &str,
        email: &str,
        groups: &[String],
        now_ms: i64,
    ) -> anyhow::Result<Account> {
        self.conn
            .execute(
                "INSERT INTO accounts (user_id, email, groups, created_ms) VALUES (?1, ?2, ?3, ?4)",
                params![user_id, email, groups_to_db(groups), now_ms],
            )
            .with_context(|| format!("creating account {user_id:?}"))?;
        Ok(Account {
            user_id: user_id.to_string(),
            email: email.to_string(),
            groups: groups.to_vec(),
            created_ms: now_ms,
        })
    }

    /// Look an account up by `user_id`.
    pub fn account(&self, user_id: &str) -> anyhow::Result<Option<Account>> {
        Ok(self
            .conn
            .query_row(
                "SELECT user_id, email, groups, created_ms FROM accounts WHERE user_id = ?1",
                params![user_id],
                Self::row_to_account,
            )
            .optional()?)
    }

    fn row_to_account(row: &rusqlite::Row) -> rusqlite::Result<Account> {
        Ok(Account {
            user_id: row.get(0)?,
            email: row.get(1)?,
            groups: groups_from_db(&row.get::<_, String>(2)?),
            created_ms: row.get(3)?,
        })
    }

    // ── IdP identity links (OAuth, C6) ───────────────────────────────────────

    /// Link an IdP identity (`ident:<provider>:<sub>`) to a local account.
    /// Idempotent for the same `(provider_id, user_id)`; errors if the
    /// provider id is already linked to a DIFFERENT account.
    pub fn link_ident(&self, provider_id: &str, user_id: &str) -> anyhow::Result<()> {
        if let Some(existing) = self.account_for_ident(provider_id)? {
            anyhow::ensure!(
                existing == user_id,
                "identity {provider_id:?} is already linked to a different account"
            );
            return Ok(());
        }
        self.conn
            .execute(
                "INSERT INTO idents (provider_id, user_id) VALUES (?1, ?2)",
                params![provider_id, user_id],
            )
            .with_context(|| format!("linking identity {provider_id:?}"))?;
        Ok(())
    }

    /// The `user_id` an IdP identity maps to, if any.
    pub fn account_for_ident(&self, provider_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT user_id FROM idents WHERE provider_id = ?1",
                params![provider_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    // ── PATs ─────────────────────────────────────────────────────────────────

    /// Mint a PAT for `user_id`. Returns the plaintext ONCE; only the hash is
    /// stored. `expires_ms` is `None` for a non-expiring token (the default,
    /// auth.md §11). `id` is a fresh opaque handle for later revocation.
    pub fn mint_pat(
        &self,
        user_id: &str,
        scopes: ScopeSet,
        label: &str,
        now_ms: i64,
        expires_ms: Option<i64>,
    ) -> anyhow::Result<MintedPat> {
        anyhow::ensure!(
            self.account(user_id)?.is_some(),
            "cannot mint a PAT for unknown account {user_id:?}"
        );
        let token = mint_token(PAT_PREFIX);
        let token_hash = hash_token(&token);
        let id = mint_token("pat_"); // opaque, not a credential — just a handle
        self.conn
            .execute(
                "INSERT INTO pats (token_hash, id, user_id, scopes, label, created_ms, expires_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    token_hash,
                    id,
                    user_id,
                    scopes.to_db_string(),
                    label,
                    now_ms,
                    expires_ms
                ],
            )
            .context("inserting PAT")?;
        Ok(MintedPat {
            token,
            info: PatInfo {
                id,
                user_id: user_id.to_string(),
                scopes,
                label: label.to_string(),
                created_ms: now_ms,
                expires_ms,
            },
        })
    }

    /// List `user_id`'s PATs (metadata only — never the secret or its hash).
    pub fn list_pats(&self, user_id: &str) -> anyhow::Result<Vec<PatInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_id, scopes, label, created_ms, expires_ms FROM pats \
             WHERE user_id = ?1 ORDER BY created_ms, id",
        )?;
        let rows = stmt.query_map(params![user_id], |row| {
            Ok(PatInfo {
                id: row.get(0)?,
                user_id: row.get(1)?,
                scopes: ScopeSet::from_db_string(&row.get::<_, String>(2)?),
                label: row.get(3)?,
                created_ms: row.get(4)?,
                expires_ms: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Revoke a PAT by its `id`, scoped to its owner: a PAT is removed only
    /// when BOTH the id AND the owning `user_id` match, so one user can never
    /// revoke another's token by guessing an id. Returns whether a row was
    /// removed.
    pub fn revoke_pat_by_id(&self, user_id: &str, id: &str) -> anyhow::Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM pats WHERE id = ?1 AND user_id = ?2",
            params![id, user_id],
        )?;
        Ok(n > 0)
    }

    // ── sessions ─────────────────────────────────────────────────────────────

    /// Create a browser session for `user_id`. Returns the `tgs_…` plaintext
    /// ONCE (the cookie value); only the hash is stored. `expires_ms` is the
    /// absolute expiry (UI sessions get a 30-day TTL — computed by the caller).
    pub fn create_session(
        &self,
        user_id: &str,
        now_ms: i64,
        expires_ms: Option<i64>,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.account(user_id)?.is_some(),
            "cannot create a session for unknown account {user_id:?}"
        );
        let token = mint_token(SESSION_PREFIX);
        let token_hash = hash_token(&token);
        self.conn
            .execute(
                "INSERT INTO sessions (token_hash, user_id, created_ms, expires_ms) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![token_hash, user_id, now_ms, expires_ms],
            )
            .context("inserting session")?;
        Ok(token)
    }

    /// Revoke a session by the HASH of its token (sign-out: the caller hashes
    /// the cookie value it holds). Returns whether a row was removed.
    pub fn revoke_session_by_hash(&self, token_hash: &str) -> anyhow::Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM sessions WHERE token_hash = ?1",
            params![token_hash],
        )?;
        Ok(n > 0)
    }

    /// hex(sha256(token)) — exposed so the session-cookie sign-out path can
    /// hash the value it holds without duplicating the crypto.
    pub fn token_hash(plaintext: &str) -> String {
        hash_token(plaintext)
    }

    // ── validation ───────────────────────────────────────────────────────────

    /// Validate a presented token plaintext, routing by prefix:
    /// `tgp_` → PAT table, `tgs_` → sessions, anything else → `None`. Returns
    /// the authenticated principal's identity + scopes, or `None` for an
    /// unknown / expired / revoked credential. The uniform-`None` is the
    /// no-existence-oracle guarantee (auth.md §12): missing, wrong, expired,
    /// and revoked all look identical to the caller.
    pub fn validate(&self, plaintext: &str, now_ms: i64) -> Option<ValidatedCredential> {
        let token_hash = hash_token(plaintext);
        if plaintext.starts_with(PAT_PREFIX) {
            self.validate_pat(&token_hash, now_ms)
        } else if plaintext.starts_with(SESSION_PREFIX) {
            self.validate_session(&token_hash, now_ms)
        } else {
            None
        }
    }

    fn validate_pat(&self, token_hash: &str, now_ms: i64) -> Option<ValidatedCredential> {
        let row = self
            .conn
            .query_row(
                "SELECT p.user_id, p.scopes, p.expires_ms, a.email, a.groups \
                 FROM pats p JOIN accounts a ON a.user_id = p.user_id \
                 WHERE p.token_hash = ?1",
                params![token_hash],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .ok()??;
        let (user_id, scopes, expires_ms, email, groups) = row;
        if expires_ms.is_some_and(|exp| now_ms >= exp) {
            return None;
        }
        Some(ValidatedCredential {
            user_id,
            email,
            groups: groups_from_db(&groups),
            scopes: ScopeSet::from_db_string(&scopes),
            kind: CredentialKind::Pat,
        })
    }

    fn validate_session(&self, token_hash: &str, now_ms: i64) -> Option<ValidatedCredential> {
        let row = self
            .conn
            .query_row(
                "SELECT s.user_id, s.expires_ms, a.email, a.groups \
                 FROM sessions s JOIN accounts a ON a.user_id = s.user_id \
                 WHERE s.token_hash = ?1",
                params![token_hash],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .ok()??;
        let (user_id, expires_ms, email, groups) = row;
        if expires_ms.is_some_and(|exp| now_ms >= exp) {
            return None;
        }
        // A session always carries the FULL scope set of an interactive user:
        // the UI session is the human's own authority. Fine-grained scoping is
        // the PAT's job (replicas / MCP / CLI get narrowed tokens).
        Some(ValidatedCredential {
            user_id,
            email,
            groups: groups_from_db(&groups),
            scopes: ScopeSet::all(),
            kind: CredentialKind::Session,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Scope;

    fn store() -> AccountStore {
        let store = AccountStore::open_in_memory().unwrap();
        store
            .create_account("alice", "alice@example.com", &["devs".into()], 1_000)
            .unwrap();
        store
    }

    #[test]
    fn pat_is_hashed_at_rest_and_validates_with_correct_scopes() {
        let store = store();
        let minted = store
            .mint_pat(
                "alice",
                ScopeSet::from_scopes([Scope::RegistryRead, Scope::RegistryWrite]),
                "laptop",
                2_000,
                None,
            )
            .unwrap();
        assert!(minted.token.starts_with("tgp_"));

        // The plaintext is NOT in the DB — only its hash. Probe the raw table.
        let stored_hash: String = store
            .conn
            .query_row("SELECT token_hash FROM pats", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored_hash, hash_token(&minted.token));
        assert_ne!(stored_hash, minted.token);
        let plaintext_rows: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pats WHERE token_hash = ?1",
                params![minted.token],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(plaintext_rows, 0, "the plaintext must never be a key");

        let v = store.validate(&minted.token, 3_000).expect("valid PAT");
        assert_eq!(v.user_id, "alice");
        assert_eq!(v.email, "alice@example.com");
        assert_eq!(v.groups, vec!["devs".to_string()]);
        assert_eq!(v.kind, CredentialKind::Pat);
        assert!(v.scopes.contains(Scope::RegistryRead));
        assert!(v.scopes.contains(Scope::RegistryWrite));
        assert!(!v.scopes.contains(Scope::Admin));
    }

    #[test]
    fn revocation_is_immediate() {
        let store = store();
        let minted = store
            .mint_pat("alice", ScopeSet::all(), "ci", 2_000, None)
            .unwrap();
        assert!(store.validate(&minted.token, 3_000).is_some());
        assert!(store.revoke_pat_by_id("alice", &minted.info.id).unwrap());
        // The very next validation fails — no cache between delete and effect.
        assert!(store.validate(&minted.token, 3_001).is_none());
        // Revoking again is a no-op (row already gone).
        assert!(!store.revoke_pat_by_id("alice", &minted.info.id).unwrap());
    }

    #[test]
    fn revoke_is_owner_scoped() {
        let store = store();
        store
            .create_account("bob", "bob@example.com", &[], 1_000)
            .unwrap();
        let alice_pat = store
            .mint_pat("alice", ScopeSet::all(), "k", 2_000, None)
            .unwrap();
        // Bob cannot revoke Alice's PAT by guessing its id.
        assert!(!store.revoke_pat_by_id("bob", &alice_pat.info.id).unwrap());
        assert!(store.validate(&alice_pat.token, 3_000).is_some());
        // Alice can.
        assert!(store.revoke_pat_by_id("alice", &alice_pat.info.id).unwrap());
    }

    #[test]
    fn pat_expiry_is_honored() {
        let store = store();
        let minted = store
            .mint_pat("alice", ScopeSet::all(), "short", 1_000, Some(5_000))
            .unwrap();
        assert!(store.validate(&minted.token, 4_999).is_some());
        assert!(
            store.validate(&minted.token, 5_000).is_none(),
            "expiry is >="
        );
        assert!(store.validate(&minted.token, 9_999).is_none());
    }

    #[test]
    fn sessions_validate_expire_and_revoke() {
        let store = store();
        let token = store.create_session("alice", 1_000, Some(10_000)).unwrap();
        assert!(token.starts_with("tgs_"));
        let v = store.validate(&token, 5_000).expect("valid session");
        assert_eq!(v.kind, CredentialKind::Session);
        // A session carries the full scope set.
        assert!(v.scopes.contains(Scope::Admin));
        // Expiry.
        assert!(store.validate(&token, 10_000).is_none());
        // Revoke-by-hash (sign-out) is immediate.
        assert!(
            store
                .revoke_session_by_hash(&AccountStore::token_hash(&token))
                .unwrap()
        );
        assert!(store.validate(&token, 5_000).is_none());
    }

    #[test]
    fn prefix_routing_and_unknown_tokens() {
        let store = store();
        let pat = store
            .mint_pat("alice", ScopeSet::all(), "k", 1_000, None)
            .unwrap();
        let session = store.create_session("alice", 1_000, None).unwrap();
        // A session value never validates against the PAT table and vice versa
        // (prefix routes the lookup), and a no-prefix token is rejected.
        assert!(store.validate(&pat.token, 2_000).is_some());
        assert!(store.validate(&session.clone(), 2_000).is_some());
        assert!(store.validate("garbage-no-prefix", 2_000).is_none());
        assert!(store.validate("tgp_totallymadeup", 2_000).is_none());
        assert!(store.validate("tgs_totallymadeup", 2_000).is_none());
        assert!(store.validate("", 2_000).is_none());
    }

    #[test]
    fn list_pats_carries_no_secrets() {
        let store = store();
        let a = store
            .mint_pat(
                "alice",
                ScopeSet::from_scopes([Scope::Admin]),
                "one",
                1_000,
                None,
            )
            .unwrap();
        let _b = store
            .mint_pat("alice", ScopeSet::all(), "two", 2_000, Some(9_000))
            .unwrap();
        let list = store.list_pats("alice").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].label, "one");
        assert_eq!(list[0].id, a.info.id);
        assert!(list[0].scopes.contains(Scope::Admin));
        assert_eq!(list[1].label, "two");
        assert_eq!(list[1].expires_ms, Some(9_000));
        // Empty for another user.
        assert!(store.list_pats("nobody").unwrap().is_empty());
    }

    #[test]
    fn idents_map_to_accounts() {
        let store = store();
        assert!(
            store
                .account_for_ident("ident:github:42")
                .unwrap()
                .is_none()
        );
        store.link_ident("ident:github:42", "alice").unwrap();
        assert_eq!(
            store
                .account_for_ident("ident:github:42")
                .unwrap()
                .as_deref(),
            Some("alice")
        );
        // Idempotent for the same pair.
        store.link_ident("ident:github:42", "alice").unwrap();
        // Re-linking to a different account errors.
        store
            .create_account("bob", "bob@example.com", &[], 1_000)
            .unwrap();
        assert!(store.link_ident("ident:github:42", "bob").is_err());
    }

    #[test]
    fn cannot_mint_for_unknown_account() {
        let store = store();
        assert!(
            store
                .mint_pat("ghost", ScopeSet::all(), "k", 1_000, None)
                .is_err()
        );
        assert!(store.create_session("ghost", 1_000, None).is_err());
    }

    #[test]
    fn is_empty_tracks_accounts() {
        let store = AccountStore::open_in_memory().unwrap();
        assert!(store.is_empty().unwrap());
        store.create_account("a", "a@x", &[], 1).unwrap();
        assert!(!store.is_empty().unwrap());
    }

    #[test]
    fn open_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("accounts.sqlite");
        let token = {
            let store = AccountStore::open(&path).unwrap();
            store.create_account("alice", "a@x", &[], 1).unwrap();
            store
                .mint_pat("alice", ScopeSet::all(), "k", 1, None)
                .unwrap()
                .token
        };
        // Reopen: the credential still validates (hash persisted).
        let store = AccountStore::open(&path).unwrap();
        assert!(store.validate(&token, 2).is_some());
    }
}
