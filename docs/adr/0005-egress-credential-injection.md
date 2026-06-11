# ADR-0005: Egress credential injection (components never hold plaintext secrets)

**Status:** accepted (2026-06-11)
**Deciders:** Aaron (owner), with analysis by Claude
**Related:** ADR-0004 (the secret-resolver seam — the *provenance* axis; this
ADR is the orthogonal *exposure* axis); RUNTIME_PLAN Phase 10b; the
tenant-isolation review (`docs/security/tenant-isolation-review.md`)

## Context

Today the host resolves a secret (e.g. `CALORIENINJAS_API_KEY`) and injects
its **value** into the component as an environment variable. The component
reads it and builds the authenticated request itself (sets `X-Api-Key`),
which it hands to the host's `http-fetch` capability. So the plaintext key
lives in the tenant component's own linear memory.

That is the weakest link in the isolation story, and it is independent of how
the secret is *resolved* (ADR-0004). It matters on two axes:
- **Leak surface inside the sandbox**: a buggy or malicious component can
  exfiltrate a key it holds (to any allowlisted host), and the secret sitting
  in the tenant's address space is the prerequisite for any
  microarchitectural side-channel against it (see the tenant-isolation
  review). Removing the secret from the component's memory is the
  highest-leverage mitigation, independent of process/VM isolation.
- **Multi-tenant / third-party future**: untrusted apps must never receive
  raw credentials.

## Decision

Move credential application to the **host's `http-fetch` egress boundary**.
The component makes an *unauthenticated* request to an allowlisted host; the
host attaches the credential just before performing the real outbound call,
resolving the value via the ADR-0004 resolver. The component never receives
the plaintext.

The app **spec** (not the component) declares the injection — e.g.:
```toml
[apps.nutrition.inject]
"api.calorieninjas.com" = { header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }
```
The host: matches an outbound `http-fetch` against the injection rules for
that app, resolves `secret` to a `SecretString`, attaches it
(header/query/bearer), and forwards. Misses pass through unmodified.

## Consequences

- **App code changes** (this is real, not free): nutrition stops reading the
  key from env and stops setting `X-Api-Key`; its strategy just fetches the
  URL and the host authenticates it. The capabilities probe still reports
  whether the strategy is *configured* (now derived from whether an injection
  rule + resolvable secret exist), so the UI behavior is preserved.
- The WIT `host` interface is unchanged (`http-fetch` stays); the injection is
  host-side config + logic. The component genuinely cannot obtain the secret
  through any in-sandbox path — not env, not the fetch it issues.
- **Scope/limit**: this protects secrets used at the HTTP egress boundary —
  which is the dominant case (API keys). A secret a component must *compute
  on* internally (rare) inherently can't be hidden from code that operates on
  it; such cases fall back to env injection (ADR-0004) and are documented as
  carrying the in-sandbox exposure.
- Sequenced after ADR-0004 (Phase 10b after 10a): injection needs the
  resolver to fetch the value host-side.
- Combined with ADR-0004's `age://` resolver, a federated secret is then
  *decrypted host-side and applied at egress* — it exists in plaintext only
  in the host, only for the duration of one outbound request, never in the
  replicated document, the relay, or the tenant component.
