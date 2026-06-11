#!/usr/bin/env bash
# Manage a local Tangram replica that syncs to a remote instance.
#
# Usage:
#   replica.sh connect [--wasm] [--remote <http base>] [--bind <addr:port>]
#                      [--data-dir <dir>] [--remote-token <token>]
#                      [--env KEY=VALUE]...
#   replica.sh status  [--remote <http base>] [--bind <addr:port>]
#                      [--remote-token <token>]
#   replica.sh stop
#
# Defaults: --remote http://127.0.0.1:8080 (a remote reached through an SSH
# tunnel, e.g. `ssh tangram`), --bind 127.0.0.1:8090, --data-dir data-replica.
# --env (repeatable) exports extra environment to the started replica.
# --wasm (connect only) runs the replica as WASM components under
# tangram-host, FEDERATED with the remote (RUNTIME_PLAN Phase 9): it starts
# only a registry app whose `remote` is <remote>/registry/sync and lets
# convergence pull the rest of the fleet down — each app fetched+verified
# from its pinned component_url+sha256 (the Phase-8 content-addressed cache),
# so an install/remove on ANY host propagates to this replica and vice versa.
# Offline fallback: whatever the registry doc + component cache already hold.
# (The native shell path is unchanged: notes+nutrition over TANGRAM_REMOTE_*.)
# --remote-token (or a TANGRAM_REMOTE_TOKEN in the environment) is sent as
# Authorization: Bearer on every sync request — required when the remote is
# a tangram-host tenant namespace (--remote http://host:8080/t/<tenant>).
set -euo pipefail

usage() {
  sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
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
REMOTE_TOKEN="${TANGRAM_REMOTE_TOKEN:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    --remote)       REMOTE="${2:?--remote needs a value}"; shift 2 ;;
    --bind)         BIND="${2:?--bind needs a value}"; shift 2 ;;
    --data-dir)     DATA_DIR="${2:?--data-dir needs a value}"; shift 2 ;;
    --env)          ENV_VARS+=("${2:?--env needs KEY=VALUE}"); shift 2 ;;
    --remote-token) REMOTE_TOKEN="${2:?--remote-token needs a value}"; shift 2 ;;
    --wasm)         WASM=yes; shift ;;
    -h|--help) usage 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

# Curl args for talking to the remote (tenant namespaces need the bearer).
REMOTE_CURL_AUTH=()
[ -n "$REMOTE_TOKEN" ] && REMOTE_CURL_AUTH=(-H "Authorization: Bearer $REMOTE_TOKEN")

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

# Fetch state JSON for an app from a base http URL ($1) and app prefix ($2);
# extra args (e.g. the remote bearer header) pass through to curl.
state_of() {
  local base="$1" app="$2"
  shift 2
  curl -sf --max-time 3 "$@" "$base/$app/api/state"
}

# state_of against the remote, with the bearer when one is configured.
remote_state_of() { state_of "$REMOTE_HTTP" "$1" ${REMOTE_CURL_AUTH[@]+"${REMOTE_CURL_AUTH[@]}"}; }

