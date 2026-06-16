//! Shared test scaffolding: a `Ctx<Feedback>` over an in-memory store (the same
//! doc-in/doc-out store the WASM guest uses), plus a tiny recorded-GitHub
//! fixture HTTP server so `create_issue` runs in CI with NO live GitHub call
//! and never files a real issue (the guided-learning fixture-server precedent).

#![allow(dead_code)]

use std::sync::Arc;

use feedback::Feedback;
use tangram_core::{Ctx, Store, genesis_bytes};

/// A fresh `Ctx` over a genesis document, in memory (no disk, no sync).
pub fn fresh_ctx() -> Ctx<Feedback> {
    let bytes = genesis_bytes::<Feedback>().expect("genesis bytes");
    let store = Arc::new(Store::<Feedback>::in_memory(&bytes).expect("in-memory store"));
    Ctx::new(store)
}

/// Run an action by name with JSON args; panics on dispatch error.
pub async fn act(ctx: &Ctx<Feedback>, name: &str, args: serde_json::Value) -> serde_json::Value {
    ctx.apply(name, args)
        .await
        .unwrap_or_else(|e| panic!("action {name} failed: {e}"))
}

/// Run an action expecting it to fail; returns the error string.
pub async fn act_err(ctx: &Ctx<Feedback>, name: &str, args: serde_json::Value) -> String {
    match ctx.apply(name, args).await {
        Ok(v) => panic!("action {name} unexpectedly succeeded: {v}"),
        Err(e) => e.to_string(),
    }
}

/// Serialize tests that mutate the process-global `FEEDBACK_GITHUB_API` /
/// `GH_TOKEN` / `FEEDBACK_REPO` env vars. Hold the guard for the test's
/// duration. An async (tokio) mutex, so it can be held across awaits without
/// the `await_holding_lock` lint.
pub async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// One request the fixture server captured: method, path, and parsed JSON body.
#[derive(Clone, Debug)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: serde_json::Value,
}

/// A local fixture GitHub REST server. It answers a `PUT
/// /repos/.../contents/...` with a Contents-API-shaped body carrying a
/// `content.download_url` (the raw URL the app embeds), and a `POST
/// /repos/.../issues` with an issue-shaped body (`number`/`html_url`).
///
/// Every request (method, path, auth header, parsed JSON body) is recorded for
/// assertions. Returns its base URL to set as `FEEDBACK_GITHUB_API`.
pub struct GithubFixture {
    pub url: String,
    requests: Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl GithubFixture {
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub async fn start() -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://{addr}");
        let raw_base = url.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                // Read the whole request (head + body). The fixture bodies are
                // small, so one read with a retry loop on the content-length is
                // sufficient for these tests.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let content_len = head
                            .lines()
                            .find_map(|l| {
                                let l = l.to_ascii_lowercase();
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if body.len() >= content_len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let (head, body_str) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let authorization = head.lines().find_map(|l| {
                    let low = l.to_ascii_lowercase();
                    low.strip_prefix("authorization:")
                        .map(|v| v.trim().to_string())
                });
                let body: serde_json::Value =
                    serde_json::from_str(body_str).unwrap_or(serde_json::Value::Null);

                let is_contents = method == "PUT" && path.contains("/contents/");
                captured.lock().unwrap().push(CapturedRequest {
                    method: method.clone(),
                    path: path.clone(),
                    authorization,
                    body,
                });

                let (status, payload) = if is_contents {
                    (
                        "201 Created",
                        serde_json::json!({
                            "content": {
                                // A download_url on the feedback-assets ref, the
                                // shape GitHub returns for a branch-pinned PUT.
                                "download_url": format!(
                                    "{raw_base}/raw/feedback-assets/assets/shot.png"
                                )
                            }
                        })
                        .to_string(),
                    )
                } else {
                    (
                        "201 Created",
                        serde_json::json!({
                            "number": 4242,
                            "html_url": format!("{raw_base}/repos/owner/repo/issues/4242")
                        })
                        .to_string(),
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
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
