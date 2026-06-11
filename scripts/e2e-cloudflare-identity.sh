#!/usr/bin/env bash
# End-to-end regression test for IDENTITY on Cloudflare (RUNTIME_PLAN
# Phase 6 / ADR-0003): OAuth accounts (account == tenant), per-tenant
# namespaces under /t/<tenant>/, and PATs as the programmatic credential —
# under miniflare with a STUB IdP standing in for GitHub (the worker's
# OAUTH_*_URL endpoints are env-overridable; defaults are real GitHub).
#
# The whole suite runs TWICE (fresh worker state each round) to prove
# repeatability. Each round asserts, in order:
#   a. sign-in: /auth/login 302s to the IdP with a state cookie; the
#      callback exchanges the code, creates the account, sets the session
#      cookie; the ACCOUNT PAGE serves; signing the same identity in again
#      lands on the SAME tenant; a different identity with the same login
#      gets a collision-safe slug (alice → alice-2)
#   b. PATs: minted from the account page API (token shown once), listed
#   c. the 401 matrix on /t/alice/notes: state, actions POST, SSE events,
#      sync POST, sync SSE, MCP, the UI page, and the namespace index all
#      answer 401 with NO token, a GARBAGE token, and BOB'S token; an
#      unknown tenant 401s identically even with a valid PAT (no existence
#      oracle — all four 401 bodies are byte-identical)
#   d. authorized access: alice's PAT reads state; her session COOKIE serves
#      the UI and state (browser path); MCP initialize+tools/list under her
#      PAT; bob's namespace is fully isolated (his data, not hers); the
#      single-user compat surface (/notes/) stays open and separate
#   e. FLAGSHIP — alice's native replica (the UNCHANGED sync client:
#      TANGRAM_REMOTE=/t/alice/notes/sync + TANGRAM_REMOTE_TOKEN=<PAT>)
#      syncs bidirectionally with her namespace, < 5s each way
#   f. revocation: DELETE the PAT → the very next request 401s; the replica
#      enters its reconnect loop WITHOUT crashing and keeps serving locally
#   g. clean teardown: every spawned process dead, scratch dirs removed
#
# Self-contained and repeatable: scratch HOME/data dirs and an isolated
# `.wrangler` state dir live under one mktemp dir (removed on exit), ports
# are probed from the ephemeral 19xxx range, and cleanup is trap-based with
# explicit PID tracking (never pkill-by-pattern). Never touches :8080.
#
# Usage:  bash scripts/e2e-cloudflare-identity.sh
# Needs:  cargo (with the wasm32-wasip2 target), node >= 20.3 (pinned
#         wrangler ~4.86), npm, curl, jq.

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CF_DIR="$REPO_ROOT/cloud/cloudflare"
SYNC_LATENCY_BOUND_MS=5000

WORK_DIR=$(mktemp -d /tmp/tangram-e2e-cfid.XXXXXX)
WRANGLER_LOG="$WORK_DIR/wrangler.log"
IDP_LOG="$WORK_DIR/idp.log"

WRANGLER_PID="" # leader of its own process group (setsid)
IDP_PID=""
PID_R=""
FAILED=1

log() { printf '[e2e-identity] %s\n' "$*"; }

dump_logs() {
    for f in "$WRANGLER_LOG" "$IDP_LOG" "$WORK_DIR"/replica-*.log; do
        [[ -s $f ]] || continue
        echo "──── tail -n 60 $f ────" >&2
        tail -n 60 "$f" >&2
    done
}

