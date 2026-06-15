#!/usr/bin/env bash
# End-to-end regression for OAuth/OIDC sign-in on the NATIVE tangram-host
# (docs/design/auth.md §7 C6), mirroring scripts/e2e-cloudflare-identity.sh but
# against the Rust host instead of the Cloudflare worker. A STUB IdP stands in
# for GitHub — the host's OAUTH_{AUTHORIZE,TOKEN,USER}_URL are env-overridable
# (defaults are real GitHub), the exact seam the CF e2e exercises.
#
# The suite runs TWICE (fresh host state each round) to prove repeatability.
# Each round asserts, in order:
#   a. start: GET /api/auth/oauth/start 302s to the stub IdP authorize URL with
#      client_id + redirect_uri + state, and sets a state cookie
#   b. callback: GET /api/auth/oauth/callback?code=…&state=… validates state,
#      exchanges the code, creates the account on FIRST sign-in, sets a session
#      cookie, and 302s to /
#   c. the session cookie authenticates: GET /api/auth shows the principal
#   d. idempotency: signing the SAME identity in again lands on the SAME account
#      (no duplicate); a DIFFERENT identity with the same login gets a
#      collision-safe slug (alice → alice-2)
#   e. CSRF: a callback whose state does NOT match the cookie is rejected (400)
#   f. PAT-only bootstrap still works: the bootstrap admin PAT logs in via
#      /api/auth/login with NO dependence on OAuth
#   g. clean teardown: every spawned process dead, scratch dirs removed
#
# Self-contained + repeatable: scratch HOME/data + the stub IdP live under one
# mktemp dir (removed on exit), ports are probed from the 19xxx range, cleanup
# is trap-based with explicit PID tracking. Never touches :8080.
#
# Usage:  bash scripts/e2e-host-oauth.sh
# Needs:  cargo (with the wasm32-wasip2 target), node >= 18, curl, jq.

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

WORK_DIR=$(mktemp -d /tmp/tangram-e2e-oauth.XXXXXX)
IDP_LOG="$WORK_DIR/idp.log"
HOST_LOG="$WORK_DIR/host.log"

IDP_PID=""
HOST_PID=""
FAILED=1

log() { printf '[e2e-oauth] %s\n' "$*"; }
fail() {
    echo "[e2e-oauth] FAIL: $*" >&2
    exit 1
}

dump_logs() {
    for f in "$IDP_LOG" "$HOST_LOG"; do
        [[ -s $f ]] || continue
        echo "──── tail -n 40 $f ────" >&2
        tail -n 40 "$f" >&2
    done
}

