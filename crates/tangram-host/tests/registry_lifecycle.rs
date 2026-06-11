//! End-to-end Phase 3 test: registry app as source of truth + bearer auth.
//!
//! Spawns the real `tangram-host` binary against a scratch HOME and a
//! bootstrap `apps.toml` (registry + notes), then walks the full lifecycle:
//! unauthenticated install is rejected (HTTP and MCP), an authed
//! `install_app` converges to a healthy app, `remove_app` drops its routes,
//! an MCP `tools/call` reinstall works, and — the Phase 3 point — the
//! registry-installed app COMES BACK after a host restart because it lives
//! in the replicated registry document, not in `apps.toml`.
//!
//! Requires the wasm components (built by CI before `cargo test`):
//!   cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
//!     --lib --target wasm32-wasip2 --release
//! The test SKIPS (with a notice) when they are missing, so a plain
//! `cargo test` without the wasm target still passes.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "test-fleet-token";

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
        .env("TANGRAM_AUTH_TOKEN", TOKEN)
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

async fn status_of(client: &reqwest::Client, url: &str) -> Option<reqwest::StatusCode> {
    client.get(url).send().await.ok().map(|r| r.status())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

#[tokio::test]
async fn registry_lifecycle_with_auth_and_restart_persistence() {
    for name in ["registry", "notes", "nutrition"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING registry_lifecycle: {} missing — build the wasm components first \
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
ui = "{root}/apps/notes/ui"
"#,
            registry = component("registry").display(),
            notes = component("notes").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host-1.log");
    let host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    // Bootstrap apps come up from apps.toml.
    wait_for("registry healthz", Duration::from_secs(90), || async {
        status_of(&client, &format!("{base}/registry/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;
    assert_eq!(
        status_of(&client, &format!("{base}/notes/healthz")).await,
        Some(reqwest::StatusCode::OK)
    );

    // ── auth: mutating action POST without a bearer token → 401 ────────────
    let install_args = serde_json::json!({
        "name": "nutrition",
        "component": component("nutrition").display().to_string(),
        "ui": root.join("apps/nutrition/ui").display().to_string(),
        "allow_hosts": ["api.calorieninjas.com"],
        "env": [{ "key": "NUTRITION_STRATEGY", "value": "offline" }],
    });
    let res = client
        .post(format!("{base}/registry/api/actions/install_app"))
        .json(&install_args)
        .send()
        .await
        .expect("unauthed install");
    assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
    // wrong token → 401 too
    let res = client
        .post(format!("{base}/registry/api/actions/install_app"))
        .bearer_auth("wrong")
        .json(&install_args)
        .send()
        .await
        .expect("wrong-token install");
    assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);

    // ── auth: MCP tools/call of a mutating tool without token → 401;
    //    read-only surfaces stay open ────────────────────────────────────────
    let mcp = format!("{base}/registry/mcp");
    let res = client
        .post(&mcp)
        .header("accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "install_app", "arguments": install_args }
        }))
        .send()
        .await
        .expect("unauthed mcp call");
    assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
    let res = client
        .post(&mcp)
        .header("accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "list_apps", "arguments": {} }
        }))
        .send()
        .await
        .expect("unauthed read mcp call");
    assert_ne!(
        res.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "non-mutating tools stay open (rmcp may still reject the missing session)"
    );

    // Non-gated app (notes, from the file, no require_auth): actions open.
    let res = client
        .post(format!("{base}/notes/api/actions/add_note"))
        .json(&serde_json::json!({ "text": "hello" }))
        .send()
        .await
        .expect("notes action");
    assert_eq!(res.status(), reqwest::StatusCode::OK);

    // ── install via authed POST → app converges to healthy ─────────────────
    let installed_at = Instant::now();
    let res = client
        .post(format!("{base}/registry/api/actions/install_app"))
        .bearer_auth(TOKEN)
        .json(&install_args)
        .send()
        .await
        .expect("authed install");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    wait_for("nutrition healthy", Duration::from_secs(90), || async {
        status_of(&client, &format!("{base}/nutrition/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;
    println!(
        "install_app → /nutrition/ healthy in {:?}",
        installed_at.elapsed()
    );

    // Fleet status reports it as a healthy registry-sourced app.
    let fleet: serde_json::Value = client
        .get(format!("{base}/api/fleet"))
        .send()
        .await
        .expect("fleet")
        .json()
        .await
        .expect("fleet json");
    let nutrition = fleet["apps"]
        .as_array()
        .expect("apps array")
        .iter()
        .find(|a| a["name"] == "nutrition")
        .expect("nutrition in fleet");
    assert_eq!(nutrition["source"], "registry");
    assert_eq!(nutrition["running"], true);
    assert_eq!(nutrition["healthy"], true);
    assert_eq!(nutrition["error"], serde_json::Value::Null);

    // The installed app actually works (dispatch through the component).
    let res = client
        .post(format!("{base}/nutrition/api/actions/log_meal"))
        .json(&serde_json::json!({
            "name": "test meal",
            "components": [{ "component": "egg", "qty_g": 100.0 }]
        }))
        .send()
        .await
        .expect("log_meal");
    assert_eq!(res.status(), reqwest::StatusCode::OK);

    // ── remove_app → routes gone ────────────────────────────────────────────
    let res = client
        .post(format!("{base}/registry/api/actions/remove_app"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "name": "nutrition" }))
        .send()
        .await
        .expect("remove");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    wait_for("nutrition routes gone", Duration::from_secs(15), || async {
        status_of(&client, &format!("{base}/nutrition/healthz")).await
            == Some(reqwest::StatusCode::NOT_FOUND)
    })
    .await;

    // ── reinstall via the MCP tools/call path (full handshake, authed) ─────
    let init = client
        .post(&mcp)
        .header("accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "phase3-test", "version": "0" }
            }
        }))
        .send()
        .await
        .expect("mcp initialize");
    assert!(init.status().is_success(), "initialize: {}", init.status());
    let session = init
        .headers()
        .get("mcp-session-id")
        .expect("mcp-session-id header")
        .to_str()
        .expect("session str")
        .to_string();
    let res = client
        .post(&mcp)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .send()
        .await
        .expect("mcp initialized");
    assert!(res.status().is_success(), "initialized: {}", res.status());
    let res = client
        .post(&mcp)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "install_app", "arguments": install_args }
        }))
        .send()
        .await
        .expect("mcp install");
    assert!(res.status().is_success(), "tools/call: {}", res.status());
    let body = res.text().await.expect("tools/call body");
    assert!(
        !body.contains("\"isError\":true"),
        "tools/call reported an error: {body}"
    );
    wait_for(
        "nutrition healthy again",
        Duration::from_secs(90),
        || async {
            status_of(&client, &format!("{base}/nutrition/healthz")).await
                == Some(reqwest::StatusCode::OK)
        },
    )
    .await;

    // ── restart persistence: the registry doc, not apps.toml, brings the
    //    installed app back ──────────────────────────────────────────────────
    drop(host); // kill + wait
    let toml_text = std::fs::read_to_string(&apps_toml).expect("read apps.toml");
    assert!(
        !toml_text.contains("nutrition"),
        "nutrition must NOT be in apps.toml — persistence must come from the registry doc"
    );
    let log2 = home.join("host-2.log");
    let _host2 = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log2);
    wait_for(
        "nutrition healthy after restart",
        Duration::from_secs(90),
        || async {
            status_of(&client, &format!("{base}/nutrition/healthz")).await
                == Some(reqwest::StatusCode::OK)
        },
    )
    .await;
    // ...and the meal logged before the restart survived in its document.
    let state: serde_json::Value = client
        .get(format!("{base}/nutrition/api/state"))
        .send()
        .await
        .expect("nutrition state")
        .json()
        .await
        .expect("state json");
    assert!(
        state["meals"]
            .as_array()
            .is_some_and(|meals| meals.iter().any(|m| m["name"] == "test meal")),
        "meal logged before restart should persist"
    );
}
