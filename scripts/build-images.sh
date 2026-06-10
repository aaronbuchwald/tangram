#!/usr/bin/env bash
# Build the per-app sandbox images (RUNTIME_PLAN Phase 0):
# static musl binaries -> FROM scratch images tagged tangram/<app>:dev.
#
# Prereqs: rustup target add x86_64-unknown-linux-musl; apt install musl-tools
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

apps=(notes nutrition)

pkgs=()
for app in "${apps[@]}"; do pkgs+=(-p "tangram-$app"); done
cargo build --release --target x86_64-unknown-linux-musl "${pkgs[@]}"

for app in "${apps[@]}"; do
    docker build -f "apps/$app/Dockerfile" -t "tangram/$app:dev" .
done

docker image ls --format '{{.Repository}}:{{.Tag}}  {{.Size}}' |
    grep '^tangram/'
