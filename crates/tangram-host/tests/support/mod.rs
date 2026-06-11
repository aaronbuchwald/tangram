//! Shared test helpers for `tangram-host` integration tests.
//!
//! Cargo treats `tests/support/mod.rs` as a non-test module: include it in
//! each test binary with `mod support;` (not `mod support { ... }`).
//!
//! Each helper here was previously copy-pasted verbatim (or near-verbatim)
//! across every lifecycle test file. This module is the single source of truth
//! for them; per-test `spawn_host` variants are kept in their own files
//! because they take different env vars and return different types.
//!
//! `dead_code` is suppressed at module level: each individual test binary uses
//! a subset of these helpers, so some items will be "unused" from any one
//! binary's perspective.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

// ─── workspace / component paths ─────────────────────────────────────────────

/// Returns the cargo workspace root (two `ancestors()` above the crate
/// manifest dir, which sits at `crates/tangram-host`).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// Path to a wasm32-wasip2 release component built by the CI pre-step.
pub fn component(name: &str) -> PathBuf {
    workspace_root().join(format!("target/wasm32-wasip2/release/{name}.wasm"))
}

// ─── HostProc ─────────────────────────────────────────────────────────────────

/// A spawned `tangram-host` process that is killed and waited on drop, so a
/// failing test never leaks a server.
pub struct HostProc(pub Child);

impl Drop for HostProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ─── wait_for ─────────────────────────────────────────────────────────────────

/// Poll `check` every 100 ms until it returns `true` or `timeout` elapses.
/// Panics with a descriptive message on timeout.
pub async fn wait_for<F, Fut>(what: &str, timeout: Duration, mut check: F)
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

// ─── status_of ────────────────────────────────────────────────────────────────

/// Issue an unauthenticated GET and return the HTTP status, or `None` if the
/// request itself failed (e.g. connection refused while the host is starting).
pub async fn status_of(client: &reqwest::Client, url: &str) -> Option<reqwest::StatusCode> {
    client.get(url).send().await.ok().map(|r| r.status())
}

// ─── free_port ────────────────────────────────────────────────────────────────

/// Bind an ephemeral TCP port on 127.0.0.1:0, read back the OS-assigned port
/// number, and return it.  The listener is dropped immediately so the port is
/// available to the spawned host.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}
