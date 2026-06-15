//! End-to-end auth checkpoint C3: multi-tenant principal resolution + scope
//! gating + admin bootstrap (docs/design/auth.md §4, §5).
//!
//! Walks: the zero-accounts boot mints a local-admin PAT (printed once to the
//! log); the top-level registry's MUTATING surface requires a resolved
//! principal carrying `registry:write` (uniform 401 without; works with the
//! admin PAT); a scope-narrowed PAT (registry:read only) is 403'd on a
//! mutation but can read; reads stay open for everyone (reads_gated = false by
//! default); a revoked PAT 401s on the very next request; and the regression
//! bar — a SELF-HOSTED host is byte-identical (no credential required over
//! loopback). Cross-user DATA isolation is structural via
//! `Principal::data_dir` confinement (unit-tested in `auth.rs` /
//! `multitenant.rs`); the per-principal dynamic registry that would surface it
//! at the top level is later work (see the C3 note in the final report).
//!
//! Requires the wasm components (built by CI before `cargo test`):
//!   cargo build -p tangram-registry -p tangram-notes \
//!     --lib --target wasm32-wasip2 --release
//! The test SKIPS (with a notice) when they are missing.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

mod support;
use support::{HostProc, component, free_port, wait_for, workspace_root};

fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let child = Command::new(env!("CARGO_BIN_EXE_tangram-host"))
        .arg(apps_toml)
        .current_dir(home)
        .env("HOME", home)
        .env("TANGRAM_DATA_DIR", home.join(".tangram"))
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        .env_remove("TANGRAM_AUTH_TOKEN")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn tangram-host");
    HostProc(child)
}

/// Read the whole log file (the host writes stdout+stderr there).
fn read_log(log: &Path) -> String {
    let mut s = String::new();
    if let Ok(mut f) = std::fs::File::open(log) {
        let _ = f.read_to_string(&mut s);
    }
    s
}

/// Parse the single-shown local-admin PAT (`tgp_…`) out of the bootstrap log
/// line. Returns `None` until the host has printed it.
fn admin_pat_from_log(log: &Path) -> Option<String> {
    read_log(log).split_whitespace().find_map(|tok| {
        let tok = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        tok.starts_with("tgp_").then(|| tok.to_string())
    })
}