fail() {
    echo "[e2e-identity] FAIL: $*" >&2
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
    stop_pid "$PID_R"
    stop_pid "$IDP_PID"
    stop_wrangler "$WRANGLER_PID"
    if [[ $status -ne 0 || $FAILED -ne 0 ]]; then
        echo "[e2e-identity] FAILED — recent process output:" >&2
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

# expect_code <expected> <what> [curl args…]
expect_code() {
    local expected=$1 what=$2 code
    shift 2
    code=$(curl -sS -o /dev/null -w '%{http_code}' "$@")
    [[ $code == "$expected" ]] || fail "$what: expected HTTP $expected, got $code"
}

# note_count <base-url> [pat] / has_note <base-url> <text> [pat]
note_count() {
    curl -fsS ${2:+-H "Authorization: Bearer $2"} "$1/api/state" | jq -er '.notes | length'
}
has_note() {
    curl -fsS ${3:+-H "Authorization: Bearer $3"} "$1/api/state" |
        jq -e --arg t "$2" '.notes | map(.text) | index($t) != null'
}

add_note() { # base text [pat]
    curl -fsS -X POST "$1/api/actions/add_note" \
        ${3:+-H "Authorization: Bearer $3"} \
        -H 'Content-Type: application/json' \
        -d "{\"text\": \"$2\"}" | jq -e '.result' >/dev/null ||
        fail "add_note '$2' on $1 was rejected"
}

measure_propagation() { # base text what [pat]
    local t0 elapsed
    t0=$(now_ms)
    wait_for "$3" 30 has_note "$1" "$2" "${4:-}"
    elapsed=$(($(now_ms) - t0))
    log "$3: ${elapsed}ms"
    ((elapsed < SYNC_LATENCY_BOUND_MS)) ||
        fail "$3 took ${elapsed}ms (bound: ${SYNC_LATENCY_BOUND_MS}ms)"
}

# signin <cookie-jar> <idp-id> <idp-login> — runs the OAuth web flow against
# the stub IdP (the test plays the browser: follow /auth/login's redirect by
# hand, then hit the callback with the code the stub will honor). Sets
# SIGNIN_TENANT.
signin() {
    local jar=$1 id=$2 login=$3 location state cb
    location=$(curl -fsS -o /dev/null -w '%{redirect_url}' -c "$jar" "$BASE/auth/login")
    [[ $location == "$IDP_BASE/authorize?"* ]] ||
        fail "(a) /auth/login did not redirect to the IdP: $location"
    [[ $location == *"client_id=e2e-client"* && $location == *"redirect_uri="* ]] ||
        fail "(a) authorize URL missing client_id/redirect_uri: $location"
    state=$(printf '%s' "$location" | sed -n 's/.*[?&]state=\([^&]*\).*/\1/p')
    [[ -n $state ]] || fail "(a) no state in the authorize redirect"
    cb=$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' -b "$jar" -c "$jar" \
        "$BASE/auth/callback?code=code:$id:$login&state=$state")
    [[ $cb == "302 $BASE/account" ]] || fail "(a) callback for $login: got '$cb'"
    SIGNIN_TENANT=$(curl -fsS -b "$jar" "$BASE/account/api/me" | jq -er '.tenant') ||
        fail "(a) /account/api/me failed after sign-in for $login"
}

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
pick_port IDP_PORT
pick_port PORT_R
BASE="http://127.0.0.1:$WORKER_PORT"
IDP_BASE="http://127.0.0.1:$IDP_PORT"
BASE_R="http://127.0.0.1:$PORT_R"
log "ports: worker=$WORKER_PORT inspector=$INSPECTOR_PORT idp=$IDP_PORT replica=$PORT_R"

# ── the stub IdP (stands in for GitHub; one per suite, stateless) ────────────
# /token swaps `code:<id>:<login>` for `tok:<id>:<login>` after checking the
# client secret; /user decodes the token back into {id, login}. The e2e
# chooses the signing identity simply by choosing the code.

cat >"$WORK_DIR/stub-idp.cjs" <<'EOF'
const http = require("http");
const port = Number(process.argv[2]);
http
  .createServer((req, res) => {
    const url = new URL(req.url, "http://stub");
    console.log(req.method + " " + url.pathname);
    if (url.pathname === "/authorize") {
      // A real browser would land here to consent; the e2e drives the
      // callback directly, so this only needs to exist.
      res.end("stub idp: sign in");
    } else if (url.pathname === "/token" && req.method === "POST") {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        const params = new URLSearchParams(body);
        const code = params.get("code") || "";
        res.setHeader("content-type", "application/json");
        if (params.get("client_secret") !== "e2e-secret" || !code.startsWith("code:")) {
          res.statusCode = 400;
          res.end(JSON.stringify({ error: "bad_verification_code" }));
          return;
        }
        res.end(JSON.stringify({ access_token: "tok:" + code.slice(5), token_type: "bearer" }));
      });
    } else if (url.pathname === "/user") {
      const m = /^Bearer tok:(\d+):(.+)$/.exec(req.headers.authorization || "");
      res.setHeader("content-type", "application/json");
      if (!m) {
        res.statusCode = 401;
        res.end("{}");
        return;
      }
      res.end(JSON.stringify({ id: Number(m[1]), login: m[2] }));
    } else {
      res.statusCode = 404;
      res.end();
    }
  })
  .listen(port, "127.0.0.1", () => console.log("stub idp listening on " + port));
