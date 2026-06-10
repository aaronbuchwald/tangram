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
apps/shell              multi-app host: serves every app under one port, prefixed
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

### Run them all in one server

The shell mounts every example app on one port, each under its own path
prefix with its full surface intact (`/notes/`, `/notes/mcp`, `/notes/sync`,
`/nutrition/`, …) and an index page at `/`:

```sh
cargo run -p tangram-shell        # http://127.0.0.1:8080
```

Apps keep separate documents (`notes.automerge`, `nutrition.automerge` in
`TANGRAM_DATA_DIR`), so one shell can't share a single `TANGRAM_REMOTE`.
Instead each app reads `TANGRAM_REMOTE_<NAME>` (name uppercased):

```sh
BIND_ADDR=127.0.0.1:8081 \
TANGRAM_DATA_DIR=data-b \
TANGRAM_REMOTE_NOTES=ws://127.0.0.1:8080/notes/sync \
TANGRAM_REMOTE_NUTRITION=ws://127.0.0.1:8080/nutrition/sync \
cargo run -p tangram-shell        # second shell, replicating both apps with the first
```

Under the hood each app crate exposes `pub fn app() -> tangram::App<…>`;
`app().serve()` runs it standalone while `app().build()?` returns its
`axum::Router` for a host to `nest_service` under a prefix (see
`apps/shell/src/main.rs`).

### Connect an agent

```sh
claude mcp add --transport http nutrition http://127.0.0.1:8080/mcp
```

Ask Claude to log a meal or register nutrition data for a new ingredient
(`add_ingredient`) — the change lands in the same document and pushes to every
UI and synced instance.

## Getting started: a persistent remote + a local replica

The day-to-day setup: a remote box runs the apps permanently; your laptop
runs a local replica that syncs to it through an SSH tunnel. You work against
the replica (UI + MCP), offline edits included, and everything converges.
There are Claude Code skills for both halves (`.claude/skills/`), or follow
the manual steps.

### 1. Remote: install the persistent service

On the remote box, in this repo (or ask Claude: `/systemd-service install`):

```sh
bash .claude/skills/systemd-service/service.sh install \
  --env NUTRITION_STRATEGY=calorieninjas        # optional; needs the key in .env
```

This builds the release shell, writes a systemd unit (working directory = the
repo, so `.env` secrets load via dotenvy), enables it at boot, starts it on
`127.0.0.1:8080`, and health-checks it. After pulling new code, rebuild with
`/systemd-service rebuild` (or `service.sh rebuild`).

### 2. Tunnel: one SSH config entry

On your **local machine**, add to `~/.ssh/config`:

```
Host tangram
    HostName <your-remote-host>
    User ubuntu
    IdentityFile ~/.ssh/<your-key>.pem
    LocalForward 8080 127.0.0.1:8080
```

Now every `ssh tangram` session doubles as the sync link: the remote's web,
MCP, and sync endpoints all appear at `localhost:8080` on your machine.

### 3. Local: run the replica

With an `ssh tangram` session open, from your local clone (or ask Claude:
`/local-replica connect`):

```sh
bash .claude/skills/local-replica/replica.sh connect
```

This starts the shell on `127.0.0.1:8090` with
`TANGRAM_REMOTE_<APP>=ws://127.0.0.1:8080/<app>/sync` (i.e. syncing to the
remote through the tunnel), waits until both apps' states converge with the
remote, and prints the URLs. Manual equivalent:

```sh
BIND_ADDR=127.0.0.1:8090 \
TANGRAM_DATA_DIR=data-replica \
TANGRAM_REMOTE_NOTES=ws://127.0.0.1:8080/notes/sync \
TANGRAM_REMOTE_NUTRITION=ws://127.0.0.1:8080/nutrition/sync \
cargo run --release -p tangram-shell
```

### 4. Point your local MCP at the replica

```sh
claude mcp add --transport http notes     http://127.0.0.1:8090/notes/mcp
claude mcp add --transport http nutrition http://127.0.0.1:8090/nutrition/mcp
```

