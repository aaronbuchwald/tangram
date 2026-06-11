//! End-to-end MCP parity suite: runs a real app (store + full router) and
//! exercises the streamable-HTTP MCP transport exactly the way real clients
//! (Claude Code / Claude Desktop) drive it.
//!
//! These tests were first run against the rmcp-served `/mcp` to capture its
//! behavior as golden, and must keep passing against the portable
//! `tangram-core` MCP layer that replaced it: byte-format differences (SSE
//! event ids, header casing) are fine, semantic/JSON-RPC differences are not.
//! Raw rmcp wire captures live in
//! `crates/tangram-core/tests/fixtures/rmcp-golden.json`.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Once;

use serde_json::{Value, json};
use tangram::prelude::*;

#[model]
#[derive(Default)]
struct Counter {
    value: i64,
}

#[actions]
impl Counter {
    /// Add `amount` to the counter and return the new value.
    pub fn increment(&mut self, amount: i64) -> i64 {
        self.value += amount;
        self.value
    }

    /// Fails with a domain error when `ok` is false; otherwise returns the
    /// current value.
    pub fn checked(&self, ok: bool) -> Result<i64, String> {
        if ok {
            Ok(self.value)
        } else {
            Err("not ok: domain failure".to_string())
        }
    }
}

const ACCEPT_BOTH: &str = "application/json, text/event-stream";

/// Serve a fresh app instance (its own document) on an ephemeral port.
async fn serve(name: &str) -> String {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // One shared scratch data dir for the whole test binary; each test
        // uses a unique app name, so each gets its own document.
        let dir = std::env::temp_dir().join(format!("tangram-mcp-test-{}", std::process::id()));
        // Safety: called once before any test server (or other thread) starts.
        unsafe { std::env::set_var("TANGRAM_DATA_DIR", &dir) };
    });
    let router = App::<Counter>::new(name)
        .instructions("test instructions")
        .build()
        .expect("app builds");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, router).into_future());
    format!("http://{addr}")
}

/// Extract the JSON-RPC message from an SSE-framed response body (the last
/// non-empty `data:` line — rmcp also emits an empty priming event).
fn sse_json(body: &str) -> Value {
    let data = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:").map(str::trim))
        .rfind(|d| !d.is_empty())
        .unwrap_or_else(|| panic!("no data line in SSE body: {body:?}"));
    serde_json::from_str(data).unwrap_or_else(|e| panic!("bad JSON in SSE data ({e}): {data:?}"))
}

struct Mcp {
    base: String,
    client: reqwest::Client,
    session: Option<String>,
}

impl Mcp {
    fn new(base: &str) -> Self {
        Self {
            base: base.to_string(),
            client: reqwest::Client::new(),
            session: None,
        }
    }

    async fn post_raw(
        &self,
        body: &str,
        accept: Option<&str>,
        content_type: Option<&str>,
        session: Option<&str>,
    ) -> reqwest::Response {
        let mut req = self
            .client
            .post(format!("{}/mcp", self.base))
            .body(body.to_string());
        if let Some(a) = accept {
            req = req.header("Accept", a);
        }
        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }
        if let Some(s) = session {
            req = req.header("Mcp-Session-Id", s);
        }
        req.send().await.expect("request")
    }

    async fn post(&self, message: Value) -> reqwest::Response {
        self.post_raw(
            &message.to_string(),
            Some(ACCEPT_BOTH),
            Some("application/json"),
            self.session.as_deref(),
        )
        .await
    }

    /// Run the initialize request and store the issued session id.
    async fn initialize(&mut self, protocol_version: &str) -> Value {
        let resp = self
            .post(json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": protocol_version,
                    "capabilities": {},
                    "clientInfo": {"name": "claude-code", "version": "2.0.0"},
                },
            }))
            .await;
        assert_eq!(resp.status(), 200, "initialize must succeed");
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            ct.starts_with("text/event-stream"),
            "initialize answers over SSE, got {ct}"
        );
        let session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .expect("initialize must issue Mcp-Session-Id")
            .to_string();
        assert!(!session.is_empty());
        self.session = Some(session);
        sse_json(&resp.text().await.unwrap())
    }

    /// JSON-RPC request → JSON-RPC message parsed out of the SSE response.
    async fn request(&self, message: Value) -> Value {
        let resp = self.post(message).await;
        assert_eq!(resp.status(), 200);
        sse_json(&resp.text().await.unwrap())
    }

    async fn notify_initialized(&self) {
        let resp = self
            .post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert_eq!(resp.status(), 202, "notifications get 202 Accepted");
        assert!(resp.text().await.unwrap().is_empty());
    }

    async fn tools_list(&self) -> Value {
        let msg = self
            .request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await;
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["id"], 1);
        msg["result"]["tools"].clone()
    }

    async fn tools_call(&self, id: i64, name: &str, arguments: Value) -> Value {
        self.request(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }))
        .await
    }
}

// ── the flows real clients run ───────────────────────────────────────────────

