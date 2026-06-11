//! The MCP plane through agentgateway (RUNTIME_PLAN D3: "agentgateway scope:
//! MCP plane only").
//!
//! With `[gateway] enabled = true` in `apps.toml` and an `agentgateway`
//! binary available, the host runs agentgateway as a SUPERVISED CHILD
//! process on an internal port and routes all public MCP traffic through it:
//!
//! ```text
//!   client ──:8080──▶ tangram-host (the ONE public listener)
//!     /<app>/mcp ───▶ proxy ──▶ agentgateway :<gw> ──▶ host internal :<int>/<app>/mcp
//!     /mcp (aggregate, every app's tools namespaced <app>_<tool>) ──▶ same
//! ```
//!
//! The host's per-app rmcp services keep serving — on an internal
//! loopback-only listener that agentgateway targets — so everything the
//! direct path enforces (the bearer gate on mutating registry tools, the
//! SDK's error envelope) still holds: agentgateway forwards the client's
//! `Authorization` header to the target, and rewrites namespaced aggregate
//! tool names (`registry_install_app`) back to the app's real tool name
//! before the internal endpoint sees them.
//!
//! agentgateway's config is GENERATED from the merged desired state on every
//! converge (never hand-edited) and written atomically; agentgateway watches
//! the file and hot-reloads, so registry installs show up on the aggregate
//! endpoint without restarting anything. agentgateway v1.2 binds its data
//! plane on the wildcard address, so every generated route carries a
//! loopback-only `source.address` authorization policy — the gateway is
//! unreachable from off the box even though the socket is wildcard.
//!
//! If the binary is missing the host logs a clear warning and falls back to
//! today's direct per-app `/mcp` serving — the gateway is an enhancement,
//! never a hard dependency.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::watch;

/// `[gateway]` in `apps.toml`. Read once at startup (changing it needs a
/// host restart — unlike app specs, which converge live).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewaySettings {
    /// Route MCP through a host-managed agentgateway child process.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the agentgateway binary. Default: `agentgateway` on $PATH.
    #[serde(default)]
    pub binary: Option<PathBuf>,
    /// Internal loopback port for the gateway's MCP listener. Default: a
    /// free port chosen at startup.
    #[serde(default)]
    pub port: Option<u16>,
}

