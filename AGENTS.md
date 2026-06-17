# AGENTS.md

Tangram is a local-first Rust SDK: define a plain-Rust model + action methods
and get a CRDT-replicated store (Automerge) with derived MCP, web UI/JSON API,
and sync surfaces from one binary. Read [README.md](README.md) for usage and
[docs/SDK_DESIGN.md](docs/SDK_DESIGN.md) for architecture before making
non-trivial changes. This file is an index, not a manual.

`CLAUDE.md` is a symlink to this file; Claude Code's skills directory is a
symlink to `.agents/skills`.

## Where things are

- `crates/tangram` — the native host: tokio/axum transports for web/sync/mcp,
  App builder; also compiles to wasm32-wasip2 as the component guest adapter
  (`guest.rs`, `export_component!`, the `http`/`time` facades)
- `crates/tangram-core` — the portable core (no tokio/hyper/rmcp; must keep
  compiling for wasm32-wasip2, CI-checked): action registry, store +
  dispatch, sync sessions/framing, and the sans-io streamable-HTTP MCP
  server (rmcp parity fixtures in its `tests/fixtures/`)
- `crates/tangram-host` — embedded-Wasmtime host: runs apps as WASM
  components per `apps.toml` with capability grants (WIT world in its `wit/`;
  README "Run apps as WASM components"); optional MCP plane through a
  supervised agentgateway child with generated config (`src/gateway.rs`,
  `[gateway]` in apps.toml; README "MCP through agentgateway") — the same
  child also fronts an LLM egress proxy at `/llm/<name>` with host-injected
  provider keys (`[[gateway.llm]]`, ADR-0012) and emits OTLP gateway/LLM
  telemetry into a one-command Langfuse stack (`deploy/observability/`,
  `scripts/observability-{up,down}.sh`; `docs/design/gateway-observability-identity.md`);
  the agent scheduler drives the `tangram` app's `tick_agents` on a fixed
  cadence so scheduled agent invocations run with no browser open
  (`src/scheduler.rs`); opt-in
  multi-tenancy — isolated, bearer-gated app sets under `/t/<tenant>/`
  (`src/tenant.rs` + the `Principal` seam in `src/auth.rs`, `[tenants]` in
  apps.toml; README "Multi-tenancy", RUNTIME_PLAN Phase 5); call-level egress —
  the egress grant is the declared call `(method, host, path, shape)` not just
  the host, credential bound to the matched call (`src/egress.rs` is the grammar
  over the `tangram-egress` canonicalizer, `[[apps.<app>.calls]]` + `enforcement`
  in apps.toml; ADR-0008, README "Call-level egress", `docs/design/fine-grained-egress.md`).
  Opt-in escape hatch (NOT the default): a bounded, auditable egress POLICY that
  narrows the declarative grant (`src/policy.rs`, `[apps.<app>.policy]`; reuses
  the egress seam, latency-budgeted, fails closed; ADR-0009, fine-grained-egress §9.2).
  Manifest verification: the `granted ⊆ declared ⊆ audited` chain at converge
  (`src/verify.rs`, `tests/verification.rs`; `docs/design/manifest-verification-plan.md`)
- `crates/tangram-egress` — the single egress-canonicalization seam
  (`canonical_host` / `canonical_path`), a dependency-light leaf crate shared by
  the host enforcer + verifier (`tangram-host`) AND the browser gate
  (`tangram-automation`) — one canonicalizer fleet-wide (the SOCKS5
  parser-differential lesson; ADR-0008)
- `crates/tangram-automation` — host-side browser + credential automation
  substrate (native-only, NOT wasm-clean; ADR-0010,
  `docs/design/task-automation-browser.md`): supervised browser-driver runner
  (`runner.rs`, reuses the `gateway.rs` Backoff/shutdown pattern), the browser
  egress gate (`egress.rs`, which consumes the shared `tangram-egress`
  canonicalizer — one canonicalizer for the whole host), the `op://`
  credential broker (`broker.rs`; resolver in `tangram-host/src/secrets.rs`),
  the record→replay→validated-LLM-fallback engine (`script.rs`), and the
  request-not-grant `AutomationRequest` + operator-policy intersection
  (`request.rs`). Wired via `[automation]` in apps.toml
- `crates/tangram-macros` — `#[model]` / `#[actions]` proc macros
- `apps/notes` — minimal example app (`apps/notes/ui` for its frontend)
- `apps/nutrition` — fuller example; pluggable resolution in
  `apps/nutrition/src/strategy/` (see README "Nutrition strategies")