#[tokio::test]
async fn initialize_handshake_and_session() {
    let base = serve("mcp-init").await;
    let mut mcp = Mcp::new(&base);

    let msg = mcp.initialize("2025-06-18").await;
    assert_eq!(msg["jsonrpc"], "2.0");
    assert_eq!(msg["id"], 0);
    let result = &msg["result"];
    assert_eq!(
        result["protocolVersion"], "2025-06-18",
        "server echoes a supported protocol version"
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "tools capability advertised: {result}"
    );
    assert!(result["serverInfo"]["name"].is_string());
    assert!(result["serverInfo"]["version"].is_string());
    assert_eq!(result["instructions"], "test instructions");

    mcp.notify_initialized().await;

    // ping keeps the session healthy
    let pong = mcp
        .request(json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}))
        .await;
    assert_eq!(pong["result"], json!({}));
}

#[tokio::test]
async fn protocol_version_negotiation() {
    let base = serve("mcp-proto").await;

    // a supported older version is echoed back
    let msg = Mcp::new(&base).initialize("2025-03-26").await;
    assert_eq!(msg["result"]["protocolVersion"], "2025-03-26");

    // an unknown version falls back to the server's latest
    let msg = Mcp::new(&base).initialize("9999-01-01").await;
    assert_eq!(msg["result"]["protocolVersion"], "2025-11-25");
}

#[tokio::test]
async fn tools_list_matches_api_actions() {
    let base = serve("mcp-parity").await;
    let mut mcp = Mcp::new(&base);
    mcp.initialize("2025-06-18").await;
    mcp.notify_initialized().await;

    let mut tools: Vec<Value> = mcp.tools_list().await.as_array().unwrap().clone();
    tools.sort_by_key(|t| t["name"].as_str().unwrap().to_string());

    let actions: Value = reqwest::get(format!("{base}/api/actions"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut actions: Vec<Value> = actions["actions"].as_array().unwrap().clone();
    actions.sort_by_key(|a| a["name"].as_str().unwrap().to_string());

    assert_eq!(tools.len(), actions.len());
    assert_eq!(tools.len(), 2);
    for (tool, action) in tools.iter().zip(&actions) {
        assert_eq!(tool["name"], action["name"]);
        assert_eq!(tool["description"], action["description"]);
        assert_eq!(
            tool["inputSchema"], action["input_schema"],
            "MCP tool schema and /api/actions schema must be identical"
        );
    }
}

#[tokio::test]
async fn tools_call_success_writes_through_to_the_doc() {
    let base = serve("mcp-call").await;
    let mut mcp = Mcp::new(&base);
    mcp.initialize("2025-06-18").await;
    mcp.notify_initialized().await;

    let msg = mcp.tools_call(2, "increment", json!({"amount": 5})).await;
    assert_eq!(msg["id"], 2);
    let result = &msg["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["type"], "text");
    assert_eq!(result["content"][0]["text"], "5");

    // the write landed in the same CRDT document the web surface serves
    let state: Value = reqwest::get(format!("{base}/api/state"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["value"], 5);
}

#[tokio::test]
async fn tools_call_tool_error_sets_is_error() {
    let base = serve("mcp-toolerr").await;
    let mut mcp = Mcp::new(&base);
    mcp.initialize("2025-06-18").await;

    let msg = mcp.tools_call(3, "checked", json!({"ok": false})).await;
    // domain failures are tool RESULTS (isError), not protocol errors
    assert!(
        msg["error"].is_null(),
        "tool failure is not a JSON-RPC error"
    );
    assert_eq!(msg["result"]["isError"], true);
    assert!(
        msg["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not ok: domain failure")
    );

    // bad arguments are also tool-level errors the agent can recover from
    let msg = mcp
        .tools_call(4, "increment", json!({"amount": "NaN"}))
        .await;
    assert_eq!(msg["result"]["isError"], true);
}

#[tokio::test]
async fn tools_call_unknown_tool_is_invalid_params() {
    let base = serve("mcp-unknown").await;
    let mut mcp = Mcp::new(&base);
    mcp.initialize("2025-06-18").await;

    let msg = mcp.tools_call(4, "nope_tool", json!({})).await;
    assert!(msg["result"].is_null());
    assert_eq!(msg["error"]["code"], -32602);
    assert!(
        msg["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nope_tool")
    );
}

#[tokio::test]
async fn second_session_is_independent() {
    let base = serve("mcp-twosessions").await;
    let mut first = Mcp::new(&base);
    first.initialize("2025-06-18").await;
    let mut second = Mcp::new(&base);
    second.initialize("2025-06-18").await;

    assert_ne!(
        first.session, second.session,
        "each session gets its own id"
    );
    // both sessions work concurrently
    second.notify_initialized().await;
    assert!(second.tools_list().await.is_array());
    assert!(first.tools_list().await.is_array());

    // writes via one session are visible to calls via the other
    second
        .tools_call(5, "increment", json!({"amount": 1}))
        .await;
    let msg = first.tools_call(6, "checked", json!({"ok": true})).await;
    assert_eq!(msg["result"]["content"][0]["text"], "1");
}

// ── session lifecycle edge cases ─────────────────────────────────────────────

#[tokio::test]
async fn session_edge_cases() {
    let base = serve("mcp-sessions").await;
    let mut mcp = Mcp::new(&base);

    // a request before initialize (no session header) is rejected
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    let resp = mcp
        .post_raw(&list, Some(ACCEPT_BOTH), Some("application/json"), None)
        .await;
    assert_eq!(resp.status(), 422, "request without a session id");

    // an unknown session id is 404 (client should re-initialize)
    let resp = mcp
        .post_raw(
            &list,
            Some(ACCEPT_BOTH),
            Some("application/json"),
            Some("not-a-real-session"),
        )
        .await;
    assert_eq!(resp.status(), 404, "unknown session id");

    // a valid session works until deleted
    mcp.initialize("2025-06-18").await;
    let session = mcp.session.clone().unwrap();
    mcp.tools_list().await;

    let resp = mcp
        .client
        .delete(format!("{base}/mcp"))
        .header("Mcp-Session-Id", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "DELETE ends the session");

    let resp = mcp
        .post_raw(
            &list,
            Some(ACCEPT_BOTH),
            Some("application/json"),
            Some(&session),
        )
        .await;
    assert_eq!(resp.status(), 404, "deleted session is gone");
}

// ── transport negotiation / malformed input ──────────────────────────────────

#[tokio::test]
async fn accept_and_content_type_negotiation() {
    let base = serve("mcp-negotiate").await;
    let mcp = Mcp::new(&base);
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "x", "version": "1"}},
    })
    .to_string();

    // POST requires accepting both application/json and text/event-stream
    let resp = mcp
        .post_raw(&init, None, Some("application/json"), None)
        .await;
    assert_eq!(resp.status(), 406, "missing Accept");
    let resp = mcp
        .post_raw(
            &init,
            Some("application/json"),
            Some("application/json"),
            None,
        )
        .await;
    assert_eq!(resp.status(), 406, "Accept without text/event-stream");

    // POST requires a JSON body
    let resp = mcp
        .post_raw(&init, Some(ACCEPT_BOTH), Some("text/plain"), None)
        .await;
    assert_eq!(resp.status(), 415, "wrong content type");

    // malformed JSON-RPC is rejected without crashing the server
    let resp = mcp
        .post_raw(
            "{not json",
            Some(ACCEPT_BOTH),
            Some("application/json"),
            None,
        )
        .await;
    assert_eq!(resp.status(), 415, "malformed JSON body");

    // and the server still works afterwards
    let mut mcp = Mcp::new(&base);
    mcp.initialize("2025-06-18").await;
    mcp.tools_list().await;
}

