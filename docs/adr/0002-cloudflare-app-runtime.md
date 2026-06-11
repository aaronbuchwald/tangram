# ADR-0002: Cloudflare app runtime — jco-transpiled components in the Durable Object

**Status:** accepted (2026-06-11)
**Deciders:** Aaron (owner), with spike/analysis by Claude
**Related:** [ADR-0001](0001-wasm-first-sandbox-runtime.md) (+ addendum),
[RUNTIME_PLAN.md](../RUNTIME_PLAN.md) Phase 7

## Context

Phase 4 left Cloudflare as a **sync relay**: one Durable Object per app
document speaking the HTTP sync protocol, knowing nothing about the apps'
models. Phase 7 upgrades CF to a **full app host** — per-app UI,
`/api/state|actions|events|capabilities`, action dispatch, and `/mcp` —
without an always-on box.

Workers cannot run WASI components natively (workers-wasi never matured;
ADR-0001 anticipated this). The app logic already exists in exactly the
right shape: every app compiles to a `wasm32-wasip2` component exporting
the `tangram:app` world (`describe`/`genesis`/`dispatch`/`state-json`,
doc-in/doc-out — `crates/tangram-host/wit/tangram.wit`), importing only
`tangram:app/host` (`http-fetch` with a per-app host allowlist, `log`,
`now-ms`) plus inert wasip2 std plumbing (env/clocks/random/stdio).

Constraint that shaped the spike: the SDK's guest adapter makes `http-fetch`
**synchronous from the guest's point of view** (the single-pass executor in
`crates/tangram/src/guest.rs` panics on a future that doesn't complete
synchronously); the host runs it async. Any JS host therefore needs a way to
suspend wasm while a `fetch()` promise settles.

## Options

### A — `jco transpile` the existing components (preferred per plan)

