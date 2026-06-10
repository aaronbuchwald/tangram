mod mcp;
mod state;
mod web;

use anyhow::Context;
use axum::http::{HeaderValue, header};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tower_http::{set_header::SetResponseHeaderLayer, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env if present (never committed; see .env.example).
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());

    // Which origins may embed this UI in an iframe. Defaults to allowing all
    // so the UI works inside Obsidian, the Tangram shell, or any other host.
    // Set FRAME_ANCESTORS to a specific list (e.g. "'self' app://obsidian.md")
    // to lock it down.
    let frame_ancestors = std::env::var("FRAME_ANCESTORS").unwrap_or_else(|_| "*".into());
    let csp = HeaderValue::from_str(&format!("frame-ancestors {frame_ancestors}"))
        .context("FRAME_ANCESTORS contains characters not valid in a header value")?;

    let app_state = AppState::default();

    let mcp_service = StreamableHttpService::new(
        {
            let app_state = app_state.clone();
            move || Ok(mcp::TangramMcp::new(app_state.clone()))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );

    let app = web::router(app_state)
        .nest_service("/mcp", mcp_service)
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            csp,
        ))
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    tracing::info!("web UI    http://{bind_addr}/");
    tracing::info!("mcp       http://{bind_addr}/mcp");
    tracing::info!("health    http://{bind_addr}/healthz");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}
