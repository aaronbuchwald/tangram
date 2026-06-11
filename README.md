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
| **Sync** (Automerge protocol over HTTP+SSE) | `/sync` | Other instances of the same app — your other devices, a shared relay, a collaborator |

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
crates/tangram          the SDK: native tokio/axum host (web + sync + MCP transports,
                        App builder) over tangram-core, plus the WASM guest adapter
crates/tangram-core     the portable core: action registry, CRDT store + dispatch,
                        sync sessions/framing, sans-io streamable-HTTP MCP server —
                        no tokio/hyper; compiles to wasm32-wasip2 (CI-checked)
crates/tangram-macros   #[model] and #[actions] proc macros
apps/notes              minimal example: a replicated notes list
apps/nutrition          fuller example: Chamber's nutrition tracker design on Tangram
apps/shell              multi-app host: serves every app under one port, prefixed
cloud/cloudflare        Durable-Object sync relay speaking the same sync interface
docs/SDK_DESIGN.md      architecture & roadmap
docs/SYNC_PROTOCOL.md   the HTTP(+SSE) sync wire contract
.agents/skills/         agent skills (SKILL.md format), tool-agnostic
```

AGENTS.md (symlinked as CLAUDE.md) is the entry point for coding agents;
skills live in `.agents/skills/` (Claude Code finds them via a symlink).

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
TANGRAM_REMOTE=http://127.0.0.1:8080/sync \
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
TANGRAM_REMOTE_NOTES=http://127.0.0.1:8080/notes/sync \
TANGRAM_REMOTE_NUTRITION=http://127.0.0.1:8080/nutrition/sync \
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

## Run an app sandboxed (gVisor)

Each app also ships as a tiny OCI image — a static musl binary plus its
`ui/` dir on `FROM scratch` (~10–15 MB) — meant to run under
[gVisor](https://gvisor.dev)'s `runsc` runtime (Phase 0 of
[docs/RUNTIME_PLAN.md](docs/RUNTIME_PLAN.md)). Build both images:

```sh
rustup target add x86_64-unknown-linux-musl   # once
sudo apt-get install -y musl-tools            # once (Debian/Ubuntu)
scripts/build-images.sh                       # → tangram/notes:dev, tangram/nutrition:dev
```

With Docker and runsc installed (gvisor.dev/docs → apt repo, then
`sudo runsc install && sudo systemctl restart docker`), run an app:

```sh
docker run -d --name notes --runtime=runsc --read-only \
  -p 127.0.0.1:19080:8080 -v notes-data:/data tangram/notes:dev
curl http://127.0.0.1:19080/healthz   # all surfaces: / /api/* /sync /mcp
```

Inside the image the app binds `0.0.0.0:8080` (required for port mapping;
keep the host publish on loopback), writes only to the `/data` volume, and
serves its UI from `/ui` via the `TANGRAM_UI_DIR` override — the
compile-time UI path apps pin with `.ui_dir(...)` doesn't exist in the
image. The nutrition app resolves meal descriptions from inside the sandbox
too: pass the key with `--env-file .env` (gVisor's netstack handles the
egress). Cold start to a serving `/healthz` is ~240 ms.

## Run apps as WASM components (tangram-host)

The WASM-first runtime ([ADR-0001](docs/adr/0001-wasm-first-sandbox-runtime.md),
Phase 2 of [docs/RUNTIME_PLAN.md](docs/RUNTIME_PLAN.md)): apps compile to
`wasm32-wasip2` components containing ONLY app logic, and one native
`tangram-host` binary owns the whole platform — HTTP serving, the sync
protocol, MCP, persistence, and static UI files. The component's world
(`crates/tangram-host/wit/tangram.wit`) imports nothing but `http-fetch`
(behind a per-app outbound host allowlist), `log`, and `now-ms`; the host is
the only thing that touches `$HOME/.<app-name>`, so an app cannot name a
file, socket, or non-granted host at all.

```sh
rustup target add wasm32-wasip2                                       # once
cargo build -p tangram-notes -p tangram-nutrition --lib \
  --target wasm32-wasip2 --release                                    # → target/wasm32-wasip2/release/{notes,nutrition}.wasm
cargo run -p tangram-host --release -- apps.toml
```

`apps.toml` is the desired state — the host watches it and converges live
(add/remove/reload apps, including when a component file is rebuilt, without
a restart):

```toml
[apps.notes]
component = "target/wasm32-wasip2/release/notes.wasm"
ui = "apps/notes/ui"

[apps.nutrition]
component = "target/wasm32-wasip2/release/nutrition.wasm"
ui = "apps/nutrition/ui"
allow_hosts = ["api.calorieninjas.com"]     # the app's ENTIRE outbound grant