Transpile `notes.wasm` / `nutrition.wasm` to JS + core wasm with
[jco](https://github.com/bytecodealliance/jco); the Worker provides the
`tangram:app/host` imports (allowlisted `fetch()`, `console` logging,
`Date.now()`) and a small hand-written WASI shim. The async-import problem is
solved by **JSPI** (WebAssembly JS Promise Integration): jco's
`--async-mode jspi` wraps `http-fetch` in `WebAssembly.Suspending` and the
`dispatch` export in `WebAssembly.promising`.

### B — workers-rs: compile a Rust worker linking tangram-core + app crates

A `workers-rs` crate for `wasm32-unknown-unknown` linking the app model/
action crates directly (they build as libs) and tangram-core for
sync/MCP/store.

## Spike evidence (2026-06-11, this repo, Node v20.20.2, wrangler 4.86 / workerd under miniflare)

### Path A — works end to end

- `jco 1.22.0` (installs and runs on Node 20) transpiles all three
  components with `--instantiation async` (core wasm passed in as
  workerd-compiled `WebAssembly.Module` imports — required because workerd
  forbids runtime `WebAssembly.compile`, so jco's default
  `fetch(new URL(...))` loading and `--base64-cutoff` inlining are both
  unusable).
- **notes under workerd**: `describe()`, `genesis()`, `dispatch("add_note")`
  against genesis bytes, `state-json` — all correct, including automerge in
  the guest (random/clock/env via a ~70-line hand-written WASI shim; the
  `@bytecodealliance/preview2-shim` node implementation is not
  workerd-compatible, and the full shim is unnecessary — the component has
  no filesystem/socket imports at all).
- **JSPI works in workerd**: nutrition resolved a description-only
  `log_meal` through an `async httpFetch` JS import that awaited a real
  25 ms timer — i.e. the guest suspended mid-dispatch and resumed —
  returning a canned CalorieNinjas response
  (`--async-mode jspi --async-imports 'tangram:app/host#http-fetch'
  --async-exports 'tangram:app/guest#dispatch'`). No compatibility flag
  needed at `compatibility_date = 2026-05-01`.
- **The Rust MCP machine reused**: a 252 KB dedicated component
  (`cloud/cloudflare/mcp-core`, world `tangram:mcp`) wrapping
  `tangram_core::mcp::McpServer` transpiles and serves
  initialize → tools/list → tools/call → resolve plus session 404s,
  byte-identically to the native protocol layer.
- Sizes (raw / gzip): notes core wasm 2.41 MiB / 722 KiB; nutrition
  2.87 MiB / 819 KiB; mcp-core 237 KiB / 91 KiB; jco JS glue ~310 KiB per
  component (minifies well). Total worker bundle ≈ 1.7 MiB gzipped — under
  even the free-plan 3 MiB post-compression limit.

### Path B — blocked on SDK surgery this phase can't do

- `cargo build -p tangram-core --target wasm32-unknown-unknown` fails out of
  the box (getrandom needs its `wasm_js` backend + RUSTFLAGS cfg).
  Fixable, but the real blocker:
- The app crates' wasm side (`#[cfg(target_family = "wasm")]`) is the
  component guest adapter: `tangram::http`/`tangram::time` lower to
  `tangram:app/host` imports that only exist under a component runtime. A
  workers-rs build would need a third backend in those facades (worker::Fetch)
  inside `crates/tangram` — which a parallel agent owns and this phase must
  not touch — plus per-app cfg plumbing, a duplicated automerge (Rust-side)
  next to the relay's JS automerge, and glue for DO storage/SSE through
  workers-rs' JS interop.

## Decision

**Path A.** The components that already ship to tangram-host run unmodified
on Workers; the Worker-side host shim is the same ~150-line capability
surface tangram-host implements (`http-fetch` allowlist, log, now-ms), and
JSPI removes the one real obstacle. App logic, genesis bytes, and the action
error contract stay single-sourced in Rust.

**MCP**: reuse `tangram_core::mcp` via the dedicated `tangram:mcp` component
(`cloud/cloudflare/mcp-core`) rather than a TS reimplementation — the
protocol state machine (negotiation quirks, rmcp-parity status codes,
session rules) is regression-pinned in Rust and 91 KiB gzipped is cheap
insurance against drift. The TS layer only adapts HTTP requests/responses
and executes tool calls through the same app dispatch path as
`/api/actions/{name}`.

Architecture: the existing per-app Durable Object grows from sync relay to
app host. The DO keeps the document (SQLite-backed storage, unchanged sync
endpoints) and gains a lazily-instantiated app component + MCP component;
dispatch is doc-in/doc-out against the DO's stored document, exactly
tangram-host's semantics (`AppRuntime::dispatch`): save → guest → merge
returned save → persist → poke sync peers and state listeners. Genesis for
an empty DO comes from the component's deterministic `genesis()`
(byte-identical to native by construction; the SYNC_PROTOCOL genesis rule
for *model-ignorant* relays no longer applies once the DO knows the model —
identical bytes cannot fork). Apps stay configured by the `APPS` var; an
app listed there without a bundled component degrades to exactly the old
relay surface.

## Consequences

- A second toolchain in `cloud/cloudflare`: cargo (wasip2) + jco transpile
  feed `dist/components/`, built by `scripts/`/npm scripts; CI builds them
  before the miniflare e2e. Components are build artifacts, never committed.
- JSPI is load-bearing. It is shipped in workerd at our pinned versions
  (validated under miniflare, which runs real workerd); treat a production
  `wrangler deploy` smoke test as part of first deployment. Fallback if a
  runtime ever lacks it: transpile sync and let description-based
  `log_meal` fail cleanly while manual logging keeps working (nutrition's
  offline strategy already gives the clean degradation).
- `mcp-core` is a standalone crate (own `[workspace]`) inside
  `cloud/cloudflare/` — additive, no root-workspace impact; it depends on
  `tangram-core` by path, so core MCP changes flow in on rebuild.
- MCP sessions + pending tool calls live in component memory: a DO
  restart/eviction drops them and clients re-initialize (same recovery as
  an rmcp server restart; spec'd 404 behavior).
- Per-session automerge sync states stay in DO memory (unchanged from the
  relay), and the document remains one storage value (2 MiB bound, noted in
  the relay README).
- Single-user scope at time of writing: no tenancy, no auth on actions/MCP.
  **Both are now delivered**: Phase 5 (multi-tenancy, `/t/<tenant>/` routing,
  bearer-gated tenant apps) and Phase 6 (CF identity — OAuth accounts, PATs,
  per-tenant namespaces) landed 2026-06-11. The "don't host secrets" caveat in
  the relay README remains valid for single-user deployments without auth.
- wrangler stays pinned `~4.86` (Node 20 constraint); jco pinned exactly
  (`1.22.0`) since its output is part of the deployable artifact.
