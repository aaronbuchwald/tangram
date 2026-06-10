---
name: local-replica
description: Set up, check, or stop a local Tangram replica that syncs to a remote instance (typically through an SSH tunnel). Use when asked to run a local replica, connect a local instance to the remote, check replica sync status, or stop the local replica.
argument-hint: connect|status|stop [--remote <ws base>] [--bind <addr:port>] [--data-dir <dir>] [--env KEY=VALUE]...
allowed-tools: Bash
---

Manage a local Tangram replica that replicates with a remote instance. All
logic lives in `replica.sh`; run it and interpret the output.

```bash
bash .agents/skills/local-replica/replica.sh connect [--remote <ws base>] [--bind <addr:port>] [--data-dir <dir>] [--env KEY=VALUE]...
bash .agents/skills/local-replica/replica.sh status
bash .agents/skills/local-replica/replica.sh stop
```

Pass the user's arguments through: $ARGUMENTS

## Defaults

- `--remote`: `ws://127.0.0.1:8080` — the remote as seen through an SSH
  tunnel (`ssh tangram` with `LocalForward 8080`). Pass a different base for
  a direct/tailnet remote, e.g. `ws://my-host:8080`.
- `--bind`: `127.0.0.1:8090` (off the tunnel-forwarded ports).
- `--data-dir`: `data-replica` — kept separate from `data/` so a replica can
  coexist with a primary instance in the same checkout.
- `--env KEY=VALUE` (repeatable, connect only): extra environment exported to
  the started replica, e.g. `--env NUTRITION_STRATEGY=offline`.

## Behavior

`connect` verifies the remote is reachable (clear error telling the user to
start their SSH tunnel if not), builds the release shell, starts it in the
background (pid file + log in the data dir; replaces a previous replica),
waits until both apps' states converge with the remote, and prints the local
and remote URLs plus the `claude mcp add` commands for pointing local MCP at
the replica. `status` reports process/tunnel health and per-app sync
convergence. `stop` sends SIGINT to the replica.

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
