//! The capabilities probe, layered on top of the derived Tangram surface.
//!
//! Every user-facing *operation* is a registered action (description-based
//! logging lives in the async `log_meal` action, dispatched identically over
//! HTTP and MCP); this custom route exists only because a capability probe
//! is not an operation — it reports which strategy is active so the UI can
//! decide whether to offer the description input.

use axum::routing::get;
use axum::{Json, Router};

use crate::strategy::Strategy;

/// The capabilities route. Resolves (and logs) the active strategy once at
/// startup. The JSON body comes from [`crate::capabilities_json`] — the same
/// constructor the WASM component publishes through `describe()`, so the
/// native route and `tangram-host`'s `GET /<app>/api/capabilities` answer
/// identically for the same environment.
pub fn routes() -> Router {
    let (strategy, reason) = Strategy::from_env_with_reason();
    tracing::info!("nutrition strategy: {} ({reason})", strategy.name());
    Router::new().route(
        "/api/capabilities",
        get(move || async move { Json(crate::capabilities_json(strategy)) }),
    )
}