Agent writes land in the local replica and replicate up the tunnel on their
own.

### 5. Watch them sync

Open both sides of the same app in two tabs:

| | notes | nutrition |
|---|---|---|
| **local replica** (`:8090`) | <http://localhost:8090/notes/> | <http://localhost:8090/nutrition/> |
| **remote via tunnel** (`:8080`) | <http://localhost:8080/notes/> | <http://localhost:8080/nutrition/> |

Add a note or log a meal in either tab — the other updates in well under a
second (CRDT sync + SSE push). `/local-replica status` reports per-app
convergence. Then the local-first test: close the SSH session — the `:8080`
tab dies (it *was* the tunnel) but `:8090` keeps working; make edits, re-run
`ssh tangram`, and watch them appear on the remote within a couple seconds
(the replica reconnects with ~2s backoff).

### Alternative: a tailnet instead of an SSH tunnel

The SSH tunnel is zero-install but lives only as long as the session. If you
want the replica to sync continuously (closer to how multi-device should
feel), put both machines on a [Tailscale](https://tailscale.com) tailnet and
skip the tunnel: keep the remote bound to `127.0.0.1` behind `tailscale
serve`, or bind it to the tailnet interface, and point the replica straight
at it (`replica.sh connect --remote ws://<remote-tailnet-name>:8080`). Same
caveat either way: `/sync` and `/mcp` have no auth yet, so only expose them
on networks where every peer is trusted — a tailnet qualifies, the public
internet does not.

## Configuration (env / `.env`)

| Variable | Default | Purpose |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` | Listen address |
| `TANGRAM_REMOTE` | — | `ws://host:port/sync` of a peer to replicate with (single-app mode) |
| `TANGRAM_REMOTE_<NAME>` | — | Per-app remote, e.g. `TANGRAM_REMOTE_NOTES` (required form in a shell) |
| `TANGRAM_DATA_DIR` | `./data` | Where the document file lives |
| `FRAME_ANCESTORS` | `*` | CSP `frame-ancestors` for iframe embedding |
| `RUST_LOG` | `info` | Log filter |
| `NUTRITION_STRATEGY` | `offline` | Nutrition app: how novel components resolve (`offline` \| `calorieninjas` \| `llm`) |
| `CALORIENINJAS_API_KEY` | — | Required for `NUTRITION_STRATEGY=calorieninjas` |
| `ANTHROPIC_API_KEY` | — | Required for `NUTRITION_STRATEGY=llm` (or `ANTHROPIC_AUTH_TOKEN`) |

## Nutrition strategies

The nutrition app ports Chamber's pluggable nutrition-resolution seam: a
*strategy* decides how a novel meal component gets its per-100g nutrient
values. Select one with `NUTRITION_STRATEGY`:

- **`offline`** (default) — deterministic and keyless. The reference dataset
  ships in the replicated genesis document; meals must be logged with
  explicit gram-quantified components, and unknown components contribute
  nothing until registered (`add_component_nutrition` / `add_ingredient`).
- **`calorieninjas`** — resolves free text via the CalorieNinjas API
  (`CALORIENINJAS_API_KEY`), mapping every nutrient field the API returns
  (calories, fiber, sodium, …) to per-100g rows.
- **`llm`** — asks Anthropic's `claude-opus-4-8` (structured output) for a
  comprehensive per-100g nutrient panel (`ANTHROPIC_API_KEY`).

With an online strategy active, meals can be logged from a plain-language
**description** — quantities included, no explicit components needed:

```sh
curl -s localhost:8080/api/log -H 'content-type: application/json' \
  -d '{"description": "1 cup brown rice and 200g grilled chicken"}'
```

`GET /api/capabilities` reports the active strategy (the web UI uses it to
offer the description box). Explicit components always win when provided;
unknown ones are then back-filled in the background. Every resolution runs
*outside* the synchronous action transaction and is cached through an
idempotent action, so it lands as an ordinary replicated change: a component
is resolved once and replays on every synced device — past meals using it
resolve retroactively.

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
