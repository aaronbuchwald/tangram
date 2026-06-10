# ADR-0001: Per-app sandbox runtime — embedded Wasmtime (WASM-first), gVisor retained

**Status:** accepted (2026-06-10)
**Deciders:** Aaron (owner), with research/analysis by Claude
**Related:** [RUNTIME_PLAN.md](../RUNTIME_PLAN.md) (rev 2 reflects this decision)

## Context

Tangram apps must move from host binaries to per-app sandboxes, managed by a
`tangram-host` (reverse proxy + reconciler) with live add/remove, path-scoped
UIs on one port, and MCP behind agentgateway. Two viable runtime tracks
emerged, and the deployment reality is **two different hosts**: an
always-on Linux remote (EC2) and a developer laptop (possibly macOS), with
more devices later. A move toward WASM was always intended; the question is
whether it is the *next* step or a later one.

Phase 0 (gVisor packaging) is **already delivered and validated**: static
musl binaries, 10–15 MB scratch images, runsc installed, all surfaces +
persistence + sync + egress verified in-sandbox, ~240 ms cold start. That
work is kept regardless of this decision.

Key facts established (2026-06):

- **WASI 0.3 shipped (Feb 2026)** with native async in the component model
  (`stream<T>`/`future<T>`), `wasi:http@0.3` RCs, supported in Wasmtime 37+.
  WASI 1.0 is targeted late 2026. Capability grants (preopened dirs,
  per-host outbound HTTP) are the security model Tangram wants: an app
  *cannot express* paths outside `$HOME/.<app-name>`.
- **`wasi:http` has no WebSockets** (incl. 0.3). Tangram's sync runs over
  WebSocket today; the automerge sync protocol itself is transport-agnostic.
- **The SDK's IO stack does not compile to WASI** (tokio, axum/hyper, rmcp
  transport, tungstenite, reqwest). The model/actions/macros/automerge core
  ports cleanly. A `tangram-core` (platform-agnostic) / host-adapter split
  is required for any WASM target.
- **Cloudflare Workers do not run WASI components** (workers-wasi never
  matured). CF is reached via `workers-rs`/JS glue around the same WASM
  core. Functionally CF is the best cloud fit for the *remote* role:
  one Durable Object per document (actor model = our store), WebSocket
  hibernation, **SQLite-backed DOs (GA)**, pre-warmed isolates, first-class
  MCP hosting (Agents SDK / `McpAgent`). Precedent: PartyKit-style CRDT
  sync servers on DOs.
- gVisor is Linux-only; on a macOS laptop it means Docker Desktop's VM.
  Wasmtime embeds in a single static host binary on both OSes.

## Options

### A — Continue gVisor track (original Phase 1): docker+runsc Backend first

`tangram-host` reconciles Docker containers with `runtime: runsc`; apps stay
native binaries in scratch images; WASM remains a later exploratory phase.

### B — WASM-first: `tangram-host` embeds Wasmtime; runsc demoted to fallback

Split the SDK into `tangram-core` + host adapters; switch sync to an
HTTP-based transport (no WS dependency); compile apps to `wasm32-wasip2`
components; `tangram-host` keeps one live component instance per app and
grants capabilities directly (preopen `$HOME/.<app-name>`, HTTP allowlist).
gVisor images/Backend retained for unported or untrusted *native* apps.

## Comparison

