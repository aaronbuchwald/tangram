# Tangram sync relay — Cloudflare Workers + Durable Objects

A serverless alternative to the always-on remote box: one Durable Object per
app holds that app's automerge document (persisted in SQLite-backed DO
storage) and speaks the exact HTTP sync interface from
[docs/SYNC_PROTOCOL.md](../../docs/SYNC_PROTOCOL.md). A replica pointed at
this relay cannot tell it from a native Tangram instance — same
`TANGRAM_REMOTE_<APP>` config, same wire protocol.

The relay knows nothing about the apps' models. It merges and stores whatever
document history peers send it (starting from a literal empty automerge
document, so the app's genesis merges in cleanly — see the genesis rule in
the protocol doc) and re-serves it to every other peer, poking connected SSE
listeners on each change.

## Routes

| Route | Purpose |
|---|---|
| `GET /` | index of configured apps |
| `POST /<app>/sync` | sync exchange (protocol doc) |
| `GET /<app>/sync/events` | SSE poke stream |
| `GET /<app>/api/state` | read-only JSON of the stored document (verification) |
| `GET /<app>/healthz` | health check |

The app list comes from the `APPS` var in `wrangler.toml` (default
`notes,nutrition`); each name maps to one Durable Object via `idFromName`.

## Develop locally (no Cloudflare account needed)

```sh
cd cloud/cloudflare
npm install
npx wrangler dev          # serves http://127.0.0.1:8787, state in .wrangler/state
```

Point two native replicas at it and watch them converge through the relay:

```sh
TANGRAM_REMOTE_NOTES=http://127.0.0.1:8787/notes/sync cargo run -p tangram-shell
```

`wrangler dev` persists DO storage under `.wrangler/state`, so relay state
survives restarts locally just like in production.

Note: `wrangler` is pinned to `~4.86` because newer releases require
Node.js ≥ 22; bump it freely once your Node is current. `npm run check`
type-checks (`tsc --noEmit`).

## Deploy

```sh
cd cloud/cloudflare
npx wrangler login
npx wrangler deploy
```

Then point replicas at the worker:

```sh
TANGRAM_REMOTE_NOTES=https://tangram-relay.<your-subdomain>.workers.dev/notes/sync
TANGRAM_REMOTE_NUTRITION=https://tangram-relay.<your-subdomain>.workers.dev/nutrition/sync
```

Same caveat as the native `/sync`: there is no auth yet — don't relay
documents you wouldn't show the internet, or front the worker with Cloudflare
Access first.

## Limits

- A document is stored as a single DO storage value (max 2 MiB). Tangram app
  states are tiny; chunk the bytes if an app ever outgrows that.
- Per-session sync states live in DO memory and vanish on DO restart —
  harmless by protocol design (peers re-converge from a fresh state).
