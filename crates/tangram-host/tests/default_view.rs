//! Phase S36: the tangram shell is the host's default `/` view.
//!
//! `GET /` redirects to `/tangram/` when a top-level app named `tangram` is
//! present (the Obsidian-style shell), so visiting the host lands you in the
//! shell. When no `tangram` app is configured, `/` falls back to the built-in
//! centered app-list index — so a host without the shell app still has a
//! usable root. Both behaviors are asserted here against the real binary.
//!
//! We assert a REDIRECT (not the shell HTML served at `/`) deliberately: the
//! shell is built with Vite `base: "./"` and fetches relative paths assuming a
//! `/tangram/` mount; serving its bytes at `/` would break asset loading. The
//! 307 preserves every relative-path assumption.
//!
//! Requires the wasm components (built by CI before `cargo test`):
//!   cargo build -p tangram-app-tangram -p tangram-notes \
//!     --lib --target wasm32-wasip2 --release
//! SKIPS (with a notice) when they are missing.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

mod support;
use support::{HostProc, component, free_port, status_of, wait_for, workspace_root};

fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let child = Command::new(env!("CARGO_BIN_EXE_tangram-host"))
        .arg(apps_toml)
        // cwd = scratch HOME so the repo's .env is NOT loaded by dotenvy
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn tangram-host");
    HostProc(child)
}

/// A client that does NOT follow redirects, so we can observe the 307 itself.
fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build client")
}

#[tokio::test]
async fn root_redirects_to_shell_when_tangram_present() {
    for name in ["tangram_app", "notes"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING default_view: {} missing — build the wasm components first \
                 (cargo build -p tangram-app-tangram -p tangram-notes \
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
    // A host WITH the tangram shell app present, plus notes alongside it.
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.tangram]
component = "{tangram}"
ui = "{root}/apps/tangram/ui/dist"

[apps.notes]
component = "{notes}"
ui = "{root}/apps/notes/ui"
"#,
            tangram = component("tangram_app").display(),
            notes = component("notes").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host-with-tangram.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = no_redirect_client();

    // Shell app comes up.
    wait_for("tangram healthz", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/tangram/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;

    // `GET /` → 307 to `/tangram/` (default view is the shell).
    let res = client.get(format!("{base}/")).send().await.expect("GET /");
    assert_eq!(
        res.status(),
        reqwest::StatusCode::TEMPORARY_REDIRECT,
        "GET / must redirect to the shell when a tangram app is present"
    );
    assert_eq!(
        res.headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/tangram/"),
        "redirect target must be the shell mount"
    );

    // `/tangram/` itself still serves the shell UI (relative-path bundle).
    let res = client
        .get(format!("{base}/tangram/"))
        .send()
        .await
        .expect("GET /tangram/");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    let body = res.text().await.expect("shell html");
    assert!(
        body.contains("./assets/"),
        "shell index must reference its relative assets: {body}"
    );
}

#[tokio::test]
async fn root_falls_back_to_builtin_index_without_tangram() {
    if !component("notes").exists() {
        eprintln!(
            "SKIPPING default_view fallback: {} missing — build the wasm components first",
            component("notes").display()
        );
        return;
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    // A host WITHOUT a tangram app — only notes.
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.notes]
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
    let log = home.join("host-no-tangram.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = no_redirect_client();

    wait_for("notes healthz", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/notes/healthz")).await == Some(reqwest::StatusCode::OK)
    })
    .await;

    // `GET /` → 200, the built-in centered app-list index (no redirect).
    let res = client.get(format!("{base}/")).send().await.expect("GET /");
    assert_eq!(
        res.status(),
        reqwest::StatusCode::OK,
        "GET / must serve the built-in index when no tangram app is present"
    );
    let body = res.text().await.expect("index html");
    assert!(
        body.contains("WASM components running on this host"),
        "fallback must be the built-in index page: {body}"
    );
    assert!(
        body.contains("/notes/"),
        "built-in index lists the configured apps: {body}"
    );
}
