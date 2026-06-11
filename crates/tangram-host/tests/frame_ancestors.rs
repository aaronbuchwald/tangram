//! Framing policy parity: a host-served (WASM-component) app must carry the
//! same `Content-Security-Policy: frame-ancestors` header the native SDK
//! emits (`crates/tangram/src/app.rs`). Without it, host-served apps have no
//! framing bound — needed before app-in-note embedding is safe (#32).
//!
//! Host-level `FRAME_ANCESTORS` configures the value (default `*`, matching
//! the SDK); this test pins both the default and an explicit override on a
//! component-backed app's responses.
//!
//! Std-only: the host binary is spawned as a subprocess on a 19xxx loopback
//! port with a scratch HOME/data dir, and probed over plain HTTP.

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

/// Build the host binary plus the notes WASM component (release, matching the
/// `apps.toml` path the test writes).
fn build_artifacts(root: &Path) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    for args in [
        vec!["build", "-p", "tangram-host"],
        vec![
            "build",
            "-p",
            "tangram-notes",
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
    (19400..19490)
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

/// An HTTP response: status, lowercased response headers, body.
type HttpResponse = (u16, Vec<(String, String)>, String);

/// Minimal HTTP GET returning (status, lowercased response headers, body).
fn http_get(port: u16, path: &str) -> std::io::Result<HttpResponse> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .map(|(head, body)| (head.to_string(), body.to_string()))
        .unwrap_or_else(|| (response.clone(), String::new()));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    Ok((status, headers, body))
}

fn wait_healthy(port: u16, path: &str, server: &mut Server) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Ok((200, _, _)) = http_get(port, path) {
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

/// The single `content-security-policy` header value, or panic.
fn csp(headers: &[(String, String)]) -> &str {
    let matches: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k == "content-security-policy")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one content-security-policy header expected, got {headers:?}"
    );
    matches[0].1.as_str()
}

/// Spawn the host binary with a scratch HOME/cwd (so the repo `.env` is not
/// loaded), `apps.toml` as its arg, and `env` applied on top.
fn spawn_host(
    bin: &Path,
    apps_toml: &Path,
    scratch: &Path,
    port: u16,
    env: &[(&str, &str)],
) -> Server {
    let mut command = Command::new(bin);
    command
        .arg(apps_toml)
        .current_dir(scratch)
        .env_remove("TANGRAM_REMOTE")
        .env_remove("FRAME_ANCESTORS")
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

/// Write an `apps.toml` with a single notes component app and return its path.
fn write_apps_toml(host_dir: &Path, root: &Path, wasm: &Path) -> PathBuf {
    let apps_toml = host_dir.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.notes]
component = "{wasm}/notes.wasm"
ui = "{root}/apps/notes/ui"
data_dir = "{host_dir}/notes-data"
"#,
            wasm = wasm.display(),
            root = root.display(),
            host_dir = host_dir.display(),
        ),
    )
    .expect("write apps.toml");
    apps_toml
}

#[test]
fn host_served_app_emits_frame_ancestors_csp() {
    let root = workspace_root();
    build_artifacts(&root);
    let debug = root.join("target/debug");
    let wasm = root.join("target/wasm32-wasip2/release");
    let host_bin = debug.join("tangram-host");

    let scratch = Scratch(std::env::temp_dir().join(format!(
        "tangram-frame-ancestors-test-{}",
        std::process::id()
    )));
    std::fs::create_dir_all(&scratch.0).expect("scratch dir");

    // ── default: no FRAME_ANCESTORS env ⇒ `frame-ancestors *` (SDK default) ──
    let default_dir = scratch.0.join("default");
    std::fs::create_dir_all(&default_dir).unwrap();
    let default_toml = write_apps_toml(&default_dir, &root, &wasm);
    let default_port = free_port();
    let mut default_host = spawn_host(&host_bin, &default_toml, &default_dir, default_port, &[]);
    wait_healthy(default_port, "/notes/healthz", &mut default_host);

    // The header rides every response on the per-app surface, including the
    // static UI served by the fallback.
    for path in ["/notes/", "/notes/api/state", "/notes/api/actions"] {
        let (status, headers, _) = http_get(default_port, path).unwrap();
        assert_eq!(status, 200, "{path}");
        assert_eq!(csp(&headers), "frame-ancestors *", "default at {path}");
    }

    // ── explicit override: FRAME_ANCESTORS scopes embedding ────────────────
    let scoped_dir = scratch.0.join("scoped");
    std::fs::create_dir_all(&scoped_dir).unwrap();
    let scoped_toml = write_apps_toml(&scoped_dir, &root, &wasm);
    let scoped_port = free_port();
    let mut scoped_host = spawn_host(
        &host_bin,
        &scoped_toml,
        &scoped_dir,
        scoped_port,
        &[("FRAME_ANCESTORS", "'self' https://notes.example")],
    );
    wait_healthy(scoped_port, "/notes/healthz", &mut scoped_host);

    let (status, headers, _) = http_get(scoped_port, "/notes/api/state").unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        csp(&headers),
        "frame-ancestors 'self' https://notes.example",
        "FRAME_ANCESTORS override is reflected"
    );
}
