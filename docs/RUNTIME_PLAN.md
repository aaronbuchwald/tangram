# Sandboxed App Runtime — Plan

**Status:** rev 2 (2026-06-10) — runtime order re-decided WASM-first; see
[ADR-0001](adr/0001-wasm-first-sandbox-runtime.md). Rev 1 sequenced gVisor
first; Phase 0 of that track is delivered and retained.
**Goal:** run each Tangram app in its own sandbox; add/remove apps on
the fly with immediate access; route every web UI under one port at
`/<app-name>/...`; proxy MCP through agentgateway. Two runtime tracks share
one host architecture: **Track W (chosen spine): WASM components under an
embedded Wasmtime** with capability grants; **Track G (retained): gVisor
containers** for unported/untrusted native apps and as the stabilization
escape hatch.

## The one architectural shift that matters

Today `tangram-shell` hosts every app **in one process** (axum `nest`). That
is exactly what per-app sandboxing must undo: one compromised app would own
the others. So the shell's role changes:

> **From in-process host → to host = reverse proxy + reconciler.**
> Each app runs as its own sandboxed server (its standalone binary already
> serves all surfaces at `/`); a new `tangram-host` routes
> `/<app>/{,api,sync,mcp}` to the app's sandbox and converges the set of
> running sandboxes toward a desired state.

Everything else in this plan hangs off that change. The good news: the SDK
needs almost nothing — apps already run standalone, configure via env, and
use relative UI paths.

## The app contract (this is the WASM door)

WASM-readiness comes from a **contract**, not an abstraction layer. A Tangram
app is, by definition:

1. one HTTP listener on `BIND_ADDR`, serving `/` (UI), `/api/*`, `/sync`,
   `/mcp`, `/healthz` — all relative paths, prefix-mountable;
2. configured **only** via environment variables (WASI supports env);
3. state confined to **one data directory** (`TANGRAM_DATA_DIR`) of plain
   files (WASI preopened dirs support this);
4. outbound network limited to declared needs (sync remotes, strategy APIs).

Anything that satisfies the contract is schedulable — a host binary (dev), an
OCI image under runsc (this plan), or a `wasm32-wasip2` component under
wasmtime/Spin or a containerd WASM shim (later). The host and gateway never
know which. **Rule going forward: no feature may violate the contract**
(no exec-into-container tricks, no shared volumes between apps, no sidecars
as load-bearing architecture).

Known WASM gap, explicitly out of scope until a later phase: the SDK runs on
tokio + rmcp's streamable HTTP server, which don't target WASI today. The
move would be a `wasi:http` adapter inside the SDK — contract-compatible by
construction, not needed now.

## Components

```
                      ┌──────────────────────────────────────────────┐
 client ──:443/:8080──▶ tangram-host (proxy)                          │
   /notes/…           │   ├─ routing table  /<app>/ → sandbox addr    │
   /nutrition/…       │   ├─ SSE/WebSocket passthrough (no buffering) │
   /<app>/mcp ────────┼──▶ agentgateway ──▶ sandbox /mcp              │
                      │   └─ hold-until-healthy on cold start         │
                      │ reconciler                                    │
                      │   ├─ desired state (file → registry app)      │
                      │   ├─ Backend trait: start/stop/health/addr    │
                      │   │    impl 1: docker+runsc   impl 2: k8s     │
                      │   └─ converge loop + status                   │
                      └──────────────────────────────────────────────┘
   sandboxes:  [runsc: notes]  [runsc: nutrition]  [runsc: <new app>]
               each: scratch image, static binary, own data volume
```

- **Proxy** (`tangram-host`, axum/hyper): path-prefix routing with live
  route-table updates; must stream SSE and pass WebSockets through; strips
  `/<app>` prefix. Single public port. On cold start, holds requests until
  the app's `/healthz` passes (this is what makes "add app → immediate
  access" feel immediate; runsc cold start is ~100–300 ms).
- **agentgateway** for the MCP plane: one MCP endpoint multiplexing every
  app's `/mcp` as namespaced targets (its core feature: path-prefix routing,
  `Mcp-Session-Id`-aware statefulness, federation). Its target list is
  generated from the same desired state the reconciler uses. UI/sync traffic
  does NOT go through it — it's an AI-traffic gateway, not a general ingress.
