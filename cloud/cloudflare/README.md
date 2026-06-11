# Tangram on Cloudflare — Workers + Durable Objects app host

A serverless Tangram host (RUNTIME_PLAN Phase 7,
[ADR-0002](../../docs/adr/0002-cloudflare-app-runtime.md)): one Durable
Object per app holds that app's automerge document (SQLite-backed DO
storage) and serves the FULL Tangram surface — the app's web UI, the JSON
actions API, live SSE state, MCP, and the exact HTTP sync interface from
[docs/SYNC_PROTOCOL.md](../../docs/SYNC_PROTOCOL.md). A replica pointed at
`/<app>/sync` cannot tell this host from a native Tangram instance — same
`TANGRAM_REMOTE_<APP>` config, same wire protocol, same genesis bytes.

App logic is **the same `wasm32-wasip2` components tangram-host runs**
(notes, nutrition), [jco](https://github.com/bytecodealliance/jco)-transpiled
to JS + core wasm and instantiated inside the DO. The Worker implements the
`tangram:app/host` capability imports exactly like tangram-host does:
`http-fetch` behind a per-app outbound host **allowlist** (JSPI suspends the
guest while the Worker's `fetch()` runs), `log`, `now-ms` — plus a minimal
hand-written WASI shim (env/clocks/random/stdio; the components have no
filesystem or socket imports at all). `/mcp` is driven by tangram-core's
sans-io MCP state machine, compiled to its own tiny component
(`mcp-core/`) so the protocol layer is the same Rust code everywhere.

Apps are routed by the `APPS` var in `wrangler.toml`; a name listed there
without a bundled component (`src/components.ts`) degrades to the plain
sync-relay surface this directory started as.

## Routes

| Route | Purpose |
|---|---|
| `GET /` | index of configured apps |
| `GET /<app>/` | the app's web UI (bundled from `apps/<app>/ui`) |
| `GET /<app>/healthz` | health check |
| `GET /<app>/api/state` | current state, rendered by the app's `state-json` |
| `GET /<app>/api/actions` | the action registry (names, schemas) |
| `POST /<app>/api/actions/{name}` | dispatch an action (SDK error envelope: 404/400/422/500) |
| `GET /<app>/api/events` | SSE: full state on connect and on every change |
| `GET /<app>/api/capabilities` | the app's capabilities object (404 if it publishes none) |
| `GET /<app>/api/genesis` | the component's deterministic genesis bytes (parity checks) |
| `POST /<app>/sync` | sync exchange ([protocol doc](../../docs/SYNC_PROTOCOL.md)) |
| `GET /<app>/sync/events` | SSE poke stream |
| `/<app>/mcp` | MCP (streamable HTTP): `claude mcp add --transport http <app> <url>/<app>/mcp` |

## Build the components

The Worker bundles transpiled app components from `dist/components/`
(gitignored — always generated):

```sh
cd cloud/cloudflare
npm ci
npm run build:components   # cargo (wasm32-wasip2) + jco transpile + loader glue
```

`build-components.sh` builds `notes.wasm` / `nutrition.wasm` from the
workspace, `mcp-core` (its own standalone crate), and transpiles each with
`--instantiation async` (workerd forbids runtime `WebAssembly.compile`;
core modules ship pre-compiled through wrangler's `CompiledWasm` rule) and —
for the apps — JSPI (`--async-mode jspi`) so the guest's synchronous
`http-fetch` import can await the Worker's `fetch()`.

## Develop locally (no Cloudflare account needed)

```sh
cd cloud/cloudflare
npm ci && npm run build:components
npx wrangler dev          # serves http://127.0.0.1:8787, state in .wrangler/state
```

Open `http://127.0.0.1:8787/notes/` for the UI, and point a native replica
at it to watch them converge:

```sh
TANGRAM_REMOTE_NOTES=http://127.0.0.1:8787/notes/sync cargo run -p tangram-shell
```

`wrangler dev` persists DO storage under `.wrangler/state`, so state
survives restarts locally just like in production.

Note: `wrangler` is pinned to `~4.86` because newer releases require
Node.js ≥ 22; bump it freely once your Node is current. `jco` is pinned
exactly (its output is part of the deployable artifact). `npm run check`
type-checks (`tsc --noEmit`; build the components first — the Worker
imports their generated bindings).

## Nutrition's strategy (secrets)

Description-based `log_meal` needs the CalorieNinjas key, granted to the
component as env (mirroring `apps.toml`'s env grants) — set it as a Worker
secret:

```sh
npx wrangler secret put CALORIENINJAS_API_KEY     # production
npx wrangler dev --var CALORIENINJAS_API_KEY:...  # local
```

Without it, nutrition degrades cleanly: `GET /nutrition/api/capabilities`
reports `{"strategy":"offline","description_input":false}`, manual
gram-quantified logging keeps working, and description-only meals fail with
a clear 422. The component's outbound network grant is its `allowHosts`
list in `src/components.ts` (`api.calorieninjas.com` only) — requests to any
other host are denied with an error naming the missing grant, enforced in
the Worker's `http-fetch` import, not in the app.

## Testing

Both miniflare e2e suites run locally, no Cloudflare account needed:

```sh
bash scripts/e2e-cloudflare-sync.sh   # the relay/sync regression (Phase 4)
bash scripts/e2e-cloudflare-apps.sh   # the app runtime (Phase 7)
# the sync suite also runs through cargo:
cargo test -p tangram-host -- --ignored e2e_cloudflare
```

`e2e-cloudflare-apps.sh` starts the Worker on an ephemeral 19xxx port with
an isolated state dir and asserts, in order: healthz/index/UI up; **genesis
byte-parity** (sha256 of `/notes/api/genesis` == a fresh native instance's
persisted genesis document); action dispatch writes through with the SDK
error envelope; SSE state events on connect and on change; MCP
initialize/tools-list/tools-call against `/notes/mcp` (session issued, the
tool call lands in the document, bogus sessions 404); **the flagship** — a
native local replica syncing bidirectionally with the miniflare-hosted app
(< 5 s each way, including a DO-side action reaching the replica); and
nutrition's offline fallback + the `http-fetch` allowlist denial. Cleanup is
trap-based with explicit PID tracking; reruns never share state and never
touch a live instance on `:8080`.

CI runs both as the `e2e-cloudflare-sync` and `e2e-cloudflare-apps` jobs
(`.github/workflows/ci.yml`) on Node 22 plus the `wasm32-wasip2` target;
locally they need node ≥ 20.3 (the wrangler pin above), npm, curl, jq.

## Deploy

```sh
cd cloud/cloudflare
npm ci && npm run build:components
npx wrangler login
npx wrangler secret put CALORIENINJAS_API_KEY   # optional: nutrition descriptions
npx wrangler deploy
```

Then everything points at the worker URL:

```sh
# replicas (native instances dial out and converge):
TANGRAM_REMOTE_NOTES=https://tangram-relay.<your-subdomain>.workers.dev/notes/sync
TANGRAM_REMOTE_NUTRITION=https://tangram-relay.<your-subdomain>.workers.dev/nutrition/sync

# agents:
claude mcp add --transport http notes https://tangram-relay.<your-subdomain>.workers.dev/notes/mcp
claude mcp add --transport http nutrition https://tangram-relay.<your-subdomain>.workers.dev/nutrition/mcp

# humans: https://tangram-relay.<your-subdomain>.workers.dev/notes/
```

(The worker name predates Phase 7; it is kept so existing deployments keep
their Durable Objects and documents.)

Caveat: there is **no auth yet** (Phase 5/6 add tenancy + OAuth) — anyone
with the URL can read state, dispatch actions, and sync. Don't host
documents you wouldn't show the internet, or front the worker with
Cloudflare Access first.

## Limits

- A document is stored as a single DO storage value (max 2 MiB). Tangram app
  states are tiny; chunk the bytes if an app ever outgrows that.
- Per-session sync states and MCP sessions live in DO memory and vanish on
  DO restart — harmless by design (sync re-converges from a fresh state;
  MCP clients re-initialize on the spec'd 404).
- JSPI is validated under miniflare/workerd (the e2e suites); smoke-test the
  first production deploy (ADR-0002).
