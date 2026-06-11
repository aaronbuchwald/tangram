---
name: local-replica
description: Set up, check, or stop a local Tangram replica that syncs to a remote instance (typically through an SSH tunnel). Use when asked to run a local replica, connect a local instance to the remote, check replica sync status, or stop the local replica.
argument-hint: connect|status|stop [--wasm] [--remote <http base>] [--bind <addr:port>] [--data-dir <dir>] [--env KEY=VALUE]...
allowed-tools: Bash
---

Manage a local Tangram replica that replicates with a remote instance. All
logic lives in `replica.sh`; run it and interpret the output.

```bash
bash .agents/skills/local-replica/replica.sh connect [--wasm] [--remote <http base>] [--bind <addr:port>] [--data-dir <dir>] [--env KEY=VALUE]...
bash .agents/skills/local-replica/replica.sh status
bash .agents/skills/local-replica/replica.sh stop
```

Pass the user's arguments through: $ARGUMENTS

## Defaults

- `--remote`: `http://127.0.0.1:8080` — the remote as seen through an SSH
  tunnel (`ssh tangram` with `LocalForward 8080`). Pass a different base for
  a direct/tailnet remote, e.g. `http://my-host:8080`, or a deployed
  Cloudflare relay, e.g. `https://tangram-relay.<subdomain>.workers.dev`
  (legacy `ws://` bases still work).
- `--bind`: `127.0.0.1:8090` (off the tunnel-forwarded ports).
- `--data-dir`: `data-replica` — kept separate from `data/` so a replica can
  coexist with a primary instance in the same checkout.
- `--env KEY=VALUE` (repeatable, connect only): extra environment exported to
  the started replica, e.g. `--env NUTRITION_STRATEGY=offline`. In `--wasm`
  mode it is passed straight to the host process, where `${KEY}` references in
  the synced registry doc expand — so the value resolves locally and never
  lands in a file or the replicated document.
- `--wasm` (connect only): run the replica as WASM components under
  `tangram-host`, FEDERATED with the remote (RUNTIME_PLAN Phase 9). It starts
  only a registry app pointed at `<remote>/registry/sync` and lets
  convergence pull the rest of the fleet down; an install/remove on ANY host
  propagates fleet-wide.

## Behavior

`connect` verifies the remote is reachable (clear error telling the user to
start their SSH tunnel if not), builds the release shell, starts it in the
background (pid file + log in the data dir; replaces a previous replica),
waits until the apps' states converge with the remote, and prints the local
and remote URLs plus the `claude mcp add` commands for pointing local MCP at
the replica. The native (default) path is unchanged: notes + nutrition over
`TANGRAM_REMOTE_*`. `status` reports process/tunnel health and per-app sync
convergence. `stop` sends SIGINT to the replica.

`connect --wasm` is FEDERATED registry-bootstrap (RUNTIME_PLAN Phase 9): it
builds only the registry component (`wasm32-wasip2`, release) and the release
`tangram-host`, generates `<data-dir>/apps.toml` with a single registry app
whose `remote` is `<remote>/registry/sync`, then runs the host on `--bind`.
The host syncs the registry DOCUMENT with the remote (the fleet's desired
state) and converges the rest of the fleet from it — each app fetched and
sha256-verified from its pinned `component_url` via the Phase-8
content-addressed cache, and each app's OWN document replicated through a
derived `<remote>/<app>/sync` remote. So one `remote` setting syncs both the
fleet membership AND the data, and an install/remove on any host propagates
everywhere. Offline fallback is whatever the persisted registry doc and the
component cache already hold. The replica's app list is discovered from its
`/api/fleet`, so `status`/`stop`/convergence track whatever the fleet
currently runs. Per-host secrets stay per-host: the synced doc carries only
env KEYS and `${VAR}` references; their VALUES must be in this host's
environment / `.env` to resolve (whichever of
`NUTRITION_STRATEGY`/`CALORIENINJAS_API_KEY`/`ANTHROPIC_*` are set are passed
through to the host process), and a missing one runs that app degraded
(nutrition → offline), never leaking. The pid file distinguishes the modes
(`replica.pid` = native shell, `replica-wasm.pid` = wasm host), so `status`
and `stop` work on either, and `connect` in either mode replaces a running
replica of the other.

Both `connect` and `status` also compare the local and remote nutrition
`/api/capabilities`: if the remote can resolve meal descriptions
(`description_input: true`) but the local replica cannot, a prominent
REMINDER prints telling the operator to add `CALORIENINJAS_API_KEY` to the
local `.env` (copied from the remote's `.env`). With the key present and
`NUTRITION_STRATEGY` unset, the calorieninjas strategy auto-enables — no
extra configuration needed.

After running, relay the printed URLs (and the API-key reminder, if it
printed) to the user. If connect fails, read the log path it prints (common
causes: tunnel not up, bind port already in use).