- **Reconciler**: a converge loop over a small `Backend` trait
  (`ensure_running(spec) -> Addr`, `stop(name)`, `health(name)`). The trait
  is the k8s/WASM seam — deliberately tiny, not a platform.
- **Packaging**: per-app OCI images, `FROM scratch` + static musl binary +
  `ui/` dir (or rust-embed), one data volume mount, read-only rootfs,
  non-root, resource limits. Tiny images keep gVisor cold starts fast.

## Phases

### Phase 0 — Contract + packaging (no orchestration yet) — ✅ delivered 2026-06-10
- [x] musl static builds (`x86_64-unknown-linux-musl`, rustls/ring — both
  binaries statically linked); per-app Dockerfile at `apps/<app>/Dockerfile`
  (`FROM scratch` + binary + `ui/`, ~10–15 MB images); built by
  `scripts/build-images.sh` → `tangram/<app>:dev`.
- [x] `docker run --runtime=runsc` validated by hand for both apps
  (read-only rootfs, host publish on 127.0.0.1): `/healthz`, UI `/`,
  `/api/actions/*`, MCP initialize all pass; persistence across container
  replacement on a named volume; bidirectional sync between a sandboxed app
  and a host replica through `/sync`; nutrition resolves a description-only
  `log_meal` via CalorieNinjas through the gVisor netstack (egress works).
- [x] Cold start (`docker run` → first `/healthz` 200): ~240 ms steady
  state; ~1.5 s for the very first runsc container after daemon start
  (platform warmup) — within the ~100–300 ms expectation above.
- [x] CI builds the musl binaries + both images (`images` job, build-only).
- Contract notes discovered: apps pinned the UI dir at an absolute
  compile-time path (violates "runnable anywhere"); fixed by the
  `TANGRAM_UI_DIR` env override in the SDK (env wins over builder value).
  Images bind `0.0.0.0:8080` inside the sandbox (required for port
  mapping); the host-side publish stays loopback. Containers currently run
  as root inside the sandbox (scratch has no passwd; non-root + resource
  limits deferred to Phase 1 hardening — read-only rootfs already used).
- The contract lives in this doc ("The app contract" above), indexed from
  AGENTS.md. Deferred: an automated `/healthz` contract test (the endpoint
  itself is served and was validated end to end).

### Phase 1 (rev 2) — tangram-core split + transport neutrality
Pure-win prep that every target needs (native, WASI, Cloudflare, browser):
- [ ] Split the SDK: `tangram-core` (model/action dispatch, automerge store
  logic, sync-protocol state machine, streamable-HTTP MCP protocol — no
  tokio/hyper) vs. host adapters (the existing tokio/axum host moves behind
  the seam unchanged for users).
- [x] Move sync off WebSockets to an HTTP transport (SSE pokes down + POST
  exchanges up) — delivered 2026-06-10: wire contract in
  [SYNC_PROTOCOL.md](SYNC_PROTOCOL.md), implemented in the SDK
  (`sync.rs`/`web.rs`); tungstenite dropped; legacy `ws://` remote values
  rewritten with a deprecation warning. One transport now serves native,
  WASI, CF, and browsers.
- [x] Default per-app data location moves to `$HOME/.<app-name>` (the future
  capability-grant root; explicit `TANGRAM_DATA_DIR` unchanged) — delivered
  2026-06-10 alongside the transport.
- Exit: apps unchanged (`#[model]`/`#[actions]` identical), native host
  behavior identical, sync interops across old↔new during a deprecation
  window or via a coordinated cutover (single-owner fleet makes this easy).

### Phase 2 (rev 2) — tangram-host with embedded Wasmtime (Track W spine)
Delivered 2026-06-10, with one design refinement over the original sketch:
**logic-in-component, platform-in-host**. The sandbox boundary is the app
LOGIC, not an HTTP server: components export a custom WIT world
(`crates/tangram-host/wit/tangram.wit` — `describe`/`genesis`/`dispatch`/
`state-json`, doc-in/doc-out) instead of serving `wasi:http`, and the native
host owns HTTP, sync, MCP, persistence, and UI files. This dodges WASI 0.3
RC churn entirely (stable wasm32-wasip2 + pinned wasmtime 45) and is a
stronger grant model than preopens: the component has NO filesystem
capability at all — the host is the only thing touching `$HOME/.<app-name>`.
- [x] `tangram-host` crate (`crates/tangram-host`): embedded Wasmtime, one
  LIVE component instance per app (instantiate once, dispatch repeatedly,
  calls serialized per app), reconciler + `notify` watcher over `apps.toml`
  (name → component path, ui dir, data_dir, allow_hosts, env with `${VAR}`
  host-expansion, optional sync remote).
