#!/usr/bin/env bash
# End-to-end regression test for the Cloudflare sync relay (cloud/cloudflare)
# running locally under `wrangler dev` (miniflare). This automates the manual
# verification from RUNTIME_PLAN Phase 4 / checkpoint-1:
#
#   native A  <-- TANGRAM_REMOTE -->  relay (miniflare)  <-- TANGRAM_REMOTE -->  native B
#
# Asserted, in order:
#   a. empty-relay genesis convergence: a note added on A shows up in the
#      relay's stored document, with exactly the expected note count (a forked
#      genesis would shadow notes into a rival container — see the genesis
#      rule in docs/SYNC_PROTOCOL.md)
#   b. bidirectional sync through the relay: A's note appears on B, a note
#      written on B appears on A, end-to-end latency < 5s
#   c. relay restart persistence: kill wrangler, restart on the same state
#      dir; the document is intact and sync resumes (a post-restart write on
#      A reaches B)
#   d. clean teardown: every spawned process dead, scratch dirs removed
#
# Self-contained and repeatable: scratch HOME/data dirs and an isolated
# `.wrangler` state dir live under one mktemp dir (removed on exit), ports are
# probed from the ephemeral 19xxx range, and cleanup is trap-based with
# explicit PID tracking (never pkill-by-pattern). Safe to re-run; never
# touches a live instance on :8080.
#
# Usage:  bash scripts/e2e-cloudflare-sync.sh
#    or:  cargo test -p tangram-host -- --ignored e2e_cloudflare
# Needs:  cargo, node >= 20.3 (for the pinned wrangler ~4.86), npm, curl, jq.

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CF_DIR="$REPO_ROOT/cloud/cloudflare"
SYNC_LATENCY_BOUND_MS=5000

WORK_DIR=$(mktemp -d /tmp/tangram-e2e-cf.XXXXXX)
STATE_DIR="$WORK_DIR/wrangler-state" # survives the in-test relay restart
WRANGLER_LOG="$WORK_DIR/wrangler.log"
LOG_A="$WORK_DIR/notes-a.log"
LOG_B="$WORK_DIR/notes-b.log"

WRANGLER_PID="" # leader of its own process group (setsid)
PID_A=""
PID_B=""
FAILED=1 # cleared just before the final summary

log() { printf '[e2e] %s\n' "$*"; }

dump_logs() {
    for f in "$WRANGLER_LOG" "$LOG_A" "$LOG_B"; do
        [[ -s $f ]] || continue
        echo "──── tail -n 60 $f ────" >&2
        tail -n 60 "$f" >&2
    done
}

fail() {
    echo "[e2e] FAIL: $*" >&2
    exit 1
}

# ── process management (explicit PIDs only) ──────────────────────────────────

