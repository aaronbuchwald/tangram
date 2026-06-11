//! The platform-portable core of the Tangram SDK (`RUNTIME_PLAN` Phase 1):
//! everything an app's derived surfaces need that does NOT depend on a
//! particular runtime or HTTP stack — the action registry, the automerge
//! document store and its single dispatch path, the sync-protocol
//! session/framing logic, and a sans-io streamable-HTTP MCP server.
//!
//! The `tangram` crate layers the native tokio/axum host on top of this and
//! re-exports the public types; future hosts (wasi:http, Cloudflare
//! workers-rs, browsers) embed this crate directly. Nothing here may depend
//! on tokio/hyper/axum/reqwest/rmcp, and the crate must keep compiling for
//! `wasm32-wasip2`.

pub mod action;
pub mod mcp;
pub mod store;
pub mod sync;

pub use action::{ActionDef, ActionError, ActionFuture, ActionHandler, Actions};
pub use store::{Ctx, Store, genesis_bytes};

/// A replicated application state: any `#[model]` struct with a `Default`
/// genesis state satisfies this automatically.
pub trait Model:
    autosurgeon::Reconcile
    + autosurgeon::Hydrate
    + serde::Serialize
    + Default
    + Clone
    + Send
    + Sync
    + 'static
{
}

impl<T> Model for T where
    T: autosurgeon::Reconcile
        + autosurgeon::Hydrate
        + serde::Serialize
        + Default
        + Clone
        + Send
        + Sync
        + 'static
{
}
