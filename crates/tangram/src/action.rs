//! Action registry: the bridge between model methods and the derived
//! MCP/HTTP surfaces. Populated by the `#[actions]` macro.

use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;

use crate::store::Ctx;

/// The boxed future an async action handler returns.
pub type ActionFuture =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, ActionError>> + Send>>;

/// How an action runs. Both kinds dispatch through the same path (the
/// store's `dispatch`), so the HTTP and MCP surfaces expose identical
/// contracts by construction.
pub enum ActionHandler<M> {
    /// A pure state transition: runs under the store lock as one attributed
    /// commit. Must not perform I/O.
    Sync(fn(&mut M, serde_json::Value) -> Result<serde_json::Value, ActionError>),
    /// An async action: runs OUTSIDE the store lock and may perform I/O
    /// (network lookups, etc.). It receives a [`Ctx`] to read state snapshots
    /// and commit attributed mutations; the lock is never held across an
    /// await.
    Async(fn(Ctx<M>, serde_json::Value) -> ActionFuture),
}

// fn pointers are Copy regardless of `M`, so implement manually instead of
// deriving (a derive would demand `M: Copy`).
impl<M> Clone for ActionHandler<M> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<M> Copy for ActionHandler<M> {}

/// One action: a named, described, schema'd operation on a model.
pub struct ActionDef<M> {
    pub name: &'static str,
    /// Human/agent-facing description (from the method's doc comment).
    pub description: &'static str,
    /// Whether the action mutates the document (`&mut self` vs `&self`;
    /// async actions are assumed to mutate).
    pub mutates: bool,
    /// JSON Schema for the action's argument object.
    pub input_schema: fn() -> serde_json::Value,
    /// Deserializes args, invokes the method, serializes the result.
    pub handler: ActionHandler<M>,
}

/// Implemented by `#[actions]` on the model's impl block.
pub trait Actions: Sized {
    fn actions() -> Vec<ActionDef<Self>>;
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("unknown action: {0}")]
    Unknown(String),
    #[error("invalid arguments: {0}")]
    BadArgs(String),
    #[error("{0}")]
    Failed(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl ActionError {
    pub fn bad_args(e: impl Display) -> Self {
        Self::BadArgs(e.to_string())
    }
    pub fn failed(e: impl Display) -> Self {
        Self::Failed(e.to_string())
    }
    pub fn internal(e: impl Display) -> Self {
        Self::Internal(e.to_string())
    }
}
