# Sandboxed App Runtime — Plan (gVisor now, WASM-open)

**Status:** draft for discussion
**Goal:** run each Tangram app in its own gVisor sandbox; add/remove apps on
the fly with immediate access; route every web UI under one port at
`/<app-name>/...`; proxy MCP through agentgateway; keep the design open to a
future WASM runtime without buying abstraction we don't need yet.

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

### Phase 1 — tangram-host: proxy + reconciler, file-driven
- `tangram-host` crate: streaming-safe proxy + reconciler with the Docker
  API (`bollard`) + `runtime: runsc` backend.
- Desired state = `apps.toml` (name, image, env, resources, enabled);
  file watcher (`notify`) → converge on save; routes appear/disappear live.
- agentgateway deployed alongside; its config generated from `apps.toml`;
  `/<app>/mcp` routed through it.
- Retire the in-process shell for deployment (keep `tangram-shell` as the
  zero-dependency dev mode — it's good DX and exercises prefix mounting).
- Exit: edit `apps.toml`, save → app reachable at `/<app>/` in seconds;
  remove → routes gone, container stopped; MCP through agentgateway.

### Phase 2 — Registry app as source of truth (API-driven, live)
- Dogfood: `apps/registry` is itself a Tangram app — model = list of app
  specs + status; actions = `install_app`, `set_enabled`, `remove_app`,
  `set_image`. The reconciler subscribes to its state stream (SSE) instead
  of the file; `apps.toml` becomes import/export.
- This gives the "client-side API call changes the source of truth and a
  container spins up live" requirement for free — plus a UI for the fleet,
  MCP tools so an *agent* can install apps, and replication of the desired
  state across devices. One less database to run.
- Auth before anything else mutating is exposed beyond localhost: bearer
  token on the registry's mutating routes at minimum.
- Exit: `POST /registry/api/actions/install_app` (or the MCP tool) → new
  sandbox serving within seconds; registry UI shows fleet status.

### Phase 3 — k8s backend (when multi-node or org needs arrive)
- Second `Backend` impl with `kube-rs`: Deployment + Service per app,
  `RuntimeClass: gvisor` (minikube's gvisor addon for dev — this is where
  minikube enters, as the test bed for THIS phase, not as the starting
  point), tangram-host as the in-cluster ingress (or kgateway +
  agentgateway's native k8s mode with label-based federation).
- Same desired-state schema → manifests; registry remains the source of
  truth. GitOps variant (registry exports to a git repo, Flux/Argo applies)
  only if a multi-operator/audit workflow demands it — for "API call → live
  container", gitops adds latency and indirection, so it's an export format
  here, not the spine.
- Exit: same registry actions converge a minikube cluster; nothing about
  apps, images, or the gateway changed.

### Phase 4 — WASM runtime (exploratory)
- SDK workstream: `wasi:http` adapter (replace tokio listener + rmcp
  transport binding behind a feature flag); compile notes to
  `wasm32-wasip2`; run under `wasmtime serve` or a containerd WASM shim —
  which slots in as just another `Backend`/RuntimeClass.
- Nothing in phases 0–3 needs rework if the contract held.

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

## Risks

- **Streaming through the proxy**: SSE buffering or WS upgrade bugs would
  break the live-update core. Mitigate: hyper-level proxy (not a generic
  middleware), integration tests for SSE latency + WS sync through the full
  chain (client → host → runsc app).
- **runsc + io: gVisor's gofer adds file I/O overhead** — fine for automerge
  documents at Tangram scale; revisit if documents grow.
- **agentgateway config drift**: generate its config from the same desired
  state, never hand-edit.
- **Per-app egress** is the weakest isolation story on plain Docker
  networking (k8s NetworkPolicy is cleaner). Until Phase 3: default-deny
  egress per container with explicit allowlists where declared.
