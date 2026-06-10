//! Per-app MCP surface, mirroring the SDK's `McpBridge`: tools come from the
//! component's `describe()` manifest and calls dispatch into the live
//! component instance — agents are just another client of the same document.

use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};

use crate::app::{AppRuntime, DispatchError};

#[derive(Clone)]
pub struct McpBridge {
    runtime: Arc<AppRuntime>,
    tools: Arc<Vec<Tool>>,
}

impl McpBridge {
    pub fn new(runtime: Arc<AppRuntime>) -> Self {
        let tools = runtime
            .describe
            .actions
            .iter()
            .map(|a| {
                let schema = match a.input_schema.clone() {
                    serde_json::Value::Object(map) => map,
                    _ => serde_json::Map::new(),
                };
                Tool::new(a.name.clone(), a.description.clone(), schema)
            })
            .collect();
        Self {
            runtime,
            tools: Arc::new(tools),
        }
    }
}

impl ServerHandler for McpBridge {
    fn get_info(&self) -> ServerInfo {
        let info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        match &self.runtime.describe.instructions {
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
        match self.runtime.dispatch(&request.name, args).await {
            Ok(result) => {
                let text =
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            // Tool-level failures (bad args, domain errors) are reported as
            // tool results so the agent can read and recover from them —
            // same mapping as the SDK's bridge.
            Err(e @ (DispatchError::BadArgs(_) | DispatchError::Failed(_))) => {
                Ok(CallToolResult::error(vec![Content::text(e.to_string())]))
            }
            Err(e @ DispatchError::Unknown(_)) => {
                Err(ErrorData::invalid_params(e.to_string(), None))
            }
            Err(e @ DispatchError::Internal(_)) => {
                Err(ErrorData::internal_error(e.to_string(), None))
            }
        }
    }
}
