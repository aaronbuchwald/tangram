//! The capabilities probe, layered on top of the derived Tangram surface.
//!
//! Every user-facing *operation* is a registered action; this custom route
//! exists only because a capabilities probe is not an operation — it reports
//! what the app can do (the offline `fixture` path is always available; the
//! `live` egress flag is ANDed in by the host in the live tier) so the UI can
//! show a "not configured" hint without the component holding any secret.

use axum::routing::get;
use axum::{Json, Router};

/// The capabilities route. The JSON body comes from
/// [`crate::capabilities_json`] — the same constructor the WASM component
/// publishes through `describe()`, so the native route and `tangram-host`'s
/// `GET /<app>/api/capabilities` answer identically.
pub fn routes() -> Router {
    Router::new().route(
        "/api/capabilities",
        get(|| async { Json(crate::capabilities_json()) }),
    )
}
