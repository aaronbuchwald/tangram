//! Replication between instances using Automerge's sync protocol over
//! WebSockets. Topology is symmetric: every instance serves `/sync` and can
//! also dial out to one remote (`TANGRAM_REMOTE`). The server is not special
//! — it's just another peer that happens to be reachable.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::Model;
use crate::action::Actions;
use crate::store::Store;

/// Core peer loop, transport-agnostic: exchange raw sync-protocol frames with
/// one peer until either side disconnects. The automerge sync protocol
/// converges and then goes quiet (`generate_sync` returns `None`); the watch
/// channel wakes us whenever the local document changes so we push updates to
/// the peer immediately — this is what makes remote UIs update quickly.
async fn peer_loop<M: Model + Actions>(
    store: Arc<Store<M>>,
    mut incoming: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::Sender<Vec<u8>>,
) {
    let mut sync_state = automerge::sync::State::new();
    let mut version = store.subscribe();

    loop {
        // Flush everything we owe the peer.
        while let Some(frame) = store.generate_sync(&mut sync_state) {
            if outgoing.send(frame).await.is_err() {
                return; // peer gone
            }
        }
        tokio::select! {
            frame = incoming.recv() => {
                let Some(frame) = frame else { return };
                match store.receive_sync(&mut sync_state, &frame) {
                    // Document changed: wake SSE streams and other peers.
                    Ok(true) => store.bump(),
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!("dropping sync peer after bad message: {e}");
                        return;
                    }
                }
            }
            changed = version.changed() => {
                if changed.is_err() {
                    return; // store dropped
                }
            }
        }
    }
}

/// Handle an inbound peer on the `/sync` WebSocket route.
pub(crate) async fn serve_peer<M: Model + Actions>(
    socket: axum::extract::ws::WebSocket,
    store: Arc<Store<M>>,
) {
    use axum::extract::ws::Message;

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);

    let forward_out = async {
        while let Some(frame) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
        }
    };
    let forward_in = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            if let Message::Binary(bytes) = msg
                && in_tx.send(bytes.to_vec()).await.is_err()
            {
                break;
            }
        }
    };

    tokio::select! {
        () = peer_loop(store, in_rx, out_tx) => {}
        () = forward_out => {}
        () = forward_in => {}
    }
}

/// Dial out to a remote instance and keep syncing with it, reconnecting with
/// backoff. `url` is e.g. `ws://other-host:8080/sync`.
pub(crate) async fn run_remote<M: Model + Actions>(url: String, store: Arc<Store<M>>) {
    use tokio_tungstenite::tungstenite::Message;

    loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((socket, _resp)) => {
                tracing::info!("syncing with remote {url}");
                let (mut ws_tx, mut ws_rx) = socket.split();
                let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
                let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);

                let forward_out = async {
                    while let Some(frame) = out_rx.recv().await {
                        if ws_tx.send(Message::Binary(frame.into())).await.is_err() {
                            break;
                        }
                    }
                };
                let forward_in = async {
                    while let Some(Ok(msg)) = ws_rx.next().await {
                        if let Message::Binary(bytes) = msg
                            && in_tx.send(bytes.to_vec()).await.is_err()
                        {
                            break;
                        }
                    }
                };

                tokio::select! {
                    () = peer_loop(store.clone(), in_rx, out_tx) => {}
                    () = forward_out => {}
                    () = forward_in => {}
                }
                tracing::warn!("lost sync connection to {url}; reconnecting");
            }
            Err(e) => {
                tracing::warn!("cannot reach remote {url}: {e}; retrying");
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