#[tokio::test]
async fn get_opens_a_session_stream() {
    let base = serve("mcp-getstream").await;
    let mut mcp = Mcp::new(&base);

    // GET without a session is rejected
    let resp = mcp
        .client
        .get(format!("{base}/mcp"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "GET without session id");

    mcp.initialize("2025-06-18").await;
    let session = mcp.session.clone().unwrap();

    // GET without accepting SSE is rejected
    let resp = mcp
        .client
        .get(format!("{base}/mcp"))
        .header("Mcp-Session-Id", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 406, "GET must accept text/event-stream");

    // a proper GET opens (and holds) the standalone SSE stream
    let resp = mcp
        .client
        .get(format!("{base}/mcp"))
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(ct.starts_with("text/event-stream"), "got {ct}");
    // the stream stays open: requests on the POST channel still work
    drop(resp);
    mcp.tools_list().await;
}

// ── the killer acceptance flow ───────────────────────────────────────────────

/// Replays the exact handshake `claude mcp add --transport http …` clients
/// perform (captured from rmcp; see fixtures), ending in a tools/call whose
/// write must be visible in the document.
#[tokio::test]
async fn claude_code_wire_flow() {
    let base = serve("mcp-claude").await;
    let client = reqwest::Client::new();

    // 1. initialize — exactly the JSON Claude Code sends
    let resp = client
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-code","version":"2.0.0"}}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let session = resp.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let init = sse_json(&resp.text().await.unwrap());
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    // 2. notifications/initialized
    let resp = client
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session)
        .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);

    // 3. tools/list (clients send the negotiated version header from here on)
    let resp = client
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session)
        .header("MCP-Protocol-Version", "2025-06-18")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let tools = sse_json(&resp.text().await.unwrap());
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"increment"), "tools: {names:?}");

    // 4. tools/call → end-to-end write through to the doc
    let resp = client
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session)
        .header("MCP-Protocol-Version", "2025-06-18")
        .body(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"increment","arguments":{"amount":42}}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let call = sse_json(&resp.text().await.unwrap());
    assert_eq!(call["result"]["isError"], false);
    assert_eq!(call["result"]["content"][0]["text"], "42");

    let state: Value = reqwest::get(format!("{base}/api/state"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["value"], 42);
}
