//! `GET /api/capabilities` parity between the native apps and tangram-host.
//!
//! The native nutrition binary serves the probe from a custom route
//! (`apps/nutrition/src/api.rs`); under the host the same JSON comes from the
//! component's `describe()` manifest. Both derive it from the app's
//! environment through ONE constructor (`nutrition::capabilities_json`), so
//! for the same env the two surfaces must answer byte-identically — this
//! test pins that, plus the 404 for apps that publish no capabilities
//! (notes), natively and under the host.
//!
//! Std-only: the binaries under test are spawned as subprocesses on 19xxx
//! loopback ports with scratch HOMEs/data dirs, and polled over plain HTTP.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// Build everything the test spawns: the native app binaries, the host
/// binary, and the WASM components (release, matching `apps.toml` paths).
fn build_artifacts(root: &Path) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    for args in [
        vec![
            "build",
            "-p",
            "tangram-notes",
            "-p",
            "tangram-nutrition",
            "-p",
            "tangram-host",
        ],
        vec![
            "build",
            "-p",
            "tangram-notes",
            "-p",
            "tangram-nutrition",
            "--lib",
            "--target",
            "wasm32-wasip2",
            "--release",
        ],
    ] {
        let status = Command::new(&cargo)
            .args(&args)
            .current_dir(root)
            .status()
            .expect("spawn cargo build");
        assert!(status.success(), "cargo {args:?} failed");
    }
}

/// A free loopback port in the test range (live instances stay on :8080).
fn free_port() -> u16 {
    (19310..19400)
        .find(|port| TcpListener::bind(("127.0.0.1", *port)).is_ok())
        .expect("a free 19xxx port")
}

/// Kill the child on drop so failures don't leak servers.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Scratch tree removed on drop.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Minimal HTTP GET: status code + body.
fn http_get(port: u16, path: &str) -> std::io::Result<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

fn wait_healthy(port: u16, path: &str, server: &mut Server) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok((200, _)) = http_get(port, path) {
            return;
        }
        if let Ok(Some(status)) = server.0.try_wait() {
            panic!("server on :{port} exited early: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "server on :{port} not healthy at {path} within 120s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Spawn one of the built binaries with a controlled environment: scratch
/// HOME and cwd (so the repo's `.env` is NOT loaded), strategy-selecting
/// vars cleared, then `env` applied on top.
fn spawn(bin: &Path, args: &[&str], scratch: &Path, port: u16, env: &[(&str, &str)]) -> Server {
    let mut command = Command::new(bin);
    command
        .args(args)
        .current_dir(scratch)
        .env_remove("NUTRITION_STRATEGY")
        .env_remove("CALORIENINJAS_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("TANGRAM_REMOTE")
        .env("HOME", scratch)
        .env("BIND_ADDR", format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    Server(
        command
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {bin:?}: {e}")),
    )
}

#[test]
fn capabilities_parity_native_vs_host() {
    let root = workspace_root();
    build_artifacts(&root);
    let debug = root.join("target/debug");
    let wasm = root.join("target/wasm32-wasip2/release");

    let scratch = Scratch(
        std::env::temp_dir().join(format!("tangram-capabilities-test-{}", std::process::id())),
    );
    std::fs::create_dir_all(&scratch.0).expect("scratch dir");

    // ── native references, one per environment ──────────────────────────
    let nutrition_bin = debug.join("tangram-nutrition");
    let online_port = free_port();
    let online_dir = scratch.0.join("native-online");
    std::fs::create_dir_all(&online_dir).unwrap();
    let mut native_online = spawn(
        &nutrition_bin,
        &[],
        &online_dir,
        online_port,
        &[("NUTRITION_STRATEGY", "calorieninjas")],
    );
    wait_healthy(online_port, "/healthz", &mut native_online);

    let offline_port = free_port();
    let offline_dir = scratch.0.join("native-offline");
    std::fs::create_dir_all(&offline_dir).unwrap();
    let mut native_offline = spawn(&nutrition_bin, &[], &offline_dir, offline_port, &[]);
    wait_healthy(offline_port, "/healthz", &mut native_offline);

    let notes_port = free_port();
    let notes_dir = scratch.0.join("native-notes");
    std::fs::create_dir_all(&notes_dir).unwrap();
    let mut native_notes = spawn(
        &debug.join("tangram-notes"),
        &[],
        &notes_dir,
        notes_port,
        &[],
    );
    wait_healthy(notes_port, "/healthz", &mut native_notes);

    // ── the host, with the same envs granted per app in apps.toml ───────
    let host_dir = scratch.0.join("host");
    std::fs::create_dir_all(&host_dir).unwrap();
    let apps_toml = host_dir.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.notes]
component = "{wasm}/notes.wasm"
ui = "{root}/apps/notes/ui"
data_dir = "{host_dir}/notes-data"

[apps.nutrition]
component = "{wasm}/nutrition.wasm"
ui = "{root}/apps/nutrition/ui"
data_dir = "{host_dir}/nutrition-data"
allow_hosts = ["api.calorieninjas.com"]

[apps.nutrition.env]
NUTRITION_STRATEGY = "calorieninjas"

[apps.nutrition-offline]
component = "{wasm}/nutrition.wasm"
ui = "{root}/apps/nutrition/ui"
data_dir = "{host_dir}/nutrition-offline-data"
"#,
            wasm = wasm.display(),
            root = root.display(),
            host_dir = host_dir.display(),
        ),
    )
    .expect("write apps.toml");

    let host_port = free_port();
    let mut host = spawn(
        &debug.join("tangram-host"),
        &[apps_toml.to_str().unwrap()],
        &host_dir,
        host_port,
        &[],
    );
    wait_healthy(host_port, "/nutrition/healthz", &mut host);
    wait_healthy(host_port, "/nutrition-offline/healthz", &mut host);
    wait_healthy(host_port, "/notes/healthz", &mut host);

    // ── parity: same env ⇒ byte-identical capabilities JSON ─────────────
    let (status, native_online_caps) = http_get(online_port, "/api/capabilities").unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        native_online_caps,
        r#"{"description_input":true,"strategy":"calorieninjas"}"#
    );
    let (status, host_online_caps) = http_get(host_port, "/nutrition/api/capabilities").unwrap();
    assert_eq!(status, 200);
    assert_eq!(host_online_caps, native_online_caps, "online parity");

    let (status, native_offline_caps) = http_get(offline_port, "/api/capabilities").unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        native_offline_caps,
        r#"{"description_input":false,"strategy":"offline"}"#
    );
    let (status, host_offline_caps) =
        http_get(host_port, "/nutrition-offline/api/capabilities").unwrap();
    assert_eq!(status, 200);
    assert_eq!(host_offline_caps, native_offline_caps, "offline parity");

    // ── apps without capabilities: 404 natively and under the host ──────
    let (status, _) = http_get(notes_port, "/api/capabilities").unwrap();
    assert_eq!(status, 404, "native notes has no capabilities route");
    let (status, body) = http_get(host_port, "/notes/api/capabilities").unwrap();
    assert_eq!(status, 404, "host notes capabilities must 404");
    assert!(body.is_empty(), "host 404 body is empty, got {body:?}");
}