EOF

node "$WORK_DIR/stub-idp.cjs" "$IDP_PORT" >>"$IDP_LOG" 2>&1 </dev/null &
IDP_PID=$!
wait_for "stub IdP readiness" 15 curl -fsS "$IDP_BASE/authorize"

# ── worker under wrangler dev (miniflare), stub IdP wired in ─────────────────

start_wrangler() { # <state-dir>
    setsid env WRANGLER_SEND_METRICS=false CI=1 \
        "$CF_DIR/node_modules/.bin/wrangler" dev \
        --config "$CF_DIR/wrangler.toml" \
        --ip 127.0.0.1 --port "$WORKER_PORT" \
        --inspector-port "$INSPECTOR_PORT" \
        --persist-to "$1" \
        --var GITHUB_CLIENT_ID:e2e-client \
        --var GITHUB_CLIENT_SECRET:e2e-secret \
        --var "OAUTH_AUTHORIZE_URL:$IDP_BASE/authorize" \
        --var "OAUTH_TOKEN_URL:$IDP_BASE/token" \
        --var "OAUTH_USER_URL:$IDP_BASE/user" \
        >>"$WRANGLER_LOG" 2>&1 </dev/null &
    WRANGLER_PID=$!
    wait_for "worker readiness on :$WORKER_PORT" 90 \
        curl -fsS "$BASE/notes/healthz"
}

# ── one full round (the suite runs it twice on fresh state) ──────────────────

