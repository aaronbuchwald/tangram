# Tangram

Build small, local-first apps whose state replicates across devices and whose
capabilities are exposed to both humans and AI — from one plain-Rust
definition.

A Tangram app is a single binary serving three surfaces over one shared,
CRDT-replicated state:

| Surface | Endpoint | Consumed by |
|---|---|---|
| **MCP** (streamable HTTP) | `/mcp` | AI agents — Claude Code, Claude Desktop, any MCP client |
| **Web UI + JSON API** | `/`, `/api/*` | Humans (standalone or iframed into Obsidian / a Tangram shell) |
| **Sync** (Automerge protocol over WebSocket) | `/sync` | Other instances of the same app — your other devices, a shared relay, a collaborator |

State lives in an [Automerge](https://automerge.org) document persisted to
disk: the app is fully functional offline, and when a peer is reachable,
changes merge from both sides automatically. Every connected UI re-renders
live (SSE push) when a change lands — whether it came from the local UI, an
MCP tool call, or another instance. Cross-instance UI updates land in tens of
milliseconds on a LAN.

## What an app looks like

```rust
use tangram::prelude::*;

#[model]                       // replicated, persisted, schema'd
#[derive(Default)]
struct Notes { notes: Vec<Note> }

#[model]
struct Note { id: String, text: String, created_at_ms: i64 }

#[actions]                     // each method → MCP tool + HTTP endpoint
impl Notes {
    /// Add a note. Returns the new note's id.
    pub fn add_note(&mut self, text: String) -> String { /* … */ }

    /// List all notes, newest first.
    pub fn list_notes(&self) -> Vec<Note> { /* … */ }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    App::<Notes>::new("notes").serve().await
}
```

`&mut self` methods are mutating actions (each becomes one attributed CRDT
change); `&self` methods are read-only actions. Doc comments become MCP tool
descriptions; parameters become JSON-schema'd arguments.

## Workspace layout

```
crates/tangram          the SDK: CRDT store, sync, web + MCP surfaces, App builder
crates/tangram-macros   #[model] and #[actions] proc macros
apps/notes              minimal example: a replicated notes list
apps/nutrition          fuller example: Chamber's nutrition tracker design on Tangram
docs/SDK_DESIGN.md      architecture & roadmap
```

## Run the examples

```sh
cargo run -p tangram-notes        # http://127.0.0.1:8080
cargo run -p tangram-nutrition
```

### See replication live

Run two instances of the same app and point the second at the first:

```sh
cargo run -p tangram-nutrition    # instance A on :8080

BIND_ADDR=127.0.0.1:8081 \
TANGRAM_DATA_DIR=data-b \
TANGRAM_REMOTE=ws://127.0.0.1:8080/sync \
cargo run -p tangram-nutrition    # instance B, replicating with A
```

Open both UIs; log a meal in either and watch it appear in the other
immediately. Kill A, keep using B offline, restart A — they reconverge.

### Connect an agent

```sh
claude mcp add --transport http nutrition http://127.0.0.1:8080/mcp
```

Ask Claude to log a meal or register nutrition data for a new ingredient
(`add_ingredient`) — the change lands in the same document and pushes to every
UI and synced instance.

## Configuration (env / `.env`)

| Variable | Default | Purpose |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` | Listen address |
| `TANGRAM_REMOTE` | — | `ws://host:port/sync` of a peer to replicate with |
| `TANGRAM_DATA_DIR` | `./data` | Where the document file lives |
| `FRAME_ANCESTORS` | `*` | CSP `frame-ancestors` for iframe embedding |
| `RUST_LOG` | `info` | Log filter |

## How it works

- **Model**: `#[model]` structs are mapped to an Automerge document with
  [autosurgeon](https://github.com/automerge/autosurgeon); the genesis state
  (`Default`) is committed deterministically (fixed actor, zero timestamp) so
  independently-started instances share a document root and merge cleanly.
  Keep `Default` deterministic (use `Vec`, not `HashMap`).
- **Actions**: run once on the receiving instance under the store lock —
  hydrate model → run method → reconcile back as one commit (named after the
  action, so history is attributable). Results are returned to the caller;
  the resulting *data* (not the action) replicates.
- **Sync**: Automerge's sync protocol over WebSocket; every instance serves
  `/sync` and can dial one `TANGRAM_REMOTE`. Topology is symmetric — a
  "server" is just a reachable peer.
- **Live UIs**: a watch channel fires on every document change; `/api/events`
  (SSE) pushes the full state JSON to UIs, and sync peers are woken to
  forward the change on.

See [docs/SDK_DESIGN.md](docs/SDK_DESIGN.md) for the full architecture and
roadmap (browser replicas, access control, presence). Current implementation
notes: sync uses raw automerge sync (not samod) and there is no auth on
`/sync` or `/mcp` yet — bind to localhost or front with TLS/auth before
exposing.
