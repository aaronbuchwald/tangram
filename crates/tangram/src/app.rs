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
/// - `TANGRAM_DATA_DIR` — where the document lives (default `./data`)
/// - `FRAME_ANCESTORS` — CSP frame-ancestors for iframe embedding (default `*`)
/// - `RUST_LOG` — log filter (default `info`)
pub struct App<M> {
    name: String,
    ui_dir: PathBuf,
    instructions: Option<String>,
    _marker: PhantomData<fn() -> M>,
}

impl<M: Model + Actions> App<M> {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ui_dir: PathBuf::from("ui"),
            instructions: None,
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

    pub async fn serve(self) -> anyhow::Result<()> {
        let _ = dotenvy::dotenv();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .try_init();

        let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
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
        if let Ok(remote) = std::env::var("TANGRAM_REMOTE") {
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

        let app = web::router(store, self.ui_dir.clone())
            .nest_service("/mcp", mcp_service)
            .layer(SetResponseHeaderLayer::overriding(
                header::CONTENT_SECURITY_POLICY,
                csp,
            ))
            .layer(TraceLayer::new_for_http());

        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .with_context(|| format!("failed to bind {bind_addr}"))?;
        tracing::info!("{} — web UI http://{bind_addr}/", self.name);
        tracing::info!("{} — mcp    http://{bind_addr}/mcp", self.name);
        tracing::info!("{} — sync   ws://{bind_addr}/sync", self.name);

        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    }
}
