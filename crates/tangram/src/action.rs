//! Action registry: the bridge between model methods and the derived
//! MCP/HTTP surfaces. Populated by the `#[actions]` macro.

use std::fmt::Display;

/// One action: a named, described, schema'd operation on a model.
pub struct ActionDef<M> {
    pub name: &'static str,
    /// Human/agent-facing description (from the method's doc comment).
    pub description: &'static str,
    /// Whether the action mutates the document (`&mut self` vs `&self`).
    pub mutates: bool,
    /// JSON Schema for the action's argument object.
    pub input_schema: fn() -> serde_json::Value,
    /// Deserializes args, invokes the method, serializes the result.
    pub handler: fn(&mut M, serde_json::Value) -> Result<serde_json::Value, ActionError>,
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
