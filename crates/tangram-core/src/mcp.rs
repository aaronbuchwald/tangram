//! A sans-io MCP server speaking the streamable-HTTP transport — the
//! portable replacement for rmcp's tokio-bound server (ADR-0001): plain
//! HTTP-shaped values in (method, headers, body), plain HTTP-shaped values
//! out (status, headers, body — SSE-framed where the transport requires it),
//! and no I/O, clocks, or runtime anywhere in between. Any host that can
//! answer an HTTP request — axum today; wasi:http, Cloudflare workers-rs, a
//! browser service worker later — can serve MCP by driving [`McpServer`].
//!
//! The wire behavior mirrors rmcp 1.7's streamable-HTTP server, captured
//! byte-for-byte from a live app in
//! `tests/fixtures/rmcp-golden.json` (status codes for every
//! negotiation/session failure, SSE-framed JSON-RPC responses, `202` for
//! notifications, session issuance on `initialize`, `404` for unknown or
//! deleted sessions). Real clients — Claude Code, Claude Desktop — were the
//! parity bar; byte-level differences from rmcp (SSE event ids, priming
//! events) are deliberate simplifications that stay inside the SSE spec.
//!
//! Tool execution is the embedder's job: [`McpServer::handle`] returns
//! either a finished [`Response`] or a [`ToolCall`] the embedder resolves
//! however it likes (async or not) and converts into the final [`Response`]
//! via [`ToolCall::succeed`] & friends. That keeps this crate free of any
//! async runtime while still serving tools backed by async dispatch.

use std::collections::HashSet;
use std::sync::Mutex;

use serde_json::{Value, json};

/// Protocol revisions this server knows, newest first. `initialize` echoes
/// the client's requested version when supported and otherwise answers with
/// the newest one (same negotiation rmcp 1.7 performs — for a tools-only
/// server every listed revision is served identically).
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// One tool exposed over `tools/list` / `tools/call` — for Tangram apps,
/// one registered action.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's argument object.
    pub input_schema: Value,
}

/// An incoming HTTP request to the MCP endpoint, reduced to the parts the
/// protocol cares about.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// HTTP method, uppercase (`POST`, `GET`, `DELETE`).
    pub method: &'a str,
    /// `Accept` header, if present.
    pub accept: Option<&'a str>,
    /// `Content-Type` header, if present.
    pub content_type: Option<&'a str>,
    /// `Mcp-Session-Id` header, if present.
    pub session_id: Option<&'a str>,
    /// Request body bytes.
    pub body: &'a [u8],
}

/// An outgoing HTTP response. [`Response::headers`] yields every header the
/// transport requires (content type, `Mcp-Session-Id`); the embedder only
/// adds connection mechanics (e.g. chunked transfer for streams).
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    /// `Mcp-Session-Id` to return (issued by `initialize`).
    pub session_id: Option<String>,
    pub body: Body,
}

/// Response payloads, pre-framed for the transport.
#[derive(Debug, PartialEq)]
pub enum Body {
    /// No body (`202 Accepted` for notifications and session deletes).
    Empty,
    /// A plain-text transport error (negotiation/session failures).
    Text(String),
    /// A complete `text/event-stream` body carrying one JSON-RPC message;
    /// the connection closes after it is sent.
    SseMessage(String),
    /// An endless `text/event-stream` (the `GET` listening stream): send
    /// these initial bytes, then hold the connection open. The embedder may
    /// interleave SSE comments (`: keep-alive`) at its own cadence.
    SseStream(String),
}

impl Response {
    fn sse(status: u16, message: &Value) -> Self {
        Self {
            status,
            session_id: None,
            body: Body::SseMessage(format!("data: {message}\n\n")),
        }
    }

    fn text(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            session_id: None,
            body: Body::Text(message.into()),
        }
    }

    fn accepted() -> Self {
        Self {
            status: 202,
            session_id: None,
            body: Body::Empty,
        }
    }

    /// Every header the response needs, ready to copy onto the transport.
    #[must_use]
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = Vec::new();
        match &self.body {
            Body::SseMessage(_) | Body::SseStream(_) => {
                headers.push(("content-type", "text/event-stream".to_string()));
                headers.push(("cache-control", "no-cache".to_string()));
            }
            Body::Text(_) => headers.push(("content-type", "text/plain".to_string())),
            Body::Empty => {}
        }
        if let Some(id) = &self.session_id {
            headers.push(("mcp-session-id", id.clone()));
        }
        headers
    }
}

