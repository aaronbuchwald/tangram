//! Tangram SDK: build small local-first apps with replicated state and
//! auto-derived MCP + web surfaces.
//!
//! Define your state as plain Rust structs with [`macro@model`], expose logic
//! with [`macro@actions`], and serve everything with [`App`]:
//!
//! ```ignore
//! use tangram::prelude::*;
//!
//! #[model]
//! #[derive(Default)]
//! struct Counter { value: i64 }
//!
//! #[actions]
//! impl Counter {
//!     /// Increment the counter.
//!     pub fn increment(&mut self) -> i64 { self.value += 1; self.value }
//! }
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     App::<Counter>::new("counter").serve().await
//! }
//! ```
//!
//! The state lives in an Automerge CRDT document persisted to disk, so it is
//! local-first by default; set `TANGRAM_REMOTE=ws://host:port/sync` to
//! replicate with another instance (changes merge from either side and every
//! connected UI updates live over SSE).

mod action;
mod app;
mod mcp;
mod store;
mod sync;
mod web;

pub use action::{ActionDef, ActionError, ActionFuture, ActionHandler, Actions};
pub use app::App;
pub use store::Ctx;
pub use tangram_macros::{actions, model};

/// Everything an app needs in scope.
pub mod prelude {
    pub use crate::{Actions, App, Ctx, actions, model};
}

/// Implementation details used by macro expansions. Not a public API.
#[doc(hidden)]
pub mod __private {
    pub use serde_json;
}

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
