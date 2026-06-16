//! A minimal streamable-HTTP MCP client for the agent tool-loop (Tools/MCP T2),
//! over the `tangram::http` facade (so it works the same way native and inside
//! the wasm component). It mirrors the browser reference client in
//! `apps/tangram/ui/src/mcpClient.ts` EXACTLY at the wire level so the two
//! agree on the handshake:
//!
//!   POST <base>/<server>/mcp  with  Accept: application/json, text/event-stream
//!   - `initialize` returns the session in the `Mcp-Session-Id` RESPONSE header;
//!     that id is REQUIRED on every subsequent request.
//!   - Response bodies are an SSE stream (one or more `data: <json>` lines); the
//!     JSON-RPC response is the `data:` payload carrying `result`/`error`
//!     (preferring the one whose `id` matches the request). A plain JSON body is
//!     also handled defensively.
//!   - protocolVersion is "2025-06-18".
//!   - tools/call result: { content: [{ type: "text", text }], isError }.
//!
//! The egress to `<base>/<server>/mcp` is itself host-enforced: the tangram
//! app's `allow_hosts` + call-level `[[calls]]` grant decides which MCP
//! endpoints the component may reach at all (an undeclared endpoint is denied at
//! the `http-fetch` boundary, `enforcement = "enforce"`). The PER-AGENT approved
//! subset is enforced one level up, in `run_definition` (it only constructs a
//! client for a server the agent's grant approves). See `lib.rs`.

use serde_json::{Value, json};

/// The MCP protocol version we advertise (matches the browser client + the
/// server's advertised version). Used by the live [`McpClient`] handshake
/// (gated out of the test build, which exercises only the pure parsers).
#[cfg(not(test))]
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// One MCP tool as returned by `tools/list`: a name, optional description, and
/// the JSON-Schema `inputSchema` (which maps straight onto an OpenAI
/// function-tool's `parameters`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    /// The MCP `inputSchema` (a JSON Schema object), or `None` for no args.
    pub input_schema: Option<Value>,
}

/// The flattened result of a `tools/call`: the text content joined and an
/// error flag, ready to feed back as a `tool` message.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallResult {
    pub text: String,
    pub is_error: bool,
}

/// Parse a streamable-HTTP MCP response body into the JSON-RPC response message.
/// The body is either a single JSON object or an SSE stream of `data:` lines;
/// we return the message carrying `result`/`error`, preferring the one whose
/// `id` matches `want_id`. Mirrors `parseMcpBody` in `mcpClient.ts`. Pure (no
/// I/O), so it is unit-tested without a live server.
pub fn parse_mcp_body(body: &str, content_type: &str, want_id: i64) -> Result<Value, String> {
    let looks_sse = content_type.contains("text/event-stream")
        || body
            .lines()
            .any(|l| l.trim_start().starts_with("data:") || l.trim_start().starts_with("event:"));

    if !looks_sse {
        return serde_json::from_str(body)
            .map_err(|e| format!("MCP response body is not JSON: {e}"));
    }

    // Collect every `data:` JSON payload, then pick the response message.
    let mut messages: Vec<Value> = Vec::new();
    for raw_line in body.lines() {
        let line = raw_line.trim_start();
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            messages.push(value);
        }
        // Non-JSON keepalive/comment frames are ignored.
    }
    if messages.is_empty() {
        return Err("MCP SSE response had no JSON data frame".to_string());
    }
    // Prefer the frame whose id matches the request AND carries result/error;
    // else the first carrying result/error; else the last.
    let has_payload = |m: &Value| m.get("result").is_some() || m.get("error").is_some();
    let id_matches = |m: &Value| m.get("id").and_then(Value::as_i64) == Some(want_id);
    if let Some(m) = messages.iter().find(|m| id_matches(m) && has_payload(m)) {
        return Ok(m.clone());
    }
    if let Some(m) = messages.iter().find(|m| has_payload(m)) {
        return Ok(m.clone());
    }
    Ok(messages
        .last()
        .cloned()
        .expect("messages is non-empty (checked above)"))
}