[apps.nutrition.env]
NUTRITION_STRATEGY = "calorieninjas"
CALORIENINJAS_API_KEY = "${CALORIENINJAS_API_KEY}"   # ${VAR} expands from the host env / .env
```

Every app serves its full surface under one port, exactly like the shell:
`/<app>/` (UI), `/<app>/api/*` (state, actions, SSE), `/<app>/sync` (the
HTTP sync protocol — interoperates bidirectionally with native instances and
the Cloudflare relay; genesis bytes are identical by construction), and
`/<app>/mcp`. An optional per-app `remote` dials out to a peer, and
`data_dir` overrides the default `$HOME/.<app-name>`. An app whose spec
grants no `allow_hosts` simply cannot reach the network: nutrition's
description-based `log_meal` then fails with an error saying which host to
grant in `apps.toml`.

Custom capability probes survive the cutover too: a component can publish an
optional `capabilities` object in its `describe()` manifest, computed at
instantiation from the env vars its spec grants, and the host serves it at
`GET /<app>/api/capabilities` (404 for apps that publish none). Nutrition
uses this to report its active strategy with the exact same JSON as its
native route — same env, same bytes — so its UI offers description-based
logging under the host as well (pinned by
`crates/tangram-host/tests/capabilities.rs`).

## Getting started: a persistent remote + a local replica

The day-to-day setup: a remote box runs the apps permanently; your laptop
runs a local replica that syncs to it through an SSH tunnel. You work against
the replica (UI + MCP), offline edits included, and everything converges.
There are agent skills for both halves (`.agents/skills/`), or follow
the manual steps.

### 1. Remote: install the persistent service

On the remote box, in this repo (or ask Claude: `/systemd-service install`):

```sh
bash .agents/skills/systemd-service/service.sh install
```

With `CALORIENINJAS_API_KEY` in the repo's `.env`, the nutrition app
auto-enables the calorieninjas strategy; pass `--env NUTRITION_STRATEGY=…`
only to override.

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
bash .agents/skills/local-replica/replica.sh connect
```

This starts the shell on `127.0.0.1:8090` with
`TANGRAM_REMOTE_<APP>=http://127.0.0.1:8080/<app>/sync` (i.e. syncing to the
remote through the tunnel), waits until both apps' states converge with the
remote, and prints the URLs. It also compares nutrition capabilities with the
remote: if the remote resolves meal descriptions but your replica doesn't, it
prints a reminder to copy `CALORIENINJAS_API_KEY` into your local `.env`
(with the key present, the calorieninjas strategy auto-enables — no
`NUTRITION_STRATEGY` needed). Pass extra env with `--env KEY=VALUE`. Manual
equivalent:

```sh
BIND_ADDR=127.0.0.1:8090 \
TANGRAM_DATA_DIR=data-replica \
TANGRAM_REMOTE_NOTES=http://127.0.0.1:8080/notes/sync \
TANGRAM_REMOTE_NUTRITION=http://127.0.0.1:8080/nutrition/sync \
cargo run --release -p tangram-shell
```

To run the replica on the WASM runtime instead of the native shell, pass
`--wasm`: `replica.sh connect --wasm` builds the components plus the release
`tangram-host`, generates a replica `apps.toml` (per-app data dirs under
`--data-dir`, per-app `remote` pointing at the remote base, nutrition's
allowlist and `${VAR}` env grants mirroring the native strategy selection),
and serves the same surfaces on `--bind`. `status`/`stop` work for either
mode — the pid file (`replica.pid` vs `replica-wasm.pid`) distinguishes them.

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
at it (`replica.sh connect --remote http://<remote-tailnet-name>:8080`). Same
caveat either way: `/sync` and `/mcp` have no auth yet, so only expose them
on networks where every peer is trusted — a tailnet qualifies, the public
internet does not.

### Alternative: a Cloudflare relay as the remote

Instead of an always-on box, the remote can be the serverless sync relay in
[`cloud/cloudflare/`](cloud/cloudflare/): one Durable Object per app stores
the document (SQLite-backed) and speaks the exact same sync interface
([docs/SYNC_PROTOCOL.md](docs/SYNC_PROTOCOL.md)), so replicas can't tell it
from a native instance:

```sh
cd cloud/cloudflare && npm install && npx wrangler login && npx wrangler deploy
```

then point each replica at the worker:

```sh
TANGRAM_REMOTE_NOTES=https://tangram-relay.<your-subdomain>.workers.dev/notes/sync \
TANGRAM_REMOTE_NUTRITION=https://tangram-relay.<your-subdomain>.workers.dev/nutrition/sync \
cargo run --release -p tangram-shell
```

Two laptops pointed at the same relay converge through it with no machine of
yours running in between. `npx wrangler dev` runs the same relay locally for
testing (see [cloud/cloudflare/README.md](cloud/cloudflare/README.md)). The
no-auth caveat above applies doubly: a deployed worker is on the public
internet.

## Configuration (env / `.env`)

| Variable | Default | Purpose |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` | Listen address |
| `TANGRAM_REMOTE` | — | `http://host:port/sync` of a peer to replicate with (single-app mode); legacy `ws://`/`wss://` values are rewritten to `http(s)://` with a warning |
| `TANGRAM_REMOTE_<NAME>` | — | Per-app remote, e.g. `TANGRAM_REMOTE_NOTES` (required form in a shell) |
| `TANGRAM_DATA_DIR` | `$HOME/.<app-name>` | Where the document file lives, directly inside it. Unset: each app uses its own `~/.<app>` (e.g. `~/.notes/notes.automerge`); `./data` if `$HOME` is also unset |
| `TANGRAM_UI_DIR` | builder value | Static UI directory; overrides the app's compiled-in path (set in container images, where that path doesn't exist) |
| `FRAME_ANCESTORS` | `*` | CSP `frame-ancestors` for iframe embedding |
| `RUST_LOG` | `info` | Log filter |
| `NUTRITION_STRATEGY` | auto | Nutrition app: how novel components resolve (`offline` \| `calorieninjas` \| `llm`); unset → `calorieninjas` if `CALORIENINJAS_API_KEY` is set, else `offline` |
| `CALORIENINJAS_API_KEY` | — | Required for `calorieninjas`; its presence auto-enables that strategy when `NUTRITION_STRATEGY` is unset |
| `ANTHROPIC_API_KEY` | — | Required for `NUTRITION_STRATEGY=llm` (or `ANTHROPIC_AUTH_TOKEN`) |

## Nutrition strategies

The nutrition app ports Chamber's pluggable nutrition-resolution seam: a
*strategy* decides how a novel meal component gets its per-100g nutrient
values. An explicit `NUTRITION_STRATEGY` wins; when unset, the presence of
`CALORIENINJAS_API_KEY` auto-enables `calorieninjas` (online resolution is
the default expectation), otherwise `offline`:

- **`offline`** (keyless default) — deterministic and keyless. The reference dataset
  ships in the replicated genesis document; meals must be logged with
  explicit gram-quantified components, and unknown components contribute
  nothing until registered (`add_component_nutrition` / `add_ingredient`).
- **`calorieninjas`** — resolves free text via the CalorieNinjas API
  (`CALORIENINJAS_API_KEY`), mapping every nutrient field the API returns
  (calories, fiber, sodium, …) to per-100g rows.
- **`llm`** — asks Anthropic's `claude-opus-4-8` (structured output) for a
  comprehensive per-100g nutrient panel (`ANTHROPIC_API_KEY`).

With an online strategy active, meals can be logged from a plain-language
**description** — quantities included, no explicit components needed. This is
the same registered `log_meal` action everywhere (HTTP action route, MCP
tool, web UI — one contract by construction):

```sh
curl -s localhost:8080/api/actions/log_meal -H 'content-type: application/json' \
  -d '{"description": "1 cup brown rice and 200g grilled chicken"}'
```

`GET /api/capabilities` reports the active strategy (the web UI uses it to
offer the description box). Explicit components always win when provided;
unknown ones are then back-filled in the background. `log_meal` is an async
action: it resolves over the network *without* holding the store lock and
caches results through an idempotent mutation, so each resolution lands as an
ordinary replicated change: a component is resolved once and replays on every
synced device — past meals using it resolve retroactively.

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
- **Sync**: Automerge's sync protocol over HTTP — every instance serves
  `POST /sync` plus an SSE poke stream at `/sync/events`, and can dial one
  `TANGRAM_REMOTE`. Topology is symmetric — a "server" is just a reachable
  peer — and the wire contract
  ([docs/SYNC_PROTOCOL.md](docs/SYNC_PROTOCOL.md)) is shared by the native
  SDK and the Cloudflare relay, so they're interchangeable as remotes.
- **Live UIs**: a watch channel fires on every document change; `/api/events`
  (SSE) pushes the full state JSON to UIs, and sync peers are woken to
  forward the change on.

See [docs/SDK_DESIGN.md](docs/SDK_DESIGN.md) for the full architecture and
roadmap (browser replicas, access control, presence). Current implementation
notes: sync uses raw automerge sync (not samod) and there is no auth on
`/sync` or `/mcp` yet — bind to localhost or front with TLS/auth before
exposing.
