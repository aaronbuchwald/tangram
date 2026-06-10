# Tangram sync over HTTP(+SSE)

This is the cross-platform wire contract for replicating a Tangram app's
Automerge document between two peers. It replaced the WebSocket transport
(ADR-0001: `wasi:http` has no WebSockets, and one HTTP transport serves
native, WASI, Cloudflare, and browsers). Every implementation of the *server*
side — the native SDK (`crates/tangram/src/sync.rs` + `web.rs`) and the
Cloudflare Durable-Object relay (`cloud/cloudflare/`) — must be
indistinguishable to a client: a replica points `TANGRAM_REMOTE_<APP>` at
either and cannot tell the difference.

The payloads are unmodified [automerge sync protocol] messages; this document
only defines how they move over HTTP.

[automerge sync protocol]: https://automerge.org/docs/under-the-hood/sync/

## Endpoints

A *sync base* is a URL like `http://host:8080/sync` (standalone app) or
`https://host/<app>/sync` (shell or relay). `TANGRAM_REMOTE*` values are sync
bases. Legacy `ws://`/`wss://` values are accepted and rewritten to
`http(s)://` with a deprecation warning.

### `POST <base>` (e.g. `POST /notes/sync`)

One sync exchange.

- Request headers:
  - `X-Tangram-Session: <uuid>` — client-generated, identifies this peer's
    sync session (required).
  - `Content-Type: application/octet-stream`
- Request body: **zero or one** automerge sync message, raw bytes. An empty
  body means "I have nothing to send — just give me yours".
- Response `200`, `Content-Type: application/octet-stream`. Body: **zero or
  more** server→client sync messages, framed as repeated
  `[u32 big-endian length][bytes]`. (The request body is unframed because it
  carries at most one message.)

The server holds one `automerge::sync::State` per session id in memory
(a map with last-seen timestamps, evicted after ~5 minutes idle; the
Durable-Object relay additionally loses them on DO restart). **A lost or
evicted session is harmless**: the next POST under that id starts from a
fresh `State` and the automerge protocol re-converges by itself — it only
costs an extra round trip or two. Sessions are cheap, ephemeral cursors, not
durable state.

### `GET <base>/events?session=<uuid>` (e.g. `GET /notes/sync/events`)

A [Server-Sent Events] stream of pokes. The server sends

```
event: poke
data:
```

immediately on connect and again whenever the document changes. A poke
carries no payload — it only tells the client to run its POST loop. Servers
may interleave SSE comment lines (`: keep-alive`) to keep intermediaries from
closing the stream; clients must ignore them.

[Server-Sent Events]: https://html.spec.whatwg.org/multipage/server-sent-events.html

## Client loop

```
session = new uuid
state   = fresh automerge sync State
open GET <base>/events?session=<session>

forever:
    # one sync round, until both sides quiesce:
    loop:
        msg  = generate_sync_message(state)        # may be None
        resp = POST <base> body=(msg or empty)
        apply each framed message in resp to (doc, state)
        if msg was None and resp body was empty: break
    wait for: a poke on the SSE stream, OR a local document change
```

If received messages change the document's heads, the client wakes its own
subscribers (UIs, other peers) — same semantics as any local change. On any
transport error or stream close, reconnect with ~2s backoff using a **fresh
session and State**.

Both sides converge to silence: after a round, nothing is sent until a real
change happens on one side. The transport is symmetric in effect (either
side's change reaches the other promptly) even though only the client dials.

## Server obligations

- Apply a non-empty POST body with `receive_sync_message`; persist and notify
  all poke streams (and any other subscribers) when the document changed.
- Answer every POST with all currently pending messages for that session
  (loop `generate_sync_message` until it returns nothing).
- Send a poke on every `/sync/events` connect and on every document change.

## Genesis rule (relays)

Tangram apps commit a deterministic genesis change (fixed actor, zero
timestamp), so independently started instances share a document root. A relay
that stores the document but doesn't know the app's model (the Cloudflare DO)
must **not** invent its own genesis — that would fork the history into rival
container objects. An empty relay starts from a literal empty automerge
document (no commits at all); the app's genesis merges in on first sync since
the empty document has no conflicting history.