- `apps/morning-brief` — the **AI-enabled-component pattern** (fetch sources →
  build prompt → call LLM → write the brief to the component's own doc, not an
  egress): a configurable brief over calendar/email with an in-tangram
  feedback/"dreaming" loop. Offline core only — `run_brief` input_mode
  "fixture" is zero-network (CI's flagship); the source/LLM seam
  (`src/source.rs` + `src/llm.rs`) is where the live egress tier slots in.
  Spec + checkpoints: `docs/design/morning-brief.md`
- `apps/guided-learning` — a *Make It Stick*-driven tutor (an AI-enabled
  component): quizzes over pasted material, gates reveal on an attempt,
  calibrates confidence vs grade, schedules spaced reviews, and co-authors a
  collaboratively-editable `.md` study artifact in its Automerge doc; the only
  egress is the Anthropic Messages call (host-injected credential, ADR-0005;
  `[apps.guided-learning]` inject in `apps.toml`). `docs/design/guided-learning.md`;
  CI is fixture-LLM (no live key)
- `apps/registry` — the fleet's source of truth (RUNTIME_PLAN Phase 3): a
  Tangram app whose replicated spec list the host merges over `apps.toml`;
  bearer auth via `TANGRAM_AUTH_TOKEN` (README "The registry app"). Give it a
  `remote` and the fleet FEDERATES — install/remove on any host propagates to
  all (RUNTIME_PLAN Phase 9; `registry::Federation`, README "Federated fleet")
- `apps/marketplace` — operator-curated catalog (RUNTIME_PLAN Phase 8):
  listings with sha256-pinned artifact URLs + capability manifests + import
  audits (seeds regenerated by `apps/marketplace/seed/refresh.sh`); installs
  go through the registry. The mechanical manifest *verification* chain ships
  host-side (`tangram-host/src/verify.rs`); the third-party-submission *pipeline*
  that would use it (sandboxed smoke-run + behavioral check) is gated future work
- `apps/auto-todo` — TODO list where each item carries a gated per-item agent
  lifecycle (`docs/design/auto-todo.md`); SAFE TIER only (AC1-AC3):
  DRAFTED→DISCOVERED→CLASSIFIED→PLAN_PROPOSED→APPROVED state machine, read-only
  rule-based discovery/classification (optional offline-fixture LLM assist),
  and the plan-hash-bound approval + per-step `confirm()` UI. `execute()` is a
  no-op; `require_auth` gates the mutating actions. AC4-AC6 (credential/browser
  tier) intentionally NOT built — gated on the automation substrate + PR #1
- `apps/feedback` — file a GitHub issue (title + body + optional drag-drop
  screenshot) on this repo from a sandboxed app. NOT a `gh` shell-out: an
  async `create_issue` action does HTTP egress to the GitHub REST API (the
  guided-learning egress precedent) — a screenshot is uploaded via the
  Contents API (a data URI doesn't render in markdown) then embedded, then
  `POST /issues` files it; `GH_TOKEN` is host-injected at the egress boundary
  (`[apps.feedback]` inject in `apps.toml`, ADR-0005). `FEEDBACK_REPO` targets
  owner/repo. Static `ui/`; submission history in the doc
- `apps/shell` — multi-app host serving every app under one port, prefixed
- `apps/tangram` — the Obsidian-style shell app (sidebar vault + live apps,
  tabbed main window); a wasm component whose `ui/` is the one app with a
  Vite build pipeline (ADR-0007). `tangram-host` serves it at `/tangram/` and
  redirects `/` there (307). In-app **agents/skills**: `/agent` defines a
  saved agent (markdown + frontmatter), `/<name>` invokes it; a *scheduled*
  invocation is a dark-blue `agent://<id>` inline link backed by a replicated
  invocation index (the link is the handle, the index holds the trigger),
  driven host-side by `src/scheduler.rs` (`docs/design/agents.md`). The inline
  surface's staged redesign — the two-layer **Agent / Run / Execution** model
  (user-facing Trigger→Run), atomic CM6 chip + live status, R1–R4 roadmap —
  is `docs/design/embedded-runs.md`. A
  right-sidebar **app chat** wires DeepSeek to the active app's MCP tools via
  a browser MCP client + tool-calling loop (`ui/src/{chatPanel,mcpClient,llmChat}.ts`)
- Two execution paths to serve all apps on one port (README "Run them all in
  one server"): `cargo run -p tangram-shell` is the simple no-WASM Axum router
  with a card index — good for quick app dev; `tangram-host` driven by
  `apps.toml` is the real sandboxed WASM runtime that serves the `tangram`
  shell at `/` (the full shell experience — build the wasm32-wasip2 components
  + `apps/tangram/ui` dist first; README "Run apps as WASM components")
- `cloud/cloudflare` — Durable-Object app host (TypeScript Worker running
  the jco-transpiled WASM app components — ADR-0002): full per-app surface
  (UI/api/sync/mcp), the sync side interchangeable with any SDK remote;
  `mcp-core/` is tangram-core's MCP machine as a component
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
  - `frontend-design` — distinctive, intentional visual design for new/reshaped UI
  - `codebase-health` — periodic codebase-health / lint consolidation passes
  - `verify-design-in-ui` — review a design doc, isolate its observable
    behaviors, and walk the running UI to confirm they landed (or where they
    diverge); run after landing frontend-verifiable work
- `.env` (gitignored; template in `.env.example`) — secrets/API keys live
  here. Never commit secrets.
- README sections worth jumping to: "Getting started" (remote service +
  tunnel + replica), "Nutrition strategies", "Configuration (env / .env)"

## Commands

```sh
cargo build --workspace                                   # build gate
cargo clippy --workspace --all-targets -- -D warnings     # lint gate
cargo fmt --check                                         # format gate
cargo nextest run --workspace                             # test gate (CI uses nextest; .config/nextest.toml)
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
- Adding a field to an existing model: it must be `Option<T>` AND carry
  `#[autosurgeon(missing = "Option::default")]` — the derived `Hydrate`
  errors on the key missing from older documents without the attribute
  (discovered when `updated_at_ms` was added to notes; see `Note` in
  `apps/notes/src/lib.rs` for the pattern).
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
- After landing a frontend-verifiable unit of work, run the
  `verify-design-in-ui` skill against its design doc to confirm the intended
  behaviors actually shipped in the running UI (or surface where they diverge)
  before considering the unit done.