impl GatewaySettings {
    /// The binary to run: the configured path (if it exists), else
    /// `agentgateway` found on $PATH. `None` triggers the direct-serving
    /// fallback.
    pub fn resolve_binary(&self) -> Option<PathBuf> {
        match &self.binary {
            Some(path) => is_executable(path).then(|| path.clone()),
            None => find_on_path("agentgateway"),
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file() && std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

// ── config generation ────────────────────────────────────────────────────────

/// agentgateway 1.2 always binds its data plane on the wildcard address, so
/// every generated route enforces loopback-only sources — the host (and
/// local processes, which could reach the internal endpoints anyway) stay,
/// the network is shut out, and the public surface remains the host's one
/// port.
const LOOPBACK_RULE: &str =
    r#"string(source.address).startsWith("127.") || string(source.address) == "::1""#;

/// agentgateway target names may not contain underscores (they become the
/// `<target>_<tool>` namespace separator on the aggregate endpoint).
fn target_name(app: &str) -> String {
    app.replace('_', "-")
}

fn loopback_policy() -> Value {
    json!({ "authorization": { "rules": [LOOPBACK_RULE] } })
}

fn mcp_target(app: &str, internal_port: u16) -> Value {
    json!({
        "name": target_name(app),
        "mcp": { "host": format!("http://127.0.0.1:{internal_port}/{app}/mcp") }
    })
}

/// Render the full agentgateway config for the given RUNNING apps: one
/// per-app route (`/<app>/mcp` → that app's internal MCP endpoint, tool
/// names unchanged) plus the aggregate `/mcp` route multiplexing every app
/// as a namespaced target. Deterministic for a given input (apps are
/// sorted), so converge can diff bytes to decide whether to rewrite.
pub fn render_config(apps: &[String], gateway_port: u16, internal_port: u16) -> Value {
    let mut apps: Vec<&String> = apps.iter().collect();
    apps.sort();
    apps.dedup();

    let mut routes = Vec::with_capacity(apps.len() + 1);
    for app in &apps {
        routes.push(json!({
            "name": format!("{app}-mcp"),
            "policies": loopback_policy(),
            "matches": [{ "path": { "pathPrefix": format!("/{app}/mcp") } }],
            "backends": [{ "mcp": { "targets": [mcp_target(app, internal_port)] } }]
        }));
    }
    // Aggregate last (order is irrelevant for matching — "/mcp" and
    // "/<app>/mcp" prefixes are disjoint — but humans read this file).
    let mut seen = std::collections::BTreeSet::new();
    let aggregate_targets: Vec<Value> = apps
        .iter()
        .filter(|app| {
            // `a_b` and `a-b` both map to target `a-b`; keep the first and
            // warn rather than emit a config agentgateway rejects.
            let kept = seen.insert(target_name(app));
            if !kept {
                tracing::warn!(
                    "{app}: target name {:?} collides on the aggregate /mcp endpoint — skipped \
                     (rename the app)",
                    target_name(app)
                );
            }
            kept
        })
        .map(|app| mcp_target(app, internal_port))
        .collect();
    routes.push(json!({
        "name": "mcp-aggregate",
        "policies": loopback_policy(),
        "matches": [{ "path": { "pathPrefix": "/mcp" } }],
        "backends": [{ "mcp": { "targets": aggregate_targets } }]
    }));

    json!({
        // The admin/stats/readiness listeners are pinned to ephemeral
        // loopback ports — never a public surface, never a port conflict.
        "config": {
            "adminAddr": "127.0.0.1:0",
            "statsAddr": "127.0.0.1:0",
            "readinessAddr": "127.0.0.1:0"
        },
        "binds": [{
            "port": gateway_port,
            "listeners": [{ "routes": routes }]
        }]
    })
}

// ── restart backoff (pure state machine, unit-tested) ────────────────────────

/// Restart backoff for the supervised child: doubles on quick crashes
/// (500 ms → 10 s cap), resets once a run survives 30 s.
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    const MIN: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(10);
    /// A run at least this long counts as healthy and resets the backoff.
    const STABLE: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        Self { next: Self::MIN }
    }

    /// The delay before the next restart, given how long the child ran.
    pub fn after_exit(&mut self, uptime: Duration) -> Duration {
        if uptime >= Self::STABLE {
            self.next = Self::MIN;
        }
        let delay = self.next;
        self.next = (delay * 2).min(Self::MAX);
        delay
    }
}

// ── the running gateway ──────────────────────────────────────────────────────

/// A host-managed agentgateway instance: generated config + supervised child
/// process + the reverse proxy the public listener uses for MCP paths.
pub struct Gateway {
    binary: PathBuf,
    /// The gateway's internal listen port (loopback use only; see
    /// [`LOOPBACK_RULE`]).
    pub port: u16,
    /// The host's internal listener that agentgateway targets per app.
    pub internal_port: u16,
    config_path: PathBuf,
    client: reqwest::Client,
    running: AtomicBool,
    child_pid: AtomicU32,
    shutdown: watch::Sender<bool>,
}

impl Gateway {
    pub fn new(binary: PathBuf, port: u16, internal_port: u16, config_path: PathBuf) -> Self {
        Self {
            binary,
            port,
            internal_port,
            config_path,
            client: reqwest::Client::new(),
            running: AtomicBool::new(false),
            child_pid: AtomicU32::new(0),
            shutdown: watch::Sender::new(false),
        }
    }

    /// Is the child process currently alive?
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// The live child's PID (0 = not running).
    pub fn child_pid(&self) -> u32 {
        self.child_pid.load(Ordering::Relaxed)
    }

    /// Regenerate the config for the given running apps; written atomically
    /// (tmp + rename) and only when the bytes changed — agentgateway watches
    /// the file and hot-reloads. Called by every converge pass.
    pub fn sync_apps(&self, apps: &[String]) {
        let rendered = render_config(apps, self.port, self.internal_port);
        let bytes = serde_json::to_vec_pretty(&rendered).expect("static json renders");
        match write_if_changed(&self.config_path, &bytes) {
            Ok(true) => tracing::info!(
                "gateway: config regenerated ({} apps) — agentgateway hot-reloads {}",
                apps.len(),
                self.config_path.display()
            ),
            Ok(false) => {}
            Err(e) => tracing::error!("gateway: writing config failed: {e:#}"),
        }
    }

