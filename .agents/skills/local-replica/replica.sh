#!/usr/bin/env bash
# Manage a local Tangram replica that syncs to a remote instance.
#
# Usage:
#   replica.sh connect [--wasm] [--remote <http base>] [--bind <addr:port>]
#                      [--data-dir <dir>] [--env KEY=VALUE]...
#   replica.sh status  [--remote <http base>] [--bind <addr:port>]
#   replica.sh stop
#
# Defaults: --remote http://127.0.0.1:8080 (a remote reached through an SSH
# tunnel, e.g. `ssh tangram`), --bind 127.0.0.1:8090, --data-dir data-replica.
# --env (repeatable) exports extra environment to the started replica.
# --wasm (connect only) runs the replica as WASM components under
# tangram-host instead of the native shell — same surfaces, same sync.
set -euo pipefail

usage() {
  sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-1}"
}

die() { echo "error: $*" >&2; exit 1; }

SUBCMD="${1:-}"
case "$SUBCMD" in
  connect|status|stop) shift ;;
  -h|--help) usage 0 ;;
  *) usage ;;
esac

REMOTE="http://127.0.0.1:8080"
BIND="127.0.0.1:8090"
DATA_DIR="data-replica"
ENV_VARS=()
WASM=""

while [ $# -gt 0 ]; do
  case "$1" in
    --remote)   REMOTE="${2:?--remote needs a value}"; shift 2 ;;
    --bind)     BIND="${2:?--bind needs a value}"; shift 2 ;;
    --data-dir) DATA_DIR="${2:?--data-dir needs a value}"; shift 2 ;;
    --env)      ENV_VARS+=("${2:?--env needs KEY=VALUE}"); shift 2 ;;
    --wasm)     WASM=yes; shift ;;
    -h|--help) usage 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "$WASM" ] && [ "$SUBCMD" != "connect" ] \
  && die "--wasm only applies to connect (status/stop detect the mode from the pid file)"

DIR="$(git rev-parse --show-toplevel 2>/dev/null)" \
  || die "not inside a git repo; run from your tangram checkout"
cd "$DIR"
REMOTE="${REMOTE%/}"
# Accept legacy ws:// bases (the SDK rewrites them too); curl needs http://.
REMOTE="${REMOTE/#ws:/http:}"
REMOTE="${REMOTE/#wss:/https:}"
REMOTE_HTTP="$REMOTE"
# Two pid files distinguish the two modes: replica.pid is the native shell,
# replica-wasm.pid is tangram-host running the WASM components.
PID_FILE="$DATA_DIR/replica.pid"
LOG_FILE="$DATA_DIR/replica.log"
WASM_PID_FILE="$DATA_DIR/replica-wasm.pid"
WASM_LOG_FILE="$DATA_DIR/replica-wasm.log"

pid_from() {
  [ -f "$1" ] || return 1
  local pid
  pid="$(cat "$1")"
  kill -0 "$pid" 2>/dev/null || return 1
  echo "$pid"
}

replica_pid() { pid_from "$PID_FILE"; }
wasm_replica_pid() { pid_from "$WASM_PID_FILE"; }

# Fetch state JSON for an app from a base http URL ($1) and app prefix ($2).
state_of() { curl -sf --max-time 3 "$1/$2/api/state"; }

# Compare nutrition capabilities local vs remote: if the remote can resolve
# meal descriptions but this replica cannot, the divergence is almost always
# a missing API key — remind the operator loudly. (Served by both modes: the
# native route and tangram-host's describe()-backed /api/capabilities.)
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
    stopped=""
    if pid="$(replica_pid)"; then
      kill -INT "$pid"
      echo "OK: stopped local replica (pid $pid)"
      stopped=yes
    fi
    if pid="$(wasm_replica_pid)"; then
      kill -INT "$pid"
      echo "OK: stopped local replica (wasm host, pid $pid)"
      stopped=yes
    fi
    [ -n "$stopped" ] || echo "no local replica running ($PID_FILE)"
    exit 0
    ;;

  status)
    if pid="$(replica_pid)"; then
      echo "local replica: running (pid $pid) at http://$BIND/"
    elif pid="$(wasm_replica_pid)"; then
      echo "local replica: running (wasm host, pid $pid) at http://$BIND/"
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