/// What [`McpServer::handle`] produced: either a finished response, or a
/// tool invocation the embedder must run and turn into the response.
#[derive(Debug)]
pub enum Handled {
    Response(Response),
    ToolCall(ToolCall),
}

/// A validated `tools/call` awaiting execution by the embedder. Exactly one
/// of the consuming methods turns it into the HTTP response; the error
/// mapping matches the Tangram action contract (domain failures are tool
/// results the agent can read, only unknown tools / internal faults are
/// JSON-RPC errors).
#[derive(Debug)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    id: Value,
}

impl ToolCall {
    /// The tool ran: a `CallToolResult` with `isError: false`.
    pub fn succeed(self, text: impl Into<String>) -> Response {
        self.call_result(text.into(), false)
    }

    /// The tool failed at the tool level (bad args, domain error): a
    /// `CallToolResult` with `isError: true` so the agent can recover.
    pub fn fail(self, message: impl Into<String>) -> Response {
        self.call_result(message.into(), true)
    }

    /// No such tool: JSON-RPC `-32602` (invalid params).
    pub fn unknown_tool(self, message: impl Into<String>) -> Response {
        Response::sse(200, &error_message(&self.id, -32602, &message.into()))
    }

    /// The server itself broke: JSON-RPC `-32603` (internal error).
    pub fn internal_error(self, message: impl Into<String>) -> Response {
        Response::sse(200, &error_message(&self.id, -32603, &message.into()))
    }

    fn call_result(self, text: String, is_error: bool) -> Response {
        Response::sse(
            200,
            &result_message(
                &self.id,
                json!({
                    "content": [{"type": "text", "text": text}],
                    "isError": is_error,
                }),
            ),
        )
    }
}

