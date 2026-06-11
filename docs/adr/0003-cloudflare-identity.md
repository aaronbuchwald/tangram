# ADR-0003: Identity on Cloudflare — hand-rolled GitHub OAuth client + an accounts DO with hashed PATs

**Status:** accepted (2026-06-11)
**Deciders:** Aaron (owner), implementation by Claude
**Related:** [ADR-0002](0002-cloudflare-app-runtime.md) (CF is a full app
host), [RUNTIME_PLAN.md](../RUNTIME_PLAN.md) Phases 5 (host multi-tenancy,
the `Principal` seam) and 6 (this phase)

## Context

Phase 6 puts accounts on the Cloudflare worker: OAuth sign-in where
**account == tenant**, per-tenant namespaces under `/t/<tenant>/<app>/...`
(mirroring tangram-host's Phase 5 routes), and personal access tokens (PATs)
as the programmatic credential a laptop replica or MCP client presents as
`Authorization: Bearer`. The SDK's sync client already sends a bearer from
`TANGRAM_REMOTE_TOKEN` (built for the host's tenant mode) — the CF side must
accept it unchanged. The native host's static per-tenant token table is
explicitly **not** replaced this phase.

Hard requirements that shaped the choice:

- testable end-to-end under **miniflare** in CI, with no external network
  and no GitHub account — so the upstream IdP must be swappable for a stub;
- **immediate revocation** — a deleted PAT must 401 on the very next
  request, because that token is what an always-on replica holds;
- the Phase 5 **uniform-401** property (no tenant-existence oracle) carries
  over verbatim.

## Options considered

1. **`cloudflare/workers-oauth-provider`** — wrong role: it makes the worker
   an OAuth *authorization server* (for MCP clients implementing the OAuth
   MCP auth spec). What Phase 6 needs first is the worker as an OAuth
   *client* of GitHub plus its own token store. Adopting the library would
   add an AS surface (client registration, consent screens, token grants) we
   have no consumer for yet, and its GitHub-side flow still has to be
   written. Revisit if/when MCP clients want spec-compliant OAuth instead of
   bearer PATs.
2. **Cloudflare Access in front of the worker** — zero code, but identity
   ends up in CF zone configuration: not testable under miniflare, not
   self-contained in the repo, and no story for minting/revoking replica
   tokens (service tokens are account-level, not per-user).
3. **Hand-rolled authorization-code flow + an accounts Durable Object**
   (chosen) — the flow is three requests (redirect to authorize, code→token
   POST, user GET); the three upstream URLs are env-overridable
   (`OAUTH_{AUTHORIZE,TOKEN,USER}_URL`, defaulting to real GitHub), which is
   exactly the stub seam the e2e needs.

## Decision

Hand-rolled GitHub OAuth (option 3), with one new Durable Object class,
`TangramAccounts` (single instance, named `accounts`), as the identity
store. `cloud/cloudflare/src/auth.ts` holds the DO; `src/account.ts` the
flow and the account page routes; routing in `src/index.ts`.

**Data model** (DO storage, SQLite-backed):

| key | value | purpose |
|---|---|---|
| `ident:github:<id>` | tenant slug | IdP identity → account (sign-in lookup) |
| `tenant:<slug>` | account record (provider, login, created) | account == tenant; slug uniqueness |
| `session:<sha256(token)>` | { tenant, expiresAtMs } | browser sessions (30-day TTL) |
| `pat:<sha256(token)>` | { tenant, id, label } | O(1) bearer auth by token hash |
| `patindex:<tenant>:<id>` | { hash, label } | list/revoke without the token |

Tenant slugs come from the IdP login (lowercased `[a-z0-9-]`, never empty)
and are collision-safe: a second identity whose login slugs to an existing
tenant gets `-2`, `-3`, … . Re-sign-in of the same identity always lands on
the same tenant.

**Tokens are stored only as SHA-256 hashes**; the plaintext is shown exactly
once (the mint response / the Set-Cookie). PATs (`tgp_<40 hex>`, 160-bit
CSPRNG) live until revoked — they are replica credentials; sessions
(`tgs_…`, HttpOnly SameSite=Lax cookie) expire after 30 days.

**Per-request authorization**: every request under `/t/<tenant>/` makes one
RPC to the accounts DO — `authorize(tenant, bearer?, session?)` — before any
routing decision. PAT bearer and session cookie are both accepted (the
tenant UI's relative fetches ride the cookie; replicas/MCP/curl send the
bearer). All failures (unknown tenant, revoked/expired/foreign/missing
credential) collapse to `false` and one byte-identical 401, preserving
Phase 5's no-existence-oracle property. Tenant DO ids derive from
`t/<tenant>/<app>` — a disjoint keyspace from the single-user surface's
`<app>`, so tenant documents are isolated by construction and existing
deployments keep their data.

**CSRF**: the OAuth `state` round-trips through a short-lived
`SameSite=Lax` cookie; the account API (mint/revoke) is cookie-authed and
relies on `SameSite=Lax` blocking cross-site POST/DELETE.

## Consequences

- **Revocation is immediate by construction**: deleting `pat:<hash>` *is*
  the revocation; there is no cache between the accounts DO and the router.
  The cost is one DO round trip per tenant request and a global serialization
  point (one accounts DO instance). Fine at this fleet's scale; if it ever
  shows up in latency, shard by tenant name or add a short worker-memory
  cache *with a revocation generation check* — never a plain TTL cache.
- The single-user surface (`/<app>/...`) stays open and byte-compatible —
  same caveat as before (don't host secrets there); `t`, `auth`, and
  `account` are now reserved top-level path segments (no app may take those
  names on CF).
- The per-tenant app set is the worker's bundled `APPS` (notes/nutrition).
  A per-tenant registry-on-CF (installing apps per account) is **out of
  scope** — the registry app is a tangram-host concept for now.
- GitHub is the only IdP; adding another is a second `ident:<provider>:`
  prefix plus its flow URLs, not a redesign. The stub IdP in
  `scripts/e2e-cloudflare-identity.sh` pins the flow's contract.
- The worker holds GitHub client credentials as Worker **secrets**
  (`GITHUB_CLIENT_ID`/`GITHUB_CLIENT_SECRET`); unset, `/auth/login` answers
  503 and only the open single-user surface works.
- The native host's Phase 5 token table is untouched; unifying host + CF
  identity (host validating CF-minted PATs, device flow) is follow-up work,
  noted in RUNTIME_PLAN.
