//! The capabilities probe, layered on top of the derived Tangram surface.
//!
//! Every user-facing *operation* is a registered action (description-based
//! logging lives in the async `log_meal` action, dispatched identically over
//! HTTP and MCP); this custom route exists only because a capability probe
//! is not an operation — it reports which strategy is active so the UI can
//! decide whether to offer the description input.

use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::strategy::Strategy;

/// The capabilities route. Resolves (and logs) the active strategy once at
/// startup.
pub fn routes() -> Router {
    let (strategy, reason) = Strategy::from_env_with_reason();
    tracing::info!("nutrition strategy: {} ({reason})", strategy.name());
    Router::new().route(
        "/api/capabilities",
        get(move || async move {
            Json(json!({
                "strategy": strategy.name(),
                "description_input": strategy.can_resolve(),
            }))
        }),
    )
}
