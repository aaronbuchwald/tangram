# Tangram — a guided tour

*A from-scratch review guide for the project owner returning to the whole
system: what to open in the UI, every way to run it, the design decisions behind
each piece, the security model with its honest edges, and a "what's shipped vs
in-design vs gated" matrix so you know the state of each thing before you read
on.*

This is a curation of the existing docs and code, not new design. It links to
the canonical source for every claim. Read it top-to-bottom for the full
picture, or jump via the map. There is a [30-minute review
path](#suggested-30-minute-review-path) at the end.

Canonical references this draws on: [README.md](../README.md),
[AGENTS.md](../AGENTS.md), [docs/SDK_DESIGN.md](SDK_DESIGN.md),
[docs/SYNC_PROTOCOL.md](SYNC_PROTOCOL.md), [docs/RUNTIME_PLAN.md](RUNTIME_PLAN.md),
[docs/adr/](adr/) (0001–0010), the design docs in [docs/design/](design/), and
[docs/security/tenant-isolation-review.md](security/tenant-isolation-review.md).

---

## 0. State-of-the-system matrix

Read this first. Everything below is detailed later; this is the one-screen
status of each piece. **Shipped** = on `main`, CI-green. **In-design** = a
written design/plan, no shipping code. **Gated** = code may exist but is held
behind an explicit owner-approval / default-off gate.

| Piece | State | Where |
|---|---|---|
| SDK: one model → UI/MCP/sync surfaces | **Shipped** | `crates/tangram`, `crates/tangram-core`, `crates/tangram-macros` |
| HTTP(+SSE) sync protocol (native + relay parity) | **Shipped** | [SYNC_PROTOCOL.md](SYNC_PROTOCOL.md), `crates/tangram-core/src/sync.rs` |
| WASM-component host (`tangram-host`, capability grants) | **Shipped** | `crates/tangram-host`, ADR-0001 |
| Registry app (fleet desired-state) + federation | **Shipped** | `apps/registry`, RUNTIME_PLAN Phase 3/9 |
| Marketplace (operator-curated, sha-pinned, manifests) | **Shipped** | `apps/marketplace`, Phase 8 |
| Multi-tenancy (`/t/<tenant>/`, bearer-gated) | **Shipped** | `crates/tangram-host/src/tenant.rs`, Phase 5, ADR-0006 |
| Cloudflare Durable-Object app host (jco) | **Shipped** | `cloud/cloudflare`, ADR-0002/0003 |
| Egress credential injection (host attaches at boundary) | **Shipped** | ADR-0005, `runtime.rs` |
| Call-level egress (`[[apps.<app>.calls]]`, observe/warn/enforce) | **Shipped** | ADR-0008, `crates/tangram-host/src/egress.rs` |
| Single canonicalization seam (shared leaf crate) | **Shipped** | `crates/tangram-egress` |
| Egress policy engine (bounded, opt-in escape hatch) | **Shipped** | ADR-0009, `crates/tangram-host/src/policy.rs` |
| Manifest verification (`granted ⊆ declared ⊆ audited`) | **Shipped** | `crates/tangram-host/src/verify.rs`, `tests/verification.rs` |
| Browser + credential automation substrate | **Shipped** | ADR-0010, `crates/tangram-automation` |
| Amazon cart demo (cart-only, stops at CAPTCHA) | **Shipped** | substrate; the **live run (AC8) is Gated** |
| Obsidian-style shell (vault, tabs, live-preview editor) | **Shipped** | `apps/tangram`, ADR-0007 |
| Artifact upload store (`POST /artifacts`) | **Shipped, Gated** | `crates/tangram-host/src/routes.rs`, **default-OFF** (Phase S2b) |
| New apps: morning-brief / guided-learning / auto-todo | **Shipped** | `apps/{morning-brief,guided-learning,auto-todo}` |
| `apps/auto-todo` credential/browser tier (AC4–AC6) | **In-design / Gated** | safe tier only ships; `docs/design/auto-todo.md` |
| Unified auth (self-hosted loopback / multi-tenant OAuth) | **In-design** | `docs/design/auth.md`, issue #31 — **no code** |
| App composability (postMessage bus, app-in-note) | **In-design** | `docs/design/app-composability-research.md` |
| Wasmtime fuel/memory resource limits | **Not built** | ADR-0006 backlog (see §6) |
| Top-level `/sync` `/mcp` auth | **Not built** | loopback/trusted-net only (see §6) |
| gVisor images (Phase 0) / k8s backend | **Shipped / backlog** | `scripts/build-images.sh`; k8s on backlog |

---

## Top-level map

| # | Section | One line |
|---|---|---|
| 1 | [What Tangram is](#1-what-tangram-is) | One Rust model → three surfaces (UI / MCP / sync). |
| 2 | [The full UI](#2-the-full-ui) | The Obsidian shell + every app's UI: what to look at and why. |
| 3 | [Every way to run it](#3-every-way-to-run-it) | shell, `tangram-host`, gVisor, Cloudflare, replica, systemd. |
| 4 | [Sync across devices](#4-sync-across-devices) | local-first, HTTP+SSE, tunnel + replica, federated fleet. |
| 5 | [The design decisions](#5-the-design-decisions-adr-0001-0010--the-design-docs) | ADR-0001..0010 + the design docs, the "why" behind each. |
| 6 | [Security model & tradeoffs](#6-security-model--critical-tradeoffs) | capabilities, egress, the containment guarantee, tenancy, automation, MUST-FIXes. |
| 7 | [Honest status ledger](#7-honest-status-ledger) | shipped vs in-design vs gated vs MUST-FIX. |

---

## 1. What Tangram is

**The thesis (one paragraph).** Tangram is a local-first Rust SDK: you write a
plain-Rust data model plus action methods, and from that one definition you get
a CRDT-replicated store (Automerge) plus three derived surfaces served by one
binary — a web UI/JSON API for humans, an MCP server for AI agents, and a sync
endpoint for your other devices. State lives in an Automerge document on disk,
so the app is fully functional offline; when a peer is reachable, changes merge
both ways automatically and every connected UI re-renders live (SSE push) in
tens of milliseconds.

**The three surfaces, from one model** ([README](../README.md),
[SDK_DESIGN](SDK_DESIGN.md)):

| Surface | Endpoint | Consumed by |
|---|---|---|
| **MCP** (streamable HTTP) | `/mcp` | AI agents — Claude Code, Claude Desktop, any MCP client |
| **Web UI + JSON API** | `/`, `/api/*` | Humans (standalone or iframed) |
| **Sync** (Automerge over HTTP+SSE) | `/sync` | Other instances — your devices, a relay, a collaborator |

The chokepoint that makes this honest is the **action**: `&mut self` methods are
mutating actions (each becomes one attributed CRDT change), `&self` methods are
reads, and `async fn` taking `Ctx<Self>` is for network work (resolve outside
the lock, commit via `Ctx::mutate` — the store lock is never held across an
await). One method definition simultaneously becomes an MCP tool, an HTTP action
endpoint, and a sync-log entry; doc comments become tool descriptions and
parameters become a JSON schema.

> **Look at this.** The 40-line app in [README "What an app looks
> like"](../README.md) and the `#[model]`/`#[actions]` macros in
> `crates/tangram-macros/src/lib.rs`.

> **Why it's built this way.** *Separation of concerns*: the model is the app,
> and the surfaces are *derived* so they cannot drift from each other. Automerge
> was chosen (over Loro/Yrs) because the three things the SDK needs most — a
> typed Rust↔CRDT mapping (autosurgeon), wire-compatible browser peers, and a
> researched access-control trajectory (Keyhive) — only exist together there.
> The `Replica` seam is the hedge if Loro's typed-mapping and auth stories
> mature. *Note:* [SDK_DESIGN.md](SDK_DESIGN.md) is the original
> pre-implementation vision; where it and the code disagree, the code wins (it
> carries a banner pointing at the as-built reality).

> **The workspace.** `tangram` (native host + WASM guest adapter),
> `tangram-core` (portable no-tokio core), `tangram-host` (Wasmtime host),
> `tangram-egress` (the egress canonicalizer leaf crate), `tangram-automation`
> (browser/credential substrate), `tangram-macros` (the proc macros). Nine apps
> under `apps/`. (AGENTS.md is the authoritative index.)

---

## 2. The full UI

Open the Obsidian-style shell first — it is the front door — then visit each
app. Every app is the same shape: a `#[model]` + an `#[actions]` impl + a
self-contained `ui/` (relative fetch paths, SSE on `api/events`). The shell is
the one app with a build pipeline (ADR-0007); the rest are single `index.html`
files.

### The Obsidian-style shell — `apps/tangram` (the front door)
Under `tangram-host`, `/` 307-redirects to `/tangram/` and you land in the
shell ([apps/tangram/README.md](../apps/tangram/README.md)):
- **A persistent left sidebar** — a folder-aware markdown **vault** tree (folders
  derived from `/`-separated paths, empty folders kept alive by a `.keep`
  sentinel), plus the **live apps** on this host (from `GET /api/fleet`), with
  icon buttons and a custom naming modal. The vault is an ordinary Tangram app:
  a deterministic `Vec<MdFile>` in its own Automerge document.
- **A tabbed main window** — tabs render a `.md` file in a **CodeMirror 6
  live-preview editor** (`apps/tangram/ui/src/livePreview.ts`) or embed another
  app as `<iframe src="../<app>/">`. Tabs de-dup; the shell never nests itself.
- **What to look at and why:** the live-preview editor and the vault model
  (`apps/tangram/src/lib.rs` — `list_files`/`read_file`/`create_file`/… as
  registered actions) are the proof that even the host's chrome is "just another
  Tangram app" plus a permitted build step. The iframe-per-app boundary is the
  composability seam (and the single-origin caveat — see §6).

### notes — the minimal example
- **Model** (`apps/notes/src/lib.rs`): `Notes { notes: Vec<Note> }`;
  `Note { id, text, created_at_ms, updated_at_ms: Option<i64> }`.
- **Actions:** `add_note`, `create_note`, `update_note`, `delete_note`,
  `list_notes` (newest-edited first).
- **UI** (`apps/notes/ui/index.html`): a two-pane editor that renders the note's
  first line as a prominent **H1 title** (Apple-Notes / Obsidian style), with a
  Saved/Saving stamp.
- **Why look:** `updated_at_ms: Option<i64>` carries
  `#[autosurgeon(missing = "Option::default")]` — *the* schema-evolution pattern
  for replicated docs (a field added later must be `Option<T>` AND carry that
  attribute, or the derived `Hydrate` errors on older documents).

### nutrition — the fuller example (strategies + egress injection)
- **Model** (`apps/nutrition/src/lib.rs`): a tiered schema (`meals`,
  `ingredients`, `component_mappings`, `nutrients`, `ingredient_nutrients`) with
  a computed `NutritionRow` view.
- **Actions:** `log_meal` (async — the core operation), `delete_meal`,
  `list_meals`, `meal_nutrition`, `add_ingredient`, `add_component_nutrition`
  (idempotent cache write), and `get_capabilities` (the strategy probe).
- **UI** (`apps/nutrition/ui/index.html`): a calendar strip, a day band of
  macros, a meal list, and a Log dialog whose *description* box appears only when
  the active strategy can resolve free text.
- **Strategy seam** (`apps/nutrition/src/strategy/`): two strategies remain —
  `calorieninjas` (default, CalorieNinjas API) and `llm` (Anthropic
  `claude-opus-4-8`). **The offline strategy was removed.** An unset/unknown
  `NUTRITION_STRATEGY` defaults to `calorieninjas`; both need a credential, and a
  missing key is a clear error at lookup time, not a panic. Without a key, manual
  gram-quantified logging still works; description-based logging errors cleanly.
- **Why look — egress credential injection (ADR-0005).** Under `tangram-host`,
  the component issues a **bare** request and the host attaches the API key at
  the `http-fetch` boundary — the plaintext key never enters the component. The
  strategy probe is the `get_capabilities` **action** (served at
  `GET /api/capabilities` from the derived `describe()` manifest — no
  hand-written route), parity-pinned by `crates/tangram-host/tests/capabilities.rs`.

### registry — the fleet's source of truth
- **Model** (`apps/registry/src/lib.rs`): `Registry { apps: Vec<AppSpec> }`,
  mirroring the `apps.toml` schema (`component` or `component_url` +
  `component_sha256`, `ui`, `allow_hosts`, `inject`, `env`, federation fields).
- **UI** (`apps/registry/ui/index.html`, at `/registry/`): the fleet view —
  install/remove/enable, with live status from `GET /api/fleet`.
- **Why look:** the registry *dogfoods the SDK* — the control plane gets
  UI+API+MCP+sync for free. The document **is** desired state; the host
  subscribes and converges. Crucial split: **live status is a host observation
  (`/api/fleet`), never written into the replicated doc**, so two hosts
  converging the same doc cannot fight.

### marketplace — a catalog with capability manifests
- **Model** (`apps/marketplace/src/lib.rs`): `Marketplace { listings: Vec<Listing> }`.
  Each `Listing` pins `component_url` + `component_sha256` and carries a required
  `CapabilityManifest` plus an `import_audit` (the `wasm-tools component wit`
  world block — the mechanical proof of the component's closed world).
- **UI** (`apps/marketplace/ui/index.html`, at `/marketplace/`): cards showing
  the capability manifest *prominently next to Install*, the import audit
  expandable. Install posts the pinned url+sha+grants to the local registry's
  `install_app`.
- **Why look:** read a card's manifest and import audit, then Install. The host
  downloads the artifact, verifies the sha-256 **before instantiation**, and
  caches it content-addressed under `$HOME/.tangram-host/components/<sha>.wasm`
  (`crates/tangram-host/tests/marketplace_lifecycle.rs`). Curation is auth-gated
  (`require_auth = true`); browsing is open. **The third-party submission
  pipeline is NOT built** — see the MUST-FIX in §6.

### morning-brief — the AI-enabled-component pattern
- **What it is** (`apps/morning-brief`): a once-a-day AI digest assembled from
  pluggable read-only sources (calendar + Gmail) → prompt → LLM → written to the
  component's **own** document. Writing local state is *not* an egress, so the
  brief is **contained to the tangram by construction**.
- **State:** the **offline core** ships (MB1–MB5): `run_brief` with input_mode
  "fixture" is zero-network and is CI's flagship. The live egress tier (real
  Google/Anthropic) is the app's own later PR.
- **Why look** (`apps/morning-brief/ui/index.html`): the run/inspect/rate
  "dreaming" loop and the source/LLM strategy seam — the slot the live tier
  drops into.

### guided-learning — a Make-It-Stick tutor
- **What it is** (`apps/guided-learning`): a retrieval-practice tutor — quiz
  rather than re-present, gate reveal on an attempt, calibrate confidence vs
  grade, schedule spaced reviews, and co-author an editable `.md` study artifact
  in its Automerge doc.
- **Egress:** the only egress is the Anthropic Messages call, with a
  **host-injected** credential (ADR-0005, `[apps.guided-learning]` inject). CI is
  fixture-LLM (no live key). Pinned by `crates/tangram-host/tests/guided_learning_egress.rs`.
- **Why look** (`apps/guided-learning/ui/index.html`): the generation/calibration
  gates — a concrete AI-enabled component whose only network reach is one LLM call.

### auto-todo — per-item agent lifecycle (safe tier only)
- **What it is** (`apps/auto-todo`): a TODO list where each item carries a gated
  per-item agent lifecycle:
  DRAFTED→DISCOVERED→CLASSIFIED→PLAN_PROPOSED→APPROVED.
- **State:** **safe tier only (AC1–AC3).** Read-only rule-based
  discovery/classification (optional offline-fixture LLM assist), a
  plan-hash-bound approval + per-step `confirm()` UI. `execute()` is a **no-op**;
  `require_auth` gates the mutating actions. **AC4–AC6 (the credential/browser
  tier) are intentionally NOT built** — gated on the automation substrate + owner
  approval.
- **Why look** (`apps/auto-todo/ui/index.html`): the approval gate and the
  no-op `execute()` — it shows the *shape* of agent automation with every
  dangerous edge stubbed.

---

## 3. Every way to run it

All satisfy the same **app contract** (RUNTIME_PLAN "The app contract"): one
HTTP listener on `BIND_ADDR` serving `/ /api/* /sync /mcp /healthz`, configured
only via env, state confined to one data dir, outbound limited to declared
needs. The test runner of record is **nextest** (`.config/nextest.toml`, which
caps `tangram-host` integration tests at 2 concurrent — they spawn real host
subprocesses): `cargo nextest run --workspace`.

### 3.1 Single app — `cargo run -p tangram-<app>`
`http://127.0.0.1:8080`. The standalone binary; fully local with no remote.

```sh
cargo run -p tangram-notes        # then open http://127.0.0.1:8080/
```

### 3.2 In-process multi-app — `tangram-shell` (the simple card index)
`cargo run -p tangram-shell` serves every example app under one port, prefixed
(`/notes/`, `/nutrition/`, …) with a static **card index**. Zero-dependency dev
mode; apps share one process (so one compromised app could touch the others —
which is exactly what the WASM host undoes). **This is the simple no-WASM Axum
router — it is NOT the Obsidian shell.**

### 3.3 WASM components — `tangram-host apps.toml` (the spine; serves the shell)
The real sandboxed runtime (ADR-0001). Apps compile to `wasm32-wasip2`
components containing **only** app logic; one native host owns
HTTP/sync/MCP/persistence/UI. The component imports nothing but `http-fetch`
(allowlisted), `log`, `now-ms` — it cannot name a file, socket, or non-granted
host (`crates/tangram-host/wit/tangram.wit`). With the `tangram` app present, `/`
307-redirects to `/tangram/` (the full shell experience). `apps.toml` is watched
and converges live.

```sh
rustup target add wasm32-wasip2                                       # once
cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
  -p tangram-marketplace -p tangram-app-tangram -p tangram-morning-brief \
  -p tangram-guided-learning -p tangram-app-auto-todo --lib \
  --target wasm32-wasip2 --release
(cd apps/tangram/ui && npm ci && npm run build)        # the shell UI → apps/tangram/ui/dist
cargo run -p tangram-host --release -- apps.toml
# open http://127.0.0.1:8080/ — 307 → /tangram/ (the Obsidian-style shell)
```

The committed `apps/tangram/ui/dist` is CI-checked against a fresh build, so a
clean checkout already has it; rebuild only when you change the shell UI.

### 3.4 gVisor sandboxed images (Phase 0 delivered; k8s on backlog)
Each app also ships as a ~10–15 MB `FROM scratch` musl image meant for `runsc`.
Build with `scripts/build-images.sh`; cold start to `/healthz` is ~240 ms
(README "Run an app sandboxed"). Retained as the escape hatch if WASI
stabilization slips and as the runtime for *unported/untrusted native* apps
(e.g. the browser-automation process). The k8s backend (`kube-rs`,
`RuntimeClass: gvisor`) is designed but on backlog.

### 3.5 Cloudflare Durable Objects (delivered, ADR-0002/0003)
`cloud/cloudflare/` runs the **same WASM app components**, **jco-transpiled** to
JS+core-wasm, **one Durable Object per app document** (SQLite-backed), serving
the full surface — UI, `POST /api/actions/{name}`, live SSE, `/mcp`, and the
unchanged `/sync`. JSPI bridges the guest's synchronous `http-fetch` to the
Worker's async `fetch()`. Accounts (ADR-0003) are hand-rolled GitHub OAuth; each
account is a tenant at `/t/<tenant>/<app>/...`, gated by a browser session or a
hashed PAT (delete-the-row revocation).

```sh
cd cloud/cloudflare && npm ci && npm run build:components && npx wrangler dev
# or: npx wrangler deploy
```

Three CI-green miniflare e2e suites cover it: `scripts/e2e-cloudflare-sync.sh`
(relay/sync), `e2e-cloudflare-apps.sh` (full surface + a native replica syncing
with the hosted app), and `e2e-cloudflare-identity.sh` (OAuth, the 401 matrix,
PATs, revocation, isolation).

### 3.6 Replica / sync (the day-to-day) — the `local-replica` skill
A remote box runs the apps permanently; your laptop runs a replica that syncs
through an **SSH tunnel** (a `~/.ssh/config` `LocalForward 8080 127.0.0.1:8080`
turns every `ssh tangram` into the sync link). `.agents/skills/local-replica`:

```sh
bash .agents/skills/local-replica/replica.sh connect          # native replica on :8090
bash .agents/skills/local-replica/replica.sh connect --wasm   # federated WASM fleet bootstrap
bash .agents/skills/local-replica/replica.sh status           # per-app convergence
bash .agents/skills/local-replica/replica.sh stop
```

### 3.7 systemd service (the live remote) — the `systemd-service` skill
A persistent box runs the release shell as a systemd unit (working dir = repo so
`.env` loads), enabled at boot and health-checked:

```sh
bash .agents/skills/systemd-service/service.sh install   # build + unit + enable + health-check
bash .agents/skills/systemd-service/service.sh rebuild    # after pulling code
```

> **Why the WASM host is the spine.** The one architectural shift is
> "in-process host → host = reverse proxy + reconciler" (RUNTIME_PLAN). The
> refinement that matters: **logic-in-component, platform-in-host** — the sandbox
> boundary is the app *logic*, not an HTTP server, which dodges WASI churn
> (stable wasm32-wasip2 + pinned Wasmtime) and is a *stronger* grant model than
> preopens (the component has no filesystem capability at all). The tradeoff was
> a one-time SDK surgery (the `tangram-core` split + an HTTP sync transport + a
> hand-rolled MCP layer), accepted because it also unlocks Cloudflare and
> browser replicas.

---

## 4. Sync across devices

**Local-first model.** Every process is a peer; a "server" is just a reachable
peer. No remote configured → fully offline. The wire contract is
[SYNC_PROTOCOL.md](SYNC_PROTOCOL.md): unmodified Automerge sync messages over
HTTP — `POST <base>` for one exchange plus a `GET <base>/events` SSE *poke*
stream that tells the client to run its POST loop. Sessions are cheap ephemeral
cursors. **This contract binds every sync server** — the native SDK and the
Cloudflare relay must be indistinguishable to a client, and *genesis bytes are
identical by construction* (a deterministic genesis change: fixed actor, zero
timestamp), so an empty relay merges cleanly without inventing a rival root.

> **Look at this.** The client loop and "Genesis rule (relays)" in
> [SYNC_PROTOCOL.md](SYNC_PROTOCOL.md); the SDK server in
> `crates/tangram/src/sync.rs` + `web.rs`; the portable state machine in
> `crates/tangram-core/src/sync.rs`.

**Federated fleet — install on one, run on all.** Because a registry *is* a
replicated CRDT whose document is the fleet's desired state, federating is just
pointing one host's registry at another's (`remote = "<peer>/registry/sync"`).
Now `install_app`/`remove_app` on *any* host propagates to all; a federated
registry additionally derives each app's own sync remote, so one `remote`
replicates both fleet membership and each app's data. Two safety rules:
federated installs must be **portable** (`component_url` + `component_sha256`,
not a host-local path) and secrets are **per-host** (only env keys/refs sync,
never values). Pinned by `crates/tangram-host/tests/federated_fleet.rs`.

> **Why one transport.** One HTTP(+SSE) transport (no WebSockets) serves native,
> WASI, Cloudflare, and browsers from one implementation (`wasi:http` has no
> WebSockets). **The tradeoff accepted:** `/sync` and `/mcp` have **no auth** on
> the top-level surface today — bind to localhost or a trusted network. Tenant
> namespaces *are* authed (§6).

---

## 5. The design decisions (ADR-0001..0010 + the design docs)

Each in plain language — the decision, why, and the tradeoff. Full text in
[docs/adr/](adr/).

- **[ADR-0001](adr/0001-wasm-first-sandbox-runtime.md) — WASM-first runtime
  (gVisor retained).** Embed Wasmtime in `tangram-host`; apps are
  `wasm32-wasip2` components with capability grants; gVisor demoted to a
  fallback. *Why:* one static binary on every OS, a strictly stronger "app can't
  name what it wasn't given" model, and a core that also reaches Cloudflare and
  the browser. *Tradeoff:* a one-time SDK refactor and WASI 0.3 RC churn —
  bounded by pinning our Wasmtime and keeping gVisor as the tested escape hatch.

- **[ADR-0002](adr/0002-cloudflare-app-runtime.md) — Cloudflare app runtime:
  jco-transpiled components in the DO.** Transpile the existing components with
  jco and provide the host imports Worker-side; reuse the Rust MCP machine as a
  component. *Why:* the components ship *unmodified*; app logic, genesis bytes,
  and the error contract stay single-sourced in Rust. *Tradeoff:* JSPI
  dependency + a second toolchain.

- **[ADR-0003](adr/0003-cloudflare-identity.md) — CF identity: hand-rolled
  GitHub OAuth client + an accounts DO with hashed PATs.** The worker is an OAuth
  *client* of GitHub plus its own token store, not an authorization server.
  *Why:* the right role, a stub-IdP-testable flow, immediate revocation.
  *Tradeoff:* one DO round-trip per tenant request; a single global accounts-DO
  serialization point (shardable later).

- **[ADR-0004](adr/0004-secret-resolution-interface.md) — secret resolution: the
  *provenance* axis.** Secrets are `scheme://locator` references resolved
  host-side through a `SecretResolver` into a `secrecy::SecretString`; ship
  exactly one resolver, `env://` (with `${VAR}` sugar). *Why:* establish the seam
  now, add `op://`/`sops://`/`age://` later without touching app specs.
  *Tradeoff:* 10a still injects the value into the component — the *exposure*
  problem is ADR-0005. (The `op://` resolver has since landed for the automation
  substrate.)

- **[ADR-0005](adr/0005-egress-credential-injection.md) — egress credential
  injection: the *exposure* axis.** The host attaches the credential at the
  `http-fetch` egress boundary; the component issues a bare request and never
  receives the plaintext. *Why:* the secret in a component's linear memory is the
  weakest link and the prerequisite for any side-channel against it. *Tradeoff:*
  real app-code change (nutrition stopped reading the key) and it protects only
  HTTP-egress secrets.

- **[ADR-0006](adr/0006-tenant-isolation-posture.md) — tenant-isolation posture
  for co-resident WASM.** A tiered policy: first-party = in-process WASM
  sufficient; semi-trusted = in-process *iff* ADR-0005 holds + SMT off +
  Wasmtime resource limits; untrusted = process-per-tenant + SMT off + LLC
  partitioning. *Why:* WASM gives memory isolation, **not** microarchitectural
  isolation, and **gVisor does not fix that** (it is a syscall barrier, not a
  cache barrier); the only high-value secret here is a poor timing target *and*
  ADR-0005 removes it. *Tradeoff:* only physical separation fully closes the
  cache channel. Sourced analysis:
  [tenant-isolation-review.md](security/tenant-isolation-review.md).

- **[ADR-0007](adr/0007-shell-build-pipeline-exception.md) — build-pipeline
  exception for the `tangram` shell app.** The first-party shell may use npm + a
  bundler (CodeMirror, marked/DOMPurify); no other app gets this. *Why:* an
  Obsidian-grade editor wants bundler-dependent packages, and the **iframe
  boundary** means the shell's toolchain has no bearing on how components are
  built or sandboxed. *Tradeoff:* the repo's first build pipeline — contained to
  the shell's `ui/`.

- **[ADR-0008](adr/0008-egress-call-level-capabilities.md) — call-level egress.**
  The egress grant moves from `(host)` to the **declared call**
  `(method, host, path, shape)`, credential bound to the matched call; an
  undeclared call on an allowlisted host is denied. Three modes
  (`observe`/`warn`/`enforce`). *Why:* close the same-host different-call exfil
  class ADR-0005 left open. *Tradeoff:* a grammar to author — kept regex-free and
  strictly additive (a host-keyed grant desugars to the maximally-broad call).
  The canonicalizer is the shared **`crates/tangram-egress`** leaf crate
  (design: [fine-grained-egress.md](design/fine-grained-egress.md)).

- **[ADR-0009](adr/0009-egress-policy-engine.md) — egress policy engine
  (opt-in escape hatch).** A bounded, auditable rule/condition AST over the same
  canonical-request seam (no second parser, no regex), latency-budgeted,
  fails **closed**, and can only **narrow** a grant. `crates/tangram-host/src/policy.rs`,
  `[apps.<app>.policy]`. *Why:* some apps need imperative narrowing the
  declarative grammar can't express. *Tradeoff:* it is never the default;
  ADR-0008's declarative grammar is the first gate.

- **[ADR-0010](adr/0010-browser-automation-host-capability.md) — browser +
  credential automation as a host capability.** `crates/tangram-automation`: a
  supervised browser runner, a browser **egress gate on the shared
  `tangram-egress` canonicalizer**, the `op://` 1Password credential broker, and
  a record→replay→validated-LLM-fallback engine. Native-only, runs under tighter
  OS confinement (the gVisor tier). First consumer: the Amazon cart demo
  (cart-only, stops before checkout). *Why:* complete real-world tasks under LLM
  guidance without ever putting a credential in LLM context. *Tradeoff:* the
  largest, most dangerous surface in the repo — hence the gates in §6. Design:
  [task-automation-browser.md](design/task-automation-browser.md).

**The design docs (in [docs/design/](design/)):**
- [SDK_DESIGN.md](SDK_DESIGN.md) — the original vision/rationale (banner-flagged
  where it diverges from as-built).
- [RUNTIME_PLAN.md](RUNTIME_PLAN.md) — the runtime plan **and the binding app
  contract**.
- [SYNC_PROTOCOL.md](SYNC_PROTOCOL.md) — the sync wire contract.
- [fine-grained-egress.md](design/fine-grained-egress.md) — call-level egress +
  the policy-engine deferral (now both shipped).
- [manifest-verification-plan.md](design/manifest-verification-plan.md) — the
  `granted ⊆ declared ⊆ audited` chain (shipped as `verify.rs`).
- [task-automation-browser.md](design/task-automation-browser.md) — the browser
  substrate (shipped).
- [auth.md](design/auth.md) — the unified two-mode auth **design** (NOT built;
  issue #31).
- [app-composability-research.md](design/app-composability-research.md) — the
  single-origin trap + the four-layer composability model (research/pre-design).
- [tenant-isolation-review.md](security/tenant-isolation-review.md) — the
  microarchitectural-isolation due-diligence.
- [tangram-shell-redesign.md](design/tangram-shell-redesign.md),
  [morning-brief.md](design/morning-brief.md),
  [guided-learning.md](design/guided-learning.md),
  [auto-todo.md](design/auto-todo.md) — the per-feature design records (each
  status-banner-flagged for what shipped).

---

## 6. Security model & critical tradeoffs

What is solid today, and what is gated or designed.

- **Capability grants (allowlist) — solid.** A component's entire view of the
  outside world is three host functions (`http-fetch`, `log`, `now-ms`); the
  empty WASI context links no preopens, no sockets, no inbound HTTP
  (`crates/tangram-host/src/runtime.rs`, `wit/tangram.wit`). `allow_hosts` is the
  coarse outbound fence; **call-level egress (ADR-0008)** narrows it to declared
  calls, and the **policy engine (ADR-0009)** can narrow further. A denial names
  the host/call to grant.

- **The containment guarantee for AI components.** An AI-enabled component
  (morning-brief, guided-learning) can do exactly three things: **fetch its
  declared sources, call its declared LLM, and write its own state.** Writing
  local state is *not* an egress (there is no host to send to), so the model's
  output is contained to the tangram by construction. The only outbound reach is
  the allowlisted+call-level+credential-injected `http-fetch`.

- **The single canonicalization seam — the SOCKS5 lesson.** Every place that
  decides "does this request match a grant" — the host enforcer, the manifest
  verifier, and the browser-automation gate — uses **one** canonicalizer
  (`crates/tangram-egress`: `canonical_host` / `canonical_path`). Parser
  *differentials* (two components disagreeing on what a host/path means) are the
  classic egress-filter bypass (the SOCKS5 lesson); one seam makes that
  impossible by construction.

- **Egress credential injection — solid, load-bearing.** The key lives only for
  one outbound request and is attached at the boundary; injection **composes
  with** the allowlist (an injected host must also be allowlisted, re-checked at
  egress — never a bypass) (`tests/egress_injection.rs`).

- **Manifest verification — shipped (mechanical half).** The converge-time
  verifier stamps a `granted ⊆ declared ⊆ audited` verdict on each app
  (`verify.rs`, `tests/verification.rs`). Honest residual: the **third-party
  submission pipeline** that would *gate on* it — plus a sandboxed smoke-run and
  a behavioral check — is **not built**, and the *upload-time* import-audit
  reject is also not yet enforced (the converge verdict is informational/soft).

- **Tenant isolation — solid for tiers 1–2, gated for tier 3.** Multi-tenancy
  (`crates/tangram-host/src/tenant.rs`): `/t/<tenant>/` namespaces, data confined
  to `<data_root>/<tenant>/<app>/`, effective `allow_hosts` = spec ∩ ceiling,
  `max_apps` cap. **Microarchitectural caveat (ADR-0006):** WASM does not isolate
  cache/SMT side-channels and gVisor does not fix that; the untrusted tier needs
  process-per-tenant + SMT-off + LLC partitioning, designed-in, not retrofitted.

- **Browser-automation risks + gates (ADR-0010).** The substrate ships, but the
  dangerous edges are gated: the **credential never enters LLM context** (the
  `op://` broker resolves and the host `fill`s it; the LLM sees only the page,
  not the secret), the browser runs under the gVisor/native-confinement tier, the
  egress gate default-denies and path-denies the order-submit endpoint, and the
  Amazon demo is **cart-only — it builds the cart and stops at CAPTCHA, never
  places an order. The live run (AC8) is gated behind explicit owner approval.**

- **The marketplace open-upload — MUST-FIX before public exposure.** `POST
  /artifacts` (Phase S2b, `crates/tangram-host/src/routes.rs`) is a
  content-addressed upload store, **default-OFF** and, when on, loopback-only
  without a token. It is arbitrary-blob storage (OWASP Unrestricted File Upload).
  Before any non-loopback bind it needs the full checklist
  (`crates/tangram-host/README.md`): size cap + quota, rate limits, the
  closed-world import-audit reject at upload, content/abuse controls (hash
  blocklist, sandboxed smoke-run, behavioral check), and operator
  delete/GC/audit-log. Today it is a dev/demo affordance only.

- **The single-origin trap + the origin-per-app direction.** Every app is served
  from one origin, so the iframe `sandbox` attribute is *not* a real boundary on
  the shared origin (`allow-scripts` + `allow-same-origin` lets framed content
  remove its own sandbox). First-party apps are a *cooperative trust domain*;
  embedding *untrusted* apps safely requires a **distinct origin** (per-app
  subdomain or a `usercontent` host) — the VS Code webview model
  ([app-composability-research §1](design/app-composability-research.md)). The
  host now **does** emit `Content-Security-Policy: frame-ancestors` on its served
  app surfaces (`crates/tangram-host/src/routes.rs` `frame_ancestors_csp`, env
  `FRAME_ANCESTORS`, default `*`; pinned by `tests/frame_ancestors.rs`) — closing
  the gap where only the native SDK path set it.

- **Auth — bearer today, a two-mode design pending.** `TANGRAM_AUTH_TOKEN` gates
  `POST /api/actions/*` and *mutating* MCP `tools/call` on registry/marketplace
  apps (constant-time compare; the host refuses to run such an app
  unauthenticated off loopback). Under `/t/<tenant>/` **every** request requires
  the tenant bearer, with a uniform 401 (no existence oracle). On Cloudflare this
  is OAuth + hashed PATs (ADR-0003). **The unified design**
  ([auth.md](design/auth.md), issue #31) — two modes (self-hosted loopback-trust
  `LocalUser`; multi-tenant OAuth/OIDC `User`) behind a `Principal` seam — is
  **design-only, gated on owner approval, no code.**

> **The biggest gated baseline.** ADR-0006's **Wasmtime fuel/memory resource
> limits are NOT yet configured** — confirmed in `runtime.rs` (no `StoreLimits`,
> no epoch, no fuel; the engine only configures a compile cache). A runaway
> component is currently unbounded; this is a prerequisite for the semi-trusted
> tier and a tracked backlog item.

---

## 7. Honest status ledger

See the [matrix in §0](#0-state-of-the-system-matrix) for the per-piece state.
The compressed version:

**Shipped + CI-green (integration tests under `crates/tangram-host/tests/`):**
the SDK + three surfaces, the HTTP+SSE sync protocol, the WASM host, the
registry + federation, the marketplace (install-by-URL + manifests), multi-
tenancy, the Cloudflare DO host + OAuth identity, egress credential injection,
**call-level egress + the policy engine**, **manifest verification**, the
**browser-automation substrate**, the **Obsidian-style shell** (vault + tabs +
live-preview editor), the artifact upload store (default-off), and the three new
apps (morning-brief offline core, guided-learning, auto-todo safe tier). Pinned
by `registry_lifecycle`, `tenant_lifecycle`, `capabilities`, `gateway_lifecycle`,
`marketplace_lifecycle`, `federated_fleet`, `egress_injection`,
`egress_enforcement`, `egress_policy`, `verification`, `frame_ancestors`,
`artifact_upload`, `guided_learning_egress`, `default_view`, plus
`tangram-automation`'s `session_flow` and the three miniflare CF e2e jobs.

**In-design (no shipping code):**
- The **unified auth** two-mode design (auth.md, issue #31).
- **App composability** — postMessage UI bus + app-in-note + host-brokered
  capability bus (research done).
- `apps/auto-todo` **AC4–AC6** (credential/browser tier).
- morning-brief's **live egress tier** (real Google/Anthropic).

**Gated (code exists, held behind an explicit gate):**
- The marketplace **open upload** (`POST /artifacts`) — default-off, loopback-
  only without a token, full MUST-FIX checklist before public.
- The browser-automation **live Amazon run (AC8)** — owner-approval gated.

**Not built — MUST-FIX before any public / untrusted exposure:**
1. **No auth on top-level `/sync` and `/mcp`** — loopback/trusted networks only.
2. **Third-party marketplace submissions** — need the submission pipeline
   (gate-on-verification + sandboxed smoke-run + behavioral check) and the
   upload-time import-audit reject.
3. **Open blob upload** — the full default-off + size/rate/type/content/abuse +
   operator-controls checklist before any non-loopback bind.
4. **Untrusted-tenant isolation** — process-per-tenant + SMT-off + LLC
   partitioning + mandatory ADR-0005 (tier 3); **plus Wasmtime resource limits**
   as the baseline.
5. **Distinct origin for embedding untrusted apps** (the `frame-ancestors`
   header now ships; the distinct-origin direction does not yet).

---

## Suggested 30-minute review path

1. **(2 min) The matrix.** Read [§0](#0-state-of-the-system-matrix) so you know
   what's shipped vs designed vs gated.
2. **(7 min) The shell + an AI app.** Build the components + shell UI and run
   `tangram-host apps.toml` ([§3.3](#33-wasm-components--tangram-host-appstoml-the-spine-serves-the-shell));
   open `/` (the Obsidian shell), browse the vault, then open guided-learning or
   morning-brief in a tab. (§2.)
3. **(5 min) Egress + containment.** Read ADR-0005 and skim ADR-0008/0009 and
   `crates/tangram-egress/src/lib.rs` — the credential-injection +
   call-level + single-canonicalizer story (§6).
4. **(5 min) Browser automation.** Read ADR-0010 + the §6 automation gates —
   the credential-never-in-LLM and cart-only-stop-before-purchase guarantees.
5. **(4 min) Sync.** Skim [SYNC_PROTOCOL.md](SYNC_PROTOCOL.md) (the client loop +
   genesis rule), then `replica.sh connect` and add a note on each side. (§4.)
6. **(4 min) The honest edges.** §6's MUST-FIX list + the resource-limits and
   single-origin gaps, and §7's ledger — the things to fix before anything
   untrusted touches this.
7. **(3 min) Auth.** Read [auth.md](design/auth.md)'s two-mode model — it is the
   one large *design-only* piece awaiting your approval (issue #31).

*Last synthesized from the docs and code on `main`. Where this tour and the code
disagree, the code wins.*
