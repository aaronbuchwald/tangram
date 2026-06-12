# ADR-0008: Egress call-level capabilities (credential bound to the declared call, not the host)

**Status:** accepted (2026-06-12)
**Deciders:** Aaron (owner), with analysis by Claude
**Related:** ADR-0005 (egress credential injection — the *exposure* axis this
extends from `(tenant, host)` to `(tenant, host, method, path, shape)`),
ADR-0004 (the secret-resolver seam), ADR-0006 (tenant isolation tiers — this is
a prerequisite for the untrusted-tier marketplace),
[`docs/design/fine-grained-egress.md`](../design/fine-grained-egress.md) (the
full research + design + the checkpointed build plan this implements),
[`docs/design/manifest-verification-plan.md`](../design/manifest-verification-plan.md)
§2.6 (the shared canonicalization seam),
`crates/tangram-host/src/{egress.rs,config.rs,runtime.rs,app.rs}`.

## Context

ADR-0005 moved credential application to the host's `http-fetch` egress
boundary so the component never holds the plaintext — but it left the
credential scoped to a **host**, not a **call**. That is the footgun
Anthropic hit in production (fine-grained-egress §2): *an allowlisted host is
not an allowlisted call.* A single host commonly serves both a legitimate
endpoint and an exfiltration path (`GET /v1/me/data` vs
`POST /v1/accounts/{other}/import` on the same `api.vendor.com`), and a
host-keyed credential is attached to *both*. The tenant-isolation review (Q4)
asked for the binding to go one grain finer than `(tenant, host)`.

The sharpest implementation hazard is the **parser-differential class** (the
SOCKS5 `attacker.com\0.good.com` bypass): the policy layer and the resolver
disagreeing on what a host/path *means*. The lesson: canonicalize once at a
single seam, and match on parsed/normalized components — never string-suffix
checks, regex, or value-matching.

## Decision

Extend the host-keyed inject rule into a **declared call**: a
method + host + path-pattern (+ optional name-level query/header constraints
and a constrained JSON-RPC-method body rung), with the credential injection
attached *to that call*. The grant becomes "make exactly these calls," not
"reach this host."

- **No WIT change.** The component still calls one bare
  `http-fetch(request-json) -> result<string,string>` and never names a
  credential. All enforcement is host-side in `HostState::http_fetch`. The
  component cannot observe which call matched, read the secret, or widen its
  grant (ADR-0005's invariant preserved).
- **A single canonicalization seam** (`egress::CanonicalRequest::from_request`):
  the request is canonicalized ONCE before any matching — method upper-cased;
  URL parsed to `(host, path, query)`; host lowercased + trailing-dot stripped +
  null-byte refused; path percent-decoded + dot-segment normalized; query and
  header NAMES only (never values). The host fence and the call match run
  against the same value, so they can never disagree. This is the same seam the
  manifest verifier's call-grain arm will consume (manifest-verification-plan
  §2.6).
- **Small, regex-free grammar** (`egress::CallSpec`): exact/`*` method;
  exact / RFC-6570 template (`/v1/items/{id}`) / `/**` subtree path; name-level
  query/header `required`/`forbidden`; the body rung is a FIXED JSON-pointer +
  literal-set membership (`body = { json_method = [...] }`) and nothing more —
  no operators, no value-matching on arbitrary fields. The body is parsed only
  when a call declares the matcher, and only up to `max_body_bytes`.
- **The host fence composes, never bypassed.** `host ∉ allow_hosts` → deny
  stays the cheap first gate; the first-matching declared call is the inner
  authoritative gate. Each call's host must also be in `allow_hosts`.
- **Inject on the matched call only**; a matched call with no inject goes out
  un-credentialed (a declared public call). Auth-bearing response headers
  (`authorization`, `www-authenticate`, `set-cookie`, …) are stripped before
  the body is handed back, closing ADR-0005's "API echoes the key" caveat.
- **Three enforcement modes** (`observe` → `warn` → `enforce`): observe never
  denies and logs a candidate; warn allows undeclared calls but loudly warns;
  enforce denies undeclared calls with a precise error naming the declared
  calls for that host (the §2 Anthropic deterministic boundary).
- **`describe()`-carried declarations are a REQUEST, not a grant**
  (fine-grained-egress §6): the host intersects the component's declared calls
  with the operator spec — a component declaring more than its spec is narrowed
  to the spec; it can never widen. The credential always stays on the operator
  call.

### Strictly additive / behavior-identical by default

A host-keyed `[apps.X.inject]` or a bare `allow_hosts` host **desugars** to the
maximally-broad implicit call `{ method = *, path = /** }` carrying that host's
inject. Existing `apps.toml` (e.g. nutrition's host-keyed inject) behaves
byte-identically. Migration default modes (§7.2): `warn` for a legacy app
declaring no `[[calls]]` (never a surprise prod denial), `enforce` for an app
that has opted in by declaring ≥1 call.

## Consequences

- **Closes the same-host different-call exfil class** ADR-0005 left open: the
  injected credential is attached only to declared calls; an undeclared request
  on an allowlisted host is denied (and un-credentialed) before it leaves the
  host. Tightens `(tenant, host)` to `(tenant, host, method, path, shape)`.
- **Prerequisite for the untrusted-tier marketplace** (ADR-0006): operator
  `[[calls]]` is authoritative; the component declaration can only narrow it.
- **DevX cost is real and is the crux** (fine-grained-egress §5): fine-grained
  is safer but more verbose and brittle when an upstream renames a path. The
  mitigations are the three modes (brittleness is a dev-time warning, not a 3am
  outage) and the observe-mode generator + `Call` authoring helper (write the
  fetch, run once, accept the generated declaration).
- **Honest residual** (fine-grained-egress §8): call-level scoping shrinks the
  egress surface to the declared calls; it cannot make a *declared* write-call
  safe against being used to smuggle data the app already could send there.
  That is the model-layer / human-review tier, orthogonal to this deterministic
  boundary. Microarchitectural side-channels remain ADR-0006's separate axis.
- **The general (imperative) policy engine is explicitly OUT** (§9.2): the v1
  contract stays declarative and regex-free. The policy-engine variant is a
  later, separately-reviewed branch — never the default grammar.