fn result_message(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_message(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The MCP server protocol state: identity, the tool list, and the set of
/// live session ids. One instance serves any number of concurrent sessions
/// and is driven from whatever concurrency model the host has (`&self`
/// everywhere; internal state behind a mutex).
pub struct McpServer {
    name: String,
    version: String,
    instructions: Option<String>,
    tools: Vec<ToolDef>,
    sessions: Mutex<HashSet<String>>,
}

impl McpServer {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        instructions: Option<String>,
        tools: Vec<ToolDef>,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            instructions,
            tools,
            sessions: Mutex::new(HashSet::new()),
        }
    }

    /// Drive one HTTP exchange through the protocol.
    pub fn handle(&self, request: &Request) -> Handled {
        match request.method {
            "POST" => self.handle_post(request),
            "GET" => Handled::Response(self.handle_get(request)),
            "DELETE" => Handled::Response(self.handle_delete(request)),
            _ => Handled::Response(Response::text(405, "Method Not Allowed")),
        }
    }

    // ── POST: the JSON-RPC channel ───────────────────────────────────────

    fn handle_post(&self, request: &Request) -> Handled {
        // Transport negotiation, mirroring rmcp exactly: the client must
        // name both media types explicitly (`*/*` is not enough — that is
        // what rmcp's golden behavior rejects, and MCP clients always send
        // both).
        let accept = request.accept.unwrap_or_default();
        if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
            return Handled::Response(Response::text(
                406,
                "Not Acceptable: Client must accept both application/json and text/event-stream",
            ));
        }
        let content_type = request.content_type.unwrap_or_default();
        if !content_type.starts_with("application/json") {
            return Handled::Response(Response::text(
                415,
                "Unsupported Media Type: Content-Type must be application/json",
            ));
        }
        let message: Value = match serde_json::from_slice(request.body) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) => {
                return Handled::Response(Response::text(
                    415,
                    "fail to deserialize request body: expected a JSON-RPC message object",
                ));
            }
            Err(e) => {
                return Handled::Response(Response::text(
                    415,
                    format!("fail to deserialize request body {e}"),
                ));
            }
        };

        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();

        match (method, id) {
            (Some("initialize"), Some(id)) => {
                Handled::Response(self.initialize(&id, message.get("params")))
            }
            // Everything else needs an established session.
            (method, id) => {
                let Some(session) = request.session_id else {
                    return Handled::Response(Response::text(
                        422,
                        "Unexpected message, expect initialize request",
                    ));
                };
                if !self
                    .sessions
                    .lock()
                    .expect("sessions lock")
                    .contains(session)
                {
                    return Handled::Response(Response::text(404, "Not Found: Session not found"));
                }
                match (method, id) {
                    (Some(method), Some(id)) => self.request(method, &id, message.get("params")),
                    // Notifications (initialized, cancelled, …) and client
                    // responses are accepted and acknowledged.
                    (Some(_), None) | (None, Some(_)) => Handled::Response(Response::accepted()),
                    (None, None) => Handled::Response(Response::text(
                        415,
                        "fail to deserialize request body: not a JSON-RPC message",
                    )),
                }
            }
        }
    }

    fn initialize(&self, id: &Value, params: Option<&Value>) -> Response {
        let requested = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let version = SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .find(|v| **v == requested)
            .unwrap_or(&SUPPORTED_PROTOCOL_VERSIONS[0]);

        let session = uuid::Uuid::new_v4().to_string();
        self.sessions
            .lock()
            .expect("sessions lock")
            .insert(session.clone());

        let mut result = json!({
            "protocolVersion": version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": self.name, "version": self.version},
        });
        if let Some(instructions) = &self.instructions {
            result["instructions"] = json!(instructions);
        }
        let mut response = Response::sse(200, &result_message(id, result));
        response.session_id = Some(session);
        response
    }

    /// A JSON-RPC request on an established session.
    fn request(&self, method: &str, id: &Value, params: Option<&Value>) -> Handled {
        match method {
            "ping" => Handled::Response(Response::sse(200, &result_message(id, json!({})))),
            "tools/list" => {
                let tools: Vec<Value> = self
                    .tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema,
                        })
                    })
                    .collect();
                Handled::Response(Response::sse(
                    200,
                    &result_message(id, json!({"tools": tools})),
                ))
            }
            "tools/call" => {
                let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
                    return Handled::Response(Response::sse(
                        200,
                        &error_message(id, -32602, "tools/call params require a tool name"),
                    ));
                };
                let arguments = params
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Handled::ToolCall(ToolCall {
                    name: name.to_string(),
                    arguments,
                    id: id.clone(),
                })
            }
            other => Handled::Response(Response::sse(
                200,
                &error_message(id, -32601, &format!("method not found: {other}")),
            )),
        }
    }

    // ── GET: the standalone listening stream ────────────────────────────

    fn handle_get(&self, request: &Request) -> Response {
        let accept = request.accept.unwrap_or_default();
        if !accept.contains("text/event-stream") {
            return Response::text(406, "Not Acceptable: Client must accept text/event-stream");
        }
        let Some(session) = request.session_id else {
            return Response::text(400, "Bad Request: Session ID is required");
        };
        if !self
            .sessions
            .lock()
            .expect("sessions lock")
            .contains(session)
        {
            return Response::text(404, "Not Found: Session not found");
        }
        // This server never initiates messages, so the stream carries only
        // an opening comment; the embedder keeps it alive.
        Response {
            status: 200,
            session_id: None,
            body: Body::SseStream(String::new()),
        }
    }

    // ── DELETE: end a session ────────────────────────────────────────────

    fn handle_delete(&self, request: &Request) -> Response {
        let Some(session) = request.session_id else {
            return Response::text(400, "Bad Request: Session ID is required");
        };
        if !self.sessions.lock().expect("sessions lock").remove(session) {
            return Response::text(404, "Not Found: Session not found");
        }
        Response::accepted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCEPT_BOTH: &str = "application/json, text/event-stream";

    fn server() -> McpServer {
        McpServer::new(
            "test-app",
            "0.1.0",
            Some("be excellent".to_string()),
            vec![ToolDef {
                name: "add_note".into(),
                description: "Add a note.".into(),
                input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            }],
        )
    }

    fn post<'a>(body: &'a str, session: Option<&'a str>) -> Request<'a> {
        Request {
            method: "POST",
            accept: Some(ACCEPT_BOTH),
            content_type: Some("application/json"),
            session_id: session,
            body: body.as_bytes(),
        }
    }

    fn response(handled: Handled) -> Response {
        match handled {
            Handled::Response(r) => r,
            Handled::ToolCall(c) => panic!("expected response, got tool call {c:?}"),
        }
    }

    /// The single JSON-RPC message inside an SSE body.
    fn sse_json(response: &Response) -> Value {
        let Body::SseMessage(body) = &response.body else {
            panic!("expected SSE message body, got {:?}", response.body);
        };
        let data = body
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("data line");
        serde_json::from_str(data).expect("valid JSON in SSE data")
    }

    fn initialize(server: &McpServer) -> (String, Value) {
        let body = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "claude-code", "version": "2.0.0"}},
        })
        .to_string();
        let resp = response(server.handle(&post(&body, None)));
        assert_eq!(resp.status, 200);
        let session = resp.session_id.clone().expect("session issued");
        let msg = sse_json(&resp);
        (session, msg)
    }

    #[test]
    fn initialize_issues_session_and_negotiates_version() {
        let server = server();
        let (session, msg) = initialize(&server);
        assert!(!session.is_empty());
        assert_eq!(msg["id"], 0);
        assert_eq!(msg["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(msg["result"]["capabilities"], json!({"tools": {}}));
        assert_eq!(msg["result"]["serverInfo"]["name"], "test-app");
        assert_eq!(msg["result"]["instructions"], "be excellent");

        // headers carry the transport contract
        let resp = response(
            server.handle(&post(
                &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                    "params": {"protocolVersion": "2025-06-18"}})
                .to_string(),
                None,
            )),
        );
        let headers = resp.headers();
        assert!(headers.contains(&("content-type", "text/event-stream".to_string())));
        assert!(headers.iter().any(|(k, _)| *k == "mcp-session-id"));
    }

    #[test]
    fn unsupported_protocol_version_falls_back_to_latest() {
        let server = server();
        let body = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                          "params": {"protocolVersion": "9999-01-01"}})
        .to_string();
        let msg = sse_json(&response(server.handle(&post(&body, None))));
        assert_eq!(
            msg["result"]["protocolVersion"],
            SUPPORTED_PROTOCOL_VERSIONS[0]
        );
    }

    #[test]
    fn session_lifecycle() {
        let server = server();
        let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();

        // before initialize: no session header → 422; bogus session → 404
        assert_eq!(response(server.handle(&post(&list, None))).status, 422);
        assert_eq!(
            response(server.handle(&post(&list, Some("bogus")))).status,
            404
        );

        let (session, _) = initialize(&server);
        assert_eq!(
            response(server.handle(&post(&list, Some(&session)))).status,
            200
        );

        // a second session coexists with the first
        let (second, _) = initialize(&server);
        assert_ne!(session, second);
        assert_eq!(
            response(server.handle(&post(&list, Some(&second)))).status,
            200
        );
        assert_eq!(
            response(server.handle(&post(&list, Some(&session)))).status,
            200
        );

        // DELETE ends a session; the other survives
        let delete = Request {
            method: "DELETE",
            accept: None,
            content_type: None,
            session_id: Some(&session),
            body: b"",
        };
        assert_eq!(response(server.handle(&delete)).status, 202);
        assert_eq!(
            response(server.handle(&delete)).status,
            404,
            "double delete"
        );
        assert_eq!(
            response(server.handle(&post(&list, Some(&session)))).status,
            404
        );
        assert_eq!(
            response(server.handle(&post(&list, Some(&second)))).status,
            200
        );
    }

    #[test]
    fn notifications_are_accepted_with_202() {
        let server = server();
        let (session, _) = initialize(&server);
        let body = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();
        let resp = response(server.handle(&post(&body, Some(&session))));
        assert_eq!(resp.status, 202);
        assert_eq!(resp.body, Body::Empty);
        // but not without a session
        assert_eq!(response(server.handle(&post(&body, None))).status, 422);
    }

    #[test]
    fn tools_list_reflects_tool_defs() {
        let server = server();
        let (session, _) = initialize(&server);
        let body = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"}).to_string();
        let msg = sse_json(&response(server.handle(&post(&body, Some(&session)))));
        assert_eq!(msg["id"], 7);
        let tools = msg["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "add_note");
        assert_eq!(tools[0]["description"], "Add a note.");
        assert_eq!(tools[0]["inputSchema"]["type"], "object");
    }

    #[test]
    fn tools_call_round_trips_through_the_embedder() {
        let server = server();
        let (session, _) = initialize(&server);
        let body = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                          "params": {"name": "add_note", "arguments": {"text": "hi"}}})
        .to_string();

        // success
        let Handled::ToolCall(call) = server.handle(&post(&body, Some(&session))) else {
            panic!("expected tool call");
        };
        assert_eq!(call.name, "add_note");
        assert_eq!(call.arguments, json!({"text": "hi"}));
        let msg = sse_json(&call.succeed("\"note-1\""));
        assert_eq!(msg["id"], 2);
        assert_eq!(msg["result"]["isError"], false);
        assert_eq!(msg["result"]["content"][0]["text"], "\"note-1\"");

        // tool-level failure → isError result, NOT a JSON-RPC error
        let Handled::ToolCall(call) = server.handle(&post(&body, Some(&session))) else {
            panic!("expected tool call");
        };
        let msg = sse_json(&call.fail("no note with id nope"));
        assert_eq!(msg["result"]["isError"], true);
        assert!(msg.get("error").is_none());

        // unknown tool → -32602
        let Handled::ToolCall(call) = server.handle(&post(&body, Some(&session))) else {
            panic!("expected tool call");
        };
        let msg = sse_json(&call.unknown_tool("unknown action: nope"));
        assert_eq!(msg["error"]["code"], -32602);

        // internal fault → -32603
        let Handled::ToolCall(call) = server.handle(&post(&body, Some(&session))) else {
            panic!("expected tool call");
        };
        let msg = sse_json(&call.internal_error("boom"));
        assert_eq!(msg["error"]["code"], -32603);

        // missing tool name is rejected by the machine itself
        let bad =
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {}}).to_string();
        let msg = sse_json(&response(server.handle(&post(&bad, Some(&session)))));
        assert_eq!(msg["error"]["code"], -32602);
    }

    #[test]
    fn malformed_json_rpc_is_rejected() {
        let server = server();
        let (session, _) = initialize(&server);

        // not JSON at all
        let resp = response(server.handle(&post("{not json", Some(&session))));
        assert_eq!(resp.status, 415);

        // JSON but not an object
        let resp = response(server.handle(&post("[1,2,3]", Some(&session))));
        assert_eq!(resp.status, 415);

        // an object that is neither request, notification, nor response
        let resp = response(server.handle(&post(r#"{"foo": 1}"#, Some(&session))));
        assert_eq!(resp.status, 415);

        // unknown request method → JSON-RPC -32601, transport still 200
        let body = json!({"jsonrpc": "2.0", "id": 9, "method": "wat/wat"}).to_string();
        let resp = response(server.handle(&post(&body, Some(&session))));
        assert_eq!(resp.status, 200);
        assert_eq!(sse_json(&resp)["error"]["code"], -32601);
    }

    #[test]
    fn accept_header_negotiation() {
        let server = server();
        let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                          "params": {"protocolVersion": "2025-06-18"}})
        .to_string();

        // both media types must be named explicitly — `*/*` is not enough
        // (matches rmcp's golden behavior; real MCP clients send both)
        for accept in [
            None,
            Some("application/json"),
            Some("text/event-stream"),
            Some("*/*"),
        ] {
            let req = Request {
                accept,
                ..post(&init, None)
            };
            assert_eq!(
                response(server.handle(&req)).status,
                406,
                "accept {accept:?}"
            );
        }

        // wrong content type
        let req = Request {
            content_type: Some("text/plain"),
            ..post(&init, None)
        };
        assert_eq!(response(server.handle(&req)).status, 415);

        // GET needs text/event-stream
        let (session, _) = initialize(&server);
        let get = |accept: Option<&'static str>| Request {
            method: "GET",
            accept,
            content_type: None,
            session_id: Some(&session),
            body: b"",
        };
        assert_eq!(response(server.handle(&get(None))).status, 406);
        assert_eq!(
            response(server.handle(&get(Some("application/json")))).status,
            406
        );
        let resp = response(server.handle(&get(Some("text/event-stream"))));
        assert_eq!(resp.status, 200);
        assert!(matches!(resp.body, Body::SseStream(_)));
    }

    #[test]
    fn get_stream_requires_a_session() {
        let server = server();
        let get = Request {
            method: "GET",
            accept: Some("text/event-stream"),
            content_type: None,
            session_id: None,
            body: b"",
        };
        assert_eq!(response(server.handle(&get)).status, 400);
        let get = Request {
            session_id: Some("bogus"),
            ..get
        };
        assert_eq!(response(server.handle(&get)).status, 404);
    }

    #[test]
    fn unsupported_http_method() {
        let server = server();
        let req = Request {
            method: "PUT",
            accept: None,
            content_type: None,
            session_id: None,
            body: b"",
        };
        assert_eq!(response(server.handle(&req)).status, 405);
    }
}
