# AGENTS.md

Tangram is a local-first Rust SDK: define a plain-Rust model + action methods
and get a CRDT-replicated store (Automerge) with derived MCP, web UI/JSON API,
and sync surfaces from one binary. Read [README.md](README.md) for usage and
[docs/SDK_DESIGN.md](docs/SDK_DESIGN.md) for architecture before making
non-trivial changes. This file is an index, not a manual.

`CLAUDE.md` is a symlink to this file; Claude Code's skills directory is a
symlink to `.agents/skills`.

## Where things are

- `crates/tangram` — the runtime: store/sync/web/mcp/app modules, App builder;
  also compiles to wasm32-wasip2 as the component guest adapter (`guest.rs`,
  `export_component!`, the `http`/`time` facades)
- `crates/tangram-host` — embedded-Wasmtime host: runs apps as WASM
  components per `apps.toml` with capability grants (WIT world in its `wit/`;
  README "Run apps as WASM components")
- `crates/tangram-macros` — `#[model]` / `#[actions]` proc macros
- `apps/notes` — minimal example app (`apps/notes/ui` for its frontend)
- `apps/nutrition` — fuller example; pluggable resolution in
  `apps/nutrition/src/strategy/` (see README "Nutrition strategies")
- `apps/shell` — multi-app host serving every app under one port, prefixed
- `cloud/cloudflare` — Durable-Object sync relay (TypeScript Worker); speaks
  the same sync interface as the SDK, interchangeable as a remote
- `docs/SDK_DESIGN.md` — architecture & roadmap
- `docs/SYNC_PROTOCOL.md` — the HTTP(+SSE) sync wire contract (binding for
  every sync server: native SDK and the Cloudflare relay)
- `docs/RUNTIME_PLAN.md` — sandboxed runtime plan **and the app contract**
  (binding: no feature may violate it); packaging in `apps/<app>/Dockerfile`
  + `scripts/build-images.sh` (README "Run an app sandboxed (gVisor)")
- `docs/adr/` — architecture decision records (ADR-0001: WASM-first runtime
  vs gVisor — read before runtime/sandbox work)
- `.agents/skills/` — agent skills (SKILL.md format):
  - `systemd-service` — install/rebuild a Tangram binary as a systemd service
  - `local-replica` — run/check/stop a local replica syncing to a remote
- `.env` (gitignored; template in `.env.example`) — secrets/API keys live
  here. Never commit secrets.
- README sections worth jumping to: "Getting started" (remote service +
  tunnel + replica), "Nutrition strategies", "Configuration (env / .env)"

## Commands

```sh
cargo build --workspace                                   # build gate
cargo clippy --workspace --all-targets -- -D warnings     # lint gate
cargo fmt --check                                         # format gate
cargo run -p tangram-notes                                # run an example
cargo run -p tangram-shell                                # run all apps, one port
```

## Conventions not obvious from code

- Every user-facing operation must be a registered action: a sync method
  (`&self`/`&mut self`, pure state transition, no I/O) or an `async fn`
  taking `Ctx<Self>` for network work (resolve outside the lock, commit via
  `Ctx::mutate`). Custom routes are reserved for non-operations (capability
  probes, static assets). The store lock is never held across an await.
- Model `Default` must be deterministic — it becomes the shared genesis
  commit. Use `Vec`, not `HashMap`.
- UI fetches use relative paths only: apps get prefix-mounted under the shell
  (`/notes/`, `/nutrition/`).
- Never commit secrets; they belong in `.env` (gitignored).

## Working agreements (self-improvement)

- If the user gives substantially the same instruction or correction a second
  or third time, propose codifying it as a rule here — show a small concrete
  diff for approval; never silently self-edit this file.
- If a multi-step workflow has been performed roughly the same way more than
  twice (by user request or agent initiative), propose capturing it as a
  skill in `.agents/skills/`, using the existing two skills as templates.
- Keep this file an index: when adding to it, prefer a pointer to a
  README/docs section or a skill over inlining content. If a section grows
  past ~10 lines, move the content out and link it.
- At the end of a session that introduced a new recurring workflow, file/CI
  convention, or service, check whether this index still tells a newcomer
  where that thing lives; if not, propose the one-line index update.
