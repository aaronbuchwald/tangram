//! The derived MCP surface: every action in the registry becomes an MCP tool,
//! served over the streamable HTTP transport at `/mcp`. Agents are just
//! another client of the same store — their writes flow through the same
//! action pipeline, land in the same CRDT document, and push to every UI and
//! sync peer like any other change.

use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};

use crate::Model;
use crate::action::{ActionError, Actions};
use crate::store::Store;

pub(crate) struct McpBridge<M> {
    store: Arc<Store<M>>,
    tools: Arc<Vec<Tool>>,
    instructions: Option<String>,
}

impl<M> Clone for McpBridge<M> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            tools: self.tools.clone(),
            instructions: self.instructions.clone(),
        }
    }
}

impl<M: Model + Actions> McpBridge<M> {
    pub fn new(store: Arc<Store<M>>, instructions: Option<String>) -> Self {
        let tools = store
            .action_defs()
            .map(|a| {
                let schema = match (a.input_schema)() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                Tool::new(a.name, a.description, schema)
            })
            .collect();
        Self {
            store,
            tools: Arc::new(tools),
            instructions,
        }
    }
}

impl<M: Model + Actions> ServerHandler for McpBridge<M> {
    fn get_info(&self) -> ServerInfo {
        let info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        match &self.instructions {
            Some(text) => info.with_instructions(text),
            None => info,
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: (*self.tools).clone(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or_else(|| serde_json::json!({}));
        match self.store.dispatch(&request.name, args).await {
            Ok(result) => {
                let text =
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            // Tool-level failures (bad args, domain errors) are reported as
            // tool results so the agent can read and recover from them.
            Err(e @ (ActionError::BadArgs(_) | ActionError::Failed(_))) => {
                Ok(CallToolResult::error(vec![Content::text(e.to_string())]))
            }
            Err(e @ ActionError::Unknown(_)) => Err(ErrorData::invalid_params(e.to_string(), None)),
            Err(e @ ActionError::Internal(_)) => {
                Err(ErrorData::internal_error(e.to_string(), None))
            }
        }
    }
}