| Dimension | A: gVisor (docker+runsc) | B: embedded Wasmtime |
|---|---|---|
| Isolation model | Syscall interception around a process that can *ask* for anything | Capability grants: app cannot name what it wasn't given |
| Ops per host | Docker + runsc install/config on every machine; not native on macOS | One static `tangram-host` binary, identical on Linux/macOS |
| Two-host deployment (stated pain) | Two infrastructure stacks to keep converged | Same binary + config both sides |
| SDK cost | Zero (delivered) | One-time surgery: core/adapter split, `wasi:http` host, MCP transport reimpl, HTTP sync transport |
| Sync transport | WS works today | WS unavailable → HTTP/SSE sync (transport swap benefits all targets; drops tungstenite) |
| App lifecycle | Long-lived process per container (matches store today) | Per-request `wasmtime serve` would break the doc actor → solved by *embedding* (host keeps instances alive) |
| Cold start | ~240 ms measured | Component instantiation typically sub-ms–tens of ms (pre-warm trivial when embedded) |
| Cloud path | EC2-shaped (containers somewhere) | CF Workers/DO adapter over the same `tangram-core`; DO relay replaces the always-on box |
| Untrusted native apps (future 3rd-party) | **Best tool** | Not applicable (WASM-only) |
| Maturity risk | Mature, boring | WASI 0.3 is RC until ~late 2026; mitigated by pinning our own embedded Wasmtime |
| Browser-replica alignment | None | Same core compiles toward the browser-WASM goal in SDK_DESIGN |
| DevX for app authors | unchanged | unchanged (`#[model]`/`#[actions]` untouched); debugging inside wasm worse, core tests stay native |

## Decision

**WASM-first (Option B).** The owner's stated direction is WASM; the
remaining cost of the gVisor track is per-machine infrastructure that WASM
makes unnecessary, while WASM's cost is a one-time SDK refactor that also
unlocks Cloudflare, browser replicas, and a strictly stronger
capability-security story. The deciding operational fact: across two (soon
more) heterogeneous hosts, "one static host binary" beats "Docker+runsc
everywhere".

gVisor is **retained, not reverted**: Phase 0 artifacts (images, CI job,
runsc Backend design) stay as (a) the escape hatch if WASI 0.3 stabilization
slips and (b) the designated runtime for unported/untrusted native apps
when third-party apps arrive (D4).

## Consequences

- New sequencing in RUNTIME_PLAN rev 2: core/adapter split + HTTP sync →
  `tangram-host` with embedded-Wasmtime Backend → registry app → CF/DO
  relay; k8s and runsc become on-demand backends, not the spine.
- The sync protocol moves off WebSockets everywhere (one transport for
  native, WASI, CF, and eventually browsers).
- rmcp's server transport is replaced in non-tokio hosts by a hand-rolled
  streamable-HTTP MCP layer in `tangram-core` (it is HTTP+SSE+JSON-RPC we
  already speak end-to-end).
- agentgateway scope (D3) is unchanged: it fronts `/mcp` regardless of what
  runtime serves it.
- Risk accepted: WASI 0.3 RC churn until ~1.0 (late 2026). Pinned Wasmtime
  + the gVisor escape hatch bound the blast radius.

## Addendum — deployment targets and the shared remote interface (accepted 2026-06-10)

The owner confirmed Option B and fixed the deployment targets:

1. **Remote on EC2** (native today, Wasmtime-hosted next) and **local
   replica** on the laptop, with per-app data confined to
   `$HOME/.<app-name>` (the default data location now; *enforced* as a
   capability grant once the embedded-Wasmtime host lands).
2. **Cloudflare Workers as an alternative remote**: a Durable-Object sync
   relay (one DO per app document, SQLite-backed storage, automerge via its
   WASM build) that is **interchangeable with the EC2 remote behind one
   shared interface** — the HTTP(+SSE) sync transport. A replica points
   `TANGRAM_REMOTE_<APP>` at either `https://<host>/<app>/sync` (EC2) or
   the Worker's URL and cannot tell the difference. CF is reached via a
   thin TS/automerge-wasm relay first, not a `workers-rs` port of the SDK;
   full app logic on CF (MCP/UI via the Agents SDK) remains Phase 4 scope.
3. **Sequencing within Option B**: the shared interface ships first
   (sync-over-HTTP in the SDK + the CF relay + the new data layout), then
   the embedded-Wasmtime host adds capability enforcement on top. If small
   interface/config divergences between EC2 and CF are unavoidable, prefer
   the *cleanest* divergence over forcing identical mechanics (e.g., the
   relay may persist doc bytes in DO SQLite while EC2 persists files — the
   wire interface, not the storage, is the contract).