if [ -n "$WASM" ]; then
  # ── wasm mode: components + tangram-host ─────────────────────────────────
  cargo build -p tangram-notes -p tangram-nutrition --lib --target wasm32-wasip2 --release
  cargo build --release -p tangram-host

  for f in "$PID_FILE" "$WASM_PID_FILE"; do
    if pid="$(pid_from "$f")"; then
      echo "replacing running replica (pid $pid)"
      kill -INT "$pid" || true
      sleep 0.5
    fi
  done

  mkdir -p "$DATA_DIR/notes" "$DATA_DIR/nutrition"
  ABS_DATA="$(cd "$DATA_DIR" && pwd)"
  APPS_TOML="$ABS_DATA/apps.toml"

  # Set non-empty in the environment, or assigned in the repo .env (which
  # tangram-host loads via dotenvy — same as the native shell).
  have_var() {
    [ -n "$(printenv "$1" 2>/dev/null)" ] && return 0
    [ -f .env ] && grep -qE "^$1=." .env
  }

  # Nutrition env, mirroring the native path: the native shell reads its
  # strategy selection from the process env / .env, so grant the component
  # exactly the strategy vars that resolve — and ONLY those: an empty
  # NUTRITION_STRATEGY would force the offline fallback and defeat the
  # CALORIENINJAS_API_KEY auto-enable (which check_capabilities reminds
  # about). Values are written as ${VAR} references so secrets stay in the
  # environment / .env, never in the generated file.
  NUTRITION_ENV_KEYS=()
  for var in NUTRITION_STRATEGY CALORIENINJAS_API_KEY ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN; do
    have_var "$var" && NUTRITION_ENV_KEYS+=("$var")
  done
  # --env KEY=VALUE pairs are granted to the nutrition component the same
  # way (and exported to the host process below, where ${KEY} expands).
  for kv in ${ENV_VARS[@]+"${ENV_VARS[@]}"}; do
    key="${kv%%=*}"
    dup=""
    for existing in ${NUTRITION_ENV_KEYS[@]+"${NUTRITION_ENV_KEYS[@]}"}; do
      [ "$existing" = "$key" ] && dup=yes
    done
    [ -n "$dup" ] || NUTRITION_ENV_KEYS+=("$key")
  done

  {
    echo "# Generated by replica.sh connect --wasm — edits converge live."
    echo
    echo "[apps.notes]"
    echo "component = \"$DIR/target/wasm32-wasip2/release/notes.wasm\""
    echo "ui = \"$DIR/apps/notes/ui\""
    echo "data_dir = \"$ABS_DATA/notes\""
    echo "remote = \"$REMOTE/notes/sync\""
    echo
    echo "[apps.nutrition]"
    echo "component = \"$DIR/target/wasm32-wasip2/release/nutrition.wasm\""
    echo "ui = \"$DIR/apps/nutrition/ui\""
    echo "data_dir = \"$ABS_DATA/nutrition\""
    echo "remote = \"$REMOTE/nutrition/sync\""
    echo "# The component's ENTIRE outbound-network grant (same as apps.toml)."
    echo "allow_hosts = [\"api.calorieninjas.com\"]"
    if [ "${#NUTRITION_ENV_KEYS[@]}" -gt 0 ]; then
      echo
      echo "[apps.nutrition.env]"
      for key in "${NUTRITION_ENV_KEYS[@]}"; do
        echo "$key = \"\${$key}\""
      done
    fi
  } > "$APPS_TOML"

  nohup env \
    BIND_ADDR="$BIND" \
    ${ENV_VARS[@]+"${ENV_VARS[@]}"} \
    "$DIR/target/release/tangram-host" "$APPS_TOML" >"$WASM_LOG_FILE" 2>&1 &
  echo $! > "$WASM_PID_FILE"
  RUN_PID_FILE="$WASM_PID_FILE"
  RUN_LOG_FILE="$WASM_LOG_FILE"
  RUN_MODE="wasm host, pid"
else
  # ── native mode (the default; unchanged) ─────────────────────────────────
  cargo build --release -p tangram-shell

  if pid="$(replica_pid)"; then
    echo "replacing running replica (pid $pid)"
    kill -INT "$pid" || true
    sleep 0.5
  fi
  if pid="$(wasm_replica_pid)"; then
    echo "replacing running replica (wasm host, pid $pid)"
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
  RUN_PID_FILE="$PID_FILE"
  RUN_LOG_FILE="$LOG_FILE"
  RUN_MODE="pid"
fi

# Wait for the local replica to serve, then for both apps to converge with
# the remote (initial sync usually lands well under a second).
for _ in $(seq 1 40); do
  curl -sf --max-time 1 "http://$BIND/" >/dev/null && break
  sleep 0.25
done
curl -sf "http://$BIND/" >/dev/null || { tail -20 "$RUN_LOG_FILE"; die "replica did not start (see $RUN_LOG_FILE)"; }

synced=""
for _ in $(seq 1 20); do
  synced=yes
  for app in notes nutrition; do
    [ "$(state_of "http://$BIND" "$app" || true)" = "$(state_of "$REMOTE_HTTP" "$app" || true)" ] || synced=""
  done
  [ -n "$synced" ] && break
  sleep 0.5
done
[ -n "$synced" ] || echo "warning: replica started but states have not converged yet (check $RUN_LOG_FILE)"

echo "OK: local replica running ($RUN_MODE $(cat "$RUN_PID_FILE")), synced to $REMOTE"
echo
echo "  local replica UI:   http://$BIND/notes/   http://$BIND/nutrition/"
echo "  remote UI (tunnel): $REMOTE_HTTP/notes/   $REMOTE_HTTP/nutrition/"
echo
echo "  point local MCP at the replica:"
echo "    claude mcp add --transport http notes     http://$BIND/notes/mcp"
echo "    claude mcp add --transport http nutrition http://$BIND/nutrition/mcp"
check_capabilities
