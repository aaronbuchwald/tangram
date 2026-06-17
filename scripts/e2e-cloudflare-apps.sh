#!/usr/bin/env bash
# End-to-end regression test for the Cloudflare APP RUNTIME (RUNTIME_PLAN
# Phase 7 / ADR-0002): the Worker under `wrangler dev` (miniflare) hosts the
# jco-transpiled app components and serves the FULL Tangram surface, not just
# sync. Complements scripts/e2e-cloudflare-sync.sh (which keeps pinning the
# relay/sync behaviors and restart persistence).
#
# Asserted, in order:
#   a. surface up: /healthz 200, the index lists the apps, the bundled UI
#      serves at /<app>/ (text/html, real markup), /<app> redirects to
#      /<app>/ so the UI's relative fetches resolve
#   b. genesis byte-parity: the DO's component genesis (/api/genesis) is
#      byte-identical (sha256) to a fresh NATIVE instance's persisted
#      genesis document — the root that makes CF-hosted and native
#      documents one replicated history
#   c. action dispatch writes through: POST /api/actions/add_note mutates
#      the DO-stored doc (visible in /api/state), with the SDK's error
#      envelope (404 unknown action, 400 bad args)
#   d. SSE on change: /api/events streams the full state on connect and
#      again when an action lands
#   e. MCP against /notes/mcp, through tangram-core's sans-io machine
#      compiled to WASM: initialize (session issued), tools/list,
#      tools/call add_note → the note is in the document; bogus session 404
#   f. FLAGSHIP — a native local replica syncs bidirectionally with the
#      miniflare-HOSTED app (not a bare relay): pre-existing DO-side notes
#      reach the replica, replica writes reach the DO, a DO-side ACTION
#      reaches the replica; propagation < 5s each way
#   g. nutrition: capabilities default to the calorieninjas strategy
#      (description_input true — capabilities report the SELECTED strategy,
#      not key presence; #28 dropped the keyless "offline" strategy); a
#      manual, gram-quantified log_meal over a seeded component logs with no
#      network; then with strategy vars set (restart) capabilities flips and
#      a description-only log_meal's in-guest strategy call is stopped CLEANLY
#      (422 + actionable error) by the per-app host ALLOWLIST (proving the
#      async http-fetch import path end to end, no external network needed)
#   h. clean teardown: every spawned process dead, scratch dirs removed
#
# Self-contained and repeatable: scratch HOME/data dirs and an isolated
# `.wrangler` state dir live under one mktemp dir (removed on exit), ports
# are probed from the ephemeral 19xxx range, and cleanup is trap-based with
# explicit PID tracking (never pkill-by-pattern). Safe to re-run; never
# touches a live instance on :8080.
#
# Usage:  bash scripts/e2e-cloudflare-apps.sh
# Needs:  cargo (with the wasm32-wasip2 target), node >= 20.3 (pinned
#         wrangler ~4.86), npm, curl, jq, sha256sum.

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CF_DIR="$REPO_ROOT/cloud/cloudflare"
SYNC_LATENCY_BOUND_MS=5000

WORK_DIR=$(mktemp -d /tmp/tangram-e2e-cfapps.XXXXXX)
STATE_DIR="$WORK_DIR/wrangler-state"
WRANGLER_LOG="$WORK_DIR/wrangler.log"
LOG_A="$WORK_DIR/notes-a.log"
LOG_GEN="$WORK_DIR/notes-genesis.log"
SSE_OUT="$WORK_DIR/sse-events.out"

WRANGLER_PID="" # leader of its own process group (setsid)
PID_A=""
PID_GEN=""
PID_SSE=""
FAILED=1

log() { printf '[e2e-apps] %s\n' "$*"; }

dump_logs() {
    for f in "$WRANGLER_LOG" "$LOG_A" "$LOG_GEN"; do
        [[ -s $f ]] || continue
        echo "──── tail -n 60 $f ────" >&2
        tail -n 60 "$f" >&2
    done
}

fail() {
    echo "[e2e-apps] FAIL: $*" >&2
    exit 1
}

# ── process management (explicit PIDs only) ──────────────────────────────────

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

