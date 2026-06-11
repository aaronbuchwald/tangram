//! Replication between instances using Automerge's sync protocol over
//! HTTP(+SSE) — the wire contract is specified in `docs/SYNC_PROTOCOL.md`.
//! Topology is symmetric: every instance serves `POST /sync` +
//! `GET /sync/events` and can also dial out to one remote (`TANGRAM_REMOTE`).
//! The server is not special — it's just another peer that happens to be
//! reachable. The same interface is served by the Cloudflare Durable-Object
//! relay in `cloud/cloudflare/`, so a replica cannot tell them apart.
//!
//! The transport-free protocol logic (per-peer sessions, the one-exchange
//! server core, response framing, poke parsing) lives in
//! [`tangram_core::sync`]; this module adds the native transports — the
//! axum-served side via [`handle_post`] and the reqwest dial-out client.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use tokio::sync::watch;

pub use tangram_core::sync::Sessions;
use tangram_core::sync::{drain_pokes, normalize_remote, parse_frames};

use crate::Model;
use crate::action::Actions;
use crate::store::Store;

/// The document surface the sync protocol runs over. The SDK's typed
/// [`Store`] implements it, and so does `tangram-host`'s untyped per-app
/// document — both sides of the protocol (server [`handle_post`] and client
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

/// Handle one `POST /sync`: apply the client's message (if any), then return
/// every message we owe that peer, framed as `[u32 big-endian length][bytes]`.
pub fn handle_post<D: DocHandle + ?Sized>(
    store: &D,
    sessions: &Sessions,
    session_id: &str,
    body: &[u8],
) -> anyhow::Result<Vec<u8>> {
    // `DocHandle` is a strict superset of `tangram_core::sync::SyncDoc`
    // (it adds `subscribe`).  Rust's orphan rules prevent a blanket
    // `impl SyncDoc for D: DocHandle`, so we use a local wrapper only at
    // this single call site rather than a free-floating private adapter.
    struct W<'a, D: ?Sized>(&'a D);
    impl<D: DocHandle + ?Sized> tangram_core::sync::SyncDoc for W<'_, D> {
        fn generate_sync(&self, state: &mut automerge::sync::State) -> Option<Vec<u8>> {
            self.0.generate_sync(state)
        }
        fn receive_sync(
            &self,
            state: &mut automerge::sync::State,
            bytes: &[u8],
        ) -> anyhow::Result<bool> {
            self.0.receive_sync(state, bytes)
        }
        fn bump(&self) {
            self.0.bump()
        }
    }
    tangram_core::sync::handle_post(&W(store), sessions, session_id, body)
}

// ── client side ──────────────────────────────────────────────────────────────

/// Dial out to a remote instance and keep syncing with it, reconnecting with
/// backoff. `url` is a sync base like `http://other-host:8080/sync` (legacy
/// `ws://`/`wss://` values are rewritten with a deprecation warning).
///
/// `token`, when set, is sent as `Authorization: Bearer <token>` on both the
/// sync POST and the SSE poke stream — required by remotes whose sync
/// endpoints are private, e.g. a tangram-host tenant namespace
/// (`https://host/t/<tenant>/<app>/sync`). The native app host wires it from
/// `TANGRAM_REMOTE_TOKEN` (or `TANGRAM_REMOTE_TOKEN_<NAME>`); tangram-host
/// from the spec's `remote_token`.
pub async fn run_remote<D: DocHandle>(url: String, token: Option<String>, store: Arc<D>) {
    let base = normalize_remote(&url);
    let client = reqwest::Client::new();
    loop {
        if let Err(e) = sync_with_remote(&client, &base, token.as_deref(), &store).await {
            tracing::warn!("sync with {base} failed: {e:#}; reconnecting");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// One connection's lifetime: open the SSE poke stream, then run a sync round
/// on every poke or local change. Only returns on failure (transport error or
/// stream close); the caller reconnects with a fresh session — the server
/// then re-syncs us from a fresh `State`.
async fn sync_with_remote<D: DocHandle>(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    store: &Arc<D>,
) -> anyhow::Result<()> {
    let session = uuid::Uuid::new_v4().to_string();
    let mut state = automerge::sync::State::new();
    let mut version = store.subscribe();

    let mut events = client
        .get(format!("{base}/events"))
        .query(&[("session", &session)]);
    if let Some(token) = token {
        events = events.bearer_auth(token);
    }
    let response = events
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
        sync_round(client, base, &session, token, &mut state, store).await?;
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

/// Exchange sync messages with the remote until both sides quiesce: we have
/// nothing to send AND the response carries nothing.
async fn sync_round<D: DocHandle>(
    client: &reqwest::Client,
    base: &str,
    session: &str,
    token: Option<&str>,
    state: &mut automerge::sync::State,
    store: &Arc<D>,
) -> anyhow::Result<()> {
    loop {
        let message = store.generate_sync(state);
        let quiet = message.is_none();
        let mut post = client
            .post(base)
            .header("X-Tangram-Session", session)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(message.unwrap_or_default());
        if let Some(token) = token {
            post = post.bearer_auth(token);
        }
        let body = post
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
