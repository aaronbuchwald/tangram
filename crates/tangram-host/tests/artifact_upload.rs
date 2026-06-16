//! End-to-end Phase S2b test: the host artifact store — upload a WASM blob,
//! the HOST computes + content-addresses the sha, the returned hash is
//! immediately installable by URL, garbage is rejected, and the default-off
//! gate + non-loopback/no-token startup refusal hold.
//!
//! Spawns the real `tangram-host` binary against a scratch HOME. Requires the
//! wasm components (built by CI before `cargo test`); SKIPS with a notice
//! when they are missing, so a plain `cargo test` without the wasm target
//! still passes.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

mod support;
use support::{HostProc, component, free_port, status_of, wait_for, workspace_root};

const TOKEN: &str = "test-artifact-token";

/// Spawn a host with the given bind + whether to set a token, and whether to
/// enable open artifact upload.
fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path, with_token: bool) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tangram-host"));
    cmd.arg(apps_toml)
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file));
    if with_token {
        cmd.env("TANGRAM_AUTH_TOKEN", TOKEN);
    } else {
        cmd.env_remove("TANGRAM_AUTH_TOKEN");
    }
    HostProc(cmd.spawn().expect("spawn tangram-host"))
}

fn write_apps_toml(home: &Path, upload_enabled: bool) -> std::path::PathBuf {
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[artifacts]
upload_enabled = {upload_enabled}

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
    apps_toml
}

