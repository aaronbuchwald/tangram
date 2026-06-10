//! tangram-host — run Tangram apps as WASM components (RUNTIME_PLAN Phase 2,
//! ADR-0001 Track W).
//!
//! One native binary owns the platform: HTTP serving, the sync protocol,
//! MCP, persistence, and static UI files. Each app is a `wasm32-wasip2`
//! component holding ONLY app logic, kept alive as one instance and called
//! per action; its capabilities are exactly what the host implements for it
//! (outbound HTTP behind a per-app allowlist, log, wall clock) plus the env
//! vars granted in `apps.toml`. The host is also the only thing touching
//! `$HOME/.<app-name>` — the component cannot name a file at all.
//!
//! Usage: `tangram-host [apps.toml]` (or `APPS_TOML=...`); `BIND_ADDR`
//! defaults to 127.0.0.1:8080. The config file is watched: edits converge
//! live — apps appear, disappear, and reload (also when a component file is
//! rebuilt) without restarting the host.

mod app;
mod config;
mod doc;
mod mcp;
mod routes;
mod runtime;

use std::collections::HashMap;
use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use notify::Watcher as _;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use crate::app::{AppRuntime, component_mtime};
use crate::config::HostConfig;
use crate::routes::AppEntry;

pub struct Host {
    engine: wasmtime::Engine,
    config_path: PathBuf,
    pub apps: RwLock<HashMap<String, AppEntry>>,
}

impl Host {
    /// One reconciliation pass: re-read `apps.toml` and converge the running
    /// set — build new/changed apps (spec changed, or the component file was
    /// rebuilt), drop removed ones. A failing app is logged and skipped; a
    /// failing RELOAD keeps the old instance serving.
    async fn converge(&self) {
        let config = match HostConfig::load(&self.config_path) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!("not converging: {e:#}");
                return;
            }
        };

        let mut apps = self.apps.write().await;
        for (name, spec) in &config.apps {
            let up_to_date = apps.get(name).is_some_and(|entry| {
                entry.runtime.spec == *spec
                    && entry.runtime.component_mtime == component_mtime(&spec.component)
            });
            if up_to_date {
                continue;
            }
            let started = std::time::Instant::now();
            match AppRuntime::build(&self.engine, name, spec).await {
                Ok(runtime) => {
                    let verb = if apps.contains_key(name) {
                        "reloaded"
                    } else {
                        "added"
                    };
                    apps.insert(name.clone(), AppEntry::new(runtime));
                    tracing::info!(
                        "{name}: {verb} (serving /{name}/ after {:?})",
                        started.elapsed()
                    );
                }
                Err(e) if apps.contains_key(name) => {
                    tracing::error!("{name}: reload failed, keeping old instance: {e:#}");
                }
                Err(e) => tracing::error!("{name}: failed to start: {e:#}"),
            }
        }
        let before = apps.len();
        apps.retain(|name, _| {
            let keep = config.apps.contains_key(name);
            if !keep {
                tracing::info!("{name}: removed (routes for /{name}/ are gone)");
            }
            keep
        });
        let _ = before;
    }
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

    let host = Arc::new(Host {
        engine: runtime::engine()?,
        config_path: config_path.clone(),
        apps: RwLock::new(HashMap::new()),
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
                _ = tick.tick() => {}
            }
            converge_host.converge().await;
        }
    });

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let router = routes::root_router(host.clone());
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    tracing::info!(
        "tangram-host — index http://{bind_addr}/ (apps from {})",
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
