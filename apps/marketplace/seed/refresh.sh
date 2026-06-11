#!/usr/bin/env bash
# Refresh the marketplace's seed data from the CURRENT first-party builds.
#
# The seed listings in apps/marketplace/src/lib.rs embed, per app:
#   - <app>.sha256 — the sha-256 the host pins the release artifact to;
#   - <app>.wit    — the import audit: the `world root` block of
#     `wasm-tools component wit`, the mechanical proof of the component's
#     closed world (what it can even NAME — no sockets, no filesystem, no
#     wasi:http; outbound reach only through tangram:app/host behind the
#     host's allow_hosts gate).
#
# Run this (then commit the seed/ diff) whenever a release changes the
# component bytes — seeds are refreshed per release, the embedded digests
# are real digests of the artifacts the release publishes.
#
# Requires: wasm-tools (cargo install wasm-tools), the wasm32-wasip2 target.

set -euo pipefail
cd "$(dirname "$0")/../../.."

cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
  --lib --target wasm32-wasip2 --release

for app in notes nutrition registry; do
  artifact="target/wasm32-wasip2/release/${app}.wasm"
  sha256sum "$artifact" | cut -d' ' -f1 > "apps/marketplace/seed/${app}.sha256"
  # The import audit: the world block only (the interface definitions that
  # follow it describe shapes, not reach).
  wasm-tools component wit "$artifact" \
    | sed -n '/^world root {/,/^}/p' > "apps/marketplace/seed/${app}.wit"
  echo "refreshed ${app}: $(cat "apps/marketplace/seed/${app}.sha256")"
done
