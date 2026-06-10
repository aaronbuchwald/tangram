//! Replication between instances using Automerge's sync protocol over
//! HTTP(+SSE) — the wire contract is specified in `docs/SYNC_PROTOCOL.md`.
//! Topology is symmetric: every instance serves `POST /sync` +
//! `GET /sync/events` and can also dial out to one remote (`TANGRAM_REMOTE`).
//! The server is not special — it's just another peer that happens to be
//! reachable. The same interface is served by the Cloudflare Durable-Object
//! relay in `cloud/cloudflare/`, so a replica cannot tell them apart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::StreamExt;
use tokio::sync::watch;

use crate::Model;
use crate::action::Actions;
use crate::store::Store;

/// The document surface the sync protocol runs over. The SDK's typed
/// [`Store`] implements it, and so does `tangram-host`'s untyped per-app
/// document — both sides of the protocol (server `handle_post` and client
/// [`run_remote`]) are generic over this seam, so one implementation serves
/// native apps and host-managed WASM components identically.
///
/// Contract: `receive_sync` persists the document itself when it changed;
/// `bump` wakes every subscriber (UIs, poke streams, peers) and is the
/// caller's job after receives so a batch of messages wakes them once.
pub trait DocHandle: Send + Sync + 'static {
    /// Next pending sync message for the peer represented by `state`.
    fn generate_sync(&self, state: &mut automerge::sync::State) -> Option<Vec<u8>>;
    /// Apply one sync message from a peer; returns true if the document
    /// changed (already persisted by the implementation).
    fn receive_sync(
        &self,
        state: &mut automerge::sync::State,
        bytes: &[u8],
    ) -> anyhow::Result<bool>;
    /// Wake subscribers after a change.
    fn bump(&self);
    /// Subscribe to the change signal.
    fn subscribe(&self) -> watch::Receiver<u64>;
}

impl<M: Model + Actions> DocHandle for Store<M> {
    fn generate_sync(&self, state: &mut automerge::sync::State) -> Option<Vec<u8>> {
        Store::generate_sync(self, state)
    }
    fn receive_sync(
        &self,
        state: &mut automerge::sync::State,
        bytes: &[u8],
    ) -> anyhow::Result<bool> {
        Store::receive_sync(self, state, bytes)
    }
    fn bump(&self) {
        Store::bump(self)
    }
    fn subscribe(&self) -> watch::Receiver<u64> {
        Store::subscribe(self)
    }
}

// ── server side ──────────────────────────────────────────────────────────────

/// Sessions idle longer than this are evicted. Losing a session is harmless:
/// the next POST starts from a fresh `automerge::sync::State` and the
/// protocol re-converges (it just costs an extra round trip or two).
const SESSION_IDLE: Duration = Duration::from_secs(5 * 60);

struct Session {
    state: automerge::sync::State,
    last_seen: Instant,
}

/// Per-peer sync states held in server memory, keyed by the client-generated
/// `X-Tangram-Session` id.
#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Session>>,
}

/// Handle one `POST /sync`: apply the client's message (if any), then return
/// every message we owe that peer, framed as `[u32 big-endian length][bytes]`.
pub fn handle_post<D: DocHandle + ?Sized>(
    store: &D,
    sessions: &Sessions,
    session_id: &str,
    body: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let mut map = sessions.map.lock().expect("sessions lock");
    map.retain(|_, s| s.last_seen.elapsed() < SESSION_IDLE);
    let session = map
        .entry(session_id.to_string())
        .or_insert_with(|| Session {
            state: automerge::sync::State::new(),
            last_seen: Instant::now(),
        });
    session.last_seen = Instant::now();

    if !body.is_empty() && store.receive_sync(&mut session.state, body)? {
        // Document changed: wake SSE pokes, UIs, and other peers.
        store.bump();
    }
    let mut response = Vec::new();
    while let Some(msg) = store.generate_sync(&mut session.state) {
        response.extend_from_slice(&u32::try_from(msg.len())?.to_be_bytes());
        response.extend_from_slice(&msg);
    }
    Ok(response)
}

// ── client side ──────────────────────────────────────────────────────────────

