#!/usr/bin/env bash
# Tear down the one-command Langfuse observability stack (observability O1;
# docs/design/gateway-observability-identity.md §7).
#
#   scripts/observability-down.sh            # stop + remove containers
#   scripts/observability-down.sh --volumes  # also drop the Postgres volume
#
# Leaves the provisioned OTLP keys in .env untouched (so a later
# scripts/observability-up.sh reuses the same project keys). The host degrades
# to host-local logs + the Prometheus metric while the stack is down — never an
# error (the tracing block only points at a configured endpoint).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

compose_file="deploy/observability/compose.yml"

if ! docker compose version >/dev/null 2>&1; then
    echo "error: 'docker compose' is required (install Docker Compose v2)" >&2
    exit 1
fi

if [ "${1:-}" = "--volumes" ]; then
    docker compose -f "$compose_file" down --volumes
    echo "observability: stack stopped and the Langfuse Postgres volume removed."
else
    docker compose -f "$compose_file" down
    echo "observability: stack stopped (the Postgres volume is kept; --volumes to drop it)."
fi
