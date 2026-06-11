//! tangram-host — run Tangram apps as WASM components (RUNTIME_PLAN Phase 2
//! + 3, ADR-0001 Track W).
//!
//! One native binary owns the platform: HTTP serving, the sync protocol,
//! MCP, persistence, and static UI files. Each app is a `wasm32-wasip2`
//! component holding ONLY app logic, kept alive as one instance and called
//! per action; its capabilities are exactly what the host implements for it
//! (outbound HTTP behind a per-app allowlist, log, wall clock) plus the env
//! vars granted in `apps.toml`. The host is also the only thing touching
//! `$HOME/.<app-name>` — the component cannot name a file at all.
//!
//! Desired state has two layers (Phase 3): `apps.toml` (bootstrap, watched
//! for edits) plus the replicated document of any app flagged
//! `registry = true` — install/remove/enable through the registry's actions
//! or MCP tools and the host converges exactly like a file change, with
//! registry entries winning name collisions. `TANGRAM_AUTH_TOKEN` gates the
//! mutating routes of registry (and `require_auth`) apps; without a token
//! the host refuses to run a registry app on a non-loopback bind.
//!
//! Usage: `tangram-host [apps.toml]` (or `APPS_TOML=...`); `BIND_ADDR`
//! defaults to 127.0.0.1:8080. The config file is watched: edits converge
//! live — apps appear, disappear, and reload (also when a component file is
//! rebuilt) without restarting the host.

mod app;
mod auth;
mod config;
mod doc;
mod mcp;
mod registry;
mod routes;
mod runtime;

use std::collections::{BTreeMap, HashMap};
use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use notify::Watcher as _;
use tangram::sync::DocHandle as _;
use tokio::sync::{Notify, RwLock};
use tracing_subscriber::EnvFilter;

use crate::app::{AppRuntime, component_mtime};
use crate::config::{AppSpec, HostConfig};
use crate::registry::Source;
use crate::routes::AppEntry;

/// Fleet status of one desired app, refreshed by every converge pass and
/// served (with live running/healthy probes) at `GET /api/fleet`.
#[derive(Debug, Clone)]
pub struct FleetStatus {
    pub source: Source,
    pub registry: bool,
    pub require_auth: bool,
    pub enabled: bool,
    /// Why the last converge could not (re)start this app, if it failed.
    pub error: Option<String>,
}

pub struct Host {
    engine: wasmtime::Engine,
    config_path: PathBuf,
    pub apps: RwLock<HashMap<String, AppEntry>>,
    pub fleet: RwLock<BTreeMap<String, FleetStatus>>,
    /// `TANGRAM_AUTH_TOKEN`: gates mutating routes on registry/require_auth
    /// apps. `None` = unauthenticated host (loopback-only for registries).
    pub auth_token: Option<String>,
    /// Whether `BIND_ADDR` is a loopback address — a registry app without a
    /// token refuses to run on a non-loopback bind.
    pub bind_loopback: bool,
    /// Nudges the converge loop (registry document changes use this channel;
    /// file edits arrive via the notify watcher).
    pub nudge: Arc<Notify>,
}