#[tokio::test]
async fn upload_computes_sha_serves_and_installs_and_rejects_garbage() {
    for name in ["registry", "notes"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING artifact_upload: {} missing — build the wasm components first \
                 (cargo build -p tangram-registry -p tangram-notes --lib \
                 --target wasm32-wasip2 --release)",
                component(name).display()
            );
            return;
        }
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let apps_toml = write_apps_toml(home, true);
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log, true);
    let client = reqwest::Client::new();

    wait_for("registry healthz", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/registry/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;

    let notes_bytes = std::fs::read(component("notes")).expect("read notes.wasm");
    let expected_sha = format!("{:x}", Sha256::digest(&notes_bytes));

    // ── upload requires the bearer token (token is set on this host) ─────────
    let res = client
        .post(format!("{base}/artifacts"))
        .header("Content-Type", "application/wasm")
        .body(notes_bytes.clone())
        .send()
        .await
        .expect("unauthed upload");
    assert_eq!(
        res.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "upload is bearer-gated when the host has a token"
    );

    // ── authed upload: the HOST computes the sha and content-addresses it ────
    let res = client
        .post(format!("{base}/artifacts"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "application/wasm")
        .body(notes_bytes.clone())
        .send()
        .await
        .expect("authed upload");
    assert_eq!(res.status(), reqwest::StatusCode::CREATED);
    let body: serde_json::Value = res.json().await.expect("upload json");
    assert_eq!(
        body["sha256"].as_str().unwrap(),
        expected_sha,
        "the host computes the sha itself"
    );
    assert_eq!(
        body["url"].as_str().unwrap(),
        format!("/artifacts/{expected_sha}.wasm")
    );

    // It landed in the content-addressed store (the SAME one install-by-URL uses).
    let cache_slot = home
        .join(".tangram-host/components")
        .join(format!("{expected_sha}.wasm"));
    assert!(cache_slot.exists(), "stored at {}", cache_slot.display());

    // ── GET /artifacts/<sha>.wasm serves the exact bytes back ────────────────
    let res = client
        .get(format!("{base}/artifacts/{expected_sha}.wasm"))
        .send()
        .await
        .expect("serve artifact");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(
        res.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/wasm")
    );
    let served = res.bytes().await.expect("served bytes");
    assert_eq!(served.as_ref(), notes_bytes.as_slice(), "byte-for-byte");

    // ── garbage is rejected (not a wasm component) ───────────────────────────
    let res = client
        .post(format!("{base}/artifacts"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "application/wasm")
        .body(b"this is definitely not a wasm component".to_vec())
        .send()
        .await
        .expect("garbage upload");
    assert_eq!(res.status(), reqwest::StatusCode::BAD_REQUEST);
    let err: serde_json::Value = res.json().await.expect("error json");
    assert!(
        err["error"].as_str().unwrap().contains("WebAssembly"),
        "garbage is rejected with a clear error: {err}"
    );

    // ── the uploaded artifact installs by URL exactly like any external one:
    //    point the registry at THIS host's /artifacts/<sha>.wasm + the hash ───
    let res = client
        .post(format!("{base}/registry/api/actions/install_app"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({
            "name": "notes",
            "component_url": format!("{base}/artifacts/{expected_sha}.wasm"),
            "component_sha256": expected_sha,
            "ui": workspace_root().join("apps/notes/ui").display().to_string(),
        }))
        .send()
        .await
        .expect("install uploaded artifact by url");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    wait_for("notes healthy", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/notes/healthz")).await == Some(reqwest::StatusCode::OK)
    })
    .await;

    drop(host);
}

/// M1 (#29): the upload route ALWAYS requires the bearer token. A host with
/// upload enabled but NO `TANGRAM_AUTH_TOKEN` configured must refuse every
/// upload with 401 — never anonymous, even on a loopback bind. (Previously a
/// no-token loopback host accepted anonymous uploads.)
#[tokio::test]
async fn upload_requires_a_token_even_on_loopback() {
    if !component("registry").exists() {
        eprintln!("SKIPPING artifact_upload no-token: registry.wasm missing");
        return;
    }
    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let apps_toml = write_apps_toml(home, true);
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    // Loopback bind, upload enabled, NO token.
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log, false);
    let client = reqwest::Client::new();

    wait_for("registry healthz", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/registry/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;

    // A well-formed wasm-component upload, no Authorization header → 401.
    let registry_bytes = std::fs::read(component("registry")).expect("read registry.wasm");
    let res = client
        .post(format!("{base}/artifacts"))
        .header("Content-Type", "application/wasm")
        .body(registry_bytes.clone())
        .send()
        .await
        .expect("anonymous upload");
    assert_eq!(
        res.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "upload must require a token even on loopback when none is configured (M1)"
    );

    // Even presenting *some* bearer cannot help — there is no token to match.
    let res = client
        .post(format!("{base}/artifacts"))
        .bearer_auth("anything")
        .header("Content-Type", "application/wasm")
        .body(registry_bytes)
        .send()
        .await
        .expect("bearer upload to no-token host");
    assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn default_off_blocks_the_upload_route() {
    if !component("registry").exists() {
        eprintln!("SKIPPING artifact_upload default-off: registry.wasm missing");
        return;
    }
    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    // upload_enabled defaults off (omit the [artifacts] flag → false).
    let apps_toml = write_apps_toml(home, false);
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log, true);
    let client = reqwest::Client::new();

    wait_for("registry healthz", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/registry/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;

    // POST and GET both 404 when upload is disabled — no capability oracle.
    let res = client
        .post(format!("{base}/artifacts"))
        .bearer_auth(TOKEN)
        .header("Content-Type", "application/wasm")
        .body(b"\0asm".to_vec())
        .send()
        .await
        .expect("disabled upload");
    assert_eq!(
        res.status(),
        reqwest::StatusCode::NOT_FOUND,
        "POST /artifacts is 404 when upload is off"
    );
    let res = client
        .get(format!("{base}/artifacts/{}.wasm", "a".repeat(64)))
        .send()
        .await
        .expect("disabled serve");
    assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn enabled_on_non_loopback_without_token_refuses_to_start() {
    if !component("notes").exists() {
        eprintln!("SKIPPING artifact_upload refusal: notes.wasm missing");
        return;
    }
    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    // Deliberately NO registry app here — so the refusal under test is the
    // ARTIFACTS gate, not the (earlier) registry-without-token guard.
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[artifacts]
upload_enabled = true

[apps.notes]
component = "{notes}"
ui = "{root}/apps/notes/ui"
"#,
            notes = component("notes").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");
    let log = home.join("host.log");
    // Non-loopback bind + NO token + open upload enabled → the host must bail
    // at startup (mirrors the registry posture). Bind a non-routable but
    // non-loopback address; we only need the startup guard, not a live socket.
    let mut child = spawn_host(home, &apps_toml, "192.0.2.1:65000", &log, false);

    // The process exits (non-zero) rather than serving open upload publicly.
    let status = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.0.try_wait().expect("try_wait") {
                return Some(status);
            }
            if std::time::Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })
    .await
    .expect("join");

    let status = status.expect("host should have exited, not kept serving open upload");
    assert!(!status.success(), "host must refuse (non-zero exit)");
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.contains("upload_enabled") && logged.contains("refusing to bind"),
        "the refusal names the artifacts gate: {logged}"
    );
}
