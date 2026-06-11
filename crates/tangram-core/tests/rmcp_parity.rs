//! Replays the wire flows captured from rmcp 1.7's streamable-HTTP server
//! (`fixtures/rmcp-golden.json`, recorded from a live Tangram notes app —
//! the exact flows Claude Code runs) through the sans-io [`McpServer`] and
//! asserts semantic parity: same status codes, same session issuance, same
//! JSON-RPC messages. Byte-format differences (SSE event ids, plain-text
//! error wording, server identity) are normalized away — they are
//! transport dressing, not protocol semantics.

use std::collections::HashMap;

use serde_json::Value;
use tangram_core::mcp::{Body, Handled, McpServer, Request, ToolDef};

fn fixture() -> Value {
    let raw = include_str!("fixtures/rmcp-golden.json");
    serde_json::from_str(raw).expect("fixture parses")
}

/// The JSON-RPC message inside an SSE body (the last non-empty data line —
/// rmcp prefixes a priming event with empty data).
fn sse_json(body: &str) -> Option<Value> {
    let data = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:").map(str::trim))
        .rfind(|d| !d.is_empty())?;
    serde_json::from_str(data).ok()
}

/// Normalize the parts that legitimately differ between servers: identity
/// (`serverInfo`) and generated ids inside successful tool results.
fn normalize(mut msg: Value) -> Value {
    if let Some(info) = msg.pointer_mut("/result/serverInfo") {
        *info = Value::String("<serverInfo>".into());
    }
    msg
}

#[test]
fn rmcp_golden_flows_replay_semantically() {
    let fixture = fixture();
    let exchanges = fixture["exchanges"].as_array().expect("exchanges");

    // The server under test exposes the same tools the captured server did:
    // lift them straight out of the captured tools/list response.
    let captured_tools = exchanges
        .iter()
        .find(|e| e["name"] == "tools/list")
        .and_then(|e| sse_json(e["response"]["body"].as_str().unwrap()))
        .expect("captured tools/list");
    let tools: Vec<ToolDef> = captured_tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| ToolDef {
            name: t["name"].as_str().unwrap().to_string(),
            description: t["description"].as_str().unwrap().to_string(),
            input_schema: t["inputSchema"].clone(),
        })
        .collect();
    assert!(!tools.is_empty());

    let server = McpServer::new(
        "notes",
        "0.1.0",
        Some(
            "A shared, replicated notes list. Notes you add are visible to humans \
         in the web UI and on every synced device."
                .to_string(),
        ),
        tools,
    );

    // rmcp session ids from the capture → session ids our server issued.
    let mut sessions: HashMap<String, String> = HashMap::new();

    for exchange in exchanges {
        let name = exchange["name"].as_str().unwrap();
        let req = &exchange["request"];
        let expected = &exchange["response"];

        let headers: HashMap<String, String> = req["headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap().to_string()))
            .collect();
        // Translate the captured session id into the one our server issued
        // for the corresponding initialize (unknown ids pass through).
        let session_id = headers
            .get("mcp-session-id")
            .map(|s| sessions.get(s).cloned().unwrap_or_else(|| s.clone()));
        let body = req["body"].as_str().unwrap_or_default();

        let request = Request {
            method: req["method"].as_str().unwrap(),
            accept: headers.get("accept").map(String::as_str),
            content_type: headers.get("content-type").map(String::as_str),
            session_id: session_id.as_deref(),
            body: body.as_bytes(),
        };

        let response = match server.handle(&request) {
            Handled::Response(r) => r,
            // Resolve tool calls with the same OUTCOME the captured app
            // produced; the envelope the machine builds around it is what
            // is under test.
            Handled::ToolCall(call) => {
                let captured = sse_json(expected["body"].as_str().unwrap()).unwrap();
                if let Some(err) = captured.get("error") {
                    call.unknown_tool(err["message"].as_str().unwrap())
                } else {
                    let result = &captured["result"];
                    let text = result["content"][0]["text"].as_str().unwrap();
                    if result["isError"] == true {
                        call.fail(text)
                    } else {
                        call.succeed(text)
                    }
                }
            }
        };

        // 1. status parity
        assert_eq!(
            response.status,
            expected["status"].as_u64().unwrap() as u16,
            "status mismatch on {name:?}"
        );

        // 2. session issuance parity
        match (expected["session_id"].as_str(), &response.session_id) {
            (Some(theirs), Some(ours)) => {
                sessions.insert(theirs.to_string(), ours.clone());
            }
            (None, None) => {}
            (theirs, ours) => {
                panic!("session header mismatch on {name:?}: rmcp {theirs:?}, ours {ours:?}")
            }
        }

        // 3. SSE / JSON-RPC parity
        let expected_msg = expected["body"].as_str().and_then(sse_json);
        match (&response.body, expected_msg) {
            (Body::SseMessage(ours), Some(theirs)) => {
                let ours = sse_json(ours).expect("our SSE carries JSON");
                assert_eq!(
                    normalize(ours),
                    normalize(theirs),
                    "JSON-RPC message mismatch on {name:?}"
                );
                assert_eq!(
                    expected["content_type"].as_str(),
                    Some("text/event-stream"),
                    "rmcp answered {name:?} over SSE"
                );
            }
            (Body::SseStream(_), None) => {
                // GET listening stream: rmcp sends a priming event, we send
                // nothing — both are content-free SSE streams.
                assert_eq!(expected["content_type"].as_str(), Some("text/event-stream"));
            }
            (Body::Empty, None) | (Body::Text(_), None) => {
                // plain-text transport errors / 202s: status (asserted
                // above) is the contract; wording is not.
                assert_ne!(
                    expected["content_type"].as_str(),
                    Some("text/event-stream"),
                    "{name:?}: rmcp sent SSE where we sent none"
                );
            }
            (ours, theirs) => {
                panic!("body shape mismatch on {name:?}: ours {ours:?}, rmcp {theirs:?}")
            }
        }
    }
}
