//! End-to-end Phase 5 test: multi-tenancy mode — two tenants (alice, bob)
//! plus the unchanged single-tenant top level, all in one host process on
//! one port.
//!
//! Walks: bootstrap (alice from an explicit apps template, bob from the
//! default registry-clone bootstrap); the 401 matrix (no token / wrong
//! token / the OTHER tenant's token, across UI, state, SSE, actions, sync,
//! and MCP — everything under /t/<tenant>/ is private); on-disk isolation
//! under `<data_root>/<tenant>/<app>/`; per-tenant registry installs that
//! converge only their own namespace; the allow_hosts ceiling intersection
//! and max_apps cap (visible in the tenant fleet); a data_dir escape attempt
//! rejected; a native sync client replicating a tenant app WITH the bearer
//! (and rejected without); restart persistence per tenant; and the
//! regression bar — top-level apps keep serving unauthenticated exactly as
//! before.
//!
//! Requires the wasm components (built by CI before `cargo test`):
//!   cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
//!     --lib --target wasm32-wasip2 --release
//! The test SKIPS (with a notice) when they are missing.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use automerge::sync::SyncDoc as _;
use tokio::sync::watch;

const ALICE_TOKEN: &str = "alice-tenant-token";
const BOB_TOKEN: &str = "bob-tenant-token";
const HOST_TOKEN: &str = "top-level-host-token";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn component(name: &str) -> PathBuf {
    workspace_root().join(format!("target/wasm32-wasip2/release/{name}.wasm"))
}

/// The spawned host, killed on drop so a failing test never leaks a server.
struct HostProc(Child);

impl Drop for HostProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let child = Command::new(env!("CARGO_BIN_EXE_tangram-host"))
        .arg(apps_toml)
        // cwd = scratch HOME so the repo's .env is NOT loaded by dotenvy
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("TANGRAM_AUTH_TOKEN", HOST_TOKEN)
        .env("TANGRAM_TEST_ALICE_TOKEN", ALICE_TOKEN)
        .env("TANGRAM_TEST_BOB_TOKEN", BOB_TOKEN)
        .env("RUST_LOG", "info")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn tangram-host");
    HostProc(child)
}

async fn wait_for<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

/// GET `url`, optionally with a bearer token, returning the status.
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

async fn get_json(client: &reqwest::Client, url: &str, token: &str) -> serde_json::Value {
    client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json")
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

fn fleet_app<'a>(fleet: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    fleet["apps"]
        .as_array()
        .expect("apps array")
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("{name} in fleet: {fleet}"))
}

/// A minimal native replica document for the sync-client test: the same
/// `DocHandle` seam the SDK and tangram-host run sync over, starting from an
/// empty automerge document (the genesis rule: it merges into any app doc).
struct TestDoc {
    doc: Mutex<automerge::AutoCommit>,
    version: watch::Sender<u64>,
}

impl TestDoc {
    fn new() -> Self {
        Self {
            doc: Mutex::new(automerge::AutoCommit::new()),
            version: watch::Sender::new(0),
        }
    }

    /// Number of entries in the replicated `notes` list (0 when the doc is
    /// still empty / genesis-only).
    fn notes_len(&self) -> usize {
        use automerge::ReadDoc as _;
        let doc = self.doc.lock().expect("doc lock");
        match doc.get(automerge::ROOT, "notes") {
            Ok(Some((_, notes))) => doc.length(&notes),
            _ => 0,
        }
    }
}

impl tangram::sync::DocHandle for TestDoc {
    fn generate_sync(&self, state: &mut automerge::sync::State) -> Option<Vec<u8>> {
        let mut doc = self.doc.lock().expect("doc lock");
        doc.sync().generate_sync_message(state).map(|m| m.encode())
    }
    fn receive_sync(
        &self,
        state: &mut automerge::sync::State,
        bytes: &[u8],
    ) -> anyhow::Result<bool> {
        let message = automerge::sync::Message::decode(bytes)?;
        let mut doc = self.doc.lock().expect("doc lock");
        let before = doc.get_heads();
        doc.sync().receive_sync_message(state, message)?;
        Ok(doc.get_heads() != before)
    }
    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }
    fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }
}