- [x] Capabilities = the host's imports, nothing else: `http-fetch`
  (enforced per-app host allowlist; deny → actionable error naming the
  grant), `log` (→ tracing), `now-ms`. wasip2 std plumbing is linked with an
  EMPTY WASI ctx (no preopens, no sockets); import audit shows no
  wasi:sockets / wasi:http at all.
- [x] SDK guest adapter: the `tangram` crate itself compiles to
  wasm32-wasip2 (`guest.rs` + `tangram::export_component!(Model {..})`;
  apps add `crate-type = ["cdylib"]`), running the SAME action registry and
  store dispatch as native. `tangram::http` / `tangram::time` facades:
  reqwest/SystemTime natively, host imports in the guest (nutrition's
  strategies ported to them). Components: notes 2.4 MiB, nutrition 2.8 MiB.
- [x] Genesis parity guaranteed by construction (one `genesis_bytes()` fn)
  and verified byte-identical guest↔native for both apps — host-managed
  docs replicate bidirectionally with native instances over the Phase-1
  HTTP sync transport (server core + dial-out client reused via the new
  `tangram::sync::DocHandle` seam — the first slice of the core split).
- [x] Full per-app surface under one port, same shapes as the SDK: UI,
  `/api/state|actions|events`, `/sync(+events)`, `/mcp` (rmcp bridge with
  tools from `describe()`), same action error envelope.
- [x] Live converge measured (release build): edit `apps.toml` → app
  serving in ~0.40 s end to end (≈50 ms debounce + ~310–370 ms component
  instantiation incl. cranelift compile); remove → routes gone in ~30 ms;
  component rebuild (mtime) hot-reloads the instance.
- [ ] agentgateway alongside, config generated from the same desired state,
  `/<app>/mcp` routed through it (later phase; the host serves MCP directly
  for now).
- `tangram-shell` stays as zero-dependency dev mode (unchanged, verified).
- Known gap: custom native routes (nutrition's `GET /api/capabilities`
  probe) don't exist in the component world; its UI degrades gracefully to
  components-only input. Needs a `describe()`-level capability story later.
- Exit met: edit `apps.toml` → component live at `/<app>/` in well under a
  second; remove → gone; same binary + config on any Linux/macOS host.

### Phase 3 — Registry app as source of truth (API-driven, live)
Unchanged from rev 1 (was Phase 2): `apps/registry` is itself a Tangram app
(install_app/set_enabled/remove_app/set_image actions); the reconciler
subscribes to its state; `apps.toml` demotes to import/export; bearer-token
auth on mutating routes before any non-localhost exposure; default-deny
egress (grants become enforced, not advisory) lands here.
Exit: registry action or MCP tool call → new sandbox serving in seconds.

