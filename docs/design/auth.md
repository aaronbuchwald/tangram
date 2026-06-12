# Design: Auth — two deployment modes, per-principal data, and IAM

**Status:** proposed (design only — held for owner approval before any code)
**This is the canonical auth design.** It **supersedes**
[`docs/design/iam-auth.md`](iam-auth.md) (PR #23) and the standalone per-user
registry design, consolidating three issues filed separately for what is one
auth redesign:

- **[#31](https://github.com/aaronbuchwald/tangram/issues/31)** — the
  umbrella: the two-mode model (self-hosted vs multi-tenant), the `Principal`
  enum, per-principal data dirs, per-mode middleware, audit, back-compat, and
  the 5-phase rollout. This doc makes #31's design canonical.
- **[#20](https://github.com/aaronbuchwald/tangram/issues/20)** — the concrete
  IAM mechanics: the Cloudflare `TangramAccounts` port (hashed PATs + browser
  sessions + optional OAuth behind the `Principal` seam), scopes, audit, the
  migration story, and the testable C0–C6 checkpoints. Folded in here as the
  **token/session/credential layer of multi-tenant mode**.
- **[#30](https://github.com/aaronbuchwald/tangram/issues/30)** — the per-user
  registry specifics: per-user registry documents, strict user isolation, and
  the external-OAuth-first recommendation. Folded in here as the
  **per-principal registry** section.

**Related:** ADR-0003 (Cloudflare identity — accounts==tenants, hashed PATs,
sessions, the e2e harness), ADR-0006 (tenant isolation posture — the tiering
this maps onto), RUNTIME_PLAN Phases 5 (host multi-tenancy, the `Principal`
seam, users==tenants) and 6 (CF identity). Existing code anchors:
`crates/tangram-host/src/auth.rs` (the `Principal` seam this extends),
`src/tenant.rs` (`/t/<tenant>/` confinement), `src/main.rs` (routing + data
roots), `apps/registry/src/lib.rs` (`TANGRAM_AUTH_TOKEN` gating).

This is a research + design deliverable. No code accompanies it; each
implementation checkpoint is its own held-for-review PR (§9).

---

## 1. The model in one paragraph

Tangram runs in two deployment shapes that share one codebase, chosen by
runtime config. **Self-hosted** is the default: one instance, one trusted user
(or trusted LAN), loopback-trust — "if connected, you're authorized" — minted
as `Principal::LocalUser`, no OAuth, behavior byte-identical to today.
**Multi-tenant** identifies users via OAuth/OIDC, mints
`Principal::User { user_id, email, groups }`, partitions every user's registry
and app data under `~/.tangram/<user_id>/`, validates a per-request credential
(the hashed-PAT/session store ported from Cloudflare — #20), enforces strict
cross-user isolation, and audits every mutation to a principal. The
`[auth] mode` key selects which. Both land in the same `Principal` seam that
`crates/tangram-host/src/auth.rs:83` already defines, so call sites downstream
consume a `Principal`, never a raw header.

---

## 2. Configuration — `[auth]` in apps.toml

```toml
# Self-hosted mode (default — omit the section entirely and you get this)
[auth]
mode = "self-hosted"
# No further config. Loopback-only; non-loopback bind is refused (as today).

# Multi-tenant mode
[auth]
mode = "multi-tenant"
oauth_issuer        = "https://accounts.google.com"   # OIDC discovery URL
oauth_client_id     = "..."
oauth_client_secret = "env://OAUTH_CLIENT_SECRET"     # resolved from .env, never inline
# optional:
default_user_id     = "owner"   # migration target for an existing single registry (§7)
reads_gated         = false     # keep list/state open (default) or require registry:read
```

Config validation rejects `multi-tenant` without a resolvable issuer +
client id/secret (an invalid OAuth config must not silently fall back to an
open mode). `env://` indirection reuses the existing `${VAR}` blanking rule
(`tenant.rs:124`) so the secret never lands in a replicated document.

---

## 3. The `Principal` enum and per-principal `data_dir()`

#31's enum **extends the existing seam** (`auth.rs:90`, today `Local |
Tenant(String)`). The two-mode design adds identity + scopes (the latter from
#20):

```rust
#[derive(Clone, Debug)]
pub enum Principal {
    /// Self-hosted mode: implicit single user, loopback-trusted.
    /// (Replaces / generalizes today's `Local`.)
    LocalUser,

    /// Multi-tenant mode: identified via OAuth, or via a minted PAT/session.
    User {
        user_id: String,        // stable local id (IdP identity maps to it)
        email: String,
        groups: Vec<String>,    // from the IdP, for RBAC later (Phase 5)
        scopes: ScopeSet,       // registry:read / registry:write / admin (#20 §3.4)
    },
}

impl Principal {
    /// Per-principal data root. LocalUser owns the whole tree; a User is
    /// confined to its own subtree — the same confinement `tenant.rs`'s
    /// `validate_tenant_data_dir` already enforces for `/t/<tenant>/`.
    pub fn data_dir(&self) -> PathBuf {
        match self {
            Self::LocalUser => PathBuf::from(".tangram"),
            Self::User { user_id, .. } => PathBuf::from(format!(".tangram/{user_id}")),
        }
    }
}
```

Per-principal partitioning (#30 + #31):

| Artifact | Self-hosted (`LocalUser`) | Multi-tenant (`User`) |
|---|---|---|
| Registry doc | `~/.tangram/registry.automerge` (shared) | `~/.tangram/<user_id>/registry.automerge` (isolated) |
| App data | `~/.tangram/<app>/<app>.automerge` | `~/.tangram/<user_id>/<app>/<app>.automerge` |
| Sync remotes | one device | per-user (or explicit sharing grant — future) |

`User` partitioning **reuses the tenant confinement machinery**: today
`tenant.rs` already roots a tenant app's data under `<data_root>/<tenant>/…`
"no matter what its spec says". A `User`'s `user_id` plays the role a tenant
name plays now (RUNTIME_PLAN Phase 5's explicit "users == tenants"). This is
not a new isolation mechanism — it is the existing one keyed by principal.

---

## 4. Auth middleware per mode

Both modes resolve a `Principal` into request extensions; routes read it from
there (`Extension<Principal>`), exactly as #31 specifies.

**Self-hosted — loopback trust.** A middleware checks `ConnectInfo<SocketAddr>`:
loopback ⇒ insert `Principal::LocalUser` and proceed; non-loopback ⇒ 403. This
is the same guarantee `main.rs` already gives (refuse to run a registry app on
a non-loopback bind without a token); the design makes it an explicit
middleware that mints the principal rather than an implicit boot check.

**Multi-tenant — OAuth/OIDC + the credential store.** This is where #20's
mechanics are the implementation:

1. **Sign-in** runs the hand-rolled authorization-code client ADR-0003 already
   uses (env-overridable `OAUTH_{AUTHORIZE,TOKEN,USER}_URL` so the e2e swaps a
   stub IdP — the *exact* seam `e2e-cloudflare-identity.sh` exercises). First
   sign-in creates a local account; the IdP identity (`ident:<provider>:<id>`)
   maps to a stable `user_id`.
2. **Per-request validation** does **not** re-hit the IdP on every call.
   Sign-in (or a minted PAT) yields a **host-local credential** validated by
   hash lookup — the Cloudflare `TangramAccounts` model ported to Rust (#20
   §3.2): `session:<sha256(token)>` (HttpOnly `tgs_…` cookie, 30-day TTL) for
   the UI, `pat:<sha256(token)>` (`tgp_…`, 160-bit CSPRNG) for replicas / MCP /
   CLI. **Tokens stored only as SHA-256 hashes; plaintext shown once.**
   `resolve_principal` consults this store (replacing today's single `ct_eq`
   against `TANGRAM_AUTH_TOKEN`) and returns a `Principal::User` carrying the
   credential's `user_id`, email, groups, and scope set.
3. **Authorization** is scope-checked, not all-or-nothing (#20 §3.4): the
   guards check *authenticated* AND *scope ⊇ the action's required scope*
   (`registry:read` / `registry:write` / `admin`), sourced from the app's
   `describe()` manifest (extending today's `mutating_tools` set in
   `AuthGate`).

The account/credential store is **host-local and per-host — never replicated
into the Automerge document** (a replicated credential is a leaked credential;
same rule as `tenant.rs`'s env blanking). Federation moves desired app state,
not secrets.

---

## 5. Per-principal registry (folding in #30)

Today `apps/registry` is one shared document whose `INSTRUCTIONS` advertise
`TANGRAM_AUTH_TOKEN` as the access model. Under this design:

- **Self-hosted:** unchanged — one shared registry at
  `~/.tangram/registry.automerge`, `LocalUser` has full access. No per-user
  subdirectories. This is exactly today's behavior.
- **Multi-tenant:** the registry becomes **per-principal**. Each `User` gets
  an independent registry document at `~/.tangram/<user_id>/registry.automerge`.
  The registry actions (`install_app`, `remove_app`, `enable`, `set_*`) mutate
  *that principal's* registry document — `User("alice")`'s installs never
  appear in `User("bob")`'s app list. Each user independently chooses which
  apps to run and (later) which remotes to sync.

**Isolation enforcement.** A route resolves `principal.data_dir()`, opens the
registry under it, and operates only there. Any attempt by `User(a)` to read
or mutate `User(b)`'s registry or app data is structurally impossible (the path
is derived from the authenticated principal, not from a request parameter) and,
if one is ever constructed (e.g. a crafted `user_id` path), rejected by the
same `validate_tenant_data_dir` path-confinement check `tenant.rs` already
applies. **Fully isolated to start** (#30 decision #2); read-only cross-user
sharing is explicit opt-in, deferred (§9).

The registry `INSTRUCTIONS` string is updated to describe PATs/sessions for
multi-tenant mode while noting the legacy token still works in self-hosted /
during the deprecation window (§7).

---

## 6. Audit log (mutations → principal)

Every mutating action and MCP `tools/call` that passes a guard writes an audit
record — the "who-changed-what-when" all three issues ask for:

- **Self-hosted:** audit is optional/skippable (implicit single user); a
  `LocalUser` log is low-value but MAY be enabled.
- **Multi-tenant:** mandatory. Record = `{ principal user_id, email, action,
  app, args-digest, outcome, ts }`. Args stored as a **digest, not plaintext**
  (avoids logging injected secrets). Append-only; surfaced read-only via an
  **admin-scoped `GET /api/audit`** (and an MCP read tool). The CRDT `ActorId`
  (`doc.rs:36`, random per process) is unchanged — the audit log is the *human*
  attribution layer, separate from the Automerge merge actor.

---

## 7. OAuth provider recommendation & migration / back-compat

**Provider — external-first.** All three issues converge here (#20 alt-B/C,
#30 decision #1, #31's OIDC client): adopt the **hand-rolled
authorization-code OAuth *client*** ADR-0003 already proved, pointed at an
external IdP (GitHub first, mirroring ADR-0003; generic OIDC discovery a near
follow-on — open decision §9). Do **not** make the host an OAuth *authorization
server* (ADR-0003 rejected that; no consumer needs spec-compliant OAuth yet).
A reverse-proxy/external-IdP front (Cloudflare Access, oauth2-proxy) is
supported as an *optional* front for orgs that already run one, but is **not**
the primary mechanism (identity leaving the repo is untestable in CI — ADR-0003's
exact objection). For a no-IdP fleet, the local-admin PAT bootstrap (#20 §3.1)
gives full IAM without any OAuth server.

**Migration / back-compat — must not break running fleets:**

- **Existing single-registry + `TANGRAM_AUTH_TOKEN` → self-hosted, no
  migration.** With no `[auth]` section the host defaults to self-hosted mode;
  the existing `~/.tangram/registry.automerge` becomes `LocalUser`'s registry,
  untouched. Existing shell UIs / scripts keep working. If `TANGRAM_AUTH_TOKEN`
  is set, it is honored (and, when multi-tenant is later enabled, auto-imported
  as a single legacy PAT with `registry:write` under a synthetic
  `legacy-shared-token` principal — logged once with a deprecation warning,
  honored for a deprecation window, then refused in favor of minted PATs).
- **Opt-in to multi-tenant migrates to `<default_user_id>/`.** Setting
  `mode = "multi-tenant"` migrates the existing single registry →
  `~/.tangram/<default_user_id>/registry.automerge`; on next login users see
  their own isolated registries.
- The native per-tenant static token table (`config.rs` `TenantSpec.token`)
  gets the same treatment — a per-tenant static token becomes a seeded
  per-tenant PAT in the store. This is where the host & CF tenant models
  finally converge (#20 §4).

---

## 8. Mapping to ADR-0006 tiers and RUNTIME_PLAN Phase 5

| Deployment tier (ADR-0006 / RUNTIME_PLAN) | Auth mode | Isolation posture |
|---|---|---|
| Single-user loopback (first-party, today) | **self-hosted**, `LocalUser`, open | In-process WASM sufficient; loopback-only; no accounts required. |
| Self-hosted multi-user fleet | **multi-tenant** via local-admin PAT bootstrap + per-operator PATs/sessions; OIDC optional | Per-operator identity + revocation + audit; credentials never federate. |
| Federated registry (Phase 9) | **multi-tenant, per-host** | The registry *document* federates; **credentials do not** — each host authenticates its own operators. |
| Multi-tenant / semi-trusted tenants | **multi-tenant**, `User` == tenant | ADR-0006 tiering applies: egress injection (ADR-0005) baseline + SMT/fuel/memory limits for semi-trusted; this design supplies the *who*, ADR-0006 supplies the *co-residency* posture. |
| Untrusted third-party (marketplace SaaS) | **multi-tenant** + ADR-0006 untrusted-tier controls | Auth identifies the tenant; process-per-tenant/core/SMT/CAT controls are mandatory and out of scope here (ADR-0006). |

Auth answers *who* a request is and *what they may touch*; ADR-0006 governs
*how co-resident code is physically isolated*. They compose; neither replaces
the other. RUNTIME_PLAN Phase 5's "users == tenants" is the design point that
lets per-principal partitioning reuse the tenant confinement machinery.

---

## 9. Phased, testable checkpoints

Aligning #31's 5 phases with #20's C0–C6 (the finer-grained, independently
shippable, fixture/e2e-testable checkpoints — #20's are a strict refinement of
#31's, so they are merged here). Each reuses the
`scripts/e2e-cloudflare-identity.sh` patterns: stub IdP via env-overridable
OAuth URLs, the 401 matrix, run-twice repeatability, trap-based teardown.

- **C0 — `Principal` carries identity + scopes (no behavior change).** Extend
  the enum to `LocalUser` / `User { … scopes }` + a `ScopeSet`;
  `resolve_principal` still backed by the env token mapped to full scope.
  Pure refactor; unit tests in `auth.rs`. *Ships green, nothing observable
  changes.* (#31 Phase 1 start.)

- **C1 — self-hosted loopback middleware.** The explicit loopback-trust
  middleware mints `LocalUser`; routes read `Extension<Principal>`. e2e:
  loopback mutate succeeds, non-loopback 403. (#31 Phase 1.)

- **C2 — the account/credential store + hashed PATs (host-local).** Port
  `TangramAccounts` to Rust (§4): create principal, mint/list/revoke PAT,
  sessions with TTL, `authorize`-equivalent. Pure-logic unit tests mirroring
  `auth.ts` (hash-at-rest, delete-is-revoke, expiry). No wiring yet. (#20 C1;
  #31 Phase 2 prep.)

- **C3 — multi-tenant gating + per-principal registry + legacy import.**
  `resolve_principal` consults the store; action→scope map drives the guards;
  registry + app data open under `principal.data_dir()`; `TANGRAM_AUTH_TOKEN`
  auto-imports as a legacy PAT. Tests: minted PAT works, revoked PAT 401s next
  request, **`User(a)` cannot read `User(b)`'s registry/app data**, old env
  token still works. (#31 Phases 2+3; #20 C2; #30 Phases 1+2.)

- **C4 — audit log.** Write audit records on every passed mutating guard;
  admin `GET /api/audit` + MCP read tool. Test: install/remove via two distinct
  principals produces two attributed records; args digested not plaintext.
  (#31 Phase 4; #20 C3; #30 Phase 3.)

- **C5 — shell UI session flow.** Login view (Sign in / paste-a-PAT), HttpOnly
  session cookie, principal chip, PAT-management view; **delete the
  `localStorage["tangram_auth_token"]` slot.** e2e: cookie-based UI mutate,
  sign-out drops access, paste-a-PAT bootstrap. (#20 C4.)

- **C6 — OAuth/OIDC sign-in (optional plane).** Port the hand-rolled
  authorization-code client with env-overridable IdP URLs; first-sign-in
  account creation. e2e with a **stub IdP**, structured exactly like
  `e2e-cloudflare-identity.sh`. (#31 Phase 2 OAuth; #20 C5; #30 Phase 1 OAuth.)

- **C7 — tenant-table convergence + rate-limit.** Per-tenant static tokens
  become seeded per-tenant PATs; per-principal mutation rate-limit. e2e:
  per-tenant uniform-401 matrix preserved, rate-limit trips and recovers.
  (#20 C6.)

C0–C5 are the core that closes #20/#30/#31; C6–C7 are convergence/hardening
follow-ons. RBAC from `groups` (#31 Phase 5) is future, built on the `groups`
field C0 already carries.

**8 checkpoints (C0–C7).**

---

## 10. Effort estimate

Rough, owner-implementation-dependent. The CF model + e2e harness materially
de-risk C2/C6 (porting, not designing from zero).

| Checkpoint | Estimate |
|---|---|
| C0 — Principal + scopes refactor | ~0.5 day |
| C1 — self-hosted loopback middleware | ~0.5 day |
| C2 — account/credential store | ~1.5 days |
| C3 — multi-tenant gating + per-principal registry + legacy import | ~2.5 days |
| C4 — audit log | ~1 day |
| C5 — shell UI session flow | ~2 days |
| C6 — OAuth/OIDC sign-in (stub-IdP e2e) | ~2 days |
| C7 — tenant convergence + rate-limit | ~1.5 days |

**Core (C0–C5): ~8 days. Full (C0–C7): ~11.5 days.**

---

## 11. Consolidated open decisions

Merged from #20's 8 and #31's key decisions, deduplicated (#30's three
"recommendations" are folded as the resolved defaults — external-OAuth-first,
fully-isolated-registries-first, per-device-sync-first — and are not re-opened
here):

1. **Local-admin password?** Username+password (argon2) for the no-IdP
   bootstrap, or **PAT-only** (recommended — no reset/lockout surface)?
2. **Reads gated or open?** Keep `list_apps` / state / fleet open (today) or
   behind `registry:read`? Recommendation: open for self-hosted compat,
   per-host `reads_gated` flag for multi-tenant fleets.
3. **Credential store backend** — embedded sqlite (rusqlite, matches the CF
   DO's SQLite, easy audit queries) vs a file-backed KV. Dependency-budget call.
4. **PAT default expiry** — none (replica-friendly, ADR-0003) vs default TTL.
   Recommendation: none, opt-in expiry.
5. **Rate-limit threshold + scope** — per-principal mutation cap, and whether
   it applies in self-hosted mode.
6. **`TANGRAM_AUTH_TOKEN` deprecation window** — one minor version or two?
7. **OIDC providers** — GitHub only first (mirrors ADR-0003) or generic OIDC
   discovery from the start?
8. **Host↔CF unification depth** — stop at "same model/format" (this design)
   or pursue shared device-flow / host-validates-CF-PATs now (Phase 6 follow-up)?
9. **Migration default user id** — fixed `default_user_id` config (recommended)
   vs prompt-on-first-multi-tenant-boot; and whether the legacy shared-token
   principal is auto-created or requires explicit opt-in.
10. **Registry sharing model** — confirm fully-isolated-first (#30 default) is
    the v1, with read-only cross-user sharing grants as the explicit-opt-in
    follow-on (marketplace-style), not v1.

**10 consolidated open decisions.**

---

## 12. Security checklist

- [ ] **Self-hosted: loopback-only enforcement** prevents internet exposure
  (non-loopback bind without a credential is refused — preserve `main.rs`'s
  current guarantee, now as explicit middleware).
- [ ] Self-hosted: no OAuth overhead for the single-user case.
- [ ] **No cross-user leak:** `User(a)` cannot read/mutate `User(b)`'s
  registry or app data — data paths derived from the authenticated principal,
  confined by `validate_tenant_data_dir`.
- [ ] Multi-tenant: token validation + **revocation immediacy** — deleting the
  hash row 401s on the very next request (no cache between delete and effect;
  any future perf cache must be a revocation-generation check, not a TTL).
- [ ] **Credential at rest:** only SHA-256 hashes stored; plaintext shown once;
  the `localStorage` token slot is deleted (C5).
- [ ] **HttpOnly cookies, not localStorage** for sessions (SameSite=Lax;
  mutating actions are POST; OAuth `state` round-trips a short-lived cookie).
- [ ] **No existence oracle:** uniform 401 for missing header / wrong PAT /
  expired session / revoked PAT / unknown principal (extend today's uniform-401).
- [ ] **Credentials never replicate** — account store is host-local, out of the
  Automerge document; federation moves desired state, not secrets.
- [ ] Multi-tenant: audit trail captures all mutations; args digested not
  plaintext; admin-read only.
- [ ] Config validation rejects invalid/partial OAuth config (no silent
  fallback to an open mode).
- [ ] Per-principal mutation rate-limit blunts a leaked-PAT brute force
  (threshold is open decision §11.5).

---

## 13. Placement & merge strategy

- Code lands in `crates/tangram-host/src/`: extend `auth.rs` (the `Principal`
  seam — the anchor), add `accounts.rs` (the credential store) and `audit.rs`;
  per-principal data-dir routing in `main.rs`/`routes.rs` reusing
  `tenant.rs`'s confinement; UI in `apps/tangram/ui/src/`. The registry app's
  `INSTRUCTIONS` (`apps/registry/src/lib.rs`) get a doc update. A new
  **ADR-0008 (native identity)** should accompany C2/C3, recording the
  port-from-CF decision the way ADR-0003 recorded the CF one.
- `tangram-core` must keep compiling for `wasm32-wasip2`: the credential store
  and OAuth client live in `tangram-host` (native-only, tokio/rusqlite),
  **not** in `tangram-core` — constraint naturally respected.
- **This design doc is a held-for-review PR.** Each implementation checkpoint
  (C0–C7) is its own PR, **held for owner approval** — auth is
  security-sensitive and several touch `tangram-host/src`.
- **Conflict note:** the implementation intersects the in-flight egress/manifest
  PRs touching `tangram-host/src` (`auth.rs`, `routes.rs` near the
  `resolve_principal` call site). Recommendation: sequence the auth work after
  those merge, or at minimum rebase C0 (the behavior-preserving `Principal`
  refactor) on top first so the seam change is isolated and easy to review.

---

*Design only. No code in this PR. This doc is the single source of truth for
Tangram auth; it supersedes `iam-auth.md` (PR #23) and the standalone per-user
registry design. Blocked on owner approval of the approach and the open
decisions in §11 before any checkpoint is implemented.*
