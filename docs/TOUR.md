# Tangram — a guided tour

*A walkthrough for the project owner returning to the whole system: what to
open, what to run, and why each piece is built the way it is.*

This is a curation of the existing docs and code, not new design. It links to
the canonical source for every claim and flags places where the docs and the
code currently disagree (see **Doc drift** call-outs). Read it top-to-bottom for
the full picture, or jump via the map. There is a [30-minute review
path](#suggested-30-minute-review-path) at the end.

---

## Top-level map

| # | Section | One line |
|---|---|---|
| 1 | [What Tangram is](#1-what-tangram-is) | One Rust model → three surfaces (UI / MCP / sync). |
| 2 | [The apps](#2-the-apps) | notes, nutrition, registry, marketplace, and the (planned) shell. |
| 3 | [Every way to run it](#3-every-way-to-run-it) | `cargo run`, shell, `tangram-host` (WASM), gVisor, systemd. |
| 4 | [Sync across devices](#4-sync-across-devices) | local-first, HTTP+SSE protocol, tunnel + replica, federated fleet. |
| 5 | [Deploy to infra](#5-deploy-to-infra-multiple-ways) | EC2+systemd (live), Cloudflare DO host, gVisor/k8s (planned). |
| 6 | [Major design decisions](#6-the-major-design-decisions-adr-0001-0007) | ADR-0001..0007 in plain language. |
| 7 | [Security model](#7-security-model) | capabilities, egress injection, the single-origin trap, tenancy tiers, auth. |
| 8 | [Honest status ledger](#8-honest-status-ledger) | delivered vs planned vs MUST-FIX. |

Canonical references this tour draws on: [README.md](../README.md),
[AGENTS.md](../AGENTS.md), [docs/SDK_DESIGN.md](SDK_DESIGN.md),
[docs/SYNC_PROTOCOL.md](SYNC_PROTOCOL.md), [docs/RUNTIME_PLAN.md](RUNTIME_PLAN.md),
[docs/adr/](adr/) (0001–0007),
[docs/security/tenant-isolation-review.md](security/tenant-isolation-review.md),
[docs/design/tangram-shell-redesign.md](design/tangram-shell-redesign.md),
[docs/design/app-composability-research.md](design/app-composability-research.md).

> **Doc drift — `codebase-health-report.md`.** This tour was asked to draw on a
> `codebase-health-report.md`; no such file exists in the repo. There **is** a
> `codebase-health` *skill* (`.agents/skills/codebase-health/`) that *produces*
> such a report on demand, but the artifact has not been committed. Treat
> §8's ledger as the live status source.

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

**The three surfaces, from one model** ([README surfaces
table](../README.md), [SDK_DESIGN](SDK_DESIGN.md)):

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

> **Why it's built this way.** *Separation of concerns* is constraint #4 in
> [SDK_DESIGN](SDK_DESIGN.md): the model is the app, and the surfaces are
> *derived* so they cannot drift from each other. Automerge was chosen
> (over Loro/Yrs) because the three things the SDK needs most — a typed Rust↔CRDT
> mapping (autosurgeon), wire-compatible browser peers, and a researched access-
> control trajectory (Keyhive) — only exist together there. **The tradeoff:**
> Automerge is not the raw-performance leader; the `Replica` seam is the hedge if
> Loro's typed-mapping and auth stories mature.

---

## 2. The apps

Five apps live under `apps/`. Each app is the same shape: a `#[model]` + an
`#[actions]` impl + a single self-contained `ui/index.html` (inline CSS/JS,
relative fetch paths, SSE on `api/events`). The first four are delivered and
CI-green; the **shell** as described in the redesign is *planned*.

### notes — the minimal example
- **Model** (`apps/notes/src/lib.rs`): `Notes { notes: Vec<Note> }`;
  `Note { id, text, created_at_ms, updated_at_ms: Option<i64> }`.
- **Actions:** `add_note`, `create_note`, `update_note`, `delete_note` (mutating),
  `list_notes` (read; newest-edited first).
- **UI** (`apps/notes/ui/index.html`): two-pane dark editor — a note list on the
  left, a borderless textarea with a Saved/Saving stamp on the right.

> **Try this.** `cargo run -p tangram-notes` → <http://127.0.0.1:8080/>. Add a
> note in the UI; then `claude mcp add --transport http notes
> http://127.0.0.1:8080/mcp` and ask the agent to add one — both land in the
> same document and both re-render live.

> **Why it's interesting.** `updated_at_ms: Option<i64>` carries
> `#[autosurgeon(missing = "Option::default")]` — this is *the* schema-evolution
> pattern for replicated docs: a field added to an existing model must be
> `Option<T>` **and** carry the `missing` attribute, or the derived `Hydrate`
> errors on documents written by older binaries (AGENTS.md "Conventions").

### nutrition — the fuller example (strategies + egress injection)
- **Model** (`apps/nutrition/src/lib.rs`): a tiered schema — `meals`,
  `ingredients`, `component_mappings`, `nutrients`, `ingredient_nutrients` — with
  a computed `NutritionRow` view.
- **Actions:** `log_meal` (async — the core operation), `delete_meal`,
  `list_meals`, `meal_nutrition` (computed), `add_ingredient`,
  `add_component_nutrition` (idempotent cache write).
- **UI** (`apps/nutrition/ui/index.html`): a calendar strip, a day band of
  macros, a meal list, and a Log dialog that offers a *description* box when the
  active strategy can resolve free text.

The **strategy seam** (`apps/nutrition/src/strategy.rs` + `strategy/`) decides
how a novel meal component gets its per-100g nutrients:
- `offline` — deterministic, keyless; unknown components contribute nothing until
  registered.
- `calorieninjas` — resolves free text via the CalorieNinjas API.
- `llm` — asks Anthropic's `claude-opus-4-8` for a nutrient panel.

Selection: explicit `NUTRITION_STRATEGY` wins; unset, the presence of
`CALORIENINJAS_API_KEY` auto-enables `calorieninjas`, else `offline`.

> **Try this.** `cargo run -p tangram-nutrition`, then log *"1 cup brown rice and
> 200g grilled chicken"* from the description box (needs a key, see below).
> `log_meal` resolves over the network **without holding the store lock**, then
> caches the result via an idempotent mutation — so a component resolved once
> replays on every synced device and *past meals using it resolve
> retroactively*.

> **Why it's interesting — egress credential injection (ADR-0005).** Run under
> `tangram-host`, the nutrition component issues a **bare** request and the host
> attaches the API key at the `http-fetch` boundary — the plaintext key never
> enters the component's address space. The component no longer reads
> `CALORIENINJAS_API_KEY` at all; "configured" is *derived host-side* from
> whether the injection secret resolves, surfaced via the shared
> `description_input` capability probe (`GET /api/capabilities`, parity-pinned by
> `crates/tangram-host/tests/capabilities.rs` and
> `tests/egress_injection.rs`). The *native* binary, with no host broker, still
> self-authenticates from its own env.

### registry — the fleet's source of truth (Phase 3)
- **Model** (`apps/registry/src/lib.rs`): `Registry { apps: Vec<AppSpec> }`. An
  `AppSpec` mirrors the `apps.toml` schema: `name`, `component` *or*
  (`component_url` + `component_sha256`), `ui`, `data_dir?`, `allow_hosts`,
  `env: Vec<EnvVar>`, `inject: Vec<Inject>`, `enabled`, and federation fields
  (`remote`, `remote_token`).
- **Actions:** `install_app`, `remove_app`, `set_enabled`, `set_component`,
  `set_allow_hosts`, `set_env`, `set_inject`, `list_apps` — every one a mutating
  action that is also an MCP tool and a fleet-UI button.
- **UI:** `apps/registry/ui/index.html` (the fleet view at `/registry/`).

> **Try this.** Under `tangram-host`, install an app live with the `curl`
> `install_app` call in [README "The registry app"](../README.md), or click
> Install in `/registry/`. The app is serving at `/<app>/` in well under a
> second; live status is `GET /api/fleet`.

> **Why it's interesting.** The registry *dogfoods the SDK*: "a database + a
> client API" is exactly what `#[model]` already generates, so the control plane
> gets UI+API+MCP+sync for free (RUNTIME_PLAN "Pushback" #2). The
> document **is** the desired state; the host subscribes and converges on every
> change (file edit, action, MCP call, or sync from a peer). Crucial split:
> **live status is a host observation (`/api/fleet`), never written into the
> replicated doc** — so two hosts converging the same doc cannot fight.

### marketplace — a catalog with capability manifests (Phase 8)
- **Model** (`apps/marketplace/src/lib.rs`): `Marketplace { listings: Vec<Listing> }`.
  A `Listing` pins `component_url` + `component_sha256` and carries a **required**
  `CapabilityManifest { allow_hosts, env_keys, inject, data_note }` plus an
  `import_audit` (the `wasm-tools component wit` world block — the mechanical
  proof of the component's closed world).
- **Actions:** `list_listings` (open), `add_listing` / `remove_listing`
  (auth-gated — the app runs with `require_auth = true`, so browsing is open but
  curation needs the bearer token).
- **UI** (`apps/marketplace/ui/index.html`): cards rendering the capability
  manifest *prominently next to Install*, with the import audit expandable.
  Install posts the pinned url+sha+grants to the local registry's `install_app`.
- **Seed:** `Default` seeds notes/nutrition/registry with real commit-time
  digests + audits (`apps/marketplace/seed/refresh.sh`, refreshed per release).

> **Try this.** Open `/marketplace/`, read a card's capability manifest and
> expand its import audit, then Install. The install reuses the Phase-8
> fetch-and-verify path: the host downloads the artifact, verifies the sha-256
> **before instantiation**, and caches it content-addressed under
> `$HOME/.tangram-host/components/<sha256>.wasm` (pinned by
> `crates/tangram-host/tests/marketplace_lifecycle.rs`).

> **MUST-FIX before public — third-party submissions & open upload.** The
> catalog is **operator-curated** today. Third-party submission is an explicitly
> recorded TODO (`apps/marketplace/README.md` + the UI footer + RUNTIME_PLAN
> Phase 8): it must first gate approval on automated *manifest ⊆ audited-imports*
> verification, a sandboxed smoke-run, and an LLM behavioral check. The
> *blob-upload* affordance (host-side content-addressed hosting via a planned
> `POST /artifacts`) is designed in
> [tangram-shell-redesign §5.3](design/tangram-shell-redesign.md) as
> **default-OFF and loudly-warned** — it is "arbitrary-blob-storage" (OWASP
> Unrestricted File Upload) and must not be enabled on a public bind until the
> AuthN/size/rate/type/content/abuse checklist exists.
>
> **Doc drift:** neither `POST /artifacts` nor `GET /artifacts/<sha>` exists in
> the code yet — it is design-only in the redesign plan. Today's only artifact
> path is install-by-URL (fetch+verify+cache), `crates/tangram-host/src/fetch.rs`.

### shell — the Obsidian-style default view (PLANNED; ADR-0007)
- **What the code is *today*** (`apps/shell/src/main.rs`): a thin ~140-line axum
  host that `nest_service`s notes + nutrition under `/notes/` and `/nutrition/`
  with their full surfaces, plus a static index of cards. This is the
  *in-process* multi-app dev mode — useful, but **not** the redesign.
- **What is planned:** an Obsidian-grade shell — persistent left sidebar
  (markdown file tree + live app list), a tab strip, markdown rendering, and apps
  embedded as `<iframe src="/<app>/">`. Status = **planning only, no code
  written** ([tangram-shell-redesign.md](design/tangram-shell-redesign.md), with
  Decisions A–G awaiting owner approval; ADR-0007 pre-approves the build
  exception).

> **Doc drift (expected, by design).** The redesign document describes a future;
> the running `tangram-shell` is the simple host. This is a roadmap, not a bug —
> but a reader who opens `apps/shell/` expecting the Obsidian UI will not find it.

---

## 3. Every way to run it

Five runtimes, ordered cheapest-to-richest. All satisfy the same **app
contract** (RUNTIME_PLAN "The app contract"): one HTTP listener on `BIND_ADDR`
serving `/ /api/* /sync /mcp /healthz`, configured only via env, state confined
to one data dir, outbound limited to declared needs.

1. **Single app — `cargo run -p tangram-<app>`** → `http://127.0.0.1:8080`.
   The standalone binary; fully local with no remote.
2. **In-process multi-app — `cargo run -p tangram-shell`** → every example app
   under one port, prefixed (`/notes/`, `/notes/mcp`, …). Zero-dependency dev
   mode; apps share one process (so one compromised app could touch the others —
   which is exactly what the WASM host undoes).
3. **WASM components — `tangram-host apps.toml`** (the spine, ADR-0001). Apps
   compile to `wasm32-wasip2` components containing **only** app logic; one native
   host owns HTTP/sync/MCP/persistence/UI. The component imports nothing but
   `http-fetch` (allowlisted), `log`, `now-ms` — it cannot name a file, socket,
   or non-granted host (`crates/tangram-host/wit/tangram.wit`, verified: no
   `wasi:sockets`/`wasi:http`). `apps.toml` is watched and converges live.
4. **gVisor sandboxed images** (Track G, Phase 0 *delivered*). Each app also
   ships as a ~10–15 MB `FROM scratch` musl image meant for `runsc`. Build with
   `scripts/build-images.sh`; cold start to `/healthz` is ~240 ms (README "Run an
   app sandboxed").
5. **systemd service** (the live remote pattern). `bash
   .agents/skills/systemd-service/service.sh install` builds the release shell,
   writes a unit (working dir = repo so `.env` loads), enables at boot, and
   health-checks; `service.sh rebuild` after pulling code.

> **Run this.** The WASM host, end to end:
> ```sh
> rustup target add wasm32-wasip2
> cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
>   -p tangram-marketplace --lib --target wasm32-wasip2 --release
> cargo run -p tangram-host --release -- apps.toml
> ```
> Then edit `apps.toml` and watch the host converge (`~0.4 s` to serving).

> **Why it's built this way.** "From in-process host → to host = reverse proxy +
> reconciler" is the one architectural shift (RUNTIME_PLAN). The refinement that
> matters: **logic-in-component, platform-in-host** — the sandbox boundary is the
> app *logic*, not an HTTP server, which dodges WASI 0.3 churn (stable wasm32-
> wasip2 + pinned Wasmtime) and is a *stronger* grant model than preopens (the
> component has no filesystem capability at all). **The tradeoff:** a one-time SDK
> surgery (the `tangram-core` split + an HTTP sync transport + a hand-rolled MCP
> layer), accepted because it also unlocks Cloudflare and browser replicas.

---

## 4. Sync across devices

**Local-first model.** Every process is a peer; a "server" is just a reachable
peer. No remote configured → fully offline. The wire contract is
[docs/SYNC_PROTOCOL.md](SYNC_PROTOCOL.md): unmodified Automerge sync messages
moved over HTTP — `POST <base>` for one exchange (one-or-zero message up, framed
messages down) plus a `GET <base>/events` SSE *poke* stream that tells the client
to run its POST loop. Sessions are cheap ephemeral cursors (a lost session just
re-converges). **This contract binds every sync server** — the native SDK and the
Cloudflare relay must be indistinguishable to a client, and *genesis bytes are
identical by construction* (a deterministic genesis change: fixed actor, zero
timestamp), so an empty relay merges cleanly without inventing a rival root.

> **Look at this.** The client loop and "Genesis rule (relays)" in
> [SYNC_PROTOCOL.md](SYNC_PROTOCOL.md); the SDK server in
> `crates/tangram/src/sync.rs` + `web.rs`; the portable state machine in
> `crates/tangram-core/src/sync.rs`.

**The day-to-day setup — tunnel + replica.** A remote box runs the apps
permanently; your laptop runs a replica that syncs through an **SSH tunnel**. One
`~/.ssh/config` entry with `LocalForward 8080 127.0.0.1:8080` makes every `ssh
tangram` session double as the sync link. Then:

> **Run this.** `bash .agents/skills/local-replica/replica.sh connect` (the
> `local-replica` skill) starts the shell on `:8090` with
> `TANGRAM_REMOTE_<APP>` pointed through the tunnel, waits for convergence, and
> prints URLs. Add a note on either side and watch it appear on the other in well
> under a second; close the tunnel, edit offline, reconnect, watch it reconverge
> (~2 s backoff). `replica.sh status` reports per-app convergence; `stop` stops it.

**Federated fleet — install on one, run on all (Phase 9).** Because a registry
*is* a replicated CRDT whose document is the fleet's desired state, federating is
just pointing one host's registry at another's (`remote =
"<peer>/registry/sync"`). Now `install_app`/`remove_app` on *any* host propagates
to all; a federated registry additionally derives each app's own sync remote
(`<base>/<app>/sync`), so one `remote` replicates both fleet membership and each
app's data. Two safety rules: federated installs must be **portable** (use
`component_url` + `component_sha256`, not a host-local path — a peer reports a
clear portability error otherwise) and secrets are **per-host** (only env *keys*
and refs sync, never values). Pinned by
`crates/tangram-host/tests/federated_fleet.rs`.

> **Run this.** `replica.sh connect --wasm` is the federated bootstrap: it starts
> one registry app pointed at `<remote>/registry/sync` and lets convergence pull
> the whole fleet down (fetched+verified via the Phase-8 cache, data via the
> derived per-app remotes). `--remote-token` / `TANGRAM_REMOTE_TOKEN` sends the
> bearer for a private (tenant) namespace.

> **Why it's built this way.** One HTTP(+SSE) transport (no WebSockets) serves
> native, WASI, Cloudflare, and browsers from one implementation (ADR-0001;
> `wasi:http` has no WebSockets). **The tradeoff accepted:** `/sync` and `/mcp`
> have **no auth** on the top-level surface today — bind to localhost or a trusted
> network (a tailnet qualifies; the public internet does not). Tenant namespaces
> *are* authed (§7).

---

## 5. Deploy to infra, multiple ways

Three tracks, honest about live-vs-planned:

**A. EC2 + systemd (the live remote).** A persistent box runs the release shell
as a systemd unit; your laptop syncs to it through the SSH tunnel (§4). This is
the canonical self-hosted deployment and **works today** (the `systemd-service`
skill). For continuous sync without a live SSH session, the README documents a
Tailscale tailnet alternative.

**B. Cloudflare Durable-Object app host (delivered through Phase 7, ADR-0002).**
`cloud/cloudflare/` is a serverless full host: **one Durable Object per app
document** (SQLite-backed) serving the *full* surface — UI, `POST
/api/actions/{name}`, live SSE, `/mcp`, and the unchanged `/sync`. It runs the
**same WASM app components** tangram-host runs, **jco-transpiled** to JS+core-wasm
(with JSPI for the guest's synchronous `http-fetch` awaiting the Worker's
`fetch()`); MCP is `tangram-core`'s sans-io machine compiled to its own component
(`cloud/cloudflare/mcp-core`). Replicas can't tell it from a native instance.
**Accounts** (Phase 6, ADR-0003): hand-rolled GitHub OAuth makes the worker grow
sign-in at `/auth/login`; every account is a tenant with a private namespace at
`/t/<tenant>/<app>/...`, gated by a browser session or a personal access token
minted on `/account`. PATs/sessions are stored only as SHA-256 hashes;
revocation is immediate (deleting the row *is* the revocation).

> **Run this.** `cd cloud/cloudflare && npm ci && npm run build:components &&
> npx wrangler dev` (local) or `npx wrangler deploy`. Three CI-green miniflare
> e2e suites cover it: `scripts/e2e-cloudflare-sync.sh` (relay/sync),
> `e2e-cloudflare-apps.sh` (full app surface + a native replica syncing with the
> hosted app), and `e2e-cloudflare-identity.sh` (OAuth, the 401 matrix, PATs,
> revocation, isolation).

**C. gVisor / k8s (Track G — retained foundation, k8s on backlog).** Phase 0
(images, CI build, runsc validation) is delivered and kept as the escape hatch if
WASI stabilization slips and as the runtime for *unported/untrusted native* apps.
The k8s backend (`kube-rs`, `RuntimeClass: gvisor`) is designed but on backlog —
the desired-state schema and the tiny `Backend` trait keep that swap honest
(RUNTIME_PLAN D1, Track G).

> **Why it's built this way.** The deciding operational fact (ADR-0001): across
> two-plus heterogeneous hosts, "one static `tangram-host` binary" beats
> "Docker+runsc everywhere," and the same WASM core reaches Cloudflare. **The
> tradeoff:** Cloudflare needed a *second* toolchain (cargo wasip2 + jco) and
> JSPI is load-bearing (a `wrangler deploy` smoke test is part of first
> production deploy; nutrition degrades cleanly to offline if JSPI is ever
> absent).

---

## 6. The major design decisions (ADR-0001..0007)

Each in plain language — the decision, why, and the tradeoff accepted. Full text
in [docs/adr/](adr/).

- **[ADR-0001](adr/0001-wasm-first-sandbox-runtime.md) — WASM-first runtime
  (gVisor retained).** *Decision:* embed Wasmtime in `tangram-host`; apps are
  `wasm32-wasip2` components with capability grants; gVisor demoted to a fallback.
  *Why:* one static binary on every OS, a strictly stronger "app can't name what
  it wasn't given" security model, and a core that also reaches Cloudflare and the
  browser. *Tradeoff:* a one-time SDK refactor and WASI 0.3 RC-grade churn until
  ~late 2026 — bounded by pinning our own Wasmtime and keeping gVisor as the
  tested escape hatch.

- **[ADR-0002](adr/0002-cloudflare-app-runtime.md) — Cloudflare app runtime:
  jco-transpiled components in the DO.** *Decision:* transpile the existing
  components with jco and provide the `tangram:app/host` imports Worker-side; reuse
  the Rust MCP machine as a component. *Why:* the components that ship to
  tangram-host run *unmodified*; app logic, genesis bytes, and the error contract
  stay single-sourced in Rust. *Tradeoff:* JSPI dependency + a second build
  toolchain in `cloud/cloudflare`; workers-rs (Path B) was blocked on SDK surgery
  this phase couldn't do.

- **[ADR-0003](adr/0003-cloudflare-identity.md) — CF identity: hand-rolled GitHub
  OAuth client + an accounts DO with hashed PATs.** *Decision:* the worker is an
  OAuth *client* of GitHub plus its own token store (`TangramAccounts` DO), not an
  OAuth authorization server. *Why:* `workers-oauth-provider` is the wrong role; a
  three-request flow with env-overridable URLs is exactly the stub-IdP seam the
  miniflare e2e needs, and gives immediate revocation. *Tradeoff:* one DO
  round-trip per tenant request and a single global accounts-DO serialization
  point (fine at this scale; shardable later).

- **[ADR-0004](adr/0004-secret-resolution-interface.md) — secret resolution: the
  *provenance* axis.** *Decision:* secrets are `scheme://locator` references
  resolved host-side through a `SecretResolver` trait into a `secrecy::SecretString`
  (redacted, zeroize-on-drop); ship exactly one resolver, `env://` (with `${VAR}`
  as sugar). *Why:* establish the seam now, iterate on provenance (`op://`,
  `sops://`, `age://` for E2EE-synced secrets) later without ever touching app
  specs or code. *Tradeoff:* 10a still injects the value into the component — the
  *exposure* problem is deferred to ADR-0005.

- **[ADR-0005](adr/0005-egress-credential-injection.md) — egress credential
  injection: the *exposure* axis.** *Decision:* the host attaches the credential at
  the `http-fetch` egress boundary; the component issues a bare request and never
  receives the plaintext. *Why:* the secret in a component's linear memory is the
  weakest link and the prerequisite for any side-channel against it — removing it
  beats hardening the channel. *Tradeoff:* real app-code change (nutrition stopped
  reading the key) and it protects only HTTP-egress secrets; a secret a component
  must *compute on* still falls back to env injection.

- **[ADR-0006](adr/0006-tenant-isolation-posture.md) — tenant-isolation posture
  for co-resident WASM.** *Decision:* a tiered policy — first-party = in-process
  WASM sufficient; semi-trusted = in-process *iff* ADR-0005 holds + SMT off +
  Wasmtime resource limits; untrusted third-party = process-per-tenant + SMT off +
  LLC partitioning, ADR-0005 mandatory. *Why:* WASM gives memory isolation, **not**
  microarchitectural isolation, and **gVisor does not fix that** (it is a syscall
  barrier, not a cache barrier); but the only high-value secret here (a stored API
  key) is a poor timing target *and* ADR-0005 removes it. *Tradeoff:* candor that
  only physical separation fully closes the cache channel — untrusted-tier controls
  must be designed in, not retrofitted. The full sourced analysis is in
  [docs/security/tenant-isolation-review.md](security/tenant-isolation-review.md).

- **[ADR-0007](adr/0007-shell-build-pipeline-exception.md) — build-pipeline
  exception for the `tangram` shell app.** *Decision:* the first-party shell may
  use npm + a bundler (CodeMirror, a docking layout); no other app gets this. *Why:*
  an Obsidian-grade editor wants bundler-dependent packages, and the **iframe
  boundary** means the shell's toolchain has no bearing on how components are built
  or sandboxed. *Tradeoff:* the repo's first build pipeline (lockfile, CI
  build/lint/typecheck) — contained to the shell's `ui/`; the runtime contract
  (relative paths, no host FS) is untouched.

---

## 7. Security model

What is solid today, and what is gated future work.

- **Capability grants (allowlist) — solid.** A component's entire view of the
  outside world is three host functions (`http-fetch`, `log`, `now-ms`); the
  empty WASI context links no preopens, no sockets, no inbound HTTP
  (`crates/tangram-host/src/runtime.rs`, `wit/tangram.wit`). `allow_hosts` is the
  *entire* outbound grant — an app with none simply cannot reach the network, and
  a denial names the host to grant.

- **Egress credential injection — solid, load-bearing.** The key is resolved
  host-side into a `SecretString` that lives only for one outbound request and is
  attached at the boundary; injection **composes with** the allowlist (an injected
  host must also be allowlisted, re-checked at egress — never a bypass)
  (ADR-0005, `runtime.rs`, `tests/egress_injection.rs`).

- **The single-origin trap — a known, documented limit for embedding untrusted
  apps.** Because every app is served from one origin, the iframe `sandbox`
  attribute is *not* a real boundary on the shared origin (`allow-scripts` +
  `allow-same-origin` lets framed content remove its own sandbox). First-party
  apps are therefore a *cooperative trust domain*; embedding *untrusted* apps
  safely requires a **distinct origin** (per-app subdomain or a `usercontent`
  host) — the VS Code webview model
  ([app-composability-research §1](design/app-composability-research.md)).
  > **Doc drift / gap:** host-run app surfaces do **not** currently emit
  > `frame-ancestors` (only the native SDK path in `crates/tangram/src/app.rs`
  > does). Closing that is "Layer 0" in the composability research.

- **Tenant isolation tiers — solid for tiers 1–2, gated for tier 3.** Multi-tenancy
  (Phase 5, `crates/tangram-host/src/tenant.rs`): `/t/<tenant>/` namespaces, data
  confined to `<data_root>/<tenant>/<app>/` (relative `data_dir` only), effective
  `allow_hosts` = spec ∩ ceiling, `max_apps` cap. See ADR-0006 for which tier
  needs what.

- **Auth — bearer today, OAuth on CF.** `TANGRAM_AUTH_TOKEN` gates `POST
  /api/actions/*` and *mutating* MCP `tools/call` on registry apps (and any app
  with `require_auth = true`); reads stay open; constant-time compare; the host
  refuses to run a registry app unauthenticated off loopback
  (`crates/tangram-host/src/auth.rs`). Under `/t/<tenant>/` **every** request
  (reads included) requires the tenant's bearer, and a missing/wrong/foreign
  token and an unknown tenant all return one uniform 401 (no existence oracle).
  On Cloudflare this is OAuth sessions + hashed PATs (ADR-0003).

> **The biggest gated item.** ADR-0006's **Wasmtime fuel/memory resource limits
> are NOT yet configured** — confirmed by reading `runtime.rs` (no `StoreLimits`,
> no epoch, no fuel). The ADR records this as a known, tracked backlog item and a
> prerequisite for the semi-trusted tier. A runaway component is currently
> unbounded.

---

## 8. Honest status ledger

**Delivered + CI-green** (checkpoints 1–9; `git tag -l 'checkpoint-*'`):
- checkpoint-1 shared sync (HTTP+SSE transport, `tangram-core` split)
- checkpoint-2 wasmtime host (the WASM spine)
- checkpoint-3 fleet and gateway (registry app + agentgateway MCP plane)
- checkpoint-4 multi-tenancy (Phase 5)
- checkpoint-5 CF app runtime (Phase 7)
- checkpoint-6 OAuth identity (Phase 6)
- checkpoint-7 marketplace (Phase 8, install-by-URL + manifests)
- checkpoint-8 federated fleet (Phase 9)
- checkpoint-9 egress injection (Phase 10a/10b)
- plus a cleanup wave (registry/state_json seam fix, miniflare CF-sync regression
  test) — the most recent merges.

Each is pinned by an integration test under `crates/tangram-host/tests/`:
`registry_lifecycle`, `tenant_lifecycle`, `capabilities`, `gateway_lifecycle`,
`marketplace_lifecycle`, `federated_fleet`, `egress_injection` — plus the three
miniflare e2e CI jobs for Cloudflare.

**Planned (designed, not built):**
- The **Obsidian-style shell redesign** — planning only (Decisions A–G await
  approval; ADR-0007 pre-approves the build exception).
- The marketplace **`POST /artifacts` blob-upload + content-addressed hosting**
  (default-off, the redesign §5.3).
- **Zero-build federation** of a host's *base* fleet to a phone/browser/stranger's
  machine — deferred; needs an artifact-hosting pipeline (RUNTIME_PLAN "base-fleet
  federation is file-first").
- **Wasmtime resource limits** (fuel/memory; ADR-0006 backlog).
- **App composability** — the `postMessage` UI bus + app-in-note + host-brokered
  capability bus (research done, design pending — task #26).

**Known MUST-FIX before any public / untrusted exposure:**
1. **No auth on top-level `/sync` and `/mcp`** — only safe on loopback/trusted
   networks today.
2. **Third-party marketplace submissions** — require manifest⊆imports
   verification, a sandboxed smoke-run, and an LLM behavioral check first.
3. **Open blob upload** — the full default-off + size/rate/type/content/abuse
   checklist (redesign §5.3) before any non-loopback bind.
4. **Untrusted-tenant isolation** — process-per-tenant + SMT-off + LLC
   partitioning + mandatory ADR-0005 (ADR-0006 tier 3); and the resource limits
   above as a baseline.
5. **Distinct origin for embedding untrusted apps** — plus closing the host-side
   `frame-ancestors` gap.

---

## Suggested 30-minute review path

1. **(3 min) The thesis.** [README "What an app looks like"](../README.md) and
   the surfaces table — confirm the one-model→three-surfaces claim still holds.
2. **(7 min) See it live.** `cargo run -p tangram-nutrition`; log a meal in the UI;
   `claude mcp add --transport http nutrition http://127.0.0.1:8080/mcp` and have
   the agent log one too. Watch both land. (§2 nutrition.)
3. **(7 min) The WASM host.** Build the components and run `tangram-host apps.toml`
   (§3 "Run this"); open `/registry/` and `/marketplace/`; install an app and watch
   `/api/fleet`. This is the platform spine.
4. **(5 min) Sync.** Skim [SYNC_PROTOCOL.md](SYNC_PROTOCOL.md) (the client loop +
   genesis rule), then `replica.sh connect` and add a note on each side. (§4.)
5. **(5 min) The decisions.** Read ADR-0001 (why WASM), ADR-0005 (egress
   injection), and the ADR-0006 *Decision* table (tenancy tiers). (§6.)
6. **(3 min) The honest edges.** §8's MUST-FIX list and the §7 resource-limits /
   single-origin gaps — the things to fix before anything untrusted touches this.

*Last synthesized 2026-06-11 from the docs and code at HEAD. Where this tour and
the code disagree, the code wins — the Doc-drift call-outs flag the known gaps.*
