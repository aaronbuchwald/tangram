//! Shared test scaffolding: build a `Ctx<GuidedLearning>` over an in-memory
//! store (the same doc-in/doc-out store the WASM guest uses), and a tiny
//! recorded-Anthropic fixture HTTP server so the LLM-backed actions run in CI
//! with no live key (the nutrition / rmcp-golden fixture precedent).

#![allow(dead_code)]

use std::sync::Arc;

use guided_learning::GuidedLearning;
use tangram_core::{Ctx, Store, genesis_bytes};

/// A fresh `Ctx` over a genesis document, in memory (no disk, no sync).
pub fn fresh_ctx() -> Ctx<GuidedLearning> {
    let bytes = genesis_bytes::<GuidedLearning>().expect("genesis bytes");
    let store = Store::<GuidedLearning>::in_memory(&bytes).expect("in-memory store");
    Ctx::new(Arc::new(store))
}

/// Run an action by name with JSON args; panics on dispatch error so tests
/// read top-down.
pub async fn act(
    ctx: &Ctx<GuidedLearning>,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    ctx.apply(name, args)
        .await
        .unwrap_or_else(|e| panic!("action {name} failed: {e}"))
}

/// Run an action expecting it to fail; returns the error string.
pub async fn act_err(ctx: &Ctx<GuidedLearning>, name: &str, args: serde_json::Value) -> String {
    match ctx.apply(name, args).await {
        Ok(v) => panic!("action {name} unexpectedly succeeded: {v}"),
        Err(e) => e.to_string(),
    }
}

// ── recorded-Anthropic fixture server ──────────────────────────────────────

/// A canned Messages-API response wrapping a structured-output JSON string in
/// the first text content block (exactly the shape `tutor::call` parses).
pub fn anthropic_response(structured_json: serde_json::Value) -> String {
    let text = structured_json.to_string();
    serde_json::json!({
        "id": "msg_fixture",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-4-8",
        "content": [{ "type": "text", "text": text }],
        "stop_reason": "end_turn"
    })
    .to_string()
}

/// A local one-shot-per-connection HTTP server that replays `body` (a full
/// Messages-API JSON response) for every POST. Returns the base URL to set as
/// `GUIDED_LEARNING_LLM_URL`. The server runs on a background task for the
/// lifetime of the returned guard.
pub struct FixtureServer {
    pub url: String,
    _handle: tokio::task::JoinHandle<()>,
}

impl FixtureServer {
    /// Serve a fixed response body for every request.
    pub async fn fixed(body: String) -> Self {
        Self::with_sequence(vec![body]).await
    }

    /// Serve a sequence of response bodies in order; the last one repeats once
    /// the sequence is exhausted (so a test can script generate-then-evaluate).
    pub async fn with_sequence(bodies: Vec<String>) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://{addr}/v1/messages");
        let handle = tokio::spawn(async move {
            let mut idx = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                // Drain the request (we don't inspect it — the fixture is
                // request-agnostic). Read until the headers end; the body is
                // irrelevant to replay.
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = bodies
                    .get(idx)
                    .or_else(|| bodies.last())
                    .cloned()
                    .unwrap_or_default();
                idx += 1;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        Self {
            url,
            _handle: handle,
        }
    }
}
