//! The derived MCP surface: every action in the registry becomes an MCP tool,
//! served over the streamable HTTP transport at `/mcp`. Agents are just
//! another client of the same store — their writes flow through the same
//! action pipeline, land in the same CRDT document, and push to every UI and
//! sync peer like any other change.
//!
//! The protocol itself is the portable sans-io [`tangram_core::mcp`] state
//! machine (which replaced the rmcp server; its wire behavior was captured
//! as golden and is held by the parity suites in `tangram-core` and this
//! crate's `tests/mcp.rs`). This module is only the axum transport around
//! it: HTTP request in, drive the machine, dispatch tool calls through the
//! store, HTTP response out.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use tangram_core::mcp::{self, Handled, McpServer, ToolDef};

use crate::Model;
use crate::action::{ActionError, Actions};
use crate::store::Store;

/// Cap on JSON-RPC request bodies — tool arguments are small.
const MAX_BODY: usize = 2 * 1024 * 1024;

/// How often an idle `GET /mcp` listening stream emits an SSE keep-alive
/// comment.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

struct McpState<M> {
    server: McpServer,
    store: Arc<Store<M>>,
}

/// The `/mcp` route: the portable MCP server over this app's action registry.
pub(crate) fn router<M: Model + Actions>(
    store: Arc<Store<M>>,
    name: String,
    instructions: Option<String>,
) -> Router {
    let tools = store
        .action_defs()
        .map(|a| ToolDef {
            name: a.name.to_string(),
            description: a.description.to_string(),
            input_schema: (a.input_schema)(),
        })
        .collect();
    let server = McpServer::new(name, env!("CARGO_PKG_VERSION"), instructions, tools);
    Router::new()
        .route("/mcp", any(handle::<M>))
        .with_state(Arc::new(McpState { server, store }))
}

async fn handle<M: Model + Actions>(
    axum::extract::State(state): axum::extract::State<Arc<McpState<M>>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
    }
    let request = mcp::Request {
        method: method.as_str(),
        accept: header_str(&headers, header::ACCEPT),
        content_type: header_str(&headers, header::CONTENT_TYPE),
        session_id: headers.get("mcp-session-id").and_then(|v| v.to_str().ok()),
        body: &body,
    };
    let response = match state.server.handle(&request) {
        Handled::Response(response) => response,
        // The machine validated a tools/call; run it through the same
        // dispatch path the HTTP action route uses and map the action
        // contract onto MCP: domain failures are tool results the agent can
        // read, only unknown tools / internal faults are JSON-RPC errors.
        Handled::ToolCall(call) => {
            match state
                .store
                .dispatch(&call.name, call.arguments.clone())
                .await
            {
                Ok(result) => {
                    let text = serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| result.to_string());
                    call.succeed(text)
                }
                Err(e @ (ActionError::BadArgs(_) | ActionError::Failed(_))) => {
                    call.fail(e.to_string())
                }
                Err(e @ ActionError::Unknown(_)) => call.unknown_tool(e.to_string()),
                Err(e @ ActionError::Internal(_)) => call.internal_error(e.to_string()),
            }
        }
    };
    into_axum(response)
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn into_axum(response: mcp::Response) -> Response {
    let mut builder = Response::builder().status(response.status);
    for (name, value) in response.headers() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            builder = builder.header(name, value);
        }
    }
    let body = match response.body {
        mcp::Body::Empty => Body::empty(),
        mcp::Body::Text(text) => Body::from(text),
        mcp::Body::SseMessage(sse) => Body::from(sse),
        // The standalone listening stream: this server never initiates
        // messages, so after the initial bytes it only carries keep-alive
        // comments until the client disconnects.
        mcp::Body::SseStream(initial) => {
            let keep_alives = futures_util::stream::unfold((), |()| async {
                tokio::time::sleep(KEEP_ALIVE).await;
                Some((
                    Ok::<_, Infallible>(Bytes::from_static(b": keep-alive\n\n")),
                    (),
                ))
            });
            let initial = futures_util::stream::iter([Ok(Bytes::from(initial))]);
            Body::from_stream(futures_util::StreamExt::chain(initial, keep_alives))
        }
    };
    builder.body(body).expect("valid response")
}
