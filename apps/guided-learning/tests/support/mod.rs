//! Shared test scaffolding: build a `Ctx<GuidedLearning>` over an in-memory
//! store (the same doc-in/doc-out store the WASM guest uses), and a tiny
//! recorded-DeepSeek fixture HTTP server so the LLM-backed actions run in CI
//! with no live key (the nutrition / rmcp-golden fixture precedent).

#![allow(dead_code)]

use std::sync::Arc;

use guided_learning::GuidedLearning;
use tangram_core::{Ctx, Store, genesis_bytes};

/// A fresh `Ctx` over a genesis document, in memory (no disk, no sync).
pub fn fresh_ctx() -> Ctx<GuidedLearning> {
    ctx_from_bytes(&genesis_bytes::<GuidedLearning>().expect("genesis bytes"))
}

/// A `Ctx` over the given document bytes (e.g. a replica's save), plus the
/// store handle so the caller can `save()` to merge with a peer.
pub fn store_and_ctx(bytes: &[u8]) -> (Arc<Store<GuidedLearning>>, Ctx<GuidedLearning>) {
    let store = Arc::new(Store::<GuidedLearning>::in_memory(bytes).expect("in-memory store"));
    let ctx = Ctx::new(store.clone());
    (store, ctx)
}

/// A `Ctx` over the given document bytes.
pub fn ctx_from_bytes(bytes: &[u8]) -> Ctx<GuidedLearning> {
    store_and_ctx(bytes).1
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

/// Serialize tests that mutate the process-global `GUIDED_LEARNING_LLM_URL`
/// env var (set per test to point at that test's fixture server). Hold the
/// guard for the test's duration. An async (tokio) mutex, so it can be held
/// across the test's awaits without the `await_holding_lock` lint.
pub async fn llm_env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

// ── recorded-DeepSeek fixture server ────────────────────────────────────────

/// A canned chat-completions response wrapping a structured-output JSON string
/// in `choices[0].message.content` (exactly the shape `tutor::call` parses —
/// the tutor runs DeepSeek in JSON mode, so the content IS the JSON object).
pub fn deepseek_response(structured_json: serde_json::Value) -> String {
    let content = structured_json.to_string();
    serde_json::json!({
        "id": "chatcmpl_fixture",
        "object": "chat.completion",
        "model": "deepseek-chat",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

/// A local one-shot-per-connection HTTP server that replays `body` (a full
/// chat-completions JSON response) for every POST. Returns the base URL to set
/// as `GUIDED_LEARNING_LLM_URL`. The server runs on a background task for the
/// lifetime of the returned guard.
pub struct FixtureServer {
    pub url: String,
    /// The request lines (e.g. "POST /v1/chat/completions HTTP/1.1") the server
    /// saw, so a containment test can assert the tutor only ever hits the one
    /// declared call.
    requests: Arc<std::sync::Mutex<Vec<String>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl FixtureServer {
    /// Serve a fixed response body for every request.
    pub async fn fixed(body: String) -> Self {
        Self::with_sequence(vec![body]).await
    }

    /// The first line of each request the server received.
    pub fn request_lines(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Serve a sequence of response bodies in order; the last one repeats once
    /// the sequence is exhausted (so a test can script generate-then-evaluate).
    pub async fn with_sequence(bodies: Vec<String>) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://{addr}/v1/chat/completions");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let handle = tokio::spawn(async move {
            let mut idx = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                // Read the request head; capture its first line for assertions.
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if let Some(line) = String::from_utf8_lossy(&buf[..n]).lines().next() {
                    captured.lock().unwrap().push(line.to_string());
                }
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
            requests,
            _handle: handle,
        }
    }
}