/// Extract the `tools` array from a `tools/list` result into [`McpTool`]s.
/// Pure; unit-tested.
pub fn parse_tools(result: &Value) -> Vec<McpTool> {
    result
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(Value::as_str)?.to_string();
                    let description = t
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let input_schema = t.get("inputSchema").filter(|s| s.is_object()).cloned();
                    Some(McpTool {
                        name,
                        description,
                        input_schema,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Flatten a `tools/call` result value into [`McpCallResult`]: join the text
/// content blocks (non-text blocks are JSON-stringified), prefix `[tool error]`
/// on an error. Mirrors `renderToolResult` in `llmChat.ts`. Pure; unit-tested.
pub fn render_call_result(result: &Value) -> McpCallResult {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .map(|c| {
                    if c.get("type").and_then(Value::as_str) == Some("text") {
                        c.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let text = text.trim().to_string();
    let body = if text.is_empty() {
        "(no content)".to_string()
    } else {
        text
    };
    McpCallResult {
        text: if is_error {
            format!("[tool error] {body}")
        } else {
            body
        },
        is_error,
    }
}

/// Convert MCP tools → OpenAI/DeepSeek function-tool schemas. The MCP
/// `inputSchema` IS a JSON Schema object, so it maps straight onto
/// `function.parameters`; a tool with no schema gets an empty-object schema.
/// When `namespaced` is true the tool's wire name is prefixed `<server>__`
/// (used when more than one server's tools are offered to one model call, so
/// the loop can route a returned tool name back to the right server). Pure;
/// unit-tested.
pub fn tools_to_openai(server: &str, tools: &[McpTool], namespaced: bool) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let name = if namespaced {
                format!("{server}__{}", t.name)
            } else {
                t.name.clone()
            };
            let parameters = t
                .input_schema
                .clone()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            let mut function = serde_json::Map::new();
            function.insert("name".into(), json!(name));
            if let Some(desc) = &t.description {
                function.insert("description".into(), json!(desc));
            }
            function.insert("parameters".into(), parameters);
            json!({ "type": "function", "function": Value::Object(function) })
        })
        .collect()
}

/// Split a (possibly namespaced) tool name back into `(server, tool)`. The
/// inverse of the `<server>__<tool>` prefixing in [`tools_to_openai`]. When the
/// name has no `__` separator (single-server, un-namespaced) it belongs to
/// `default_server`. Pure; unit-tested.
pub fn split_tool_name<'a>(name: &'a str, default_server: &'a str) -> (&'a str, &'a str) {
    match name.split_once("__") {
        Some((server, tool)) => (server, tool),
        None => (default_server, name),
    }
}

/// A streamable-HTTP MCP client for one server endpoint (`<base>/<server>/mcp`).
/// Holds the captured session id and the JSON-RPC id counter. The actual
/// network I/O goes through the `tangram::http` facade so the SAME code runs
/// native and in the wasm component (where it becomes the host's enforced
/// `http-fetch`).
#[cfg(not(test))]
pub struct McpClient {
    endpoint: String,
    session_id: Option<String>,
    next_id: i64,
    initialized: bool,
}

#[cfg(not(test))]
impl McpClient {
    /// A client for `server`'s MCP endpoint under `base` (e.g.
    /// `http://127.0.0.1:8080`). The endpoint is `<base>/<server>/mcp`.
    pub fn new(base: &str, server: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            endpoint: format!("{base}/{server}/mcp"),
            session_id: None,
            next_id: 1,
            initialized: false,
        }
    }

    /// One JSON-RPC round-trip: POST the request, capture/refresh the session
    /// id from the response header, parse the SSE/JSON body, and return the
    /// `result` (or an error on a JSON-RPC `error`).
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        use tangram::http;

        let id = self.next_id;
        self.next_id += 1;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let mut req = http::Request::post(self.endpoint.clone())
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(sid) = &self.session_id {
            req = req.header("mcp-session-id", sid);
        }

        let resp = http::fetch(req).await.map_err(|e| e.to_string())?;

        // Capture the session id from the initialize response header (case-
        // insensitive; the host lowercases header names).
        if let Some((_, value)) = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
        {
            self.session_id = Some(value.clone());
        }

        let content_type = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        let text = String::from_utf8_lossy(&resp.body);
        if !resp.is_success() && text.is_empty() {
            return Err(format!("MCP {method} failed: HTTP {}", resp.status));
        }
        let msg = parse_mcp_body(&text, content_type, id)?;
        if let Some(err) = msg.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            return Err(format!("MCP {method} error {code}: {message}"));
        }
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    /// JSON-RPC `initialize`; captures the session id. Idempotent.
    pub async fn initialize(&mut self) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }
        self.rpc(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "tangram-agent", "version": "0.1.0" },
            }),
        )
        .await?;
        self.initialized = true;
        Ok(())
    }

    /// `tools/list` → the server's tools.
    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        self.initialize().await?;
        let result = self.rpc("tools/list", json!({})).await?;
        Ok(parse_tools(&result))
    }

    /// `tools/call` → the flattened result content.
    pub async fn call_tool(&mut self, name: &str, args: Value) -> Result<McpCallResult, String> {
        self.initialize().await?;
        let result = self
            .rpc("tools/call", json!({ "name": name, "arguments": args }))
            .await?;
        Ok(render_call_result(&result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mcp_body_handles_plain_json() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let msg = parse_mcp_body(body, "application/json", 1).unwrap();
        assert_eq!(msg["id"], 1);
        assert!(msg["result"]["tools"].is_array());
    }

    #[test]
    fn parse_mcp_body_picks_matching_sse_data_frame() {
        // An SSE stream with a keepalive comment, a non-matching ping, and the
        // real response — we must pick the response whose id matches.
        let body = ": keepalive\n\
                    event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n\
                    \n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\
                    \n";
        let msg = parse_mcp_body(body, "text/event-stream", 7).unwrap();
        assert_eq!(msg["id"], 7);
        assert_eq!(msg["result"]["ok"], true);
    }

    #[test]
    fn parse_mcp_body_sniffs_sse_without_header() {
        // No content-type header survived, but the body is SSE-framed.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":42}\n\n";
        let msg = parse_mcp_body(body, "", 1).unwrap();
        assert_eq!(msg["result"], 42);
    }

    #[test]
    fn parse_mcp_body_errors_on_empty_sse() {
        let body = ": only a comment\n\n";
        assert!(parse_mcp_body(body, "text/event-stream", 1).is_err());
    }

    #[test]
    fn parse_tools_reads_name_description_schema() {
        let result = json!({
            "tools": [
                { "name": "log_meal", "description": "Log a meal",
                  "inputSchema": { "type": "object", "properties": { "desc": { "type": "string" } } } },
                { "name": "noargs" },
            ]
        });
        let tools = parse_tools(&result);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "log_meal");
        assert_eq!(tools[0].description.as_deref(), Some("Log a meal"));
        assert!(tools[0].input_schema.is_some());
        assert_eq!(tools[1].name, "noargs");
        assert!(tools[1].input_schema.is_none());
    }

    #[test]
    fn render_call_result_joins_text_and_flags_errors() {
        let ok = render_call_result(&json!({
            "content": [{ "type": "text", "text": "logged 200 kcal" }],
            "isError": false
        }));
        assert_eq!(ok.text, "logged 200 kcal");
        assert!(!ok.is_error);

        let err = render_call_result(&json!({
            "content": [{ "type": "text", "text": "no such food" }],
            "isError": true
        }));
        assert!(err.is_error);
        assert!(err.text.starts_with("[tool error] no such food"));

        // Empty content → "(no content)".
        let empty = render_call_result(&json!({ "content": [] }));
        assert_eq!(empty.text, "(no content)");
    }

    #[test]
    fn tools_to_openai_maps_schema_and_namespaces() {
        let tools = vec![McpTool {
            name: "log_meal".into(),
            description: Some("Log a meal".into()),
            input_schema: Some(json!({ "type": "object", "properties": {} })),
        }];
        // Un-namespaced (single server).
        let plain = tools_to_openai("nutrition", &tools, false);
        assert_eq!(plain[0]["function"]["name"], "log_meal");
        assert_eq!(plain[0]["type"], "function");
        assert_eq!(plain[0]["function"]["description"], "Log a meal");
        assert!(plain[0]["function"]["parameters"].is_object());
        // Namespaced (multi-server).
        let ns = tools_to_openai("nutrition", &tools, true);
        assert_eq!(ns[0]["function"]["name"], "nutrition__log_meal");
    }

    #[test]
    fn tools_to_openai_defaults_empty_schema() {
        let tools = vec![McpTool {
            name: "noargs".into(),
            description: None,
            input_schema: None,
        }];
        let out = tools_to_openai("notes", &tools, false);
        assert_eq!(out[0]["function"]["parameters"]["type"], "object");
        assert!(out[0]["function"].get("description").is_none());
    }

    #[test]
    fn split_tool_name_roundtrips() {
        assert_eq!(
            split_tool_name("nutrition__log_meal", "fallback"),
            ("nutrition", "log_meal")
        );
        // No separator → the default server, name unchanged.
        assert_eq!(
            split_tool_name("log_meal", "nutrition"),
            ("nutrition", "log_meal")
        );
    }
}