stop_pid() {
    local pid=$1
    [[ -n $pid ]] || return 0
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
    done
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

cleanup() {
    local status=$?
    trap - EXIT
    stop_pid "$HOST_PID"
    stop_pid "$IDP_PID"
    if [[ $status -ne 0 || $FAILED -ne 0 ]]; then
        echo "[e2e-oauth] FAILED — recent process output:" >&2
        dump_logs
    fi
    rm -rf "$WORK_DIR"
    [[ $status -ne 0 ]] && exit "$status"
    [[ $FAILED -eq 0 ]] || exit 1
}
trap cleanup EXIT
trap 'exit 130' INT TERM

port_in_use() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
PICKED=" "
pick_port() {
    local port
    for _ in $(seq 1 200); do
        port=$((19000 + RANDOM % 1000))
        [[ $PICKED == *" $port "* ]] && continue
        if ! port_in_use "$port"; then
            PICKED+="$port "
            printf -v "$1" '%s' "$port"
            return 0
        fi
    done
    fail "no free 19xxx port"
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

# ── prerequisites ────────────────────────────────────────────────────────────

log "scratch dir: $WORK_DIR"
log "building tangram-host + the registry component…"
cargo build -p tangram-host --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
cargo build -p tangram-registry --lib --target wasm32-wasip2 --release \
    --manifest-path "$REPO_ROOT/Cargo.toml" --quiet
HOST_BIN="$REPO_ROOT/target/debug/tangram-host"
REGISTRY_WASM="$REPO_ROOT/target/wasm32-wasip2/release/tangram_registry.wasm"
[[ -x $HOST_BIN ]] || fail "missing $HOST_BIN"
[[ -f $REGISTRY_WASM ]] || REGISTRY_WASM="$REPO_ROOT/target/wasm32-wasip2/release/registry.wasm"
[[ -f $REGISTRY_WASM ]] || fail "missing the registry wasm component"

pick_port IDP_PORT
pick_port HOST_PORT
IDP_BASE="http://127.0.0.1:$IDP_PORT"
BASE="http://127.0.0.1:$HOST_PORT"
log "ports: idp=$IDP_PORT host=$HOST_PORT"

# ── the stub IdP (stands in for GitHub; stateless) ───────────────────────────
# /token swaps `code:<id>:<login>` for `tok:<id>:<login>` after checking the
# client secret; /user decodes the token back into {id, login}. The e2e chooses
# the signing identity by choosing the code.
cat >"$WORK_DIR/stub-idp.cjs" <<'EOF'
const http = require("http");
const port = Number(process.argv[2]);
http
  .createServer((req, res) => {
    const url = new URL(req.url, "http://stub");
    console.log(req.method + " " + url.pathname);
    if (url.pathname === "/authorize") {
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

# ── one full round (run twice on fresh state) ────────────────────────────────

# signin <cookie-jar> <id> <login> — plays the browser through the host's OAuth
# endpoints against the stub IdP. Sets SIGNIN_USER (the resolved user_id).
signin() {
    local jar=$1 id=$2 login=$3 location state cb
    # (a) start → 302 to the IdP authorize URL, state cookie set.
    location=$(curl -fsS -o /dev/null -w '%{redirect_url}' -c "$jar" "$BASE/api/auth/oauth/start")
    [[ $location == "$IDP_BASE/authorize?"* ]] ||
        fail "(a) start did not redirect to the IdP: $location"
    [[ $location == *"client_id=e2e-client"* && $location == *"redirect_uri="* ]] ||
        fail "(a) authorize URL missing client_id/redirect_uri: $location"
    state=$(printf '%s' "$location" | sed -n 's/.*[?&]state=\([^&]*\).*/\1/p')
    [[ -n $state ]] || fail "(a) no state in the authorize redirect"
    # (b) callback with the code the stub honors + the matching state.
    cb=$(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}' -b "$jar" -c "$jar" \
        "$BASE/api/auth/oauth/callback?code=code:$id:$login&state=$state")
    [[ $cb == "302 $BASE/" || $cb == "302 /" ]] || fail "(b) callback for $login: got '$cb'"
    # (c) the session cookie authenticates.
    SIGNIN_USER=$(curl -fsS -b "$jar" "$BASE/api/auth" | jq -er '.principal.user_id') ||
        fail "(c) /api/auth showed no principal after sign-in for $login"
}

run_round() {
    local round=$1
    local home="$WORK_DIR/r$round-home"
    mkdir -p "$home"
    cat >"$home/apps.toml" <<EOF
[auth]
mode = "multi-tenant"
oauth_issuer = "github"
oauth_client_id = "e2e-client"
oauth_client_secret = "\${OAUTH_CLIENT_SECRET}"

[apps.registry]
component = "$REGISTRY_WASM"
ui = "$REPO_ROOT/apps/registry/ui"
registry = true
EOF

    log "── round $round ─────────────────────────────────────────────"
    env -i HOME="$home" PATH="$PATH" \
        TANGRAM_DATA_DIR="$home/.tangram" \
        BIND_ADDR="127.0.0.1:$HOST_PORT" \
        RUST_LOG=info \
        OAUTH_CLIENT_SECRET="e2e-secret" \
        OAUTH_AUTHORIZE_URL="$IDP_BASE/authorize" \
        OAUTH_TOKEN_URL="$IDP_BASE/token" \
        OAUTH_USER_URL="$IDP_BASE/user" \
        "$HOST_BIN" "$home/apps.toml" >>"$HOST_LOG" 2>&1 </dev/null &
    HOST_PID=$!
    wait_for "host readiness" 60 curl -fsS "$BASE/registry/healthz"
    log "host ready"

    # (a)-(c) first sign-in creates the account; the session authenticates.
    signin "$WORK_DIR/r$round-alice.jar" 1001 alice
    [[ $SIGNIN_USER == alice ]] || fail "(c) alice's user_id: got '$SIGNIN_USER'"
    log "(a-c) OAuth start → callback → session principal: OK"

    # (d) idempotency + collision-safe slug.
    signin "$WORK_DIR/r$round-alice2.jar" 1001 alice
    [[ $SIGNIN_USER == alice ]] || fail "(d) re-sign-in moved accounts: $SIGNIN_USER"
    signin "$WORK_DIR/r$round-imposter.jar" 3001 alice
    [[ $SIGNIN_USER == alice-2 ]] || fail "(d) slug collision not de-duplicated: '$SIGNIN_USER'"
    log "(d) idempotent re-login + collision-safe slug (alice-2): OK"

    # (e) CSRF: a state that does not match the cookie is rejected.
    curl -fsS -o /dev/null -c "$WORK_DIR/r$round-csrf.jar" "$BASE/api/auth/oauth/start"
    local csrf_code
    csrf_code=$(curl -sS -o /dev/null -w '%{http_code}' -b "$WORK_DIR/r$round-csrf.jar" \
        "$BASE/api/auth/oauth/callback?code=code:9:eve&state=forged-state")
    [[ $csrf_code == 400 ]] || fail "(e) forged state not rejected: got $csrf_code"
    log "(e) CSRF state mismatch rejected (400): OK"

    # (f) PAT-only bootstrap still works alongside OAuth.
    local admin
    admin=$(grep -o 'tgp_[A-Za-z0-9_-]*' "$HOST_LOG" | head -n 1)
    [[ -n $admin ]] || fail "(f) no bootstrap admin PAT in the host log"
    local login_code
    login_code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/auth/login" \
        -H 'Content-Type: application/json' -d "{\"token\":\"$admin\"}")
    [[ $login_code == 200 ]] || fail "(f) PAT login failed: got $login_code"
    log "(f) PAT-only bootstrap login works with OAuth configured: OK"

    # (g) round teardown.
    stop_pid "$HOST_PID"
    HOST_PID=""
    : >"$HOST_LOG" # fresh log for the next round's PAT grep
    log "(g) round $round teardown: OK"
}

run_round 1
run_round 2

stop_pid "$IDP_PID"
IDP_PID=""
FAILED=0
log "PASS (both rounds)"