# Stop one tracked process: TERM, wait up to 5s, then KILL. (CONT first in
# case the test left it SIGSTOPped — a stopped process won't see the TERM.)
stop_pid() {
    local pid=$1
    [[ -n $pid ]] || return 0
    kill -CONT "$pid" 2>/dev/null || true
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
    done
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

# Stop wrangler and everything it spawned (workerd, …) via its process group;
# we started it with setsid so the group is exactly its own tree.
stop_wrangler() {
    local pid=$1
    [[ -n $pid ]] || return 0
    kill -TERM -- "-$pid" 2>/dev/null || true
    for _ in $(seq 1 100); do
        # the group is gone when no process lists it as its pgid
        [[ -z $(ps -o pid= -g "$pid" 2>/dev/null) ]] && break
        sleep 0.1
    done
    kill -KILL -- "-$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

assert_group_dead() {
    local pid=$1 what=$2
    [[ -n $pid ]] || return 0
    local survivors
    survivors=$(ps -o pid=,comm= -g "$pid" 2>/dev/null || true)
    [[ -z $survivors ]] || fail "stray $what processes survived teardown: $survivors"
}

cleanup() {
    local status=$?
    trap - EXIT
    stop_pid "$PID_B"
    stop_pid "$PID_A"
    stop_wrangler "$WRANGLER_PID"
    if [[ $status -ne 0 || $FAILED -ne 0 ]]; then
        echo "[e2e] FAILED — recent process output:" >&2
        dump_logs
    fi
    rm -rf "$WORK_DIR"
    [[ $status -ne 0 ]] && exit "$status"
    [[ $FAILED -eq 0 ]] || exit 1
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ── small helpers ────────────────────────────────────────────────────────────

now_ms() { date +%s%3N; }

port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

# pick_port <varname> — a free port in the ephemeral 19xxx test range,
# distinct from prior picks (assigned to the named variable, not echoed, so
# the dedupe survives — command substitution would fork the bookkeeping away).
PICKED_PORTS=" "
pick_port() {
    local port
    for _ in $(seq 1 200); do
        port=$((19000 + RANDOM % 1000))
        [[ $PICKED_PORTS == *" $port "* ]] && continue
        if ! port_in_use "$port"; then
            PICKED_PORTS+="$port "
            printf -v "$1" '%s' "$port"
            return 0
        fi
    done
    fail "could not find a free 19xxx port"
}

# wait_for <description> <timeout-seconds> <command…> — poll until success.
wait_for() {
    local desc=$1 timeout=$2
    shift 2
    local deadline=$((SECONDS + timeout))
    until "$@" >/dev/null 2>&1; do
        ((SECONDS < deadline)) || fail "timed out (${timeout}s) waiting for: $desc"
        sleep 0.2
    done
}

# state_url <base> → "<base>/api/state" responding with {"notes":[…]}
note_count() { curl -fsS "$1/api/state" | jq -er '.notes | length'; }
has_note() { curl -fsS "$1/api/state" | jq -e --arg t "$2" '.notes | map(.text) | index($t) != null'; }
assert_note_count() { # base expected what
    local got
    got=$(note_count "$1") || fail "$3: GET $1/api/state failed"
    [[ $got == "$2" ]] || fail "$3: expected exactly $2 note(s), got $got (duplicate/forked containers?)"
}

# Add a note via the JSON actions API and confirm acceptance.
add_note() { # base text
    curl -fsS -X POST "$1/api/actions/add_note" \
        -H 'Content-Type: application/json' \
        -d "{\"text\": \"$2\"}" | jq -e '.result' >/dev/null ||
        fail "add_note '$2' on $1 was rejected"
}

# Measure how long until $text is visible at $base; assert the bound.
measure_propagation() { # base text what
    local t0 elapsed
    t0=$(now_ms)
    wait_for "$3" 30 has_note "$1" "$2"
    elapsed=$(($(now_ms) - t0))
    log "$3: ${elapsed}ms"
    ((elapsed < SYNC_LATENCY_BOUND_MS)) ||
        fail "$3 took ${elapsed}ms (bound: ${SYNC_LATENCY_BOUND_MS}ms)"
}

# ── prerequisites ────────────────────────────────────────────────────────────

T_START=$(now_ms)
log "scratch dir: $WORK_DIR"

log "building tangram-notes (debug)…"
cargo build -p tangram-notes --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
NOTES_BIN="$REPO_ROOT/target/debug/tangram-notes"
[[ -x $NOTES_BIN ]] || fail "missing $NOTES_BIN after build"

# npm ci only when the lockfile changed (CI runners are always fresh).
if [[ ! -f "$CF_DIR/node_modules/.package-lock.json" ||
    "$CF_DIR/package-lock.json" -nt "$CF_DIR/node_modules/.package-lock.json" ]]; then
    log "installing cloud/cloudflare deps (npm ci)…"
    (cd "$CF_DIR" && npm ci --no-audit --no-fund >/dev/null)
else
    log "cloud/cloudflare node_modules up to date (skipping npm ci)"
fi

# Since Phase 7 the Worker bundles the jco-transpiled app components
# (ADR-0002), so they must exist for wrangler to start. This script's
# assertions are unchanged: it still pins the sync-relay wire behavior.
log "building + transpiling the app components (build-components.sh)…"
bash "$CF_DIR/build-components.sh" >/dev/null

pick_port RELAY_PORT
pick_port INSPECTOR_PORT
pick_port PORT_A
pick_port PORT_B
RELAY="http://127.0.0.1:$RELAY_PORT"
BASE_A="http://127.0.0.1:$PORT_A"
BASE_B="http://127.0.0.1:$PORT_B"
log "ports: relay=$RELAY_PORT inspector=$INSPECTOR_PORT A=$PORT_A B=$PORT_B"

# ── relay under wrangler dev (miniflare), isolated state dir ─────────────────

rm -rf "$STATE_DIR" # paranoia: mktemp already made WORK_DIR fresh

start_wrangler() {
    setsid env WRANGLER_SEND_METRICS=false CI=1 \
        "$CF_DIR/node_modules/.bin/wrangler" dev \
        --config "$CF_DIR/wrangler.toml" \
        --ip 127.0.0.1 --port "$RELAY_PORT" \
        --inspector-port "$INSPECTOR_PORT" \
        --persist-to "$STATE_DIR" \
        >>"$WRANGLER_LOG" 2>&1 </dev/null &
    WRANGLER_PID=$!
    wait_for "relay readiness on :$RELAY_PORT" 90 \
        curl -fsS "$RELAY/notes/healthz"
}

log "starting relay (wrangler dev) on :$RELAY_PORT…"
start_wrangler
log "relay ready"

# ── (a) empty-relay genesis convergence ──────────────────────────────────────

start_notes() { # home_tag port log_file
    local home="$WORK_DIR/$1"
    mkdir -p "$home"
    env -i HOME="$home" PATH="$PATH" \
        TANGRAM_DATA_DIR="$home/data" \
        BIND_ADDR="127.0.0.1:$2" \
        TANGRAM_REMOTE="$RELAY/notes/sync" \
        "$NOTES_BIN" >>"$3" 2>&1 </dev/null &
}

log "starting native instance A on :$PORT_A…"
start_notes home-a "$PORT_A" "$LOG_A"
PID_A=$!
wait_for "instance A readiness" 30 curl -fsS "$BASE_A/healthz"

NOTE_A="alpha from A $(date +%s)"
add_note "$BASE_A" "$NOTE_A"
wait_for "A's note in the relay document" 15 has_note "$RELAY/notes" "$NOTE_A"
assert_note_count "$RELAY/notes" 1 "(a) relay state after first sync"
assert_note_count "$BASE_A" 1 "(a) instance A"
log "(a) empty-relay genesis convergence: OK (relay holds exactly 1 note)"

# ── (b) bidirectional sync through the relay ─────────────────────────────────

log "starting native instance B on :$PORT_B…"
start_notes home-b "$PORT_B" "$LOG_B"
PID_B=$!
wait_for "instance B readiness" 30 curl -fsS "$BASE_B/healthz"

measure_propagation "$BASE_B" "$NOTE_A" "(b) A→relay→B initial convergence"
assert_note_count "$BASE_B" 1 "(b) instance B after initial sync"

NOTE_B="bravo from B $(date +%s)"
add_note "$BASE_B" "$NOTE_B"
measure_propagation "$BASE_A" "$NOTE_B" "(b) B→relay→A propagation"
assert_note_count "$BASE_A" 2 "(b) instance A after B's note"
assert_note_count "$RELAY/notes" 2 "(b) relay after B's note"
log "(b) bidirectional sync through the relay: OK"

# ── (c) relay restart persistence ────────────────────────────────────────────

# Freeze both native peers across the restart so the restarted relay cannot
# have been re-fed by a reconnecting client — whatever it serves right after
# readiness provably came from the persisted state dir.
kill -STOP "$PID_A" "$PID_B"

log "(c) stopping wrangler (pid $WRANGLER_PID)…"
stop_wrangler "$WRANGLER_PID"
WRANGLER_PID=""
wait_for "relay port :$RELAY_PORT to be released" 15 bash -c "! (exec 3<>/dev/tcp/127.0.0.1/$RELAY_PORT) 2>/dev/null"
curl -fsS --max-time 2 "$RELAY/notes/healthz" >/dev/null 2>&1 &&
    fail "(c) relay still answering after kill"

log "(c) restarting wrangler on the same state dir…"
start_wrangler
assert_note_count "$RELAY/notes" 2 "(c) relay state after restart"
has_note "$RELAY/notes" "$NOTE_A" >/dev/null && has_note "$RELAY/notes" "$NOTE_B" >/dev/null ||
    fail "(c) restarted relay lost note contents"
log "(c) relay restart persistence: OK (both notes served from disk state," \
    "native peers were frozen so nothing could have re-pushed them)"
kill -CONT "$PID_A" "$PID_B"

NOTE_C="charlie post-restart $(date +%s)"
add_note "$BASE_A" "$NOTE_C"
# Native peers reconnect with ~2s backoff after the relay dropped; give the
# round trip a little headroom beyond the steady-state bound, but report it.
t0=$(now_ms)
wait_for "(c) post-restart A→relay→B propagation" 30 has_note "$BASE_B" "$NOTE_C"
log "(c) post-restart A→relay→B propagation: $(($(now_ms) - t0))ms"
assert_note_count "$BASE_B" 3 "(c) instance B after post-restart write"
assert_note_count "$RELAY/notes" 3 "(c) relay after post-restart write"
log "(c) sync resumes after relay restart: OK"

# ── (d) teardown ─────────────────────────────────────────────────────────────

log "(d) tearing down…"
stop_pid "$PID_B"
stop_pid "$PID_A"
WRANGLER_GROUP=$WRANGLER_PID
stop_wrangler "$WRANGLER_PID"
for pid in "$PID_A" "$PID_B"; do
    kill -0 "$pid" 2>/dev/null && fail "(d) tangram-notes pid $pid survived teardown"
done
assert_group_dead "$WRANGLER_GROUP" "wrangler/workerd"
PID_A="" PID_B="" WRANGLER_PID=""
log "(d) teardown: OK (all tracked processes dead; scratch dirs removed on exit)"

FAILED=0
log "PASS in $((($(now_ms) - T_START) / 1000))s"
