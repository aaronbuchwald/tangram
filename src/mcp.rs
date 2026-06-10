//! MCP frontend: exposes the app's capabilities as tools over the
//! Model Context Protocol (streamable HTTP transport, mounted at /mcp).
//!
//! Add a tool by adding a method inside the `#[tool_router]` impl block.
//! Parameters are plain structs deriving `Deserialize` + `JsonSchema`.

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};

use crate::state::AppState;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AddNoteRequest {
    #[schemars(description = "The text of the note to add")]
    pub text: String,
}

#[derive(Clone)]
pub struct TangramMcp {
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl TangramMcp {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl TangramMcp {
    #[tool(description = "Add a note to the shared scratchpad")]
    async fn add_note(
        &self,
        Parameters(AddNoteRequest { text }): Parameters<AddNoteRequest>,
    ) -> String {
        let note = self.state.add_note(text).await;
        serde_json::to_string(&note).unwrap_or_default()
    }

    #[tool(description = "List all notes on the shared scratchpad")]
    async fn list_notes(&self) -> String {
        let notes = self.state.list_notes().await;
        serde_json::to_string(&notes).unwrap_or_default()
    }

    #[tool(description = "Delete all notes from the shared scratchpad")]
    async fn clear_notes(&self) -> String {
        let count = self.state.clear_notes().await;
        format!("Cleared {count} note(s)")
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TangramMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "A shared scratchpad. Notes added here are visible to humans in the \
                 web UI, and notes they add are visible to you via list_notes.",
        )
    }
}
