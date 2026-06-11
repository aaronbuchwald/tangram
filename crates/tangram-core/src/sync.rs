//! The platform-portable half of the sync protocol (wire contract in
//! `docs/SYNC_PROTOCOL.md`): per-peer session tracking, the one-exchange
//! server logic behind `POST /sync`, the `[u32 BE length][bytes]` response
//! framing, and the SSE poke parsing the dial-out client uses. Transports
//! (axum server routes, the reqwest dial-out loop, a Cloudflare worker)
//! live in the host adapters and drive these functions.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The document surface one sync exchange runs over. The store implements it
/// (via the SDK's `DocHandle` seam), and so does `tangram-host`'s untyped
/// per-app document.
///
/// Contract: `receive_sync` persists the document itself when it changed;
/// `bump` wakes every subscriber (UIs, poke streams, peers) and is the
/// caller's job after receives so a batch of messages wakes them once.
pub trait SyncDoc {
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
}

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
pub fn handle_post<D: SyncDoc + ?Sized>(
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

/// Rewrite legacy WebSocket remote URLs (the pre-HTTP transport) so old
/// configs keep working.
pub fn normalize_remote(url: &str) -> String {
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

/// Minimal SSE parse: drop complete blank-line-terminated blocks from `buf`
/// and report whether any was a real event. The server only ever sends
/// `event: poke`, so the event's contents don't matter; comment-only blocks
/// (`: keep-alive`) are not pokes.
pub fn drain_pokes(buf: &mut String) -> bool {
    let mut poked = false;
    while let Some(end) = buf.find("\n\n") {
        poked |= buf[..end]
            .lines()
            .any(|line| !line.is_empty() && !line.starts_with(':'));
        buf.drain(..end + 2);
    }
    poked
}

/// Split a response body into its `[u32 big-endian length][bytes]` frames.
pub fn parse_frames(mut bytes: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
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
