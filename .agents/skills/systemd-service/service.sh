#!/usr/bin/env bash
# Install or rebuild a Tangram binary as a systemd service.
#
# Usage:
#   service.sh install [--dir <repo>] [--name <svc>] [--binary <pkg>] \
#                      [--bind <addr:port>] [--env KEY=VALUE]...
#   service.sh rebuild [--dir <repo>] [--name <svc>] [--binary <pkg>]
#
# Defaults: --dir = `git rev-parse --show-toplevel` from the current
# directory; --name = tangram-shell; --binary = tangram-shell.
set -euo pipefail

usage() {
  sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-1}"
}

die() { echo "error: $*" >&2; exit 1; }

SUBCMD="${1:-}"
case "$SUBCMD" in
  install|rebuild) shift ;;
  -h|--help) usage 0 ;;
  *) usage ;;
esac

DIR=""
NAME="tangram-shell"
BINARY="tangram-shell"
BIND=""
ENVS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --dir)    DIR="${2:?--dir needs a value}"; shift 2 ;;
    --name)   NAME="${2:?--name needs a value}"; shift 2 ;;
    --binary) BINARY="${2:?--binary needs a value}"; shift 2 ;;
    --bind)
      [ "$SUBCMD" = install ] || die "--bind is only valid for install"
      BIND="${2:?--bind needs a value}"; shift 2 ;;
    --env)
      [ "$SUBCMD" = install ] || die "--env is only valid for install"
      case "${2:-}" in *=*) ;; *) die "--env expects KEY=VALUE, got '${2:-}'";; esac
      ENVS+=("$2"); shift 2 ;;
    -h|--help) usage 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

if [ -z "$DIR" ]; then
  DIR="$(git rev-parse --show-toplevel 2>/dev/null)" \
    || die "not inside a git repo; pass --dir <repo path>"
fi
DIR="$(cd "$DIR" && pwd)" || die "cannot resolve --dir"
[ -f "$DIR/Cargo.toml" ] || die "$DIR does not look like a cargo workspace (no Cargo.toml)"

UNIT="/etc/systemd/system/${NAME}.service"
EXEC="$DIR/target/release/$BINARY"

build() {
  echo "==> cargo build --release -p $BINARY (in $DIR)"
  (cd "$DIR" && cargo build --release -p "$BINARY")
  [ -x "$EXEC" ] || die "build succeeded but $EXEC is missing or not executable"
}

# Resolve the address to probe: --bind if given, else BIND_ADDR from the
# unit file, else the app default 127.0.0.1:8080.
probe_addr() {
  if [ -n "$BIND" ]; then
    echo "$BIND"
  elif [ -f "$UNIT" ] && grep -q '^Environment=BIND_ADDR=' "$UNIT"; then
    sed -n 's/^Environment=BIND_ADDR=//p' "$UNIT" | head -n1
  else
    echo "127.0.0.1:8080"
  fi
}

verify() {
  echo "==> verifying service '$NAME'"
  local ok=0
  for _ in $(seq 1 10); do
    if [ "$(systemctl is-active "$NAME" 2>/dev/null || true)" = "active" ]; then
      ok=1; break
    fi
    sleep 1
  done
  if [ "$ok" -ne 1 ]; then
    echo "FAIL: service '$NAME' is not active" >&2
    echo "--- journalctl -u $NAME -n 20 ---" >&2
    journalctl -u "$NAME" -n 20 --no-pager >&2 || true
    exit 1
  fi
  echo "systemctl is-active $NAME: active"

  local addr code path
  addr="$(probe_addr)"
  for path in /healthz /; do
    for _ in $(seq 1 10); do
      code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://$addr$path" || true)"
      [ "$code" != "000" ] && break
      sleep 1
    done
    if [ "$code" = "200" ]; then
      echo "HTTP check: GET http://$addr$path -> 200 OK"
      return 0
    fi
    echo "HTTP check: GET http://$addr$path -> $code (trying next probe)" >&2
  done
  echo "FAIL: no 200 response from http://$addr (/healthz or /)" >&2
  echo "--- journalctl -u $NAME -n 20 ---" >&2
  journalctl -u "$NAME" -n 20 --no-pager >&2 || true
  exit 1
}

install_cmd() {
  build

  echo "==> writing $UNIT"
  local env_lines=""
  [ -n "$BIND" ] && env_lines+="Environment=BIND_ADDR=$BIND"$'\n'
  # Pin the repo-local data dir explicitly: the SDK's default (when unset)
  # is $HOME/.<app-name>, but installed services predate that and keep
  # their documents in <repo>/data.
  env_lines+="Environment=TANGRAM_DATA_DIR=$DIR/data"$'\n'
  local kv
  for kv in ${ENVS[@]+"${ENVS[@]}"}; do
    env_lines+="Environment=$kv"$'\n'
  done

  sudo tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=Tangram service '$NAME' ($BINARY)
After=network.target

[Service]
User=$(id -un)
WorkingDirectory=$DIR
${env_lines}ExecStart=$EXEC
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF

  echo "==> systemctl daemon-reload && enable --now && restart"
  sudo systemctl daemon-reload
  sudo systemctl enable --now "$NAME"
  # enable --now is a no-op when already running; restart picks up the new
  # unit/binary so re-running install is idempotent.
  sudo systemctl restart "$NAME"
  verify
  echo "OK: '$NAME' installed and serving (unit: $UNIT)"
}

rebuild_cmd() {
  [ -f "$UNIT" ] || die "$UNIT does not exist; run 'install' first"
  build
  echo "==> systemctl restart $NAME"
  sudo systemctl restart "$NAME"
  verify
  echo "OK: '$NAME' rebuilt and restarted"
}

case "$SUBCMD" in
  install) install_cmd ;;
  rebuild) rebuild_cmd ;;
esac