stop_wrangler() {
    local pid=$1
    [[ -n $pid ]] || return 0
    kill -TERM -- "-$pid" 2>/dev/null || true
    for _ in $(seq 1 100); do
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
    stop_pid "$PID_SSE"
    stop_pid "$PID_A"
    stop_pid "$PID_GEN"
    stop_wrangler "$WRANGLER_PID"
    if [[ $status -ne 0 || $FAILED -ne 0 ]]; then
        echo "[e2e-apps] FAILED — recent process output:" >&2
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

wait_for() {
    local desc=$1 timeout=$2
    shift 2
    local deadline=$((SECONDS + timeout))
    until "$@" >/dev/null 2>&1; do
        ((SECONDS < deadline)) || fail "timed out (${timeout}s) waiting for: $desc"
        sleep 0.2
    done
}

note_count() { curl -fsS "$1/api/state" | jq -er '.notes | length'; }
has_note() { curl -fsS "$1/api/state" | jq -e --arg t "$2" '.notes | map(.text) | index($t) != null'; }
assert_note_count() { # base expected what
    local got
    got=$(note_count "$1") || fail "$3: GET $1/api/state failed"
    [[ $got == "$2" ]] || fail "$3: expected exactly $2 note(s), got $got (duplicate/forked containers?)"
}

add_note() { # base text
    curl -fsS -X POST "$1/api/actions/add_note" \
        -H 'Content-Type: application/json' \
        -d "{\"text\": \"$2\"}" | jq -e '.result' >/dev/null ||
        fail "add_note '$2' on $1 was rejected"
}

measure_propagation() { # base text what
    local t0 elapsed
    t0=$(now_ms)
    wait_for "$3" 30 has_note "$1" "$2"
    elapsed=$(($(now_ms) - t0))
    log "$3: ${elapsed}ms"
    ((elapsed < SYNC_LATENCY_BOUND_MS)) ||
        fail "$3 took ${elapsed}ms (bound: ${SYNC_LATENCY_BOUND_MS}ms)"
}

# mcp_post <base> <session-or-empty> <json> — POST one MCP message; prints
# the response body, with response headers saved to $WORK_DIR/mcp-headers.
mcp_post() {
    local base=$1 session=$2 body=$3
    local args=(-sS -D "$WORK_DIR/mcp-headers" -X POST "$base/mcp"
        -H 'Accept: application/json, text/event-stream'
        -H 'Content-Type: application/json')
    [[ -n $session ]] && args+=(-H "Mcp-Session-Id: $session")
    curl "${args[@]}" -d "$body"
}

# The JSON payload of a single-message SSE body ("data: {...}").
sse_data() { sed -n 's/^data: //p' | head -n 1; }

# ── prerequisites ────────────────────────────────────────────────────────────

T_START=$(now_ms)
log "scratch dir: $WORK_DIR"

log "building tangram-notes (debug)…"
cargo build -p tangram-notes --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
NOTES_BIN="$REPO_ROOT/target/debug/tangram-notes"
[[ -x $NOTES_BIN ]] || fail "missing $NOTES_BIN after build"

if [[ ! -f "$CF_DIR/node_modules/.package-lock.json" ||
    "$CF_DIR/package-lock.json" -nt "$CF_DIR/node_modules/.package-lock.json" ]]; then
    log "installing cloud/cloudflare deps (npm ci)…"
    (cd "$CF_DIR" && npm ci --no-audit --no-fund >/dev/null)
else
    log "cloud/cloudflare node_modules up to date (skipping npm ci)"
fi

log "building + transpiling the app components (build-components.sh)…"
bash "$CF_DIR/build-components.sh" >/dev/null

pick_port WORKER_PORT
pick_port INSPECTOR_PORT
pick_port PORT_A
pick_port PORT_GEN
BASE="http://127.0.0.1:$WORKER_PORT"
BASE_A="http://127.0.0.1:$PORT_A"
log "ports: worker=$WORKER_PORT inspector=$INSPECTOR_PORT replica=$PORT_A genesis-probe=$PORT_GEN"

# ── worker under wrangler dev (miniflare), isolated state dir ────────────────

start_wrangler() { # [extra wrangler args…]
    setsid env WRANGLER_SEND_METRICS=false CI=1 \
        "$CF_DIR/node_modules/.bin/wrangler" dev \
        --config "$CF_DIR/wrangler.toml" \
        --ip 127.0.0.1 --port "$WORKER_PORT" \
        --inspector-port "$INSPECTOR_PORT" \
        --persist-to "$STATE_DIR" \
        "$@" \
        >>"$WRANGLER_LOG" 2>&1 </dev/null &
    WRANGLER_PID=$!
    wait_for "worker readiness on :$WORKER_PORT" 90 \
        curl -fsS "$BASE/notes/healthz"
}

log "starting worker (wrangler dev) on :$WORKER_PORT…"
start_wrangler
log "worker ready"

# ── (a) surface up ───────────────────────────────────────────────────────────

curl -fsS "$BASE/" | grep -q "/notes/mcp" || fail "(a) index does not list the notes app surface"
UI_TYPE=$(curl -fsS -o "$WORK_DIR/ui.html" -w '%{content_type}' "$BASE/notes/")
[[ $UI_TYPE == text/html* ]] || fail "(a) UI content-type: $UI_TYPE"
grep -qi "<html" "$WORK_DIR/ui.html" || fail "(a) /notes/ did not serve the app UI"
REDIRECT=$(curl -fsS -o /dev/null -w '%{http_code} %{redirect_url}' "$BASE/notes")
[[ $REDIRECT == "301 $BASE/notes/" ]] || fail "(a) /notes redirect: got '$REDIRECT'"
log "(a) healthz + index + UI + prefix redirect: OK"

# ── (b) genesis byte-parity with native ──────────────────────────────────────

# A fresh NATIVE instance persists its deterministic genesis verbatim on
# first start (Store::open writes genesis_bytes); the DO serves its
# component's genesis() at /api/genesis. Byte-identical roots are what let
# CF-hosted documents replicate with native instances (ADR-0002).
GEN_HOME="$WORK_DIR/home-genesis"
mkdir -p "$GEN_HOME"
env -i HOME="$GEN_HOME" PATH="$PATH" \
    TANGRAM_DATA_DIR="$GEN_HOME/data" \
    BIND_ADDR="127.0.0.1:$PORT_GEN" \
    "$NOTES_BIN" >>"$LOG_GEN" 2>&1 </dev/null &
PID_GEN=$!
wait_for "genesis-probe instance readiness" 30 curl -fsS "http://127.0.0.1:$PORT_GEN/healthz"
NATIVE_GENESIS=$(sha256sum "$GEN_HOME/data/notes.automerge" | cut -d' ' -f1)
stop_pid "$PID_GEN"
PID_GEN=""
CF_GENESIS=$(curl -fsS "$BASE/notes/api/genesis" | sha256sum | cut -d' ' -f1)
[[ $NATIVE_GENESIS == "$CF_GENESIS" ]] ||
    fail "(b) genesis mismatch: native=$NATIVE_GENESIS cf=$CF_GENESIS"
log "(b) genesis byte-parity native↔CF: OK ($CF_GENESIS)"

# ── (c) action dispatch writes through ───────────────────────────────────────

NOTE_DO="delta via DO action $(date +%s)"
add_note "$BASE/notes" "$NOTE_DO"
wait_for "(c) action write visible in /api/state" 10 has_note "$BASE/notes" "$NOTE_DO"
assert_note_count "$BASE/notes" 1 "(c) DO state after first action"

CODE=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/notes/api/actions/no_such_action" \
    -H 'Content-Type: application/json' -d '{}')
[[ $CODE == 404 ]] || fail "(c) unknown action: expected 404, got $CODE"
BODY=$(curl -sS -w '\n%{http_code}' -X POST "$BASE/notes/api/actions/add_note" \
    -H 'Content-Type: application/json' -d '{"text": 42}')
[[ $BODY == *"invalid arguments"* && $BODY == *400 ]] ||
    fail "(c) bad args: expected 400 + 'invalid arguments', got: $BODY"
log "(c) action dispatch + error envelope: OK"

# ── (d) SSE full-state stream on change ──────────────────────────────────────

curl -sS -N --max-time 120 "$BASE/notes/api/events" >>"$SSE_OUT" 2>/dev/null &
PID_SSE=$!
wait_for "(d) initial state event on connect" 10 grep -q "event: state" "$SSE_OUT"
grep -q "$NOTE_DO" "$SSE_OUT" || fail "(d) initial state event missing existing note"
NOTE_SSE="echo for sse $(date +%s)"
add_note "$BASE/notes" "$NOTE_SSE"
wait_for "(d) state event carrying the new note" 10 grep -q "$NOTE_SSE" "$SSE_OUT"
stop_pid "$PID_SSE"
PID_SSE=""
log "(d) SSE full-state events on connect and on change: OK"

# ── (e) MCP through the tangram-core machine ─────────────────────────────────

INIT=$(mcp_post "$BASE/notes" "" \
    '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}')
echo "$INIT" | sse_data | jq -e '.result.serverInfo.name == "notes"' >/dev/null ||
    fail "(e) MCP initialize: unexpected response: $INIT"
SESSION=$(grep -i '^mcp-session-id:' "$WORK_DIR/mcp-headers" | tr -d '\r' | awk '{print $2}')
[[ -n $SESSION ]] || fail "(e) MCP initialize issued no session id"

mcp_post "$BASE/notes" "$SESSION" '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null

LIST=$(mcp_post "$BASE/notes" "$SESSION" '{"jsonrpc":"2.0","id":1,"method":"tools/list"}')
echo "$LIST" | sse_data | jq -e '.result.tools | map(.name) | index("add_note") != null' >/dev/null ||
    fail "(e) MCP tools/list missing add_note: $LIST"

NOTE_MCP="foxtrot via mcp $(date +%s)"
CALL=$(mcp_post "$BASE/notes" "$SESSION" \
    "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"add_note\",\"arguments\":{\"text\":\"$NOTE_MCP\"}}}")
echo "$CALL" | sse_data | jq -e '.result.isError == false' >/dev/null ||
    fail "(e) MCP tools/call failed: $CALL"
wait_for "(e) MCP write visible in /api/state" 10 has_note "$BASE/notes" "$NOTE_MCP"

mcp_post "$BASE/notes" "bogus-session" '{"jsonrpc":"2.0","id":3,"method":"tools/list"}' >/dev/null
CODE=$(awk 'toupper($1) ~ /^HTTP/ {print $2; exit}' "$WORK_DIR/mcp-headers")
[[ $CODE == 404 ]] || fail "(e) MCP bogus session: expected 404, got '$CODE'"
log "(e) MCP initialize/tools-list/tools-call via the Rust machine: OK"

# ── (f) FLAGSHIP: native replica ↔ miniflare-hosted app ──────────────────────

log "(f) starting native replica on :$PORT_A…"
HOME_A="$WORK_DIR/home-a"
mkdir -p "$HOME_A"
env -i HOME="$HOME_A" PATH="$PATH" \
    TANGRAM_DATA_DIR="$HOME_A/data" \
    BIND_ADDR="127.0.0.1:$PORT_A" \
    TANGRAM_REMOTE="$BASE/notes/sync" \
    "$NOTES_BIN" >>"$LOG_A" 2>&1 </dev/null &
PID_A=$!
wait_for "replica readiness" 30 curl -fsS "$BASE_A/healthz"

# CF-side history (two action notes + one MCP note) reaches the replica…
measure_propagation "$BASE_A" "$NOTE_MCP" "(f) CF→replica initial convergence"
assert_note_count "$BASE_A" 3 "(f) replica after initial sync"
# …replica writes reach the CF-hosted app…
NOTE_A="golf from replica $(date +%s)"
add_note "$BASE_A" "$NOTE_A"
measure_propagation "$BASE/notes" "$NOTE_A" "(f) replica→CF propagation"
# …and a CF-side ACTION (through the app logic) reaches the replica.
NOTE_DO2="hotel via DO action $(date +%s)"
add_note "$BASE/notes" "$NOTE_DO2"
measure_propagation "$BASE_A" "$NOTE_DO2" "(f) CF action→replica propagation"
assert_note_count "$BASE_A" 5 "(f) replica fully converged"
assert_note_count "$BASE/notes" 5 "(f) CF fully converged"
log "(f) FLAGSHIP bidirectional replica↔CF-hosted-app sync: OK"

# ── (g) nutrition: default capabilities + offline manual logging, then
#        allowlist enforcement ─────────────────────────────────────────────

# Default strategy (NUTRITION_STRATEGY unset) is calorieninjas; it resolves
# over the network, so description_input is advertised true regardless of
# whether a key is injected (#28 removed the keyless "offline" strategy —
# capabilities report the SELECTED strategy, not key presence). The no-key
# behavior is enforced at resolve time below, not in the capabilities shape.
CAPS=$(curl -fsS "$BASE/nutrition/api/capabilities")
echo "$CAPS" | jq -e '.strategy == "calorieninjas" and .description_input == true' >/dev/null ||
    fail "(g) nutrition capabilities default to calorieninjas: $CAPS"

# A manual, gram-quantified meal over a SEEDED component ("oatmeal" is in the
# genesis component_mappings) logs with no network: explicit components win
# and nothing needs resolving, so no strategy call is made — this works
# regardless of key. (A description-only meal, or an unseeded component,
# would reach for the strategy's network; that no-key failure path is proven
# network-free by the allowlist-denial probe below.)
curl -fsS -X POST "$BASE/nutrition/api/actions/log_meal" -H 'Content-Type: application/json' \
    -d '{"description":"breakfast","components":[{"component":"oatmeal","qty_g":80}]}' |
    jq -e '.result' >/dev/null || fail "(g) manual log_meal failed offline"
curl -fsS "$BASE/nutrition/api/state" | jq -e '.meals | length == 1' >/dev/null ||
    fail "(g) logged meal not in nutrition state"
log "(g) nutrition default (calorieninjas) capabilities + offline manual logging: OK"

# Restart with strategy vars (same state dir): capabilities must flip, and
# the strategy's outbound call must be DENIED by the per-app allowlist —
# api.anthropic.com is not in nutrition's grant — proving the guest's
# http-fetch import runs (JSPI) and the capability check holds, without
# external network.
log "(g) restarting worker with NUTRITION_STRATEGY=llm (allowlist probe)…"
stop_wrangler "$WRANGLER_PID"
WRANGLER_PID=""
start_wrangler --var NUTRITION_STRATEGY:llm --var ANTHROPIC_API_KEY:e2e-stub
CAPS=$(curl -fsS "$BASE/nutrition/api/capabilities")
echo "$CAPS" | jq -e '.strategy == "llm" and .description_input == true' >/dev/null ||
    fail "(g) nutrition capabilities with NUTRITION_STRATEGY=llm: $CAPS"
DENIED=$(curl -sS -w '\n%{http_code}' -X POST "$BASE/nutrition/api/actions/log_meal" \
    -H 'Content-Type: application/json' -d '{"description":"2 eggs and toast"}')
[[ $DENIED == *422 && $DENIED == *denied* && $DENIED == *allow_hosts* ]] ||
    fail "(g) expected allowlist denial naming the grant, got: $DENIED"
# The notes document survived the restart (served from persisted DO state).
assert_note_count "$BASE/notes" 5 "(g) notes state after worker restart"
log "(g) strategy env plumbing + http-fetch allowlist enforcement: OK"

# ── (h) teardown ─────────────────────────────────────────────────────────────

log "(h) tearing down…"
stop_pid "$PID_A"
WRANGLER_GROUP=$WRANGLER_PID
stop_wrangler "$WRANGLER_PID"
kill -0 "$PID_A" 2>/dev/null && fail "(h) tangram-notes pid $PID_A survived teardown"
assert_group_dead "$WRANGLER_GROUP" "wrangler/workerd"
PID_A="" WRANGLER_PID=""
log "(h) teardown: OK (all tracked processes dead; scratch dirs removed on exit)"

FAILED=0
log "PASS in $((($(now_ms) - T_START) / 1000))s"