async fn get_status(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> Option<reqwest::StatusCode> {
    let mut req = client.get(url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    req.send().await.ok().map(|r| r.status())
}

async fn action(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    args: &serde_json::Value,
) -> reqwest::StatusCode {
    let mut req = client.post(url).json(args);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    req.send().await.expect("action post").status()
}

#[tokio::test]
async fn multi_tenant_gates_mutations_by_scope_with_admin_bootstrap() {
    for name in ["registry", "notes"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING multi_tenant_lifecycle: {} missing — build the wasm components first \
                 (cargo build -p tangram-registry -p tangram-notes --lib \
                 --target wasm32-wasip2 --release)",
                component(name).display()
            );
            return;
        }
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[auth]
mode = "multi-tenant"

[apps.registry]
component = "{registry}"
ui = "{root}/apps/registry/ui"
registry = true

[apps.notes]
component = "{notes}"
ui = "{notes_ui}"
"#,
            registry = component("registry").display(),
            notes = component("notes").display(),
            notes_ui = root.join("apps/notes/ui").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();
    let ok = Some(reqwest::StatusCode::OK);
    let unauthorized = Some(reqwest::StatusCode::UNAUTHORIZED);

    // ── the admin PAT is minted on the zero-accounts boot and shown once ─────
    wait_for("admin PAT in log", Duration::from_secs(30), || async {
        admin_pat_from_log(&log).is_some()
    })
    .await;
    let admin = admin_pat_from_log(&log).expect("admin PAT");
    assert!(admin.starts_with("tgp_"));

    // ── the registry app comes up ────────────────────────────────────────────
    wait_for("registry healthz", Duration::from_secs(120), || async {
        get_status(&client, &format!("{base}/registry/healthz"), None).await == ok
    })
    .await;

    // ── reads stay open (reads_gated = false) ───────────────────────────────
    for path in [
        "/registry/healthz",
        "/registry/api/state",
        "/api/fleet",
        "/",
    ] {
        assert_eq!(
            get_status(&client, &format!("{base}{path}"), None).await,
            ok,
            "read {path} must stay open with reads_gated=false"
        );
    }

    // ── the MUTATING surface requires a resolved principal (uniform 401) ─────
    let install = format!("{base}/registry/api/actions/install_app");
    let install_args = serde_json::json!({
        "name": "notes2",
        "component": component("notes").display().to_string(),
        "ui": root.join("apps/notes/ui").display().to_string(),
    });
    for bad in [None, Some("tgp_wrongtoken"), Some("not-even-a-token")] {
        assert_eq!(
            action(&client, &install, bad, &install_args).await,
            reqwest::StatusCode::UNAUTHORIZED,
            "mutation with creds {bad:?} must 401"
        );
    }

    // ── with the admin PAT (registry:write) the mutation is accepted ─────────
    assert_eq!(
        action(&client, &install, Some(&admin), &install_args).await,
        reqwest::StatusCode::OK,
        "admin PAT carries registry:write"
    );

    // ── a scope-narrowed PAT: registry:read only ─────────────────────────────
    // Mint it directly in the host's own store (same path the host opened).
    let store_path = home.join(".tangram").join("accounts.sqlite");
    // Open the SAME on-disk store the host uses and mint a read-only PAT row
    // for the local-admin account directly (minting a row is exactly what the
    // store API does — this keeps the test free of an HTTP API that lands in
    // C5). rusqlite serializes via file locking; the host is idle here.
    let read_only = {
        let conn = rusqlite::Connection::open(&store_path).expect("open store");
        mint_read_only_pat(&conn)
    };
    // The read-only PAT can READ (reads are open anyway, but it resolves)…
    assert_eq!(
        get_status(
            &client,
            &format!("{base}/registry/api/state"),
            Some(&read_only)
        )
        .await,
        ok
    );
    // …but is FORBIDDEN (403) on a mutation — it lacks registry:write.
    assert_eq!(
        action(&client, &install, Some(&read_only), &install_args).await,
        reqwest::StatusCode::FORBIDDEN,
        "a registry:read PAT must be 403'd on a mutation"
    );

    // ── revocation immediacy: revoke the admin PAT, next mutation 401s ───────
    revoke_all_pats_for(&store_path, "local-admin", &admin);
    assert_eq!(
        action(&client, &install, Some(&admin), &install_args).await,
        reqwest::StatusCode::UNAUTHORIZED,
        "a revoked PAT 401s on the very next request"
    );
    drop(_host);
    let _ = unauthorized; // silence unused in some build configs
}

/// Mint a `registry:read`-only PAT for the local-admin account directly in the
/// store (the host's own `mint_pat` semantics: tgp_ + base64url(20), hashed at
/// rest). Kept local to the test so it does not depend on a host HTTP API that
/// only lands in C5.
fn mint_read_only_pat(conn: &rusqlite::Connection) -> String {
    use base64::Engine as _;
    use rand::RngCore as _;
    use sha2::{Digest as _, Sha256};

    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = format!(
        "tgp_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    );
    let hash = {
        let d = Sha256::digest(token.as_bytes());
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let id = format!("pat_test_{}", &hash[..8]);
    conn.execute(
        "INSERT INTO pats (token_hash, id, user_id, scopes, label, created_ms, expires_ms) \
         VALUES (?1, ?2, 'local-admin', 'registry:read', 'test-readonly', 0, NULL)",
        rusqlite::params![hash, id],
    )
    .expect("insert read-only PAT");
    token
}

/// Revoke every PAT for `user_id` (here: simulate the admin signing out / the
/// device being revoked) by deleting the matching row by hash.
fn revoke_all_pats_for(store_path: &Path, user_id: &str, admin_token: &str) {
    use sha2::{Digest as _, Sha256};
    let hash = {
        let d = Sha256::digest(admin_token.as_bytes());
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let conn = rusqlite::Connection::open(store_path).expect("open store");
    conn.execute(
        "DELETE FROM pats WHERE token_hash = ?1 AND user_id = ?2",
        rusqlite::params![hash, user_id],
    )
    .expect("revoke");
}

/// Mint a full-scope PAT for a NEW account directly in the store (the host's
/// own `mint_pat` semantics). Used to stand a second attributed principal up
/// without the C5 account-creation HTTP API.
fn mint_full_pat_for_new_account(
    conn: &rusqlite::Connection,
    user_id: &str,
    email: &str,
) -> String {
    use base64::Engine as _;
    use rand::RngCore as _;
    use sha2::{Digest as _, Sha256};

    conn.execute(
        "INSERT OR IGNORE INTO accounts (user_id, email, groups, created_ms) \
         VALUES (?1, ?2, '', 0)",
        rusqlite::params![user_id, email],
    )
    .expect("insert account");
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = format!(
        "tgp_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    );
    let hash = {
        let d = Sha256::digest(token.as_bytes());
        d.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let id = format!("pat_{user_id}_{}", &hash[..8]);
    conn.execute(
        "INSERT INTO pats (token_hash, id, user_id, scopes, label, created_ms, expires_ms) \
         VALUES (?1, ?2, ?3, 'registry:read,registry:write,admin', 'test', 0, NULL)",
        rusqlite::params![hash, id, user_id],
    )
    .expect("insert PAT");
    token
}

/// C4 — the per-principal audit log: two distinct principals each pass a
/// mutating guard → two attributed records; the args are a DIGEST not the
/// plaintext; and the audit read is admin-scoped (a registry:read PAT is 403'd).
#[tokio::test]
async fn audit_log_attributes_mutations_per_principal_and_is_admin_scoped() {
    for name in ["registry", "notes"] {
        if !component(name).exists() {
            eprintln!("SKIPPING audit_log test: {name} component missing");
            return;
        }
    }
    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[auth]
mode = "multi-tenant"

[apps.registry]
component = "{registry}"
ui = "{root}/apps/registry/ui"
registry = true
"#,
            registry = component("registry").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();
    let ok = Some(reqwest::StatusCode::OK);

    wait_for("admin PAT in log", Duration::from_secs(30), || async {
        admin_pat_from_log(&log).is_some()
    })
    .await;
    let admin = admin_pat_from_log(&log).expect("admin PAT");
    wait_for("registry healthz", Duration::from_secs(120), || async {
        get_status(&client, &format!("{base}/registry/healthz"), None).await == ok
    })
    .await;

    let store_path = home.join(".tangram").join("accounts.sqlite");
    // A SECOND attributed principal with full scope.
    let second = {
        let conn = rusqlite::Connection::open(&store_path).expect("open store");
        mint_full_pat_for_new_account(&conn, "second", "second@example.com")
    };

    let install = format!("{base}/registry/api/actions/install_app");
    let args_a = serde_json::json!({
        "name": "auditone",
        "component": component("notes").display().to_string(),
        "ui": root.join("apps/notes/ui").display().to_string(),
    });
    let args_b = serde_json::json!({
        "name": "audittwo",
        "component": component("notes").display().to_string(),
        "ui": root.join("apps/notes/ui").display().to_string(),
    });
    // admin (local-admin) and second each pass the mutating guard once.
    assert_eq!(
        action(&client, &install, Some(&admin), &args_a).await,
        reqwest::StatusCode::OK
    );
    assert_eq!(
        action(&client, &install, Some(&second), &args_b).await,
        reqwest::StatusCode::OK
    );

    // The audit read requires admin: the read-only PAT is 403'd, the bare read
    // 401s.
    let read_only = {
        let conn = rusqlite::Connection::open(&store_path).expect("open store");
        mint_read_only_pat(&conn)
    };
    assert_eq!(
        get_status(&client, &format!("{base}/api/audit"), None).await,
        Some(reqwest::StatusCode::UNAUTHORIZED),
        "audit read with no credential must 401"
    );
    assert_eq!(
        get_status(&client, &format!("{base}/api/audit"), Some(&read_only)).await,
        Some(reqwest::StatusCode::FORBIDDEN),
        "audit read with a non-admin PAT must 403"
    );

    // The admin read returns two attributed records, args digested.
    let body: serde_json::Value = client
        .get(format!("{base}/api/audit"))
        .bearer_auth(&admin)
        .send()
        .await
        .expect("audit read")
        .json()
        .await
        .expect("audit json");
    let records = body["records"].as_array().expect("records array");
    let install_records: Vec<_> = records
        .iter()
        .filter(|r| r["action"] == "install_app")
        .collect();
    assert_eq!(
        install_records.len(),
        2,
        "two install_app mutations → two records: {records:?}"
    );
    let users: std::collections::HashSet<&str> = install_records
        .iter()
        .filter_map(|r| r["user_id"].as_str())
        .collect();
    assert!(users.contains("local-admin"), "admin attributed: {users:?}");
    assert!(users.contains("second"), "second attributed: {users:?}");
    // Args are a 64-hex digest, never the plaintext app name.
    for r in &install_records {
        let digest = r["args_digest"].as_str().expect("digest");
        assert_eq!(digest.len(), 64, "args_digest is a sha-256 hex digest");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!digest.contains("auditone") && !digest.contains("audittwo"));
        assert_eq!(r["outcome"], "passed");
    }
    drop(_host);
}

#[tokio::test]
async fn self_hosted_mode_needs_no_credential_over_loopback() {
    // The regression bar: with no [auth] section (self-hosted default) the
    // top-level mutating surface is OPEN over loopback — byte-identical to
    // before this checkpoint.
    for name in ["registry", "notes"] {
        if !component(name).exists() {
            eprintln!("SKIPPING self_hosted regression: {name} component missing");
            return;
        }
    }
    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.notes]
component = "{notes}"
ui = "{notes_ui}"
"#,
            notes = component("notes").display(),
            notes_ui = root.join("apps/notes/ui").display(),
        ),
    )
    .expect("write apps.toml");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    wait_for("notes healthz", Duration::from_secs(120), || async {
        get_status(&client, &format!("{base}/notes/healthz"), None).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;
    // No credential, yet the mutating action is accepted (loopback trust).
    assert_eq!(
        action(
            &client,
            &format!("{base}/notes/api/actions/add_note"),
            None,
            &serde_json::json!({ "text": "self-hosted note" }),
        )
        .await,
        reqwest::StatusCode::OK,
        "self-hosted loopback mutation must stay open"
    );
    // And the account store was NOT created (multi-tenant only).
    assert!(
        !home.join(".tangram").join("accounts.sqlite").exists(),
        "self-hosted mode must not create the credential store"
    );
}