# The apps to check for convergence. In --wasm (registry-bootstrap) mode the
# fleet is dynamic — discover the running, enabled, non-registry apps from the
# local host's /api/fleet. The native shell doesn't serve /api/fleet, so the
# query yields nothing and we fall back to the fixed notes+nutrition pair.
replica_apps() {
  local fleet apps
  fleet="$(curl -sf --max-time 3 "http://$BIND/api/fleet" 2>/dev/null || true)"
  if [ -n "$fleet" ]; then
    # Prefer a real JSON parse (skip the registry + disabled apps); fall back
    # to a grep over the name fields if python3 isn't available.
    if command -v python3 >/dev/null 2>&1; then
      apps="$(printf '%s' "$fleet" | python3 -c '
import sys, json
try:
    f = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for a in f.get("apps", []):
    if not a.get("registry") and a.get("enabled", True):
        print(a["name"])
' 2>/dev/null || true)"
    else
      apps="$(printf '%s' "$fleet" | grep -o '"name":"[^"]*"' | cut -d'"' -f4)"
    fi
    if [ -n "$apps" ]; then printf '%s\n' "$apps"; return 0; fi
  fi
  printf '%s\n' notes nutrition
}

# Compare nutrition capabilities local vs remote: if the remote can resolve
# meal descriptions but this replica cannot, the divergence is almost always
# a missing API key — remind the operator loudly. (Served by both modes: the
# native route and tangram-host's describe()-backed /api/capabilities.)
check_capabilities() {
  local local_caps remote_caps local_di remote_di
  local_caps="$(curl -sf --max-time 3 "http://$BIND/nutrition/api/capabilities" || true)"
  remote_caps="$(curl -sf --max-time 3 ${REMOTE_CURL_AUTH[@]+"${REMOTE_CURL_AUTH[@]}"} \
    "$REMOTE_HTTP/nutrition/api/capabilities" || true)"
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
    if curl -sf --max-time 3 ${REMOTE_CURL_AUTH[@]+"${REMOTE_CURL_AUTH[@]}"} "$REMOTE_HTTP/" >/dev/null; then
      echo "remote: reachable at $REMOTE_HTTP/ (tunnel up)"
    else
      echo "remote: NOT reachable at $REMOTE_HTTP/ — is the SSH tunnel up?"
      exit 1
    fi
    # Apps are discovered from the local fleet (wasm registry-bootstrap mode),
    # falling back to notes+nutrition for the native shell.
    for app in $(replica_apps); do
      local_state="$(state_of "http://$BIND" "$app" || true)"
      remote_state="$(remote_state_of "$app" || true)"
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
# means the `ssh tangram` tunnel (LocalForward 8080) is up. (A tenant
# namespace also answers 401 without the bearer — the auth args matter here.)
curl -sf --max-time 3 ${REMOTE_CURL_AUTH[@]+"${REMOTE_CURL_AUTH[@]}"} "$REMOTE_HTTP/" >/dev/null \
  || die "remote not reachable at $REMOTE_HTTP/ — start your SSH tunnel (ssh tangram) first \
(tenant remotes also need --remote-token)"

if [ -n "$WASM" ]; then
  # ── wasm mode: FEDERATED registry-bootstrap under tangram-host ───────────
  # We start ONLY a registry app whose document syncs with the remote's
  # registry (RUNTIME_PLAN Phase 9). The host converges the rest of the fleet
  # from that synced desired state — every app fetched+verified from its
  # pinned component_url+sha256 via the Phase-8 content-addressed cache — so
  # an install/remove on ANY host (this replica included) propagates fleet-
  # wide. The only component we build/ship locally is the registry itself.
  cargo build -p tangram-registry --lib --target wasm32-wasip2 --release
  cargo build --release -p tangram-host

  for f in "$PID_FILE" "$WASM_PID_FILE"; do
    if pid="$(pid_from "$f")"; then
      echo "replacing running replica (pid $pid)"
      kill -INT "$pid" || true
      sleep 0.5
    fi
  done

  mkdir -p "$DATA_DIR/registry"
  ABS_DATA="$(cd "$DATA_DIR" && pwd)"
  APPS_TOML="$ABS_DATA/apps.toml"

  # Set non-empty in the environment, or assigned in the repo .env (which
  # tangram-host loads via dotenvy — same as the native shell).
  have_var() {
    [ -n "$(printenv "$1" 2>/dev/null)" ] && return 0
    [ -f .env ] && grep -qE "^$1=." .env
  }

  # Per-host secrets stay per-host (Phase 9 §3): the synced registry doc
  # carries only env KEYS and ${VAR} references, never values. For those
  # references to resolve on THIS replica, the host process must see the
  # values in its environment — so collect whichever strategy/secret vars are
  # present (env or repo .env) and pass them straight through to the host.
  # A var that isn't set here simply expands to empty: the app runs degraded
  # (nutrition → offline), exactly as Phase 9 intends, and check_capabilities
  # reminds about CALORIENINJAS_API_KEY below.
  PASSTHROUGH_ENV=()
  for var in NUTRITION_STRATEGY CALORIENINJAS_API_KEY ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN; do
    if have_var "$var"; then
      PASSTHROUGH_ENV+=("$var=$(printenv "$var" 2>/dev/null || grep -E "^$var=" .env | head -1 | cut -d= -f2-)")
    fi
  done

  # The bearer for a private remote (a tenant namespace) is referenced as
  # ${TANGRAM_REMOTE_TOKEN} so the secret stays out of the generated file.
  remote_token_line() {
    if [ -n "$REMOTE_TOKEN" ]; then
      echo "remote_token = \"\${TANGRAM_REMOTE_TOKEN}\""
    fi
  }
  {
    echo "# Generated by replica.sh connect --wasm — federated registry-bootstrap."
    echo "# Edits converge live. The rest of the fleet arrives via the registry"
    echo "# document syncing with $REMOTE/registry/sync (Phase 9)."
    echo
    echo "[apps.registry]"
    echo "component = \"$DIR/target/wasm32-wasip2/release/registry.wasm\""
    echo "ui = \"$DIR/apps/registry/ui\""
    echo "data_dir = \"$ABS_DATA/registry\""
    echo "registry = true"
    echo "remote = \"$REMOTE/registry/sync\""
    remote_token_line
  } > "$APPS_TOML"

  nohup env \
    BIND_ADDR="$BIND" \
    TANGRAM_REMOTE_TOKEN="$REMOTE_TOKEN" \
    ${PASSTHROUGH_ENV[@]+"${PASSTHROUGH_ENV[@]}"} \
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
    TANGRAM_REMOTE_TOKEN="$REMOTE_TOKEN" \
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

# In --wasm mode the fleet arrives via the registry document: give the host a
# few seconds to sync the registry and fetch+converge the apps it lists before
# we enumerate them. (Native mode serves notes+nutrition immediately.)
if [ -n "$WASM" ]; then
  for _ in $(seq 1 40); do
    [ "$(replica_apps | grep -vc '^$')" -gt 0 ] && break
    sleep 0.5
  done
fi

APPS="$(replica_apps)"
synced=""
for _ in $(seq 1 20); do
  synced=yes
  for app in $APPS; do
    [ "$(state_of "http://$BIND" "$app" || true)" = "$(remote_state_of "$app" || true)" ] || synced=""
  done
  [ -n "$synced" ] && break
  sleep 0.5
done
[ -n "$synced" ] || echo "warning: replica started but states have not converged yet (check $RUN_LOG_FILE)"

echo "OK: local replica running ($RUN_MODE $(cat "$RUN_PID_FILE")), synced to $REMOTE"
if [ -z "$APPS" ]; then
  echo "  (no apps converged yet — the registry may still be pulling the fleet; check $RUN_LOG_FILE)"
fi
echo
echo "  local replica UI / MCP per app:"
for app in $APPS; do
  echo "    $app  http://$BIND/$app/   (mcp: claude mcp add --transport http $app http://$BIND/$app/mcp)"
done
echo
echo "  remote UI (tunnel): $REMOTE_HTTP/"
check_capabilities
