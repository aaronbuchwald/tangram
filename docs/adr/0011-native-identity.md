# ADR-0011: Native host identity — port the Cloudflare credential store to Rust (hashed PATs + sessions)

**Status:** accepted (2026-06-15)
**Deciders:** Aaron (owner), with analysis by Claude
**Related:** ADR-0003 (Cloudflare identity — the `TangramAccounts` model this
ports: accounts==tenants, hashed PATs, browser sessions, optional OAuth behind
the `Principal` seam, the e2e harness), ADR-0006 (tenant isolation posture —
the tiering this auth layer composes with), `docs/design/auth.md` (the canonical
auth design and the C0–C7 checkpoints; this ADR accompanies C2/C3), the
`Principal` seam in `crates/tangram-host/src/auth.rs`.

## Context

`docs/design/auth.md` defines two deployment modes that share one codebase.
Self-hosted (the default) is loopback-trusted and needs no accounts.
Multi-tenant identifies users, partitions their data, validates a per-request
credential, and audits mutations. That credential layer needs a host-local
store of accounts, identity links, tokens, and sessions.

ADR-0003 already designed and shipped exactly this model for Cloudflare in
TypeScript (`TangramAccounts`): SHA-256-hashed Personal Access Tokens, browser
sessions with a TTL, optional OAuth behind the same seam, validated by hash
lookup. The native host needs the same semantics. The question is whether to
invent a new mechanism or port the proven one, and where it lives in the
crate graph.

## Decision

**Port the Cloudflare `TangramAccounts` model to Rust verbatim in semantics,
as a host-local store over embedded SQLite (rusqlite), living in
`tangram-host` only.** (`crates/tangram-host/src/accounts.rs`.)

- **Embedded SQLite (rusqlite, bundled).** No external DB process; the store
  is a single file under the host data root. Resolved decision auth.md §11.3.
- **Tokens are hashed at rest, shown once.** A PAT is `tgp_` + base64url(20
  CSPRNG bytes); a session is `tgs_` + base64url(20). We store ONLY
  `hex(sha256(plaintext))`. The plaintext is returned to the caller exactly
  once at mint time and is unrecoverable thereafter. Validation hashes the
  presented token and looks the row up by hash — so deleting the row 401s on
  the very next request (revocation immediacy; no cache between delete and
  effect). This is the ADR-0003 discipline, unchanged.
- **Prefix routes the lookup.** `tgp_…` → the PAT table (replicas / MCP / CLI,
  with a scope set); `tgs_…` → the sessions table (the HttpOnly UI cookie,
  full interactive authority). Anything else validates to nothing.
- **All time is passed in (`now_ms`).** No ambient clock in the store, so
  expiry and TTL are deterministically testable — mirroring the CF tests.
- **Owner-scoped revocation.** A PAT is revoked by `(user_id, id)`: one user
  can never revoke another's token by guessing an id.
- **Credentials never replicate.** The store is host-local and per-host, out
  of every Automerge document — a replicated credential is a leaked
  credential. Federation moves desired app state, not secrets (auth.md §4).
- **Native deps stay in `tangram-host`.** `tangram-core` must keep compiling
  for `wasm32-wasip2`; rusqlite/rand are native-only and live in the host
  crate, not the portable core (auth.md §13).

Scopes (`registry:read` / `registry:write` / `admin`) extend the `Principal`
seam in `auth.rs`: a `Principal::User` carries the scope set its credential
validated to, and authorization becomes scope-checked rather than
all-or-nothing. `LocalUser` and `Tenant` keep full authority within their
surface.

## Consequences

- The native host and the Cloudflare worker now share one account model and one
  set of security invariants — the host & CF tenant models converge (auth.md
  §7). The same e2e patterns (`scripts/e2e-cloudflare-identity.sh`: stub IdP,
  the 401 matrix, run-twice repeatability) apply to both.
- A SHA-256 hash is not a password hash. PATs/sessions are 160-bit random
  tokens, not user-chosen secrets, so a fast hash is correct here (there is no
  low-entropy input to brute-force); we deliberately did NOT reach for argon2.
  The leaked-token brute-force surface is blunted by the per-principal mutation
  rate-limit (C7), not by a slow hash.
- No password auth exists by design (PAT-only bootstrap, auth.md §11.1) — no
  reset/lockout surface to get wrong.
- This is the storage + crypto layer only. Wiring it into request handling
  (principal resolution, scope guards, per-principal data isolation, the
  admin-PAT bootstrap) is C3; the audit log is C4; the UI is C5; OAuth is C6.