run_round() {
    local round=$1
    local jar_alice="$WORK_DIR/r$round-alice.jar" jar_bob="$WORK_DIR/r$round-bob.jar"
    local home_r="$WORK_DIR/r$round-home-replica"
    local log_r="$WORK_DIR/replica-r$round.log"

    log "── round $round ─────────────────────────────────────────────"
    log "starting worker (wrangler dev) on :$WORKER_PORT (fresh state)…"
    start_wrangler "$WORK_DIR/r$round-wrangler-state"
    log "worker ready"

    # (a) sign-in: two accounts, idempotency, collision-safe slugs ------------
    signin "$jar_alice" 1001 alice
    local t_alice=$SIGNIN_TENANT
    [[ $t_alice == alice ]] || fail "(a) alice's tenant slug: got '$t_alice'"
    signin "$jar_bob" 1002 bob
    local t_bob=$SIGNIN_TENANT
    [[ $t_bob == bob ]] || fail "(a) bob's tenant slug: got '$t_bob'"

    # The same identity signs in again → SAME tenant (no alice-2).
    signin "$WORK_DIR/r$round-alice2.jar" 1001 alice
    [[ $SIGNIN_TENANT == "$t_alice" ]] ||
        fail "(a) re-sign-in of the same identity moved tenants: $SIGNIN_TENANT"
    # A DIFFERENT identity with the same login → collision-safe slug.
    signin "$WORK_DIR/r$round-imposter.jar" 3001 alice
    [[ $SIGNIN_TENANT == "alice-2" ]] ||
        fail "(a) slug collision not de-duplicated: got '$SIGNIN_TENANT'"

    # The account page serves with the session; without one it redirects.
    local page_type
    page_type=$(curl -fsS -b "$jar_alice" -o "$WORK_DIR/account.html" \
        -w '%{content_type}' "$BASE/account")
    [[ $page_type == text/html* ]] || fail "(a) account page content-type: $page_type"
    grep -qi "account" "$WORK_DIR/account.html" || fail "(a) /account did not serve the page"
    local anon
    anon=$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' "$BASE/account")
    [[ $anon == "302 $BASE/auth/login" ]] || fail "(a) anonymous /account: got '$anon'"
    log "(a) OAuth sign-in, account page, idempotent + collision-safe slugs: OK"

    # (b) PATs minted + listed ------------------------------------------------
    local minted pat_alice pat_alice_id pat_bob
    minted=$(curl -fsS -b "$jar_alice" -X POST "$BASE/account/api/pats" \
        -H 'Content-Type: application/json' -d '{"label":"laptop replica"}')
    pat_alice=$(echo "$minted" | jq -er '.token')
    pat_alice_id=$(echo "$minted" | jq -er '.id')
    [[ $pat_alice == tgp_* ]] || fail "(b) unexpected PAT shape: $pat_alice"
    pat_bob=$(curl -fsS -b "$jar_bob" -X POST "$BASE/account/api/pats" \
        -H 'Content-Type: application/json' -d '{"label":"bob"}' | jq -er '.token')
    curl -fsS -b "$jar_alice" "$BASE/account/api/pats" |
        jq -e --arg id "$pat_alice_id" '.pats | map(.id) | index($id) != null' >/dev/null ||
        fail "(b) minted PAT not in alice's list"
    # Minting requires a session: a PAT is not a session credential.
    expect_code 401 "(b) tokenless mint" -X POST "$BASE/account/api/pats" -d '{}'
    log "(b) PAT mint + list (token shown once): OK"

    # (c) the 401 matrix on alice's namespace ---------------------------------
    local ns="$BASE/t/$t_alice/notes"
    expect_code 401 "(c) state, no token" "$ns/api/state"
    expect_code 401 "(c) state, garbage token" -H 'Authorization: Bearer tgp_bogus' "$ns/api/state"
    expect_code 401 "(c) state, bob's token" -H "Authorization: Bearer $pat_bob" "$ns/api/state"
    expect_code 401 "(c) state, bob's cookie" -b "$jar_bob" "$ns/api/state"
    expect_code 401 "(c) action POST, no token" -X POST "$ns/api/actions/add_note" \
        -H 'Content-Type: application/json' -d '{"text":"sneak"}'
    expect_code 401 "(c) state SSE, no token" "$ns/api/events"
    expect_code 401 "(c) sync POST, no token" -X POST "$ns/sync" \
        -H 'X-Tangram-Session: e2e' -H 'Content-Type: application/octet-stream' --data-binary ''
    expect_code 401 "(c) sync SSE, no token" "$ns/sync/events"
    expect_code 401 "(c) MCP, no token" -X POST "$ns/mcp" \
        -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}'
    expect_code 401 "(c) MCP, bob's token" -X POST "$ns/mcp" \
        -H "Authorization: Bearer $pat_bob" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    expect_code 401 "(c) UI page, no token" "$ns/"
    expect_code 401 "(c) namespace index, no token" "$BASE/t/$t_alice/"
    expect_code 401 "(c) unknown tenant, valid PAT" \
        -H "Authorization: Bearer $pat_alice" "$BASE/t/ghost/notes/api/state"
    # No existence oracle: all failure modes return ONE byte-identical body.
    curl -sS "$ns/api/state" >"$WORK_DIR/401-no-token"
    curl -sS -H "Authorization: Bearer $pat_bob" "$ns/api/state" >"$WORK_DIR/401-bob"
    curl -sS -H "Authorization: Bearer $pat_alice" "$BASE/t/ghost/notes/api/state" \
        >"$WORK_DIR/401-ghost"
    cmp -s "$WORK_DIR/401-no-token" "$WORK_DIR/401-bob" &&
        cmp -s "$WORK_DIR/401-no-token" "$WORK_DIR/401-ghost" ||
        fail "(c) 401 bodies differ between failure modes (existence oracle)"
    log "(c) full 401 matrix (state/actions/SSE/sync/MCP/UI/index, uniform body): OK"

    # (d) authorized access ---------------------------------------------------
    expect_code 200 "(d) state with alice's PAT" \
        -H "Authorization: Bearer $pat_alice" "$ns/api/state"
    expect_code 200 "(d) state with alice's session cookie" -b "$jar_alice" "$ns/api/state"
    local ui_type
    ui_type=$(curl -fsS -b "$jar_alice" -o "$WORK_DIR/tenant-ui.html" -w '%{content_type}' "$ns/")
    [[ $ui_type == text/html* ]] || fail "(d) tenant UI content-type: $ui_type"
    grep -qi "<html" "$WORK_DIR/tenant-ui.html" || fail "(d) tenant UI did not serve markup"

    # MCP under the PAT: initialize issues a session, tools/list works.
    local init session
    init=$(curl -sS -D "$WORK_DIR/mcp-headers" -X POST "$ns/mcp" \
        -H "Authorization: Bearer $pat_alice" \
        -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}')
    echo "$init" | sed -n 's/^data: //p' | head -n 1 |
        jq -e '.result.serverInfo.name == "notes"' >/dev/null ||
        fail "(d) MCP initialize under the PAT: $init"
    session=$(grep -i '^mcp-session-id:' "$WORK_DIR/mcp-headers" | tr -d '\r' | awk '{print $2}')
    [[ -n $session ]] || fail "(d) MCP initialize issued no session id"
    curl -sS -X POST "$ns/mcp" -H "Authorization: Bearer $pat_alice" \
        -H "Mcp-Session-Id: $session" \
        -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' |
        sed -n 's/^data: //p' | head -n 1 |
        jq -e '.result.tools | map(.name) | index("add_note") != null' >/dev/null ||
        fail "(d) MCP tools/list under the PAT failed"

    # Tenant isolation: alice's note is hers alone.
    local note_alice="r$round alice via CF $(date +%s)"
    add_note "$ns" "$note_alice" "$pat_alice"
    wait_for "(d) alice's write visible" 10 has_note "$ns" "$note_alice" "$pat_alice"
    [[ $(note_count "$BASE/t/$t_bob/notes" "$pat_bob") == 0 ]] ||
        fail "(d) bob's namespace sees alice's data"
    [[ $(note_count "$BASE/notes") == 0 ]] ||
        fail "(d) the single-user surface sees tenant data"
    log "(d) PAT + session access, MCP under PAT, tenant isolation: OK"

    # (e) FLAGSHIP: alice's replica syncs with TANGRAM_REMOTE_TOKEN=<PAT> -----
    log "(e) starting alice's native replica on :$PORT_R…"
    mkdir -p "$home_r"
    env -i HOME="$home_r" PATH="$PATH" \
        TANGRAM_DATA_DIR="$home_r/data" \
        BIND_ADDR="127.0.0.1:$PORT_R" \
        TANGRAM_REMOTE="$ns/sync" \
        TANGRAM_REMOTE_TOKEN="$pat_alice" \
        "$NOTES_BIN" >>"$log_r" 2>&1 </dev/null &
    PID_R=$!
    wait_for "replica readiness" 30 curl -fsS "$BASE_R/healthz"
    measure_propagation "$BASE_R" "$note_alice" "(e) CF→replica initial convergence"
    local note_replica="r$round from replica $(date +%s)"
    add_note "$BASE_R" "$note_replica"
    measure_propagation "$ns" "$note_replica" "(e) replica→CF propagation" "$pat_alice"
    [[ $(note_count "$BASE/t/$t_bob/notes" "$pat_bob") == 0 ]] ||
        fail "(e) replica sync leaked into bob's namespace"
    log "(e) FLAGSHIP replica↔tenant sync through the PAT: OK"

    # (f) revocation ----------------------------------------------------------
    expect_code 204 "(f) revoke alice's PAT" -b "$jar_alice" \
        -X DELETE "$BASE/account/api/pats/$pat_alice_id"
    expect_code 401 "(f) revoked PAT on state" \
        -H "Authorization: Bearer $pat_alice" "$ns/api/state"
    expect_code 401 "(f) revoked PAT on sync" -H "Authorization: Bearer $pat_alice" \
        -X POST "$ns/sync" -H 'X-Tangram-Session: e2e' \
        -H 'Content-Type: application/octet-stream' --data-binary ''
    # Force the replica to hit the remote: a local write triggers a sync
    # round, which now 401s → it must enter the reconnect loop, not crash.
    add_note "$BASE_R" "r$round after revocation $(date +%s)"
    sleep 4
    kill -0 "$PID_R" 2>/dev/null || fail "(f) replica crashed after revocation"
    grep -q "reconnecting" "$log_r" ||
        fail "(f) replica did not enter its reconnect loop (no 'reconnecting' in log)"
    curl -fsS "$BASE_R/api/state" | jq -e '.notes' >/dev/null ||
        fail "(f) replica stopped serving locally after revocation"
    log "(f) revocation: immediate 401, replica reconnect-loops without crashing: OK"

    # (g) round teardown ------------------------------------------------------
    stop_pid "$PID_R"
    kill -0 "$PID_R" 2>/dev/null && fail "(g) replica pid $PID_R survived teardown"
    PID_R=""
    local wrangler_group=$WRANGLER_PID
    stop_wrangler "$WRANGLER_PID"
    assert_group_dead "$wrangler_group" "wrangler/workerd"
    WRANGLER_PID=""
    log "(g) round $round teardown: OK"
}

run_round 1
run_round 2

stop_pid "$IDP_PID"
IDP_PID=""

FAILED=0
log "PASS (both rounds) in $((($(now_ms) - T_START) / 1000))s"
