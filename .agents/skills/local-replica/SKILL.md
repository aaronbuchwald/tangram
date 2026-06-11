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
  mode the variable is additionally granted to the nutrition component (as a
  `${KEY}` reference in the generated `apps.toml`, so the value never lands
  in a file).
- `--wasm` (connect only): run the replica as WASM components under
  `tangram-host` instead of the native shell.

## Behavior

`connect` verifies the remote is reachable (clear error telling the user to
start their SSH tunnel if not), builds the release shell, starts it in the
background (pid file + log in the data dir; replaces a previous replica),
waits until both apps' states converge with the remote, and prints the local
and remote URLs plus the `claude mcp add` commands for pointing local MCP at
the replica. `status` reports process/tunnel health and per-app sync
convergence. `stop` sends SIGINT to the replica.

`connect --wasm` does the same against the WASM runtime: it builds the
notes/nutrition components (`wasm32-wasip2`, release) and the release
`tangram-host`, generates `<data-dir>/apps.toml` (per-app scratch data dirs
under the data dir, per-app `remote` pointing at the configured remote base,
nutrition's `api.calorieninjas.com` allowlist, and `${VAR}` env grants for
whichever of `NUTRITION_STRATEGY`/`CALORIENINJAS_API_KEY`/`ANTHROPIC_*` are
set in the environment or the repo `.env` — mirroring the native shell's
strategy selection), then runs the host on `--bind`. The pid file
distinguishes the modes (`replica.pid` = native shell, `replica-wasm.pid` =
wasm host), so `status` and `stop` work on either, and `connect` in either
mode replaces a running replica of the other.

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
