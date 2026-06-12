# Design: IAM / auth for the native host

**Status:** proposed (design only — held for owner approval before any code)
**Issue:** [#20](https://github.com/aaronbuchwald/tangram/issues/20) — replace
the single shared plaintext bearer token with a proper IAM/auth system
**Related:** ADR-0003 (Cloudflare identity — accounts==tenants, hashed PATs,
sessions), ADR-0006 (tenant isolation posture), RUNTIME_PLAN Phases 5
(host multi-tenancy, the `Principal` seam) and 6 (CF identity)

This is a research + design deliverable. It cites the real code paths, picks
an approach with a recommendation, and lays out phased, testable checkpoints.
No code changes accompany it.

---

## 1. Problem & current-state audit

Today the native host has exactly one mutating-surface credential: a single
shared, plaintext bearer token. Tracing it end to end:

- **The token itself** — `TANGRAM_AUTH_TOKEN`, resolved at host boot and
  stored as a plain `String`. `crates/tangram-host/src/auth.rs` builds an
  `AuthGate { token, mutating_tools }` per registry/`require_auth` app and
  compares the presented bearer with `ct_eq` (constant-time). One token,
  one host, no identity attached.
- **What it gates** — `bearer_guard` on `POST /api/actions/{name}` (every
  action POST) and `mcp_guard` on `/mcp` (only `tools/call` of a *mutating*
  tool; reads stay open). Everything else (UI, state, events, sync) is open
  read/CRDT surface. With no token configured, nothing is gated and the host
  refuses to run a registry app on a non-loopback bind
  (`auth.rs` module docs + `main.rs`).
- **Who presents it** — the shell UI. `apps/tangram/ui/src/manage.ts` reads
  the token from `localStorage["tangram_auth_token"]` and sends it as
  `Authorization: Bearer <token>` to `../registry/api/actions/*`.
  `apps/tangram/ui/src/main.ts` (lines ~70–107) renders a single
  `<input type=password placeholder="TANGRAM_AUTH_TOKEN …">` whose value is
  persisted verbatim to that localStorage slot. The standalone registry and
  marketplace UIs share the same slot.
- **The registry contract** — `apps/registry/src/lib.rs` `INSTRUCTIONS`
  literally tells agents: "Mutating tools require Authorization: Bearer
  `<TANGRAM_AUTH_TOKEN>`". The shared token *is* the registry's access model.

The problems the issue names, mapped to the above:

| Problem | Where it bites today |
|---|---|
| No multi-user support | One `AuthGate.token`; the credential carries no principal. Mutations cannot be attributed. |
| Plaintext credential hygiene | Stored plaintext in host env *and* in browser `localStorage` (`manage.ts:13`), copy-pasted into a visible field, no hashing at rest. |
| No audit trail | Action dispatch records the document `ActorId` (`doc.rs:36`, random per process), never a human/credential principal. Nothing answers "who installed this app, when". |
| All-or-nothing access | The token is uniform: holding it authorizes every mutating action on every gated app. No scopes, no read-vs-admin split. |
| No session lifecycle | A static env string: no expiry, no revocation short of rotating the env and restarting, no rate-limit. |

**What already exists and is reusable.** Two pieces of the eventual design
are already built and proven:

1. **The `Principal` seam** (`auth.rs:83–129`). `resolve_principal(headers,
   tenant, expected_token)` already returns a typed `Principal` (today
   `Local` | `Tenant(String)`) and is wired into the request path
   (`routes.rs:447`). The doc comment is explicit: *"Phase 6 swaps the token
   lookup in `resolve_principal` for OAuth claims without touching call
   sites — everything downstream consumes a `Principal`, never a raw
   header."* This is the designed extension point; the IAM work *is* filling
   it in.
2. **The Cloudflare identity model** (ADR-0003, `cloud/cloudflare/src/
   auth.ts` + `account.ts`). A full, tested implementation of exactly the
   shape we want: an accounts store keyed by `sha256(token)`, sessions
   (`tgs_…`, 30-day TTL, HttpOnly SameSite=Lax cookie) and PATs (`tgp_…`,
   160-bit CSPRNG, live until revoked), an `authorize(tenant, bearer?,
   session?)` per-request check where **deleting the hash row IS the
   revocation** (no cache), a uniform-401 no-existence-oracle property, and a
   miniflare e2e (`scripts/e2e-cloudflare-identity.sh`) that pins the whole
   contract with a stub IdP. RUNTIME_PLAN Phase 6 explicitly flags
   "host↔CF credential unification is follow-up" — that follow-up is this
   issue.

The native host's tenant model (`tenant.rs` + `config.rs` `TenantSpec.token`)
is a *static per-tenant token table* loaded from `[tenants]` in `apps.toml`.
It is the same shared-plaintext-token weakness as the top-level surface, just
scoped per tenant: still plaintext, still no per-user identity, no lifecycle.

---

## 2. Approach decision

### Recommendation

**Build a native account/credential store that is the Rust port of the
Cloudflare `TangramAccounts` model (ADR-0003): hashed PATs + browser sessions
behind the existing `Principal` seam, with an audit log of mutations →
principal.** Identity (authn) is pluggable: a local-admin password/PAT
bootstrap for self-hosted fleets, and optional OAuth/OIDC sign-in that
**converges on the same store and the same PAT/session primitives the CF
worker already uses**.

One-line rationale: the seam, the data model, the revocation semantics, and
the e2e harness already exist on the Cloudflare side — porting that proven
model to the host unifies the two identity planes instead of inventing a
third, and it drops cleanly into `resolve_principal` without touching call
sites.

### Why this fits Tangram's deployment shapes

Tangram runs in four shapes; the design must serve all four without forcing a
heavyweight IdP on the single-user case:

- **Single-user local device** (`cargo run -p tangram-shell`, loopback bind).
  Today: no token needed on loopback. *Keep that.* The new store ships with a
  "no accounts configured → loopback-only, open" default, identical to
  today's behavior. A local user who wants to bind non-loopback gets a
  one-command `tangram auth init` that mints the first admin PAT. No OAuth,
  no server.
- **Multi-user fleet** (a shared host, several operators managing the
  registry). This is the case the issue is really about: each operator gets
  their own PAT/session, mutations are attributed, individual credentials
  revoke independently. The local account store with optional OIDC sign-in
  serves this.
- **Federated registry** (`registry::Federation`, RUNTIME_PLAN Phase 9). The
  *registry document* federates; **credentials must not**. The account store
  is host-local and per-host (never replicated into the Automerge doc — a
  replicated credential is a leaked credential, exactly the `${VAR}`-blanking
  rule in `tenant.rs:124`). Federation propagates *desired app state*; each
  host authenticates its own operators.
- **Cloudflare multi-tenant** (ADR-0003, already shipped). Unchanged. The
  host design deliberately mirrors its model so that the long-term
  convergence (host validating CF-minted PATs / a shared device flow) is a
  small step, not a rewrite. The PAT format (`tgp_…`), the hash-at-rest rule,
  and the uniform-401 property are kept byte-compatible on purpose.

### Alternatives considered

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| **A. Native hashed-PAT/session store mirroring ADR-0003** (recommended) | Reuses a proven, tested model; fills the existing `Principal` seam; per-user identity + revocation + audit; converges host & CF | New persistence + UI work; we own the account store | **Chosen** |
| B. Full OAuth/OIDC authorization-server (e.g. adopt `workers-oauth-provider`-style AS) | Standards-compliant, delegates identity | ADR-0003 already rejected making *our side* an AS — no consumer needs spec-compliant OAuth yet; heavy for single-user/loopback; still needs a local token store for replicas | Rejected for now; revisit if MCP clients demand spec OAuth |
| C. Reverse-proxy / external IdP in front (Cloudflare Access, oauth2-proxy) | Zero auth code in-repo | Identity leaves the repo (untestable in CI, ADR-0003's exact objection); no per-PAT mint/revoke for headless replicas; no in-app audit; doesn't help loopback dev | Rejected as the *primary* mechanism; fine as an *optional* front for an org that already runs one |
| D. Rotate-only API keys (keep shared token, add rotation) | Smallest change | Doesn't solve multi-user, audit, or scopes — only lifecycle, partially | Rejected — doesn't meet the issue |
| E. mTLS client certs | Strong, no bearer in localStorage | Painful UX for browser + laptop replicas; no human-readable identity; no easy revocation list at this scale | Rejected |

Identity (authn) is intentionally **pluggable on top of A**: OIDC is the
recommended sign-in for multi-user fleets, but the store and the
PAT/session primitives are the stable core, so a fleet with no IdP still gets
full IAM via the local-admin bootstrap.

---

## 3. Designed flow

### 3.1 Identity / authn

Two ways to become a principal, both landing in the same store:

1. **Local admin bootstrap** (no IdP). `tangram auth init` mints the first
   admin account + a PAT (shown once). Subsequent accounts are created by an
   admin via an action (`create_principal`) or a sign-up gated by an
   invite/registration policy. This is the single-user and air-gapped-fleet
   path.
2. **OAuth/OIDC sign-in** (optional, multi-user). The host runs the same
   hand-rolled authorization-code client ADR-0003 uses (env-overridable
   `OAUTH_{AUTHORIZE,TOKEN,USER}_URL` so the e2e swaps a stub IdP — reuse the
   exact seam). First sign-in creates an account; the IdP identity
   (`ident:<provider>:<id>`) maps to a stable local principal id.

Both mint a **browser session** (cookie) for UI use and can mint **PATs**
(bearer) for replicas / MCP / CLI.

### 3.2 The account store (Rust port of `TangramAccounts`)

A host-local store (sqlite or an Automerge-free embedded KV; **not** the
replicated app document). Records mirror ADR-0003's table:

| key | value | purpose |
|---|---|---|
| `principal:<id>` | { id, display, provider?, created, role/scopes } | the account |
| `ident:<provider>:<extid>` | principal id | IdP identity → account (sign-in) |
| `session:<sha256(token)>` | { principal, expiresAtMs } | browser sessions (TTL) |
| `pat:<sha256(token)>` | { principal, id, label, scopes } | O(1) bearer auth by hash |
| `patindex:<principal>:<id>` | { hash, label, scopes, created } | list/revoke without the token |
| `audit:<ts>:<seq>` | { principal, action, app, args-digest, outcome } | who-changed-what-when |

**Tokens are stored only as SHA-256 hashes** (the CF rule). Plaintext is
shown exactly once. PAT format `tgp_<40 hex>` (160-bit CSPRNG); session
`tgs_…` HttpOnly SameSite=Lax cookie, 30-day TTL. These match CF byte-for-byte.

### 3.3 Credential lifecycle

- **Creation** — sessions on sign-in; PATs via an authenticated mint action
  (label required, optional scope set, optional expiry).
- **Expiration** — sessions expire by TTL (checked + lazily deleted on read,
  exactly `sessionRecord()` in `auth.ts:183`). PATs default to no-expiry
  (replica credentials) but MAY carry an `expiresAtMs`.
- **Revocation** — deleting `pat:<hash>` (or `session:<hash>`) *is* the
  revocation; the next `resolve_principal` misses. No cache between deletion
  and effect (ADR-0003's load-bearing property — preserve it; if a perf
  cache is ever added it must be a revocation-generation check, never a plain
  TTL, per ADR-0003 consequences).
- **Rate-limit** — a per-principal token-bucket on mutating actions
  (sliding window in the store; out of scope for the first checkpoint, listed
  as an open decision for the threshold).

### 3.4 Authorization — scopes, not all-or-nothing

`resolve_principal` returns a richer `Principal` carrying a scope set. The
guards (`bearer_guard`, `mcp_guard`) check both *authenticated* and
*scope ⊇ required-scope-of-action*. Minimum viable scope vocabulary:

- `registry:read` (list apps / fleet) — currently open, can stay open or
  become a read scope per the open decision below.
- `registry:write` (install/remove/enable/set_*) — replaces today's blanket
  mutating gate.
- `admin` (create/revoke principals, mint PATs for others).

Action → required scope is declared where mutating-tool names are collected
today (the `mutating_tools` set in `AuthGate`), extended to a
`name → scope` map sourced from the app's `describe()` manifest.

### 3.5 Audit logging (mutations → principal)

Every mutating action and MCP `tools/call` that passes a guard writes an
`audit:*` record: principal id, action name, app, a digest (not the
plaintext) of the args, timestamp, and outcome (ok / validation-error). This
is the "who-changed-what-when" the issue asks for. Surfaced read-only via an
admin-scoped `GET /api/audit` (and an MCP read tool). The CRDT `ActorId`
(`doc.rs:36`) stays as-is for merge mechanics; the audit log is the *human*
attribution layer, separate from the Automerge actor.

### 3.6 Multi-user vs single-user scope, per tier

| Deployment tier | Mode |
|---|---|
| Single-user loopback (default) | **Single-user, open** — no accounts required; behavior byte-identical to today. Optional: require a PAT even on loopback (opt-in). |
| Self-hosted multi-user fleet | **Multi-user** via local-admin bootstrap + per-operator PATs/sessions; OIDC optional. |
| Federated registry | **Multi-user, per-host** — credentials never federate; the registry *document* does. |
| Cloudflare multi-tenant | **Account == tenant** — unchanged (ADR-0003). |

---

## 4. Registry gating: from shared bearer to the new model

Today: `AuthGate.authorized(headers)` does a single `ct_eq` against the one
`TANGRAM_AUTH_TOKEN` (`auth.rs:48`). The registry's `INSTRUCTIONS` advertise
that token.

New: the guards call `resolve_principal` (the existing seam), which now
consults the account store (`pat:<hash>` / `session:<hash>`) instead of a
single string, then checks the action's required scope. The `mcp_guard`
body-inspection logic and the uniform-401 are unchanged in shape — only the
*lookup* swaps, exactly as the seam's doc comment promised.

**Migration / back-compat for existing `TANGRAM_AUTH_TOKEN` deployments**
(must not break running fleets):

1. If `TANGRAM_AUTH_TOKEN` is set and the account store is empty, the host
   **auto-imports it as a single legacy PAT** with `registry:write` scope
   under a synthetic `legacy-shared-token` principal (logged once with a
   deprecation warning). Existing shell UIs / scripts keep working unchanged.
2. The env token is honored for one minor-version deprecation window, then
   the host warns louder, then (a later checkpoint) refuses it in favor of
   minted PATs.
3. The registry `INSTRUCTIONS` string is updated to describe PATs while
   noting the legacy token still works during the window.

The native tenant table (`config.rs` `TenantSpec.token`) gets the same
treatment: a per-tenant static token becomes a seeded per-tenant legacy PAT,
and tenants gain their own account scope under the same store. This is the
natural place the host & CF tenant models finally converge.

---

## 5. Shell UI auth flow

Replace the plaintext `<input>` (`main.ts:70–74`) and the `localStorage`
token slot (`manage.ts:13`) with a real session:

- **Login screen** — a small auth view: "Sign in" (OIDC, if configured) or
  "Enter a PAT" (for the bootstrap/no-IdP case). On success the host sets the
  `tgs_…` HttpOnly SameSite=Lax session cookie; the UI never holds the token
  in JS-readable storage. Relative fetches then ride the cookie (the same
  trick the CF tenant UI uses — `auth.ts` notes the cookie path).
- **Session indicator** — replace the token field with a principal chip:
  display name / provider, a "sign out" affordance (drops the session —
  `logout()` equivalent), and, for admins, a link to a PAT-management view
  (mint/list/revoke, label + scopes, token shown once — port `account.html`/
  `account.ts`).
- **Removal of plaintext storage** — `tangram_auth_token` localStorage usage
  is deleted; `manage.ts`'s `registryAction` stops attaching a bearer from
  localStorage and relies on the cookie (with a transitional fallback to a
  pasted PAT for the bootstrap flow only).
- **401 handling** — on 401 the UI routes to the login screen instead of
  surfacing "set the auth token first".

---

## 6. Security analysis

- **Credential at rest** — only SHA-256 hashes stored; plaintext shown once.
  Removes the localStorage-plaintext and visible-field weaknesses. (Per
  ADR-0003; SHA-256 of a 160-bit CSPRNG token is fine — these are
  high-entropy random tokens, not passwords, so no KDF needed. A local-admin
  *password*, if offered, must use a password KDF (argon2/scrypt) — open
  decision.)
- **Revocation immediacy** — no cache; delete-is-revoke. Preserves the
  always-on-replica guarantee from ADR-0003.
- **No existence oracle** — keep the uniform-401 (`tenant_unauthorized` /
  `unauthorized` already do this; extend to per-principal failures so a wrong
  PAT, an expired session, and a revoked PAT are indistinguishable).
- **CSRF** — session is SameSite=Lax + HttpOnly; mutating actions are POST;
  OAuth `state` round-trips a short-lived cookie (reuse ADR-0003's pattern).
- **Constant-time** — keep `ct_eq` for any direct compares; hash lookups are
  inherently not a timing oracle on the secret.
- **Credentials never replicate** — the account store is host-local and out
  of the Automerge document; federation moves desired state, not secrets
  (mirrors `tenant.rs`'s `${VAR}`-blanking of registry-sourced env).
- **Audit integrity** — append-only log; admin-read only; args stored as a
  digest, not plaintext (avoids logging injected secrets).
- **Side-channel posture** — unchanged and out of scope here; ADR-0006
  governs co-resident isolation. Egress credential injection (ADR-0005)
  remains the secret-handling boundary; IAM gates *who can configure* it.
- **Rate-limit** — per-principal bucket on mutations blunts a leaked-PAT
  brute force / abuse (threshold is an open decision).

---

## 7. Phased, testable checkpoints

Each is independently shippable and fixture/e2e-testable. Reuse the
`scripts/e2e-cloudflare-identity.sh` patterns (stub IdP via env-overridable
OAuth URLs, the 401 matrix, repeatability run-twice, trap-based teardown).

- **C0 — `Principal` carries identity + scopes (no behavior change).**
  Extend `Principal` (add a `Pat`/`User` variant with id + scope set) and a
  `Scope` type; `resolve_principal` still backed by the single env token
  (mapped to full scope). Pure refactor; unit tests in `auth.rs`. *Ships
  green, changes nothing observable.*

- **C1 — the account store + hashed PATs (host-local).** Port
  `TangramAccounts` to Rust (the table in §3.2): create principal, mint/
  list/revoke PAT, sessions with TTL, `authorize`-equivalent. Pure-logic unit
  tests mirroring `auth.ts` semantics (hash-at-rest, delete-is-revoke,
  expiry). No wiring yet.

- **C2 — registry gating swaps to the store + legacy import.**
  `resolve_principal` consults the store; `TANGRAM_AUTH_TOKEN` auto-imports
  as a legacy PAT (§4). Action→scope map drives the guards. Back-compat test:
  an existing shell UI flow with the old env token still works. New test:
  minted PAT works, revoked PAT 401s on the next request.

- **C3 — audit log.** Write `audit:*` on every passed mutating guard; admin
  `GET /api/audit` + MCP read tool. Test: install/remove via two distinct
  PATs produces two attributed records; args are digested not plaintext.

- **C4 — shell UI session flow.** Login view, session cookie, principal chip,
  PAT-management view; delete the localStorage token slot. e2e: cookie-based
  UI mutate, sign-out drops access, paste-a-PAT bootstrap path.

- **C5 — OAuth/OIDC sign-in (optional plane).** Port the hand-rolled
  authorization-code client with env-overridable IdP URLs; first-sign-in
  account creation. e2e with a **stub IdP**, structured exactly like
  `e2e-cloudflare-identity.sh` (no external network, no real GitHub).

- **C6 — tenant table convergence + rate-limit.** Per-tenant static tokens
  become seeded per-tenant PATs in the store; add the per-principal
  mutation rate-limit. e2e: per-tenant isolation preserved (the existing
  uniform-401 matrix), rate-limit trips and recovers.

(C0–C4 are the core that closes the issue; C5–C6 are the convergence/hardening
follow-ons and can land later.)

---

## 8. Effort estimate

7 checkpoints (C0–C6). Rough, owner-implementation-dependent:

- C0: ~0.5 day (refactor + tests)
- C1: ~1.5 days (store + port the CF semantics + unit tests)
- C2: ~1.5 days (wiring + legacy import + back-compat tests)
- C3: ~1 day (audit log + read surface)
- C4: ~2 days (UI: login view, cookie flow, PAT management, remove plaintext)
- C5: ~2 days (OAuth client + stub-IdP e2e)
- C6: ~1.5 days (tenant convergence + rate-limit + e2e)

**Core (C0–C4): ~6.5 days. Full (C0–C6): ~10 days.** The CF model and e2e
harness materially de-risk C1/C5 (we are porting, not designing from zero).

---

## 9. Open decisions for the owner

1. **Local-admin password?** Offer username+password (argon2) for the no-IdP
   bootstrap, or PAT-only (simpler, no KDF, no password reset surface)?
   Recommendation: **PAT-only** for the first cut; passwords add a reset/
   lockout surface we don't need.
2. **Reads gated or open?** Keep `list_apps` / state / fleet open (today's
   behavior) or move them behind `registry:read`? Recommendation: keep open
   for single-user compat; make it a per-host config flag for fleets.
3. **Store backend** — embedded sqlite (rusqlite) vs a file-backed KV. sqlite
   matches the CF DO's SQLite backing and gives easy audit queries;
   adds a dependency. Owner's call on dependency budget.
4. **PAT default expiry** — none (replica-friendly, ADR-0003 choice) vs a
   default TTL with renewal. Recommendation: **none**, opt-in expiry.
5. **Rate-limit threshold + scope** — per-principal mutation cap value, and
   whether it applies in single-user mode.
6. **Deprecation window length** for `TANGRAM_AUTH_TOKEN` (one minor? two?).
7. **OIDC providers** — GitHub only (mirrors ADR-0003) first, or generic
   OIDC discovery from the start?
8. **Host↔CF unification depth** — stop at "same model/format" (this design)
   or pursue the shared device-flow / host-validates-CF-PATs convergence now
   (RUNTIME_PLAN Phase 6 follow-up)?

---

## 10. Placement & merge strategy

- **Code lands in `crates/tangram-host/src/`**: extend `auth.rs` (the
  `Principal` seam — the design's anchor), add an `accounts.rs` (the store)
  and `audit.rs`; UI in `apps/tangram/ui/src/`. The registry app's
  `INSTRUCTIONS` (`apps/registry/src/lib.rs`) get a doc update. A new
  **ADR-0008 (native identity)** should accompany C1/C2, recording the
  port-from-CF decision the way ADR-0003 recorded the CF one.
- **PR-held vs direct:** this *design doc* is a held-for-review PR. Each
  implementation checkpoint (C0–C6) should be its own PR, **held for owner
  approval** — auth is security-sensitive and several touch
  `tangram-host/src`.
- **Dependency / conflict note:** the implementation intersects the in-flight
  PRs that touch `tangram-host/src` — **#1 fine-grained-egress-section-1**,
  **#2 manifest-verification**, **#4 egress-policy-engine** (and the auto-memory
  "egress policy-engine follow-up"). `auth.rs` and `routes.rs` (the
  `resolve_principal` call site, `routes.rs:447`) are the likely contention
  points. Recommendation: **sequence the IAM work after those egress/manifest
  PRs merge**, or at minimum rebase C0 (the pure `Principal` refactor) on top
  of them first so the seam change is isolated and easy to review. C0 is
  deliberately behavior-preserving precisely so it can slot in around the
  egress work without semantic conflicts.
- **CI:** `tangram-core` must keep compiling for `wasm32-wasip2` — the
  account store and OAuth client live in `tangram-host` (native-only,
  tokio/rusqlite), **not** in `tangram-core`, so this constraint is naturally
  respected. New e2e scripts follow the `e2e-cloudflare-identity.sh` template
  (self-contained, ephemeral ports, trap teardown).

---

*Design only. No code in this PR. Blocked on owner approval of the approach
and the open decisions in §9 before any checkpoint is implemented.*