    /// Supervise the child: spawn, forward its output to tracing, restart on
    /// crash with [`Backoff`], kill on host shutdown.
    pub fn spawn_supervisor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let gw = self.clone();
        tokio::spawn(async move {
            let mut shutdown = gw.shutdown.subscribe();
            let mut backoff = Backoff::new();
            loop {
                if *shutdown.borrow() {
                    return;
                }
                let mut child = match tokio::process::Command::new(&gw.binary)
                    .arg("-f")
                    .arg(&gw.config_path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(child) => child,
                    Err(e) => {
                        tracing::error!("gateway: failed to spawn {}: {e}", gw.binary.display());
                        tokio::select! {
                            _ = tokio::time::sleep(backoff.after_exit(Duration::ZERO)) => continue,
                            _ = shutdown.changed() => return,
                        }
                    }
                };
                let pid = child.id().unwrap_or(0);
                gw.child_pid.store(pid, Ordering::Relaxed);
                gw.running.store(true, Ordering::Relaxed);
                tracing::info!(
                    "gateway: agentgateway running (pid {pid}, mcp on 127.0.0.1:{})",
                    gw.port
                );
                forward_output(&mut child);

                let started = std::time::Instant::now();
                tokio::select! {
                    status = child.wait() => {
                        gw.running.store(false, Ordering::Relaxed);
                        gw.child_pid.store(0, Ordering::Relaxed);
                        let delay = backoff.after_exit(started.elapsed());
                        tracing::warn!(
                            "gateway: agentgateway exited ({status:?}) — restarting in {delay:?}"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = shutdown.changed() => return,
                        }
                    }
                    _ = shutdown.changed() => {
                        gw.running.store(false, Ordering::Relaxed);
                        gw.child_pid.store(0, Ordering::Relaxed);
                        let _ = child.kill().await;
                        tracing::info!("gateway: agentgateway stopped");
                        return;
                    }
                }
            }
        })
    }

    /// Stop the supervisor and kill the child (used on Ctrl-C; the child is
    /// also `kill_on_drop` as a backstop).
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Reverse-proxy one MCP request to the gateway, streaming both ways —
    /// SSE responses (and the `Mcp-Session-Id` header) pass through intact.
    pub async fn proxy(&self, req: Request) -> Response {
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| req.uri().path().to_string());
        let url = format!("http://127.0.0.1:{}{path_and_query}", self.port);
        let (parts, body) = req.into_parts();

        let mut upstream = self.client.request(parts.method, &url);
        for (name, value) in &parts.headers {
            if !skip_request_header(name) {
                upstream = upstream.header(name, value);
            }
        }
        let upstream = upstream.body(reqwest::Body::wrap_stream(body.into_data_stream()));

        match upstream.send().await {
            Ok(resp) => {
                let mut out = Response::builder().status(resp.status().as_u16());
                if let Some(headers) = out.headers_mut() {
                    for (name, value) in resp.headers() {
                        if !skip_response_header(name) {
                            headers.insert(name.clone(), value.clone());
                        }
                    }
                }
                out.body(Body::from_stream(resp.bytes_stream()))
                    .unwrap_or_else(|e| {
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("proxy: {e}")).into_response()
                    })
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                format!("mcp gateway unreachable (restarting?): {e}"),
            )
                .into_response(),
        }
    }
}

/// Forward the child's stdout/stderr lines into tracing under the
/// `agentgateway` target (its access log is the routing-hop evidence).
fn forward_output(child: &mut tokio::process::Child) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "agentgateway", "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "agentgateway", "{line}");
            }
        });
    }
}

/// Hop-by-hop request headers (plus Host, which reqwest derives from the
/// URL) that must not be forwarded.
fn skip_request_header(name: &HeaderName) -> bool {
    name == header::HOST
        || name == header::CONNECTION
        || name == header::TE
        || name == header::TRAILER
        || name == header::TRANSFER_ENCODING
        || name == header::UPGRADE
}

fn skip_response_header(name: &HeaderName) -> bool {
    name == header::CONNECTION || name == header::TRANSFER_ENCODING
}

