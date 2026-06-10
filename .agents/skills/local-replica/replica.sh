#!/usr/bin/env bash
# Manage a local Tangram replica that syncs to a remote instance.
#
# Usage:
#   replica.sh connect [--remote <ws base>] [--bind <addr:port>] [--data-dir <dir>]
#                      [--env KEY=VALUE]...
#   replica.sh status  [--remote <ws base>] [--bind <addr:port>]
#   replica.sh stop
#
# Defaults: --remote ws://127.0.0.1:8080 (a remote reached through an SSH
# tunnel, e.g. `ssh tangram`), --bind 127.0.0.1:8090, --data-dir data-replica.
# --env (repeatable) exports extra environment to the started replica.
set -euo pipefail

usage() {
  sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-1}"
}

die() { echo "error: $*" >&2; exit 1; }

SUBCMD="${1:-}"
case "$SUBCMD" in
  connect|status|stop) shift ;;
  -h|--help) usage 0 ;;
  *) usage ;;
esac

REMOTE="ws://127.0.0.1:8080"
BIND="127.0.0.1:8090"
DATA_DIR="data-replica"
ENV_VARS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --remote)   REMOTE="${2:?--remote needs a value}"; shift 2 ;;
    --bind)     BIND="${2:?--bind needs a value}"; shift 2 ;;
    --data-dir) DATA_DIR="${2:?--data-dir needs a value}"; shift 2 ;;
    --env)      ENV_VARS+=("${2:?--env needs KEY=VALUE}"); shift 2 ;;
    -h|--help) usage 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

DIR="$(git rev-parse --show-toplevel 2>/dev/null)" \
  || die "not inside a git repo; run from your tangram checkout"
cd "$DIR"
REMOTE="${REMOTE%/}"
REMOTE_HTTP="${REMOTE/ws:/http:}"
REMOTE_HTTP="${REMOTE_HTTP/wss:/https:}"
PID_FILE="$DATA_DIR/replica.pid"
LOG_FILE="$DATA_DIR/replica.log"

replica_pid() {
  [ -f "$PID_FILE" ] || return 1
  local pid
  pid="$(cat "$PID_FILE")"
  kill -0 "$pid" 2>/dev/null || return 1
  echo "$pid"
}

# Fetch state JSON for an app from a base http URL ($1) and app prefix ($2).
state_of() { curl -sf --max-time 3 "$1/$2/api/state"; }

# Compare nutrition capabilities local vs remote: if the remote can resolve
# meal descriptions but this replica cannot, the divergence is almost always
# a missing API key — remind the operator loudly.
check_capabilities() {
  local local_caps remote_caps local_di remote_di
  local_caps="$(curl -sf --max-time 3 "http://$BIND/nutrition/api/capabilities" || true)"
  remote_caps="$(curl -sf --max-time 3 "$REMOTE_HTTP/nutrition/api/capabilities" || true)"
  [ -n "$local_caps" ] && [ -n "$remote_caps" ] || return 0
  local_di="$(printf '%s' "$local_caps" | grep -o '"description_input":[a-z]*' | cut -d: -f2)"
  remote_di="$(printf '%s' "$remote_caps" | grep -o '"description_input":[a-z]*' | cut -d: -f2)"
  if [ "$remote_di" = "true" ] && [ "$local_di" != "true" ]; then
    echo
    echo "==============================================================================="
    echo "REMINDER: the remote can resolve meal descriptions but this replica cannot —"
    echo "add CALORIENINJAS_API_KEY to .env on this machine (copy it from the remote's"
    echo ".env) to enable description-based logging locally. (With the key present and"
    echo "NUTRITION_STRATEGY unset, the calorieninjas strategy auto-enables.)"
    echo "==============================================================================="
  fi
}

case "$SUBCMD" in
  stop)
    if pid="$(replica_pid)"; then
      kill -INT "$pid"
      echo "OK: stopped local replica (pid $pid)"
    else
      echo "no local replica running ($PID_FILE)"
    fi
    exit 0
    ;;

  status)
    if pid="$(replica_pid)"; then
      echo "local replica: running (pid $pid) at http://$BIND/"
    else
      echo "local replica: not running"
    fi
    if curl -sf --max-time 3 "$REMOTE_HTTP/" >/dev/null; then
      echo "remote: reachable at $REMOTE_HTTP/ (tunnel up)"
    else
      echo "remote: NOT reachable at $REMOTE_HTTP/ — is the SSH tunnel up?"
      exit 1
    fi
    for app in notes nutrition; do
      local_state="$(state_of "http://$BIND" "$app" || true)"
      remote_state="$(state_of "$REMOTE_HTTP" "$app" || true)"
      if [ -n "$local_state" ] && [ "$local_state" = "$remote_state" ]; then
        echo "$app: in sync"
      else
        echo "$app: states differ (may still be converging)"
      fi
    done
    check_capabilities
    exit 0
    ;;
esac

# ── connect ────────────────────────────────────────────────────────────────

# The remote must be reachable BEFORE we start: with the default remote this
# means the `ssh tangram` tunnel (LocalForward 8080) is up.
curl -sf --max-time 3 "$REMOTE_HTTP/" >/dev/null \
  || die "remote not reachable at $REMOTE_HTTP/ — start your SSH tunnel (ssh tangram) first"

cargo build --release -p tangram-shell

if pid="$(replica_pid)"; then
  echo "replacing running replica (pid $pid)"
  kill -INT "$pid" || true
  sleep 0.5
fi

mkdir -p "$DATA_DIR"
nohup env \
  BIND_ADDR="$BIND" \
  TANGRAM_DATA_DIR="$DATA_DIR" \
  TANGRAM_REMOTE_NOTES="$REMOTE/notes/sync" \
  TANGRAM_REMOTE_NUTRITION="$REMOTE/nutrition/sync" \
  ${ENV_VARS[@]+"${ENV_VARS[@]}"} \
  "$DIR/target/release/tangram-shell" >"$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"

# Wait for the local replica to serve, then for both apps to converge with
# the remote (initial sync usually lands well under a second).
for _ in $(seq 1 40); do
  curl -sf --max-time 1 "http://$BIND/" >/dev/null && break
  sleep 0.25
done
curl -sf "http://$BIND/" >/dev/null || { tail -20 "$LOG_FILE"; die "replica did not start (see $LOG_FILE)"; }

synced=""
for _ in $(seq 1 20); do
  synced=yes
  for app in notes nutrition; do
    [ "$(state_of "http://$BIND" "$app" || true)" = "$(state_of "$REMOTE_HTTP" "$app" || true)" ] || synced=""
  done
  [ -n "$synced" ] && break
  sleep 0.5
done
[ -n "$synced" ] || echo "warning: replica started but states have not converged yet (check $LOG_FILE)"

echo "OK: local replica running (pid $(cat "$PID_FILE")), synced to $REMOTE"
echo
echo "  local replica UI:   http://$BIND/notes/   http://$BIND/nutrition/"
echo "  remote UI (tunnel): $REMOTE_HTTP/notes/   $REMOTE_HTTP/nutrition/"
echo
echo "  point local MCP at the replica:"
echo "    claude mcp add --transport http notes     http://$BIND/notes/mcp"
echo "    claude mcp add --transport http nutrition http://$BIND/nutrition/mcp"
check_capabilities