### Phase 4 — Cloudflare adapter (remote-in-the-cloud)
- Not WASI: a `workers-rs` host adapter over the same `tangram-core`
  (Workers don't run WASI components; workers-wasi never matured).
- [x] The **DO sync relay** — delivered 2026-06-10 (`cloud/cloudflare/`):
  one Durable Object per app document, SQLite-backed DO storage for doc
  bytes, the Phase-1 HTTP sync transport for peers (same interface as the
  native SDK — interchangeable remotes), automerge via its WASM build.
  Replaces the always-on EC2 role for sync+persistence at near-zero idle
  cost. Validated under `wrangler dev`: native↔relay↔native convergence
  from an empty relay (genesis merges cleanly), state survives relay
  restarts.
- [ ] MCP surface via CF's Agents SDK (`McpAgent`) when full app logic moves.
- Exit (met for sync): a laptop replica syncs through a DO relay with the
  EC2 box off.

### Track G — gVisor (delivered foundation, on-demand backend)
- Phase 0 above is **kept**: images, CI job, runsc install, validation. It
  is the escape hatch if WASI 0.3 stabilization slips, and the designated
  runtime for **unported or untrusted native apps** (per D4/D5) — a
  `docker+runsc` Backend impl behind the same trait and desired-state
  schema whenever needed.
- k8s extension (was Phase 3): unchanged design — `kube-rs` Backend,
  `RuntimeClass: gvisor`, minikube as that phase's test bed; WASM under k8s
  arrives via containerd wasm shims as just another RuntimeClass. Backlog
  until multi-node/org needs are real.

## Pushback / positions taken (challenge these)

1. **Don't start on k8s/minikube.** Target today is one box; the hard
   requirements (on-the-fly add/remove, instant access, path routing) are
   met by a few hundred lines of reconciler over the Docker API with runsc.
   k8s would front-load image registries, cluster lifecycle, ingress
   controllers, and YAML while making "immediate" harder, not easier. The
   k8s path stays real because the *desired-state schema* and the *Backend
   trait* are designed first — Phase 3 is a backend swap, not a rewrite.
2. **Registry-as-Tangram-app over a separate database.** The "database +
   client API" version is exactly what the SDK already generates from a
   model. Dogfooding it gives API+UI+MCP+sync for free and makes the
   platform self-describing. (Fallback if this feels too cute: sqlite +
   axum CRUD in tangram-host; the reconciler interface doesn't change.)
3. **agentgateway for MCP only**, not as the general ingress. It's built
   for the agent plane (MCP multiplexing, sessions, tool governance); SSE
   UIs and sync WebSockets are ordinary HTTP that tangram-host proxies fine.
4. **WASM-readiness = enforcing the contract**, not building a runtime
   abstraction now. The only speculative artifact we keep is the small
   `Backend` trait, which Phase 1 needs anyway.
5. **The file watcher survives** as Phase 1's control plane and later as
   import/export — it's the right dev/bootstrap UX even after the registry
   exists.

## Decisions (resolved 2026-06-10)

- **D1 — topology: single box for now.** Phase 3 (k8s backend) is backlog;
  Docker API + runsc is the Phase 1–2 substrate. The desired-state schema
  and `Backend` trait keep the k8s swap honest.
- **D2 — control plane: registry as a Tangram app** (Phase 2), with the
  Phase 1 file watcher kept as bootstrap and import/export.
- **D3 — agentgateway scope: MCP plane only.** tangram-host proxies
  UI/SSE/sync directly.
- **D4 — trust model: own/trusted apps for months.** Read-only rootfs and
  resource limits from Phase 0; default-deny egress is a Phase 2 hardening
  item, not a blocker.
- **D5 (rev 2, 2026-06-10) — WASM-first runtime.** The spine is WASM
  components under an embedded Wasmtime in `tangram-host`; gVisor (Track G,
  Phase 0 delivered) is retained as the stabilization escape hatch and the
  runtime for unported/untrusted native apps. Full comparison and rationale:
  [ADR-0001](adr/0001-wasm-first-sandbox-runtime.md). Supersedes the
  runsc-first sequencing in D1 (the single-box topology decision itself is
  unchanged).

## Risks

- **WASI 0.3 is RC-grade until ~late 2026** (1.0 target). Mitigation: we
  embed and pin our own Wasmtime (no runtime drift), and Track G stands as
  the tested escape hatch.
- **The core/adapter split is real SDK surgery** (tokio/axum/rmcp transport
  out of `tangram-core`; MCP streamable-HTTP reimplemented portably). Keep
  the native adapter's behavior bit-identical via the existing end-to-end
  checks (schema parity, sync, SSE) before any WASI work starts.
- **Streaming through the proxy**: SSE buffering bugs would break the
  live-update core. Mitigate: hyper-level proxy (not a generic middleware),
  integration tests for SSE latency + sync through the full chain
  (client → host → sandboxed app).
- **Sync transport migration**: moving off WebSockets must not strand
  replicas; single-owner fleet now makes a coordinated cutover cheap — do
  it before there are external users.
- **agentgateway config drift**: generate its config from the same desired
  state, never hand-edit.
- (Track G) **runsc gofer I/O overhead** — fine for automerge documents at
  Tangram scale; **per-app egress on plain Docker networking** is weak
  (k8s NetworkPolicy is cleaner) — both inherited only if/when Track G runs
  third-party apps; WASM grants make egress allowlists first-class on the
  spine.
