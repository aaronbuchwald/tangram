#!/usr/bin/env bash
# One-command, observability-by-default Langfuse stack (observability O1;
# docs/design/gateway-observability-identity.md §7).
#
#   scripts/observability-up.sh
#
# Brings up Langfuse + Postgres (deploy/observability/compose.yml, all on
# loopback) and, on first run, provisions a Langfuse project + OTLP ingest key
# pair into .env (gitignored) so the host's agentgateway OTLP exporter
# authenticates with no manual UI step. The keys are written as:
#
#   LANGFUSE_PUBLIC_KEY / LANGFUSE_SECRET_KEY  — the raw Langfuse API keys
#   OTEL_EXPORTER_OTLP_ENDPOINT                — the OTLP/HTTP ingest URL
#   OTEL_EXPORTER_OTLP_HEADERS                 — authorization=Basic base64(pk:sk)
#   OTEL_EXPORTER_OTLP_PROTOCOL                — http/protobuf
#
# The OTEL_* vars are the standard OpenTelemetry exporter env the agentgateway
# child inherits (from .env), so the ingest CREDENTIAL stays host-side and never
# lands in the generated config (ADR-0005). Then point the host at it (apps.toml):
#   [gateway]
#   otlp_endpoint = "http://127.0.0.1:3000/api/public/otel"
# and restart tangram-host — traces appear at http://127.0.0.1:3000.
#
# Tear down with scripts/observability-down.sh.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

compose_file="deploy/observability/compose.yml"
env_file="$repo_root/.env"
endpoint="http://127.0.0.1:3000/api/public/otel"

if ! docker compose version >/dev/null 2>&1; then
    echo "error: 'docker compose' is required (install Docker Compose v2)" >&2
    exit 1
fi

# Read an existing value for KEY from .env (empty if absent / no .env).
env_get() {
    [ -f "$env_file" ] || return 0
    sed -n "s/^$1=//p" "$env_file" | tail -n1
}

# Upsert KEY=VALUE in .env (replace an existing line, else append). VALUE is
# written verbatim; never logged.
env_set() {
    local key="$1" value="$2"
    touch "$env_file"
    if grep -q "^$key=" "$env_file"; then
        local tmp
        tmp="$(mktemp)"
        grep -v "^$key=" "$env_file" >"$tmp"
        mv "$tmp" "$env_file"
    fi
    printf '%s=%s\n' "$key" "$value" >>"$env_file"
}

# First-run provisioning: reuse the keys already in .env if present (idempotent
# — Langfuse re-applies the same pair as a no-op), else generate a fresh pair.
public_key="$(env_get LANGFUSE_PUBLIC_KEY)"
secret_key="$(env_get LANGFUSE_SECRET_KEY)"
if [ -z "$public_key" ] || [ -z "$secret_key" ]; then
    public_key="pk-lf-$(openssl rand -hex 16)"
    secret_key="sk-lf-$(openssl rand -hex 16)"
    echo "observability: provisioning a fresh Langfuse OTLP key pair into .env"
else
    echo "observability: reusing the existing Langfuse OTLP key pair from .env"
fi

# Bring the stack up, handing Langfuse the keys to provision headlessly.
LANGFUSE_INIT_PROJECT_PUBLIC_KEY="$public_key" \
LANGFUSE_INIT_PROJECT_SECRET_KEY="$secret_key" \
    docker compose -f "$compose_file" up -d

# Wait for the Langfuse health endpoint (the OTLP ingest comes up with it).
echo -n "observability: waiting for Langfuse on http://127.0.0.1:3000 "
for _ in $(seq 1 60); do
    if curl -fsS http://127.0.0.1:3000/api/public/health >/dev/null 2>&1; then
        echo "ready"
        break
    fi
    echo -n "."
    sleep 2
done

# Persist the keys + the standard OpenTelemetry exporter env into .env. The
# agentgateway child inherits these from the host (dotenvy), so the ingest
# CREDENTIAL stays host-side — never in the generated config (ADR-0005).
otlp_basic="Basic $(printf '%s:%s' "$public_key" "$secret_key" | base64 | tr -d '\n')"
env_set LANGFUSE_PUBLIC_KEY "$public_key"
env_set LANGFUSE_SECRET_KEY "$secret_key"
env_set OTEL_EXPORTER_OTLP_ENDPOINT "$endpoint"
env_set OTEL_EXPORTER_OTLP_HEADERS "authorization=$otlp_basic"
env_set OTEL_EXPORTER_OTLP_PROTOCOL "http/protobuf"

cat <<EOF

observability: Langfuse is up at http://127.0.0.1:3000 (loopback only).
Keys + OpenTelemetry exporter env written to .env (LANGFUSE_PUBLIC_KEY /
LANGFUSE_SECRET_KEY / OTEL_EXPORTER_OTLP_ENDPOINT / OTEL_EXPORTER_OTLP_HEADERS /
OTEL_EXPORTER_OTLP_PROTOCOL).

Enable export in apps.toml [gateway] and restart tangram-host:
    otlp_endpoint = "$endpoint"

Tear down: scripts/observability-down.sh
EOF
