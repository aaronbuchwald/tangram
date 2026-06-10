//! App assembly: one builder that wires the store, web surface, MCP surface,
//! and sync peers together and serves them on a single port.

use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::http::{HeaderValue, header};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::action::Actions;
use crate::store::Store;
use crate::{Model, mcp, sync, web};

/// Builder + runtime for a Tangram app.
///
/// Environment (all optional, `.env` is loaded if present):
/// - `BIND_ADDR` — listen address (default `127.0.0.1:8080`)
/// - `TANGRAM_REMOTE` — `ws://host:port/sync` of a peer instance to replicate with
///   (single-app mode only; see [`App::remote`] for multi-app hosts)
/// - `TANGRAM_REMOTE_<NAME>` — per-app remote, e.g. `TANGRAM_REMOTE_NOTES`
/// - `TANGRAM_DATA_DIR` — where the document lives (default `./data`)
/// - `FRAME_ANCESTORS` — CSP frame-ancestors for iframe embedding (default `*`)
/// - `RUST_LOG` — log filter (default `info`)
pub struct App<M> {
    name: String,
    ui_dir: PathBuf,
    instructions: Option<String>,
    remote: Option<String>,
    _marker: PhantomData<fn() -> M>,
}

impl<M: Model + Actions> App<M> {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ui_dir: PathBuf::from("ui"),
            instructions: None,
            remote: None,
            _marker: PhantomData,
        }
    }

    /// Directory of static UI files (default `ui/`).
    pub fn ui_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.ui_dir = dir.into();
        self
    }

    /// Instructions handed to MCP clients connecting to this app.
    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = Some(text.into());
        self
    }

    /// `ws://host:port/sync` of a peer instance to replicate with. Takes
    /// precedence over `TANGRAM_REMOTE_<NAME>` (and, in single-app mode,
    /// `TANGRAM_REMOTE`).
    pub fn remote(mut self, url: impl Into<String>) -> Self {
        self.remote = Some(url.into());
        self
    }

    /// Open the store, start the remote sync client if one is configured, and
    /// return the fully assembled router for this app (JSON API + SSE + sync
    /// WebSocket + static UI + `/mcp`). No binding or env-loading side
    /// effects, so a host can [`nest`](axum::Router::nest) several apps'
    /// routers under different path prefixes on one server.
    ///
    /// The sync remote resolves to the first of: an explicit
    /// [`remote`](App::remote), or `TANGRAM_REMOTE_<NAME>` (name uppercased,
    /// `-` → `_`). `TANGRAM_REMOTE` is only consulted by [`serve`](App::serve),
    /// because a host with several apps cannot share one remote URL.
    pub fn build(self) -> anyhow::Result<axum::Router> {
        let data_dir =
            PathBuf::from(std::env::var("TANGRAM_DATA_DIR").unwrap_or_else(|_| "data".into()));
        let frame_ancestors = std::env::var("FRAME_ANCESTORS").unwrap_or_else(|_| "*".into());
        let csp = HeaderValue::from_str(&format!("frame-ancestors {frame_ancestors}"))
            .context("FRAME_ANCESTORS contains characters not valid in a header value")?;

        let store = Arc::new(Store::<M>::open(
            data_dir.join(format!("{}.automerge", self.name)),
        )?);

        // Replicate with a remote peer if one is configured; local-first
        // means everything below works identically without it.
        if let Some(remote) = self.remote.clone().or_else(|| self.env_remote()) {
            tokio::spawn(sync::run_remote(remote, store.clone()));
        }

        let mcp_service = StreamableHttpService::new(
            {
                let bridge = mcp::McpBridge::new(store.clone(), self.instructions.clone());
                move || Ok(bridge.clone())
            },
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        );

        Ok(web::router(store, self.ui_dir.clone())
            .nest_service("/mcp", mcp_service)
            .layer(SetResponseHeaderLayer::overriding(
                header::CONTENT_SECURITY_POLICY,
                csp,
            ))
            .layer(TraceLayer::new_for_http()))
    }

    /// The per-app remote from the environment: `TANGRAM_REMOTE_<NAME>`.
    fn env_remote(&self) -> Option<String> {
        let suffix = self.name.to_uppercase().replace('-', "_");
        std::env::var(format!("TANGRAM_REMOTE_{suffix}")).ok()
    }

    pub async fn serve(mut self) -> anyhow::Result<()> {
        let _ = dotenvy::dotenv();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .try_init();

        // Single-app mode: the plain TANGRAM_REMOTE applies (lowest priority).
        if self.remote.is_none() && self.env_remote().is_none() {
            self.remote = std::env::var("TANGRAM_REMOTE").ok();
        }

        let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
        let name = self.name.clone();
        let app = self.build()?;

        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .with_context(|| format!("failed to bind {bind_addr}"))?;
        tracing::info!("{name} — web UI http://{bind_addr}/");
        tracing::info!("{name} — mcp    http://{bind_addr}/mcp");
        tracing::info!("{name} — sync   ws://{bind_addr}/sync");

        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    }
}