/// Dial out to a remote instance and keep syncing with it, reconnecting with
/// backoff. `url` is a sync base like `http://other-host:8080/sync` (legacy
/// `ws://`/`wss://` values are rewritten with a deprecation warning).
pub async fn run_remote<D: DocHandle>(url: String, store: Arc<D>) {
    let base = normalize_remote(&url);
    let client = reqwest::Client::new();
    loop {
        if let Err(e) = sync_with_remote(&client, &base, &store).await {
            tracing::warn!("sync with {base} failed: {e:#}; reconnecting");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Rewrite legacy WebSocket remote URLs (the pre-HTTP transport) so old
/// configs keep working.
fn normalize_remote(url: &str) -> String {
    let rewritten = if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else {
        return url.trim_end_matches('/').to_string();
    };
    tracing::warn!(
        "ws:// sync remotes are deprecated (sync runs over HTTP now): using {rewritten} for {url}"
    );
    rewritten.trim_end_matches('/').to_string()
}

/// One connection's lifetime: open the SSE poke stream, then run a sync round
/// on every poke or local change. Only returns on failure (transport error or
/// stream close); the caller reconnects with a fresh session — the server
/// then re-syncs us from a fresh `State`.
async fn sync_with_remote<D: DocHandle>(
    client: &reqwest::Client,
    base: &str,
    store: &Arc<D>,
) -> anyhow::Result<()> {
    let session = uuid::Uuid::new_v4().to_string();
    let mut state = automerge::sync::State::new();
    let mut version = store.subscribe();

    let response = client
        .get(format!("{base}/events"))
        .query(&[("session", &session)])
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("opening poke stream")?;
    tracing::info!("syncing with remote {base}");
    let mut pokes = response.bytes_stream();
    let mut sse_buf = String::new();

    loop {
        // Converge both directions, then sleep until there's work again. The
        // first iteration doubles as the initial sync (the server also pokes
        // on connect).
        sync_round(client, base, &session, &mut state, store).await?;
        loop {
            tokio::select! {
                chunk = pokes.next() => {
                    let chunk = chunk.context("poke stream closed")?.context("poke stream")?;
                    sse_buf.push_str(&String::from_utf8_lossy(&chunk));
                    if drain_pokes(&mut sse_buf) {
                        break;
                    }
                }
                changed = version.changed() => {
                    changed.context("store dropped")?;
                    break;
                }
            }
        }
    }
}

/// Minimal SSE parse: drop complete blank-line-terminated blocks from `buf`
/// and report whether any was a real event. The server only ever sends
/// `event: poke`, so the event's contents don't matter; comment-only blocks
/// (`: keep-alive`) are not pokes.
fn drain_pokes(buf: &mut String) -> bool {
    let mut poked = false;
    while let Some(end) = buf.find("\n\n") {
        poked |= buf[..end]
            .lines()
            .any(|line| !line.is_empty() && !line.starts_with(':'));
        buf.drain(..end + 2);
    }
    poked
}

/// Exchange sync messages with the remote until both sides quiesce: we have
/// nothing to send AND the response carries nothing.
async fn sync_round<D: DocHandle>(
    client: &reqwest::Client,
    base: &str,
    session: &str,
    state: &mut automerge::sync::State,
    store: &Arc<D>,
) -> anyhow::Result<()> {
    loop {
        let message = store.generate_sync(state);
        let quiet = message.is_none();
        let body = client
            .post(base)
            .header("X-Tangram-Session", session)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(message.unwrap_or_default())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .context("sync POST")?
            .bytes()
            .await
            .context("sync POST body")?;

        let mut changed = false;
        for frame in parse_frames(&body)? {
            changed |= store.receive_sync(state, frame)?;
        }
        if changed {
            store.bump();
        }
        if quiet && body.is_empty() {
            return Ok(());
        }
    }
}

/// Split a response body into its `[u32 big-endian length][bytes]` frames.
fn parse_frames(mut bytes: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
    let mut frames = Vec::new();
    while !bytes.is_empty() {
        anyhow::ensure!(bytes.len() >= 4, "truncated sync frame header");
        let len = u32::from_be_bytes(bytes[..4].try_into().expect("4 bytes")) as usize;
        anyhow::ensure!(bytes.len() >= 4 + len, "truncated sync frame");
        frames.push(&bytes[4..4 + len]);
        bytes = &bytes[4 + len..];
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::{drain_pokes, parse_frames};

    #[test]
    fn frames_roundtrip() {
        let mut body = Vec::new();
        for msg in [b"hello".as_slice(), b"".as_slice(), b"world".as_slice()] {
            body.extend_from_slice(&(msg.len() as u32).to_be_bytes());
            body.extend_from_slice(msg);
        }
        let frames = parse_frames(&body).unwrap();
        assert_eq!(frames, vec![b"hello".as_slice(), b"", b"world"]);
        assert!(parse_frames(&body[..body.len() - 1]).is_err());
        assert!(parse_frames(&[0, 0]).is_err());
    }

    #[test]
    fn sse_pokes() {
        let mut buf = String::from("event: poke\ndata:\n\n: keep-al");
        assert!(drain_pokes(&mut buf));
        assert_eq!(buf, ": keep-al"); // partial block stays buffered
        buf.push_str("ive\n\n");
        assert!(!drain_pokes(&mut buf)); // comment-only block is not a poke
        assert!(buf.is_empty());
    }
}