impl Host {
    /// One reconciliation pass, in two stages: (1) converge the registry
    /// apps named in `apps.toml`, (2) read their replicated spec lists,
    /// merge them over the file config (registry wins name collisions), and
    /// converge the full set — build new/changed apps (spec changed, or the
    /// component file was rebuilt), drop removed/disabled ones. A failing
    /// app is logged, reported in the fleet status, and skipped; a failing
    /// RELOAD keeps the old instance serving.
    async fn converge(&self) {
        let config = match HostConfig::load(&self.config_path) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!("not converging: {e:#}");
                return;
            }
        };

        let mut apps = self.apps.write().await;
        let mut errors: BTreeMap<String, String> = BTreeMap::new();

        // Stage 1: registry apps from the file, so their documents can be
        // read below. (They are re-checked as part of stage 2's full set —
        // `ensure_app` is idempotent for an up-to-date app.)
        for (name, spec) in config.apps.iter().filter(|(_, s)| s.registry && s.enabled) {
            if let Err(e) = self.ensure_app(&mut apps, name, spec).await {
                errors.insert(name.clone(), e);
            }
        }

        // The registry layer of desired state: every running registry app's
        // replicated spec list.
        let mut registry_entries = Vec::new();
        for (name, spec) in &config.apps {
            if !spec.registry {
                continue;
            }
            if let Some(entry) = apps.get(name) {
                // state_json is verbatim component output (a String since the
                // float-rendering fix); the registry layer needs the parsed
                // tree to walk specs — exact now that float_roundtrip is on.
                match serde_json::from_str(&entry.runtime.state_json().await) {
                    Ok(state) => registry_entries.extend(registry::parse_state(name, &state)),
                    Err(e) => tracing::warn!("{name}: unparseable registry state: {e}"),
                }
            }
        }

        // Stage 2: converge the merged desired state.
        let desired = registry::merge(&config.apps, registry_entries);
        for (name, want) in &desired {
            if !want.spec.enabled {
                continue;
            }
            if let Err(e) = self.ensure_app(&mut apps, name, &want.spec).await {
                errors.insert(name.clone(), e);
            }
        }
        apps.retain(|name, _| {
            let keep = desired.get(name).is_some_and(|want| want.spec.enabled);
            if !keep {
                tracing::info!("{name}: removed (routes for /{name}/ are gone)");
            }
            keep
        });

        *self.fleet.write().await = desired
            .into_iter()
            .map(|(name, want)| {
                let error = errors.remove(&name);
                let status = FleetStatus {
                    source: want.source,
                    registry: want.spec.registry,
                    require_auth: want.spec.require_auth,
                    enabled: want.spec.enabled,
                    error,
                };
                (name, status)
            })
            .collect();
    }

    /// Converge one app toward its spec: no-op when up to date, otherwise
    /// (re)instantiate. Returns the failure message for the fleet status.
    async fn ensure_app(
        &self,
        apps: &mut HashMap<String, AppEntry>,
        name: &str,
        spec: &AppSpec,
    ) -> Result<(), String> {
        if spec.registry && self.auth_token.is_none() && !self.bind_loopback {
            let msg = "refusing to run a registry app on a non-loopback bind without \
                       TANGRAM_AUTH_TOKEN (set the token or bind 127.0.0.1)"
                .to_string();
            tracing::error!("{name}: {msg}");
            apps.remove(name);
            return Err(msg);
        }
        let up_to_date = apps.get(name).is_some_and(|entry| {
            entry.runtime.spec == *spec
                && entry.runtime.component_mtime == component_mtime(&spec.component)
        });
        if up_to_date {
            return Ok(());
        }
        let started = std::time::Instant::now();
        match AppRuntime::build(&self.engine, name, spec).await {
            Ok(runtime) => {
                let gate_token = self
                    .auth_token
                    .as_deref()
                    .filter(|_| spec.registry || spec.require_auth);
                let mut entry = AppEntry::new(runtime, gate_token);
                if spec.registry {
                    // Registry doc changes (actions, MCP calls, sync from a
                    // replica) re-trigger converge, just like a file edit.
                    let mut rx = entry.runtime.doc.subscribe();
                    let nudge = self.nudge.clone();
                    entry.watch_task = Some(tokio::spawn(async move {
                        while rx.changed().await.is_ok() {
                            nudge.notify_one();
                        }
                    }));
                }
                let verb = if apps.contains_key(name) {
                    "reloaded"
                } else {
                    "added"
                };
                apps.insert(name.to_string(), entry);
                tracing::info!(
                    "{name}: {verb} (serving /{name}/ after {:?})",
                    started.elapsed()
                );
                Ok(())
            }
            Err(e) if apps.contains_key(name) => {
                tracing::error!("{name}: reload failed, keeping old instance: {e:#}");
                Err(format!("reload failed (old instance still serving): {e:#}"))
            }
            Err(e) => {
                tracing::error!("{name}: failed to start: {e:#}");
                Err(format!("failed to start: {e:#}"))
            }
        }
    }
}

