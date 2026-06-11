//! `tangram:mcp` — tangram-core's sans-io MCP state machine as a WASM
//! component, so the Cloudflare Worker's `/mcp` endpoint runs the same Rust
//! protocol code (negotiation, sessions, JSON-RPC framing, the action error
//! contract) as the native hosts. See `wit/mcp.wit` and ADR-0002.
//!
//! State (the `McpServer` plus pending tool calls) lives in component
//! statics: one component instance == one MCP server, instantiated per
//! Durable Object. The host serializes calls (JS is single-threaded), but
//! the statics are mutex-guarded anyway to keep the code obviously sound.

use std::collections::HashMap;
use std::sync::Mutex;

use tangram_core::mcp::{Body, Handled, McpServer, Request, Response, ToolCall, ToolDef};

mod wit {
    wit_bindgen::generate!({
        path: "wit",
        world: "mcp",
    });
}

use wit::exports::tangram::mcp::machine::{
    BodyKind, Guest, Handled as WitHandled, HttpRequest, HttpResponse, ToolCall as WitToolCall,
};

static SERVER: Mutex<Option<McpServer>> = Mutex::new(None);
static PENDING: Mutex<Option<PendingCalls>> = Mutex::new(None);

#[derive(Default)]
struct PendingCalls {
    next_token: u64,
    calls: HashMap<u64, ToolCall>,
}

fn convert(response: Response) -> HttpResponse {
    let (body_kind, body) = match response.body {
        Body::Empty => (BodyKind::Empty, String::new()),
        Body::Text(t) => (BodyKind::Text, t),
        Body::SseMessage(t) => (BodyKind::SseMessage, t),
        Body::SseStream(t) => (BodyKind::SseStream, t),
    };
    HttpResponse {
        status: response.status,
        session_id: response.session_id,
        body_kind,
        body,
    }
}

struct Component;

impl Guest for Component {
    fn init(
        name: String,
        version: String,
        instructions: Option<String>,
        tools_json: String,
    ) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        struct Tool {
            name: String,
            description: String,
            input_schema: serde_json::Value,
        }
        let tools: Vec<Tool> = serde_json::from_str(&tools_json)
            .map_err(|e| format!("tools-json must be [{{name,description,input_schema}}]: {e}"))?;
        let tools = tools
            .into_iter()
            .map(|t| ToolDef {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
            })
            .collect();
        *SERVER.lock().expect("server lock") =
            Some(McpServer::new(name, version, instructions, tools));
        *PENDING.lock().expect("pending lock") = Some(PendingCalls::default());
        Ok(())
    }

    fn handle(request: HttpRequest) -> WitHandled {
        let server = SERVER.lock().expect("server lock");
        let Some(server) = server.as_ref() else {
            return WitHandled::Response(HttpResponse {
                status: 500,
                session_id: None,
                body_kind: BodyKind::Text,
                body: "mcp machine not initialized".into(),
            });
        };
        let handled = server.handle(&Request {
            method: &request.method,
            accept: request.accept.as_deref(),
            content_type: request.content_type.as_deref(),
            session_id: request.session_id.as_deref(),
            body: &request.body,
        });
        match handled {
            Handled::Response(response) => WitHandled::Response(convert(response)),
            Handled::ToolCall(call) => {
                let name = call.name.clone();
                let args_json = call.arguments.to_string();
                let mut pending = PENDING.lock().expect("pending lock");
                let pending = pending.as_mut().expect("init created pending map");
                let token = pending.next_token;
                pending.next_token += 1;
                pending.calls.insert(token, call);
                WitHandled::ToolCall(WitToolCall {
                    token,
                    name,
                    args_json,
                })
            }
        }
    }

    fn resolve(token: u64, outcome: String, text: String) -> Result<HttpResponse, String> {
        let call = PENDING
            .lock()
            .expect("pending lock")
            .as_mut()
            .and_then(|p| p.calls.remove(&token))
            .ok_or_else(|| format!("no pending tool call with token {token}"))?;
        let response = match outcome.as_str() {
            "ok" => call.succeed(text),
            "fail" => call.fail(text),
            "unknown-tool" => call.unknown_tool(text),
            "internal-error" => call.internal_error(text),
            other => return Err(format!("unknown outcome {other:?}")),
        };
        Ok(convert(response))
    }
}

wit::export!(Component with_types_in wit);