#[tokio::test]
async fn tenants_are_isolated_authed_and_persistent() {
    for name in ["registry", "notes", "nutrition"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING tenant_lifecycle: {} missing — build the wasm components first \
                 (cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
                 --lib --target wasm32-wasip2 --release)",
                component(name).display()
            );
            return;
        }
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    // alice: explicit bootstrap template (registry + notes), an app cap of 3
    // and an outbound ceiling. bob: the default bootstrap (no apps template
    // → a registry clone). Tokens come from the host env via ${VAR}.
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.registry]
component = "{registry}"
ui = "{root}/apps/registry/ui"
registry = true

[apps.notes]
component = "{notes}"
ui = "{notes_ui}"

[tenants.alice]
token = "${{TANGRAM_TEST_ALICE_TOKEN}}"
max_apps = 3
allow_hosts_ceiling = ["api.calorieninjas.com"]

[tenants.alice.apps.registry]
component = "{registry}"
ui = "{root}/apps/registry/ui"
registry = true

[tenants.alice.apps.notes]
component = "{notes}"
ui = "{notes_ui}"

[tenants.bob]
token = "${{TANGRAM_TEST_BOB_TOKEN}}"
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
    let log = home.join("host-1.log");
    let host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();
    let ok = Some(reqwest::StatusCode::OK);
    let unauthorized = Some(reqwest::StatusCode::UNAUTHORIZED);
    let not_found = Some(reqwest::StatusCode::NOT_FOUND);

    // ── bootstrap: top level + both tenants come up ─────────────────────────
    wait_for("alice notes healthz", Duration::from_secs(60), || async {
        get_status(
            &client,
            &format!("{base}/t/alice/notes/healthz"),
            Some(ALICE_TOKEN),
        )
        .await
            == ok
    })
    .await;
    wait_for("bob registry healthz", Duration::from_secs(60), || async {
        get_status(
            &client,
            &format!("{base}/t/bob/registry/healthz"),
            Some(BOB_TOKEN),
        )
        .await
            == ok
    })
    .await;
    // Regression: the top-level apps serve UNAUTHENTICATED, exactly as
    // before tenants existed.
    for path in [
        "/registry/healthz",
        "/notes/healthz",
        "/notes/api/state",
        "/api/fleet",
        "/",
    ] {
        assert_eq!(
            get_status(&client, &format!("{base}{path}"), None).await,
            ok,
            "top-level {path} must stay open"
        );
    }
    assert_eq!(
        action(
            &client,
            &format!("{base}/notes/api/actions/add_note"),
            None,
            &serde_json::json!({ "text": "top-level note" }),
        )
        .await,
        reqwest::StatusCode::OK,
        "top-level actions stay unauthenticated"
    );

    // ── the 401 matrix: EVERYTHING under /t/<tenant>/ requires that
    //    tenant's bearer — reads, SSE, sync, and MCP included ────────────────
    for path in [
        "/t/alice/",
        "/t/alice/api/fleet",
        "/t/alice/notes/",
        "/t/alice/notes/healthz",
        "/t/alice/notes/api/state",
        "/t/alice/notes/api/events",
        "/t/alice/notes/sync/events",
        "/t/alice/notes/mcp",
        "/t/alice/mcp",
        "/t/ghost/notes/api/state", // nonexistent tenant: same 401, no oracle
    ] {
        let url = format!("{base}{path}");
        assert_eq!(
            get_status(&client, &url, None).await,
            unauthorized,
            "no token: {path}"
        );
        assert_eq!(
            get_status(&client, &url, Some(BOB_TOKEN)).await,
            unauthorized,
            "bob's token on alice's namespace: {path}"
        );
        assert_eq!(
            get_status(&client, &url, Some(HOST_TOKEN)).await,
            unauthorized,
            "the host token is not a tenant token: {path}"
        );
    }
    // Mutations and sync exchanges too.
    assert_eq!(
        action(
            &client,
            &format!("{base}/t/alice/notes/api/actions/add_note"),
            None,
            &serde_json::json!({ "text": "x" }),
        )
        .await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    let sync_unauthed = client
        .post(format!("{base}/t/alice/notes/sync"))
        .header("X-Tangram-Session", "no-token-session")
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(Vec::new())
        .send()
        .await
        .expect("unauthed sync post");
    assert_eq!(sync_unauthed.status(), reqwest::StatusCode::UNAUTHORIZED);

    // With the right token the same surfaces answer.
    for path in [
        "/t/alice/",
        "/t/alice/api/fleet",
        "/t/alice/notes/",
        "/t/alice/notes/api/state",
    ] {
        assert_eq!(
            get_status(&client, &format!("{base}{path}"), Some(ALICE_TOKEN)).await,
            ok,
            "alice's token on {path}"
        );
    }
    // Tenant MCP serves with the bearer (direct mode — no gateway here).
    let mcp_init = client
        .post(format!("{base}/t/alice/notes/mcp"))
        .bearer_auth(ALICE_TOKEN)
        .header("accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "phase5-test", "version": "0" }
            }
        }))
        .send()
        .await
        .expect("tenant mcp initialize");
    assert!(
        mcp_init.status().is_success(),
        "tenant mcp: {}",
        mcp_init.status()
    );
    // No gateway in this test → the per-tenant aggregate 404s (authed).
    assert_eq!(
        get_status(&client, &format!("{base}/t/alice/mcp"), Some(ALICE_TOKEN)).await,
        not_found
    );

    // ── isolation: same app name in both tenants, fully separate docs ──────
    assert_eq!(
        action(
            &client,
            &format!("{base}/t/alice/notes/api/actions/add_note"),
            Some(ALICE_TOKEN),
            &serde_json::json!({ "text": "alice secret" }),
        )
        .await,
        reqwest::StatusCode::OK
    );
    // bob installs the same app through HIS registry (the default bootstrap
    // gave him only a registry) — converges only his namespace.
    assert_eq!(
        action(
            &client,
            &format!("{base}/t/bob/registry/api/actions/install_app"),
            Some(BOB_TOKEN),
            &serde_json::json!({
                "name": "notes",
                "component": component("notes").display().to_string(),
                "ui": root.join("apps/notes/ui").display().to_string(),
            }),
        )
        .await,
        reqwest::StatusCode::OK
    );
    wait_for("bob's notes healthy", Duration::from_secs(30), || async {
        get_status(
            &client,
            &format!("{base}/t/bob/notes/healthz"),
            Some(BOB_TOKEN),
        )
        .await
            == ok
    })
    .await;
    assert_eq!(
        action(
            &client,
            &format!("{base}/t/bob/notes/api/actions/add_note"),
            Some(BOB_TOKEN),
            &serde_json::json!({ "text": "bob secret" }),
        )
        .await,
        reqwest::StatusCode::OK
    );

    let alice_state = get_json(
        &client,
        &format!("{base}/t/alice/notes/api/state"),
        ALICE_TOKEN,
    )
    .await;
    let bob_state = get_json(&client, &format!("{base}/t/bob/notes/api/state"), BOB_TOKEN).await;
    let texts = |state: &serde_json::Value| -> Vec<String> {
        state["notes"]
            .as_array()
            .expect("notes array")
            .iter()
            .map(|n| n["text"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(texts(&alice_state), vec!["alice secret"]);
    assert_eq!(texts(&bob_state), vec!["bob secret"]);
    let top_state: serde_json::Value = client
        .get(format!("{base}/notes/api/state"))
        .send()
        .await
        .expect("top state")
        .json()
        .await
        .expect("top state json");
    assert_eq!(texts(&top_state), vec!["top-level note"]);

    // …and on disk: each doc under its own tenant tree, nothing crossed.
    let tenants_root = home.join(".tangram-tenants");
    for doc in [
        "alice/registry/registry.automerge",
        "alice/notes/notes.automerge",
        "bob/registry/registry.automerge",
        "bob/notes/notes.automerge",
    ] {
        assert!(
            tenants_root.join(doc).exists(),
            "expected {doc} under {}",
            tenants_root.display()
        );
    }
    // The top-level apps stayed in their usual `$HOME/.<app>` homes.
    assert!(home.join(".notes/notes.automerge").exists());
    assert!(!home.join(".tangram-tenants/notes").exists());

    // The global fleet (unauthenticated) lists no tenant apps; each tenant
    // fleet lists exactly their own.
    let global_fleet: serde_json::Value = client
        .get(format!("{base}/api/fleet"))
        .send()
        .await
        .expect("global fleet")
        .json()
        .await
        .expect("fleet json");
    let global_names: Vec<&str> = global_fleet["apps"]
        .as_array()
        .expect("apps")
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert_eq!(
        global_names,
        ["notes", "registry"],
        "no tenant apps on the global fleet"
    );
    let bob_fleet = get_json(&client, &format!("{base}/t/bob/api/fleet"), BOB_TOKEN).await;
    let bob_names: Vec<&str> = bob_fleet["apps"]
        .as_array()
        .expect("apps")
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert_eq!(bob_names, ["notes", "registry"]);
    assert_eq!(bob_fleet["tenant"], "bob");

    // ── allow_hosts ceiling: the effective grant is the intersection ───────
    assert_eq!(
        action(
            &client,
            &format!("{base}/t/alice/registry/api/actions/install_app"),
            Some(ALICE_TOKEN),
            &serde_json::json!({
                "name": "nutrition",
                "component": component("nutrition").display().to_string(),
                "ui": root.join("apps/nutrition/ui").display().to_string(),
                "allow_hosts": ["api.calorieninjas.com", "evil.example.com"],
            }),
        )
        .await,
        reqwest::StatusCode::OK
    );
    wait_for(
        "alice's nutrition healthy",
        Duration::from_secs(30),
        || async {
            get_status(
                &client,
                &format!("{base}/t/alice/nutrition/healthz"),
                Some(ALICE_TOKEN),
            )
            .await
                == ok
        },
    )
    .await;
    let alice_fleet = get_json(&client, &format!("{base}/t/alice/api/fleet"), ALICE_TOKEN).await;
    let nutrition = fleet_app(&alice_fleet, "nutrition");
    assert_eq!(
        nutrition["allow_hosts"],
        serde_json::json!(["api.calorieninjas.com"]),
        "evil.example.com must be intersected away by the tenant ceiling"
    );
    assert_eq!(nutrition["source"], "registry");
    assert_eq!(nutrition["healthy"], true);
    // bob's namespace is untouched by alice's install.
    assert_eq!(
        get_status(
            &client,
            &format!("{base}/t/bob/nutrition/healthz"),
            Some(BOB_TOKEN)
        )
        .await,
        not_found
    );
    // …and so is the top level (also: tenant installs never reach /<app>/).
    assert_eq!(
        get_status(&client, &format!("{base}/nutrition/healthz"), None).await,
        not_found
    );

    // ── max_apps: the 4th app errors in alice's fleet and never runs ───────
    assert_eq!(
        action(
            &client,
            &format!("{base}/t/alice/registry/api/actions/install_app"),
            Some(ALICE_TOKEN),
            &serde_json::json!({
                "name": "extra",
                "component": component("notes").display().to_string(),
                "ui": root.join("apps/notes/ui").display().to_string(),
            }),
        )
        .await,
        reqwest::StatusCode::OK
    );
    wait_for(
        "extra over-cap error in fleet",
        Duration::from_secs(30),
        || async {
            let fleet = get_json(&client, &format!("{base}/t/alice/api/fleet"), ALICE_TOKEN).await;
            fleet["apps"].as_array().is_some_and(|apps| {
                apps.iter().any(|a| {
                    a["name"] == "extra"
                        && a["running"] == false
                        && a["error"]
                            .as_str()
                            .is_some_and(|e| e.contains("max_apps = 3"))
                })
            })
        },
    )
    .await;
    assert_eq!(
        get_status(
            &client,
            &format!("{base}/t/alice/extra/healthz"),
            Some(ALICE_TOKEN)
        )
        .await,
        not_found,
        "an over-cap app must not serve"
    );

    // ── data_dir escape: rejected, recorded in the tenant fleet ────────────
    let evil_dir = format!("{}/evil-escape", home.display());
    assert_eq!(
        action(
            &client,
            &format!("{base}/t/bob/registry/api/actions/install_app"),
            Some(BOB_TOKEN),
            &serde_json::json!({
                "name": "evil",
                "component": component("notes").display().to_string(),
                "ui": root.join("apps/notes/ui").display().to_string(),
                "data_dir": evil_dir,
            }),
        )
        .await,
        reqwest::StatusCode::OK
    );
    wait_for(
        "evil data_dir error in fleet",
        Duration::from_secs(30),
        || async {
            let fleet = get_json(&client, &format!("{base}/t/bob/api/fleet"), BOB_TOKEN).await;
            fleet["apps"].as_array().is_some_and(|apps| {
                apps.iter().any(|a| {
                    a["name"] == "evil"
                        && a["running"] == false
                        && a["error"]
                            .as_str()
                            .is_some_and(|e| e.contains("relative path"))
                })
            })
        },
    )
    .await;
    assert!(
        !Path::new(&evil_dir).exists(),
        "no document may land outside the tenant tree"
    );

    // ── sync with the bearer: a native replica converges alice's notes ─────
    let replica = std::sync::Arc::new(TestDoc::new());
    let sync_task = tokio::spawn(tangram::sync::run_remote(
        format!("{base}/t/alice/notes/sync"),
        Some(ALICE_TOKEN.to_string()),
        replica.clone(),
    ));
    wait_for(
        "replica converges alice's notes",
        Duration::from_secs(15),
        || async { replica.notes_len() == 1 },
    )
    .await;
    sync_task.abort();
    // (The tokenless rejection is pinned above: POST /sync and GET
    // /sync/events without the bearer → 401.)

    // ── restart persistence, per tenant ─────────────────────────────────────
    drop(host); // kill + wait
    let toml_text = std::fs::read_to_string(&apps_toml).expect("read apps.toml");
    assert!(
        !toml_text.contains("nutrition") && !toml_text.contains("[tenants.bob.apps"),
        "installed apps must come back from the tenants' registry docs, not the file"
    );
    let log2 = home.join("host-2.log");
    let _host2 = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log2);
    wait_for(
        "bob's notes after restart",
        Duration::from_secs(60),
        || async {
            get_status(
                &client,
                &format!("{base}/t/bob/notes/healthz"),
                Some(BOB_TOKEN),
            )
            .await
                == ok
        },
    )
    .await;
    wait_for(
        "alice's nutrition after restart",
        Duration::from_secs(60),
        || async {
            get_status(
                &client,
                &format!("{base}/t/alice/nutrition/healthz"),
                Some(ALICE_TOKEN),
            )
            .await
                == ok
        },
    )
    .await;
    let bob_state = get_json(&client, &format!("{base}/t/bob/notes/api/state"), BOB_TOKEN).await;
    assert_eq!(
        texts(&bob_state),
        vec!["bob secret"],
        "bob's data survives the restart"
    );
    let alice_state = get_json(
        &client,
        &format!("{base}/t/alice/notes/api/state"),
        ALICE_TOKEN,
    )
    .await;
    assert_eq!(texts(&alice_state), vec!["alice secret"]);
    // The auth wall stands after the restart too.
    assert_eq!(
        get_status(
            &client,
            &format!("{base}/t/alice/notes/api/state"),
            Some(BOB_TOKEN)
        )
        .await,
        unauthorized
    );
}