/// Is this bind address loopback? Unparseable hosts count as non-loopback
/// (fail safe — the check guards running an unauthenticated registry).
fn bind_is_loopback(bind_addr: &str) -> bool {
    if let Ok(addr) = bind_addr.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }
    matches!(
        bind_addr.rsplit_once(':').map(|(host, _)| host),
        Some("localhost")
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let config_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("APPS_TOML").ok())
        .unwrap_or_else(|| "apps.toml".into());
    let config_path = std::fs::canonicalize(&config_path)
        .with_context(|| format!("apps config {config_path} not found"))?;

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let bind_loopback = bind_is_loopback(&bind_addr);
    let auth_token = std::env::var("TANGRAM_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    if auth_token.is_none() {
        tracing::warn!(
            "TANGRAM_AUTH_TOKEN is not set — mutating routes are UNAUTHENTICATED; \
             registry apps are limited to loopback binds (set the token in .env before \
             exposing the host)"
        );
        if !bind_loopback
            && HostConfig::load(&config_path).is_ok_and(|config| config.has_registry())
        {
            anyhow::bail!(
                "refusing to bind {bind_addr}: apps.toml contains a registry app and \
                 TANGRAM_AUTH_TOKEN is not set — set the token or bind 127.0.0.1"
            );
        }
    }

    let host = Arc::new(Host {
        engine: runtime::engine()?,
        config_path: config_path.clone(),
        apps: RwLock::new(HashMap::new()),
        fleet: RwLock::new(BTreeMap::new()),
        auth_token,
        bind_loopback,
        nudge: Arc::new(Notify::new()),
    });
    host.converge().await;

    // Watch the config file's DIRECTORY (editors replace the file, which
    // would orphan a file watch) and nudge the converge loop on any event
    // there; a slow tick also picks up component rebuilds (mtime changes).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = tx.send(());
        }
    })?;
    let watch_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    watcher.watch(&watch_dir, notify::RecursiveMode::NonRecursive)?;

    let converge_host = host.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = rx.recv() => {
                    if event.is_none() {
                        return; // watcher gone
                    }
                    // Debounce editor write bursts, then drain.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    while rx.try_recv().is_ok() {}
                }
                // A registry document changed (install_app & co.) —
                // converge immediately, no debounce.
                _ = converge_host.nudge.notified() => {}
                _ = tick.tick() => {}
            }
            converge_host.converge().await;
        }
    });

    let router = routes::root_router(host.clone());
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    tracing::info!(
        "tangram-host — index http://{bind_addr}/ (apps from {}, fleet status /api/fleet)",
        config_path.display()
    );
    for name in host.apps.read().await.keys() {
        tracing::info!("{name} — web UI http://{bind_addr}/{name}/");
        tracing::info!("{name} — mcp    http://{bind_addr}/{name}/mcp");
        tracing::info!("{name} — sync   http://{bind_addr}/{name}/sync");
    }

    // Race the server against Ctrl-C instead of graceful shutdown: SSE
    // state/poke streams and MCP sessions never close on their own, and
    // persistence is synchronous on every change — same pattern as
    // App::serve.
    tokio::select! {
        result = axum::serve(listener, router).into_future() => result?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("tangram-host — shutting down");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bind_is_loopback;

    #[test]
    fn loopback_detection() {
        assert!(bind_is_loopback("127.0.0.1:8080"));
        assert!(bind_is_loopback("[::1]:8080"));
        assert!(bind_is_loopback("localhost:8080"));
        assert!(!bind_is_loopback("0.0.0.0:8080"));
        assert!(!bind_is_loopback("[::]:8080"));
        assert!(!bind_is_loopback("192.168.1.4:8080"));
        assert!(!bind_is_loopback("example.com:8080"));
        assert!(!bind_is_loopback("garbage"));
    }
}
