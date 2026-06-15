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
4. outbound network limited to declared needs (sync remotes, strategy APIs) —
   at the **call grain** (method + host + path + request-shape), not just the
   host: an app declares the exact calls it makes and the host injects the
   credential onto the matched call only, denying undeclared calls on an
   allowlisted host (ADR-0008; fine-grained-egress §4). A host-keyed
   `allow_hosts`/`inject` (the original grain) is the maximally-broad call, so
   existing apps are unchanged.

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

**UI-authoring exception (ADR-0007), narrow and one-app-only:** ordinary apps
keep the strict single-file, no-build UI; the **first-party `tangram` shell
app** may build its UI with a bundler (a `dist/` from Vite/esbuild). Only the
*authoring* rule is waived — the built output is still a self-contained `ui/`
of static files served identically, so the *runtime* contract (relative
paths, prefix-mountable, iframable, no host FS) is unchanged. The shell's
**backend is a normal wasm component** under the unchanged capability
contract. "No feature may violate the contract" still binds every non-shell
app. See `apps/tangram/` and [ADR-0007](adr/0007-shell-build-pipeline-exception.md).

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
- [x] Split the SDK: `tangram-core` (model/action dispatch, automerge store
  logic, sync-protocol state machine, streamable-HTTP MCP protocol — no
  tokio/hyper) vs. host adapters (the existing tokio/axum host moves behind
  the seam unchanged for users) — delivered 2026-06-11.
  `crates/tangram-core` holds the action registry, `Store`/`Ctx` + dispatch
  (change signal is a plain callback; the native host wires it to a watch
  channel), the sync session/framing server core, and a **sans-io
  streamable-HTTP MCP server** (`tangram_core::mcp`) that replaced rmcp in
  the SDK: rmcp 1.7's wire behavior was captured from a live app as golden
  (`crates/tangram-core/tests/fixtures/rmcp-golden.json`), the same
  end-to-end suite (`crates/tangram/tests/mcp.rs`) passed against rmcp and
  then against the new layer, and the live bar was the real Claude Code
  client (`claude mcp add --transport http …` → connected; agent
  `tools/call` writes landed in the doc). `crates/tangram` no longer
  depends on rmcp at all (tangram-host's rmcp bridge is swapped in a
  follow-up); CI enforces that `tangram-core` keeps compiling for
  wasm32-wasip2 with no tokio/hyper/axum/reqwest/rmcp.
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
- [x] agentgateway alongside — delivered 2026-06-11 (`[gateway]` in
  apps.toml; `crates/tangram-host/src/gateway.rs`; README "MCP through
  agentgateway"). The host stays the single entry point: agentgateway
  (v1.2.1, official binary) runs as a host-SUPERVISED child (restart with
  backoff, killed on shutdown) on an internal port, its config GENERATED
  from the merged desired state on every converge (atomic write,
  agentgateway hot-reloads; registry installs appear on the MCP plane
  without restarts). Public `/<app>/mcp` is proxied host→gateway→an
  internal loopback listener serving the per-app rmcp services, preserving
  `Mcp-Session-Id` statefulness + SSE end to end and the bearer gate on
  mutating registry tools (the gateway forwards Authorization; enforcement
  stays host-side). NEW: the aggregate `/mcp` endpoint — every app's tools
  on one session, namespaced `<app>_<tool>` (agentgateway multiplexing).
  agentgateway 1.2 binds wildcard, so every generated route carries a
  loopback-only source authorization rule. Missing binary → clear warning +
  direct per-app serving exactly as before (pinned by
  `tests/gateway_lifecycle.rs`, alongside handshake/auth/converge/crash-
  recovery coverage).
- `tangram-shell` stays as zero-dependency dev mode (unchanged, verified).
- [x] Capabilities parity (the former known gap): `describe()` carries an
  optional `capabilities` object, computed by the app at instantiation from
  its granted env (nutrition derives it from the same constructor its
  `get_capabilities` action returns — no hand-written route), and the
  host serves it at `GET /<app>/api/capabilities` (404 for apps that
  publish none, matching a native app without the probe). Byte-for-byte
  parity native↔host is pinned by `crates/tangram-host/tests/capabilities.rs`.
- Exit met: edit `apps.toml` → component live at `/<app>/` in well under a
  second; remove → gone; same binary + config on any Linux/macOS host.

### Phase 3 — Registry app as source of truth (API-driven, live)
Delivered 2026-06-11. `apps/registry` is itself a Tangram app; the host
merges its replicated spec list over `apps.toml` (the file stays as
bootstrap + the registry's own entry — D2's import/export role).
- [x] `apps/registry`: `#[model]` list of app specs mirroring the
  `apps.toml` schema (name, component, ui, data_dir?, allow_hosts, env with
  `${VAR}` host-side expansion, enabled) + `install_app` / `set_enabled` /
  `remove_app` / `set_component` / `set_allow_hosts` / `set_env` /
  `list_apps` actions; builds native AND wasm32-wasip2 like every app; fleet
  UI in the shared design system. Live status (running/healthy/error) is
  deliberately NOT in the model — it's a per-host observation, served by the
  host at `GET /api/fleet`.
- [x] Host integration (`registry = true` in apps.toml): the host runs the
  registry like any app, subscribes to its document, and converges on every
  change (action, MCP call, or sync from a replica) exactly like a file
  edit; registry entries win on name collision (except: the registry's own
  doc cannot redefine a registry app — file-controlled). Registry-installed
  apps persist across host restarts because they live in the replicated doc
  (proved by the `registry_lifecycle` integration test and live).
- [x] Bearer-token auth: `TANGRAM_AUTH_TOKEN` gates `POST /api/actions/*`
  and MCP `tools/call` of MUTATING tools on registry apps (→ 401 without
  `Authorization: Bearer`); reads stay open. Per-app `require_auth = true`
  extends the gate to any app. Without a token the host warns and refuses
  to run a registry app on a non-loopback bind. Default-deny egress was
  already enforced in Phase 2 (allow_hosts is the entire grant).
- [x] Tests in `cargo test`/CI: reconciler merge precedence + enabled=false,
  auth guards (401/200/missing-header, MCP mutating-only), spec validation,
  and the end-to-end lifecycle (install via authed POST → healthy →
  remove → routes gone → restart → app returns from the doc) in
  `crates/tangram-host/tests/registry_lifecycle.rs` (CI builds the wasm
  components first; the test self-skips without them).
- Exit met: registry action or MCP tool call → new component serving in
  ~0.5 s (debounce-free converge on doc change; instantiation dominates).

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
  restarts. Since 2026-06-11 that validation is a repeatable regression
  test, not a one-off: `scripts/e2e-cloudflare-sync.sh` (miniflare e2e —
  genesis convergence, bidirectional sync < 5 s, restart persistence with
  peers frozen) runs as the `e2e-cloudflare-sync` CI job and via
  `cargo test -p tangram-host -- --ignored e2e_cloudflare`.
- [x] MCP surface — delivered with Phase 7 (2026-06-11), but NOT via CF's
  Agents SDK: `/<app>/mcp` is served by tangram-core's sans-io MCP machine
  compiled to a component (`cloud/cloudflare/mcp-core`), keeping one Rust
  protocol implementation across every host (rationale in ADR-0002).
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
- **Duplicate transitive dep versions** (noted 2026-06-11, checked and not
  actionable): `cargo tree --duplicates` shows multiple versions of several
  crates (notably in the wasmtime ecosystem — e.g. `wit-parser`, `wasm-encoder`,
  `id-arena`). All duplicates are upstream version disagreements within the
  wasmtime crate graph; none are under our direct control. No action needed
  until an upstream wasmtime release resolves them.

## Beyond Phase 4 — product backlog (added 2026-06-11)

Target end-state, as decided by the owner: **(1)** WASM apps under one
orchestrator (single command, single port, file- AND registry-defined desired
state, all MCP behind one agentgateway instance) with app-state syncing across
remote host, local host, and Cloudflare (miniflare-tested, deployable);
**(2)** multi-tenancy with OAuth sign-in on Cloudflare — account creation,
hosted use of the remote, and OAuth-connected local instances.

- [x] **Phase 5 — multi-tenancy mode** — delivered 2026-06-11. One host
  process, one public port, N tenants; single-tenant mode stays the default
  and byte-identical (no `[tenants]` section → nothing changes; pinned by
  the unchanged registry/gateway/capabilities integration tests).
  - Config: `[tenants.<name>]` in apps.toml — `token = "${VAR}"` (REQUIRED,
    env-expanded; unresolved → tenant 401s and its apps don't run),
    `max_apps` (default 8), `allow_hosts_ceiling`, and an `apps` bootstrap
    template reusing the `[apps.*]` schema (empty → a registry instance
    cloned from the file's registry app). `[tenants] data_root` defaults to
    `$HOME/.tangram-tenants`.
  - Routing: `/t/<tenant>/<app>/{,api,sync,mcp}` through the live-table
    dispatcher keyed `(tenant, app)`; `/t/<tenant>/` index +
    `/t/<tenant>/api/fleet`; `t` (and `mcp`) reserved as app names.
  - Identity seam for Phase 6: every request under `/t/<tenant>/` (reads,
    SSE, sync, MCP included — tenant data is private, unlike the
    trusted-localhost top level) resolves to a `Principal` via one
    constant-time token lookup (`auth::resolve_principal`); wrong/other/no
    token and unknown tenants answer one uniform 401 (no existence oracle).
    OAuth later swaps the lookup, not the call sites.
  - Isolation, measured by `tests/tenant_lifecycle.rs` (host process spawned
    twice, scratch HOME): same app name in alice+bob → separate docs under
    `<data_root>/<tenant>/<app>/`; the cross-tenant 401 matrix; a tenant
    spec's `data_dir` must be relative (escapes → converge error in that
    tenant's fleet, no file written); registry-sourced tenant entries get NO
    `${VAR}` host-env expansion (the host env holds other tenants' tokens).
  - Per-tenant registry drives only its tenant's desired state; `max_apps`
    errors the newest excess install in the tenant fleet (never evicting the
    registry or an earlier install); effective `allow_hosts` = spec ∩
    ceiling, reported in the tenant fleet; installed apps + data persist
    across host restarts via the tenant's replicated registry doc.
  - Sync with auth: the SDK sync client sends `Authorization: Bearer` from
    `TANGRAM_REMOTE_TOKEN`/`TANGRAM_REMOTE_TOKEN_<NAME>` (host specs:
    `remote_token = "${VAR}"`; `replica.sh --remote-token`); a native
    replica converges a tenant app with the token and is 401-rejected
    without (same test).
  - Gateway: per-tenant aggregate `/t/<tenant>/mcp` lists only that tenant's
    tools; the global `/mcp` excludes tenant apps; the bearer is enforced at
    the host's INTERNAL endpoints, so talking to agentgateway's port
    directly still cannot reach a tenant app tokenless (pinned by
    `tenant_mcp_is_scoped_and_authed_through_the_gateway` in
    `tests/gateway_lifecycle.rs`).
- [x] **Phase 6 — identity (CF side)** — delivered 2026-06-11. OAuth
  accounts on the Cloudflare worker (account == tenant), PATs for local
  replicas' sync + MCP auth. Architecture in
  [ADR-0003](adr/0003-cloudflare-identity.md); the native host's Phase-5
  token table stays as-is (host↔CF credential unification is follow-up).
  - [x] OAuth sign-in (`/auth/login|callback|logout`): hand-rolled
    authorization-code flow with GitHub as the upstream IdP (the
    `workers-oauth-provider` library is an authorization *server* — wrong
    role; see the ADR). Upstream endpoints are env-overridable
    (`OAUTH_{AUTHORIZE,TOKEN,USER}_URL`), which is the stub-IdP seam the
    miniflare e2e uses. New account → tenant created with a collision-safe
    slug from the IdP login (`alice`, then `alice-2`); re-sign-in is
    idempotent.
  - [x] Accounts DO (`TangramAccounts`, one instance): accounts, 30-day
    browser sessions, and PATs — all tokens stored as SHA-256 hashes,
    plaintext shown exactly once. `/account` page (session-gated) mints,
    lists, and revokes PATs and links the tenant's apps.
  - [x] Tenant namespaces mirroring Phase 5: `/t/<tenant>/<app>/{,api,sync,
    mcp}`; DO ids from `t/<tenant>/<app>` (disjoint from the single-user
    surface's ids — full isolation, existing deployments keep their data);
    EVERY request under `/t/<tenant>/` (reads, SSE, sync, MCP, UI) resolves
    a principal — PAT bearer or session cookie — via one accounts-DO RPC, or
    answers ONE uniform 401 (no existence oracle, same property as the
    host). Per-tenant app set = the worker's bundled APPS (notes/nutrition);
    a per-tenant registry-on-CF is explicitly out of scope.
  - [x] Replicas use the UNCHANGED sync client: `TANGRAM_REMOTE_<APP>=
    https://<worker>/t/<tenant>/<app>/sync` + `TANGRAM_REMOTE_TOKEN=<PAT>`
    (verified in the e2e — no `crates/tangram` changes). Revocation is
    immediate (the PAT row deletion IS the revocation; no cache): the
    replica's next exchange 401s and it reconnect-loops without crashing.
  - [x] Miniflare e2e (`scripts/e2e-cloudflare-identity.sh`, CI job
    `e2e-cloudflare-identity`; runs its whole suite twice on fresh state):
    stub-IdP sign-in for alice+bob, slug collision (`alice-2`), the full
    401 matrix (state/actions POST/state SSE/sync POST/sync SSE/MCP/UI/
    index × {no token, garbage, bob's PAT, bob's cookie, unknown tenant} —
    bodies byte-identical), PAT + cookie access, MCP under the PAT,
    bidirectional replica↔tenant sync (8–76 ms), isolation (bob and the
    open single-user surface see none of alice's data), and revocation.
- **Phase 7 — CF app runtime**: upgrade Cloudflare from sync relay to full
  app host — spike `jco`-transpiled tangram:app components with a Worker-side
  host shim over DO storage (fallback: workers-rs + tangram-core); record the
  choice as ADR-0002. Serves UI/api/sync/mcp per (tenant, app); miniflare e2e
  extended to the full surface. Prereq: the tangram-core split.
  **Single-user surface delivered 2026-06-11** (per-tenant routing waits for
  Phase 5/6); evidence:
  - [x] Spike + [ADR-0002](adr/0002-cloudflare-app-runtime.md): jco-transpiled
    components chosen (Path A) — notes/nutrition run unmodified under workerd,
    incl. JSPI for the guest's synchronous `http-fetch` import awaiting the
    Worker's `fetch()`; workers-rs probe (Path B) blocked on `crates/tangram`
    surgery. Bundle ≈ 1.7 MiB gzipped (full evidence in the ADR).
  - [x] Each app's Durable Object serves the full surface (`cloud/cloudflare`):
    bundled UI, `/api/state|actions|events|capabilities` (state rendered by the
    component's `state-json`), `POST /api/actions/{name}` doc-in/doc-out
    against DO SQLite storage with the SDK's error envelope, `/healthz`,
    `/api/genesis`, and the unchanged Phase-4 `/sync(+events)`.
  - [x] `/mcp` through **tangram-core's sans-io MCP machine compiled to its own
    component** (`cloud/cloudflare/mcp-core`, world `tangram:mcp`) — the same
    Rust protocol layer as every host; tool calls dispatch through the same
    path as the actions API.
  - [x] Genesis from the component's deterministic `genesis()`, asserted
    byte-identical (sha256) to a fresh native instance's persisted genesis in
    the e2e.
  - [x] Capability parity with tangram-host: per-app `allow_hosts` enforced in
    the Worker's `http-fetch` (denial names the grant), env grants from Worker
    vars/secrets (nutrition's CalorieNinjas key; manual logging still works
    without it, description-based logging errors cleanly).
  - [x] Miniflare e2e (`scripts/e2e-cloudflare-apps.sh`, CI job
    `e2e-cloudflare-apps`; the Phase-4 sync e2e kept green): UI/healthz,
    dispatch write-through + error envelope, SSE state events, MCP
    initialize/tools-list/tools-call against `/notes/mcp`, allowlist denial,
    and the flagship — a native replica syncing bidirectionally with the
    miniflare-HOSTED app (9–86 ms propagation, incl. a DO-side action
    reaching the replica).
- [x] **Phase 8 — marketplace** — delivered 2026-06-11: a Tangram app
  cataloging installable apps with REQUIRED capability manifests displayed
  alongside the mechanical import audit ("what can this app actually do");
  install via registry with URL+sha256-verified artifacts; seeded with the
  first-party apps.
  - Host install-from-URL (the platform piece): `AppSpec` gains
    `component_url` + `component_sha256` as the alternative to the local
    `component` path (exactly one source required; sha-256 = 64 hex,
    validated at parse/install time). The host downloads the artifact,
    verifies the digest BEFORE instantiation, and caches it immutably,
    content-addressed by hash, under `$HOME/.tangram-host/components/` —
    re-converging on the same hash (including across host restarts) is a
    cache hit, never a refetch. Fetch failure / hash mismatch is a clear
    converge error in the fleet status (a running instance keeps serving,
    like a failed reload; nothing unverified ever reaches the cache), with
    a short retry backoff so the converge tick doesn't hammer the artifact
    server. The registry's `install_app` passes url+sha through (additive
    autosurgeon-`missing` model fields, older docs unaffected).
  - `apps/marketplace`: listings = name/description/version/url/sha256/
    ui/publisher + REQUIRED capability manifest {allow_hosts, env_keys,
    data_note} + `import_audit` (the `wasm-tools component wit` world
    block — the closed-world proof). `Default` seeds notes/nutrition/
    registry with REAL commit-time digests + audits, generated by
    `apps/marketplace/seed/refresh.sh` and refreshed per release. UI
    renders the manifest prominently next to Install (which posts the
    pinned url+sha+grants to the local registry's `install_app` with the
    user's bearer token); the import audit expands per card. Runs with
    `require_auth = true`: browsing open, curation token-gated.
  - Pinned by host unit tests (source validation, cache keying, mismatch
    rejection + no-refetch counters) and
    `crates/tangram-host/tests/marketplace_lifecycle.rs` (artifact server →
    marketplace listing → registry install-by-URL → healthy; wrong sha →
    fleet error, app not running; cache hit across converges AND a host
    restart, proven by the server's hit counter).
  - **TODO, explicitly not now** (also marked in `apps/marketplace/README.md`
    and the UI footer): third-party submissions — a pipeline gating listing
    approval on automated capability verification (manifest ⊆ audited
    imports), a sandboxed smoke-run, and an LLM behavioral sanity check.
    Until then the catalog is operator-curated via `add_listing`.
- [x] **Phase 9 — federated fleet state** — delivered 2026-06-11. Installing
  or removing an app on ANY tangram-host propagates to all of them
  (FULL-PROPAGATION, owner-approved). The insight: the registry app is already
  a replicated CRDT whose document IS the fleet's desired state, and the host
  already converges from it — so "install on one → runs on all" is just making
  the registry DOCUMENT sync between hosts and relying on the existing
  converge. Phase 8's `component_url` + `component_sha256` is what makes a
  synced entry portable (fetch-and-verify-anywhere, not a host-local path).
  - Registry document sync was already wired by construction (a registry app
    is an `AppSpec`, so it can carry `remote`; the host already starts the
    dial-out sync client for any spec with a `remote` AND already re-converges
    on a registry doc change — action, MCP call, or **sync from a peer**).
    Federation = setting `remote = "<peer>/registry/sync"` on the registry app.
    A federated registry additionally DERIVES each installed app's own sync
    remote (`<base>/<app>/sync`, carrying the registry's `remote_token`), so
    one `remote` setting replicates both the fleet membership and each app's
    data (`registry::Federation` / `sync_base`).
  - Portability enforcement: a federated registry's entries are seen by every
    peer, so a local `component` PATH is host-local. Entries are tagged
    `federated` through the merge (`registry::RegistryDesired`); a peer that
    lacks a path-only entry reports a clear PORTABILITY fleet error
    (`Host::ensure_app`), keeps converging everything else, and never thrashes
    the shared doc. Use `component_url` + `component_sha256` for portable
    installs (a parse-time warning nudges toward it).
  - Per-host secrets: the replicated document carries env KEYS and `${VAR}`
    references only; each host expands them from its own environment, so a peer
    missing a secret runs the app degraded (nutrition keeps manual logging but
    description-based logging errors) or errors cleanly — secret VALUES never
    sync.
  - Anti-flap / idempotence: runtime failures (fetch errors, missing
    artifacts, unresolved secrets) live ONLY in `GET /api/fleet`, never written
    back to the registry document. The document is desired state, so two hosts
    converging the same doc cannot oscillate (the converge is already
    idempotent for up-to-date apps).
  - `.agents/skills/local-replica` `--wasm` is now federated registry-bootstrap:
    it starts one registry app pointed at `<remote>/registry/sync` and lets
    convergence pull the rest of the fleet down (fetched+verified via the
    Phase-8 cache, data via the derived per-app remotes); the native path is
    unchanged.
  - Pinned by `crates/tangram-host/tests/federated_fleet.rs` (two hosts on
    19xxx; self-skips without the wasm components): install-by-url on A → runs
    on BOTH (~0.7–2.3 s), a note on A replicates to B via the derived remote,
    remove on B → gone on A, both restart → fleet restored from the persisted
    synced docs, a path-only entry → clear portability fleet error while the
    rest stays healthy, and a `${SECRET}` unset on B → degraded with no leak
    and no converge crash. Registry/tenant/gateway/marketplace tests stay green.
- [x] **Phase 10a — secret-resolver seam** — delivered 2026-06-11 (ADR-0004).
  A spec secret is now a `scheme://locator` *reference* resolved host-side at
  converge through a `SecretResolver` trait + scheme registry
  (`crates/tangram-host/src/secrets.rs`), into a `secrecy::SecretString`
  (redacted `Debug`, zeroize-on-drop, never logged). Phase 10a ships EXACTLY
  ONE resolver — `EnvResolver` for `env://NAME` (the host process env, today's
  source) — and rewrites the existing `${VAR}` expansion so `${VAR}` is sugar
  for `env://VAR`. Behavior is byte-identical: a missing var still expands to
  empty → app runs degraded (nutrition keeps manual logging, description-based
  logging errors); an unknown scheme is a clear error. Resolution still INJECTS
  the value into the component env in
  10a (ADR-0005 / Phase 10b is what later moves it to the egress boundary so
  the component never sees plaintext). The federated/tenant/nutrition flows are
  unaffected (regression tests stay green); new unit tests cover env:// resolve,
  ${VAR}→env:// equivalence, unknown-scheme error, missing-var degradation, and
  SecretString Debug redaction.
- [x] **Phase 10b — egress credential injection** — delivered 2026-06-11
  (ADR-0005). The plaintext credential no longer enters the component's
  address space: the host attaches it at the `http-fetch` egress boundary. A
  per-app spec declares injection rules keyed by outbound host —
  `[apps.<app>.inject]` in apps.toml, or the `inject` list on a registry
  `install_app` (replicated as the registry model's `Inject` rows) — each
  naming exactly one kind (`header` / `bearer` / `query`) and a `secret`
  `scheme://locator` reference resolved through the 10a `SecretRegistry`
  (`config::InjectRule` / `InjectKind`). In `HostState::http_fetch`
  (`crates/tangram-host/src/runtime.rs`) a request to an injection-matched
  host has its credential resolved host-side into a `SecretString` (lived only
  for the request, never logged) and attached just before the real outbound
  call; non-matching requests pass through unmodified. Injection COMPOSES with
  the allowlist — an injected host must also be in `allow_hosts` (validated at
  load and re-checked at egress), never a bypass. Nutrition migrated: the
  component issues a bare CalorieNinjas request and no longer reads
  `CALORIENINJAS_API_KEY` or sets `X-Api-Key` (the native binary, with no host
  broker, still self-authenticates from its own env). The capabilities probe's
  `description_input` is now derived host-side — ANDed with whether an
  injection secret resolves — so a missing/unresolvable key reports
  `description_input:false` and the strategy stays offline/degraded (no crash,
  no leak); apps with no inject rule keep native/host capabilities parity.
  apps.toml + the marketplace nutrition seed (a new `inject` manifest grant;
  the API key moves out of `env_keys`) carry the rule. Env injection (10a)
  remains for the rare secret a component must compute on internally — that
  case retains in-sandbox exposure (ADR-0005 scope note). New tests: config
  inject parse/validate/kind + env-isolation + configured-iff-resolves, a
  registry parse_state inject test, registry install/set_inject validation,
  the marketplace seed grant, and a host integration test
  (`tests/egress_injection.rs`, self-skips without components / live key)
  proving capabilities gating, env isolation, and that the host-injected
  request authenticates while a sibling app with no rule cannot.
- [x] **Phase 10c — fine-grained egress (call-level capabilities)** — merged
  (ADR-0008; design + build plan
  [docs/design/fine-grained-egress.md](design/fine-grained-egress.md)). The
  egress grant moves from `(host)` to the **declared call**
  `(method, host, path-pattern, request-shape)`, with the credential bound to
  the matched call — closing the same-host different-call exfil class ADR-0005
  left open. No WIT change; all enforcement host-side in
  `HostState::http_fetch`. A single canonicalization seam
  (`egress::CanonicalRequest`, the SOCKS5 parser-differential discipline)
  shared with the manifest verifier (manifest-verification-plan §2.6); the
  canonicalizer itself was extracted to the shared leaf crate
  `crates/tangram-egress` (consumed by the host enforcer, the verifier, AND the
  browser-automation gate — one canonicalizer fleet-wide). Small regex-free
  grammar (`egress::CallSpec`): method + path template/subtree + name-level
  query/header constraints + the constrained JSON-RPC-method body rung. Three
  modes (`observe`/`warn`/`enforce`), migration defaults (warn for legacy,
  enforce for apps declaring ≥1 call). `[[apps.<app>.calls]]` in apps.toml;
  `describe()`-carried declarations intersected with the operator spec (a
  request, never authority); observe-mode `[[calls]]` generator + `Call`
  authoring helper. Strictly additive — a host-keyed `allow_hosts`/`inject`
  desugars to the maximally-broad call, so the live fleet is byte-identical.
  Pinned by `tests/egress_enforcement.rs`. The opt-in bounded **policy engine**
  (§9.2, ADR-0009) also merged as `crates/tangram-host/src/policy.rs`
  (`[apps.<app>.policy]`): a latency-budgeted, fail-closed AST over the same
  egress seam that can only NARROW a grant — an escape hatch, never the default.
- [x] **Phase S1 — tangram shell (foundational slice)** — delivered 2026-06-11.
  A new first-party app `tangram` (crate `tangram-app-tangram`, on-host name
  `tangram`): the Obsidian-style shell. Design:
  [docs/design/tangram-shell-redesign.md](design/tangram-shell-redesign.md);
  build-pipeline carve-out: [ADR-0007](adr/0007-shell-build-pipeline-exception.md);
  iframe/composition model: [docs/design/app-composability-research.md](design/app-composability-research.md).
  - Backend: a normal wasm component (native + wasm32-wasip2) — a markdown
    **vault** (flat deterministic `Vec<MdFile>`, folders derived from
    `/`-separated paths, `.keep` sentinels for empty folders) in its
    replicated Automerge document; create/rename/delete files+folders and
    read/write bodies as registered actions. `Default` seeds one
    deterministic welcome note. (`apps/tangram/src/lib.rs`.)
  - Frontend: the ONE app with a build pipeline (ADR-0007) — Vite + TS
    bundling marked + DOMPurify and the shell chrome; committed `ui/dist/`
    served as the app's UI dir, all asset URLs relative. Persistent left
    sidebar (vault folder tree + the live `GET /api/fleet` apps list) and a
    tab strip whose tabs render a `.md` file or embed an app as
    `<iframe src="../<app>/">`. (`apps/tangram/ui/`, `apps/tangram/README.md`.)
  - Host wiring: `[apps.tangram]` in apps.toml (`ui = "apps/tangram/ui/dist"`).
    CI: a `shell-frontend` job (npm ci + typecheck + build + committed-dist
    freshness check); the wasm component added to the `check` job's component
    build.
  - **Since shipped on top of S1:** the CodeMirror 6 live-preview editor
    (`apps/tangram/ui/src/livePreview.ts`), the folder-aware vault polish (icon
    buttons, custom naming modal, tab-dedup, no self-nesting), and making
    `tangram` the host's default `/` route (307 redirect from `/`).
  - **Deferred follow-ups** (NOT yet built; see `apps/tangram/README.md`):
    folding registry/marketplace management into the sidebar; postMessage
    shell↔app coordination + app-in-note (composability); split/docking panes;
    multi-tenant/federated shell views. (The marketplace WASM-blob upload +
    content-addressed hosting behind the default-off `[artifacts]` gate shipped
    as Phase S2b — `POST /artifacts`, `crates/tangram-host/README.md`.)
- [x] **Manifest verification** — the `granted ⊆ declared ⊆ audited` chain
  (function-level import audit at the converge chokepoint, soft-flag/hard-fail
  dispositions, the `verified` fleet field): `crates/tangram-host/src/verify.rs`,
  pinned by `tests/verification.rs` (design: `docs/design/manifest-verification-plan.md`).
- [x] **Browser + credential automation substrate** (ADR-0010,
  `docs/design/task-automation-browser.md`) — `crates/tangram-automation`: a
  supervised browser runner, the browser egress gate on the shared
  `tangram-egress` canonicalizer, the `op://` 1Password credential broker, and
  a record→replay→validated-LLM-fallback engine. First consumer: the Amazon
  cart demo (cart-only, stops at CAPTCHA, never places an order; the live run is
  owner-gated). Native-only.
- [x] **New apps** — `apps/morning-brief` (AI-enabled component, offline core),
  `apps/guided-learning` (Make-It-Stick tutor, host-injected Anthropic egress),
  `apps/auto-todo` (per-item agent lifecycle, safe tier AC1–AC3 only). Specs in
  `docs/design/{morning-brief,guided-learning,auto-todo}.md`.

Sequencing: wave 1 (registry+auth, tangram-core, parity fixes) → wave 2
(agentgateway single-instance/single-port, miniflare e2e) → checkpoint-3 →
Phase 5 → {Phase 6, Phase 7 in parallel} → Phase 8 (CF surface after 7) →
Phase 9 (federated fleet over the registry doc).

## Decision: base-fleet federation is file-first (2026-06-11)

A host's BASE fleet (the apps in `apps.toml`, `source: file`) is **per-host
bootstrap config and does not federate**. Federation (Phase 9) syncs the
registry *document* — i.e. apps installed at runtime via `install_app`, which
carry `component_url` + `sha256` and are therefore portable. The base fleet
stays file-defined; a **dev replica** (which has the repo) mirrors it by
building the components locally (`replica.sh --wasm` reads the repo `apps.toml`
and builds each app — see that skill).

**Deferred (not built):** making a host's base fleet arrive on a **zero-build
client** (a phone, a browser, or a stranger's machine that cannot compile)
purely over sync. That requires the host to *register* its base apps in the
registry document with fetchable `component_url` + `sha256` (the "registry-
first" model) **plus an artifact-hosting pipeline** (components published to
GitHub releases / an `/artifacts` route / R2-CDN with verified hashes,
refreshed per release). This is a prerequisite *of* the Cloudflare multi-tenant
SaaS / browser-client milestone, not an independent feature, and should be
designed alongside that artifact pipeline (and the per-client "run here?"
opt-in question) rather than bolted on. For the canonical self-hosted
deployment (a server + the owner's own devices, all with the repo), file-first
+ local-build replicas is the correct fit and already works.