/// Atomic write (tmp + rename), skipped when the content is unchanged so
/// agentgateway only reloads on real config changes. Returns whether the
/// file was (re)written.
fn write_if_changed(path: &Path, bytes: &[u8]) -> anyhow::Result<bool> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(false);
    }
    let dir = path.parent().context("config path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Find a free loopback port (bind-and-drop; the tiny race is acceptable for
/// a port the host hands to its own child immediately).
pub fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// Where the generated config lives: the host's own data root,
/// `$HOME/.tangram-host` (`./data/tangram-host` without a HOME) — never
/// hand-edited, regenerated on every converge.
pub fn default_config_file() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".tangram-host/agentgateway.json"),
        Err(_) => PathBuf::from("data/tangram-host/agentgateway.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_and_default_off() {
        let parsed: GatewaySettings = toml::from_str("enabled = true").unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.binary, None);
        assert_eq!(parsed.port, None);
        let parsed: GatewaySettings =
            toml::from_str("enabled = true\nbinary = \"/x/agentgateway\"\nport = 19200").unwrap();
        assert_eq!(parsed.binary, Some(PathBuf::from("/x/agentgateway")));
        assert_eq!(parsed.port, Some(19200));
        assert!(!GatewaySettings::default().enabled, "default is off");
    }

    #[test]
    fn renders_per_app_and_aggregate_routes() {
        let apps = vec![
            "nutrition".to_string(),
            "notes".to_string(),
            "registry".to_string(),
        ];
        let config = render_config(&apps, 19200, 19300);

        // One public-ish surface knob: the bind port; admin planes pinned to
        // ephemeral loopback.
        assert_eq!(config["binds"][0]["port"], 19200);
        assert_eq!(config["config"]["adminAddr"], "127.0.0.1:0");
        assert_eq!(config["config"]["statsAddr"], "127.0.0.1:0");
        assert_eq!(config["config"]["readinessAddr"], "127.0.0.1:0");

        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .expect("routes");
        assert_eq!(routes.len(), 4, "3 per-app + 1 aggregate");

        // Sorted per-app routes, each a single target at the internal port.
        for (i, app) in ["notes", "nutrition", "registry"].iter().enumerate() {
            let route = &routes[i];
            assert_eq!(route["name"], format!("{app}-mcp"));
            assert_eq!(
                route["matches"][0]["path"]["pathPrefix"],
                format!("/{app}/mcp")
            );
            let targets = route["backends"][0]["mcp"]["targets"]
                .as_array()
                .expect("targets");
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0]["name"], *app);
            assert_eq!(
                targets[0]["mcp"]["host"],
                format!("http://127.0.0.1:19300/{app}/mcp")
            );
        }

        // Aggregate multiplexes every app.
        let aggregate = &routes[3];
        assert_eq!(aggregate["name"], "mcp-aggregate");
        assert_eq!(aggregate["matches"][0]["path"]["pathPrefix"], "/mcp");
        let targets = aggregate["backends"][0]["mcp"]["targets"]
            .as_array()
            .expect("aggregate targets");
        let names: Vec<&str> = targets.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["notes", "nutrition", "registry"]);

        // Every route is loopback-gated (the bind is a wildcard socket).
        for route in routes {
            let rules = route["policies"]["authorization"]["rules"]
                .as_array()
                .expect("authz rules");
            assert_eq!(rules.len(), 1);
            assert!(rules[0].as_str().unwrap().contains("source.address"));
        }
    }

    #[test]
    fn render_sanitizes_underscores_and_dedupes_collisions() {
        let apps = vec!["my_app".to_string(), "my-app".to_string()];
        let config = render_config(&apps, 1, 2);
        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .unwrap();
        // Both per-app routes exist (paths keep the real app name)…
        assert_eq!(routes[0]["matches"][0]["path"]["pathPrefix"], "/my-app/mcp");
        assert_eq!(routes[1]["matches"][0]["path"]["pathPrefix"], "/my_app/mcp");
        // …and the per-app target for my_app is sanitized (no underscores —
        // agentgateway rejects them in target names).
        assert_eq!(
            routes[1]["backends"][0]["mcp"]["targets"][0]["name"],
            "my-app"
        );
        // The aggregate keeps only the first of the colliding pair.
        let aggregate = routes.last().unwrap();
        let targets = aggregate["backends"][0]["mcp"]["targets"]
            .as_array()
            .unwrap();
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn render_with_no_apps_keeps_the_aggregate_bind() {
        let config = render_config(&[], 19200, 19300);
        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["name"], "mcp-aggregate");
        assert_eq!(
            routes[0]["backends"][0]["mcp"]["targets"],
            serde_json::json!([])
        );
    }

    #[test]
    fn render_is_deterministic_for_unsorted_input() {
        let a = render_config(&["b".into(), "a".into()], 1, 2);
        let b = render_config(&["a".into(), "b".into(), "a".into()], 1, 2);
        assert_eq!(a, b);
    }

    #[test]
    fn backoff_doubles_on_crash_loop_and_resets_after_stable_run() {
        let mut backoff = Backoff::new();
        let crash = Duration::from_millis(10);
        assert_eq!(backoff.after_exit(crash), Duration::from_millis(500));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(1));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(2));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(4));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(8));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(10), "capped");
        assert_eq!(
            backoff.after_exit(crash),
            Duration::from_secs(10),
            "stays capped"
        );
        // A stable run resets the ladder.
        assert_eq!(
            backoff.after_exit(Duration::from_secs(31)),
            Duration::from_millis(500)
        );
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(1));
    }

    #[test]
    fn write_if_changed_diffs_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.json");
        assert!(write_if_changed(&path, b"v1").unwrap(), "first write");
        assert!(
            !write_if_changed(&path, b"v1").unwrap(),
            "unchanged → no-op"
        );
        assert!(
            write_if_changed(&path, b"v2").unwrap(),
            "changed → rewritten"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"v2");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "tmp file renamed away"
        );
    }
}
