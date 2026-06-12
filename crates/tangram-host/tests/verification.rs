//! End-to-end manifest-verification tests (design:
//! `docs/design/manifest-verification-plan.md`).
//!
//! Spawns the real `tangram-host` binary against a scratch HOME and a
//! bootstrap `apps.toml` carrying `[apps.<app>.declared]` manifests, then
//! asserts the converge-time verification chain `granted ⊆ declared ⊆
//! audited`.
//!
//! Requires the wasm components (built by CI before `cargo test`):
//!   cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
//!     --lib --target wasm32-wasip2 --release
//! The test SKIPS (with a notice) when they are missing.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

mod support;
use support::{HostProc, component, free_port, status_of, wait_for, workspace_root};

fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let child = Command::new(env!("CARGO_BIN_EXE_tangram-host"))
        .arg(apps_toml)
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        .env_remove("TANGRAM_AUTH_TOKEN")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn tangram-host");
    HostProc(child)
}

fn have_components(names: &[&str], test: &str) -> bool {
    for name in names {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING {test}: {} missing — build the wasm components first \
                 (cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
                 --lib --target wasm32-wasip2 --release)",
                component(name).display()
            );
            return false;
        }
    }
    true
}

/// Fetch `/api/fleet` and return the entry for `app`, if present.
async fn fleet_entry(client: &reqwest::Client, base: &str, app: &str) -> Option<serde_json::Value> {
    let fleet: serde_json::Value = client
        .get(format!("{base}/api/fleet"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    fleet["apps"]
        .as_array()?
        .iter()
        .find(|a| a["name"] == app)
        .cloned()
}

/// CP2 — a spec over-granting a host fails converge with a clear error.
#[tokio::test]
async fn over_grant_fails_converge() {
    if !have_components(&["nutrition"], "over_grant_fails_converge") {
        return;
    }
    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    // nutrition imports http-fetch, so the vacuous-grant gate does not fire;
    // the over-grant gate does: granted {api.evil.com} ⊄ declared
    // {api.calorieninjas.com}.
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.nutrition]
component = "{nutrition}"
ui = "{root}/apps/nutrition/ui"
allow_hosts = ["api.evil.com"]

[apps.nutrition.declared.network]
kind = "hosts"
hosts = ["api.calorieninjas.com"]
"#,
            nutrition = component("nutrition").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    wait_for(
        "nutrition over-grant fleet error",
        Duration::from_secs(120),
        || async {
            match fleet_entry(&client, &base, "nutrition").await {
                Some(entry) => entry["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("api.evil.com") && e.contains("does not declare")),
                None => false,
            }
        },
    )
    .await;

    let entry = fleet_entry(&client, &base, "nutrition")
        .await
        .expect("nutrition in fleet");
    assert_eq!(entry["running"], false, "over-granted app must not run");
    assert_eq!(
        status_of(&client, &format!("{base}/nutrition/healthz")).await,
        Some(reqwest::StatusCode::NOT_FOUND),
        "the unverified app's routes are absent"
    );
}

/// CP3 — a component that imports no http-fetch cannot be granted any host
/// (notes), and notes with no grant verifies trivially.
#[tokio::test]
async fn vacuous_grant_fails() {
    if !have_components(&["notes"], "vacuous_grant_fails") {
        return;
    }
    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    // `badnotes` grants notes (no http-fetch) an outbound host AND declares it,
    // so the over-grant gate passes but the vacuous-grant gate hard-fails.
    // `goodnotes` grants nothing → trivially verified, runs.
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.badnotes]
component = "{notes}"
ui = "{root}/apps/notes/ui"
allow_hosts = ["api.example.com"]

[apps.badnotes.declared.network]
kind = "hosts"
hosts = ["api.example.com"]

[apps.goodnotes]
component = "{notes}"
ui = "{root}/apps/notes/ui"
"#,
            notes = component("notes").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    // goodnotes (no grant) comes up healthy.
    wait_for("goodnotes healthy", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/goodnotes/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;

    // badnotes hard-fails with the vacuous-grant message and does not run.
    wait_for(
        "badnotes vacuous-grant error",
        Duration::from_secs(30),
        || async {
            match fleet_entry(&client, &base, "badnotes").await {
                Some(entry) => entry["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("no http-fetch")),
                None => false,
            }
        },
    )
    .await;
    let bad = fleet_entry(&client, &base, "badnotes")
        .await
        .expect("badnotes in fleet");
    assert_eq!(bad["running"], false, "vacuous-grant app must not run");
    assert_eq!(
        status_of(&client, &format!("{base}/badnotes/healthz")).await,
        Some(reqwest::StatusCode::NOT_FOUND),
    );
}
