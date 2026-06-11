# Design: Fine-grained egress — from host allowlists to call-level capabilities

**Status:** design / proposal (no production code). Read-only research +
design.
**Date:** 2026-06-11
**Author:** Aaron (owner), with research + design by Claude
**Related:** ADR-0005 (egress credential injection — the *exposure* axis this
extends), ADR-0004 (the secret-resolver seam), ADR-0006 (tenant isolation
tiers), `docs/security/tenant-isolation-review.md`, `wit/tangram.wit`,
`crates/tangram-host/src/{runtime.rs,config.rs}`, RUNTIME_PLAN Phase 10b.

---

## 1. Problem

Tangram's outbound network capability is a **host (domain) allowlist** plus
**per-host credential injection**. An app spec lists `allow_hosts` (exact host
names), the host's `http-fetch` (`runtime.rs::http_fetch`) denies any outbound
request whose URL host is not on that list, and a matching `[apps.<app>.inject]`
rule (`config.rs::InjectRule`) attaches a credential — header / bearer / query —
at the egress boundary so the component never holds the plaintext (ADR-0005).

The grain is the **host**. That is the footgun. *An allowlisted host is not an
allowlisted call.* A single host commonly serves both a legitimate endpoint and
an exfiltration path:

- **Same-host, different-account exfil.** A multi-tenant SaaS API at
  `api.vendor.com` exposes both `GET /v1/me/data` (the app's intended read) and
  `POST /v1/accounts/{other}/import`. The credential Tangram injects on every
  request to `api.vendor.com` is, by construction, attached to *both*. A
  compromised or buggy component can replay the host-injected credential against
  a different endpoint/account on the same host — exfiltrating a victim's data
  to an attacker-controlled destination *inside the allowlisted domain*.
- **Same-host, different-function.** `api.github.com` is one host; `GET` of one
  repo and `POST` of a comment to any repo are different calls. Allowlisting the
  host grants both.

This is not hypothetical; it is the exact class Anthropic hit in production (§2).
ADR-0005 closed the *secret-in-component-memory* axis but explicitly left the
credential **scoped to a host, not a call** — the tenant-isolation review
(`Q4`, "Caveats worth noting") flagged it directly:

> "ADR-0005 should bind *which secret* is injected to *which (tenant, host)*
> pair … the host should avoid handing back auth-bearing response headers."

We want to go one grain finer than `(tenant, host)`: bind the injected
credential to **(tenant, host, method, path, request-shape)** — the exact
declared call — so a credential is usable *only* for the requests the app
declared, and an undeclared request on an allowlisted host is denied (and
un-credentialed) before it leaves the host.

---

## 2. How Anthropic handled it (cited)

Anthropic ships agents (Claude Code, Claude Cowork/"Cowork", managed agents)
that run model-driven, potentially-untrusted tool calls, and they converged on
**deterministic environmental boundaries** — sandboxes + an **egress proxy with
a domain allowlist** — as the primary defense, with the model layer second:

> "The deterministic boundary is what gets hit when everything probabilistic
> misses."
> — *How we contain Claude across products*, Anthropic Engineering.

Their network isolation routes all traffic through a **proxy running outside
the sandbox** that enforces a per-session domain allowlist (nothing pre-allowed
by default), with user confirmation for new domains
(*Making Claude Code more secure and autonomous with sandboxing*).

**The acknowledged gap is exactly Tangram's footgun.** A disclosed Claude Cowork
incident: a malicious file in the workspace carried hidden instructions plus an
**attacker-controlled API key**; Claude followed them, read other files, and
called Anthropic's own Files API using the attacker's key. The egress proxy
checked the destination, saw `api.anthropic.com` was on the allowlist, and let
it through — the data was uploaded to the *attacker's* account. The destination
was legitimate; the *call* was not. Anthropic's own framing of the lesson:

> "Every function reachable through any domain on an allowlist is now an attack
> surface."
> — *How we contain Claude across products*, Anthropic Engineering.

Their mitigation is instructive and directly parallel to Tangram's host-side
injection: a **defensive proxy inside the VM that intercepts traffic to their
API and validates the authentication token**, rejecting attacker-embedded
credentials. That is: move from "is the *destination* allowed?" to "is *this
specific authenticated call* allowed?" — credential validation at the egress
boundary, not just destination filtering. They also report a recurring
meta-lesson: **battle-tested primitives** (hypervisors, seccomp, gVisor) held;
their own **custom glue** around those primitives is where bugs appeared (e.g.
a SOCKS5 hostname null-byte *parser differential* — `attacker.com\x00.google.com`
passed an `endsWith(".google.com")` filter but resolved to `attacker.com` —
where the policy layer and the resolver disagreed on what a hostname string
meant). The takeaway for our design: **canonicalize once at the seam**, and
prefer matching on parsed, normalized request components over string suffix
checks.

**Accurate statement of the gap they acknowledge:** a domain allowlist is a
*destination* filter; it cannot distinguish a benign call from a malicious one
to the *same* destination, and any function reachable on an allowlisted domain
is in scope. They did not (publicly) move to a general method+path capability
grammar for arbitrary user domains — they hard-coded a credential-validating
proxy for their *own* API. Tangram, owning both the host and the app contract,
can generalize that into a declarative call-level capability.

---

## 3. Prior art: what scopes a credential to specific calls

Five families, roughly weakest→strongest binding of "credential ⇒ exact call":

**(a) L7 egress proxies / firewalls — match method + path + headers, not just
host.** AWS Network Firewall's proxy, with TLS interception, inspects HTTP-layer
content and applies fine-grained rules on **HTTP method and URL path** (and can
combine multiple match conditions per rule). Google Cloud **Secure Web Proxy**
enforces granular egress policy by "source, identities, destination, or request
types." These prove the *enforcement mechanism* (parse the request, match
beyond host) but are infrastructure-level and don't bind a *credential* to the
matched call — they allow/deny.

**(b) Service-mesh L7 authorization — method/path policy as data.** Istio
`AuthorizationPolicy` allows/denies on properties of the request itself —
**HTTP methods and URL paths** (`to.operation.methods`, `paths`), plus source
identity. Linkerd shapes egress with Gateway-API `HTTPRoute` matchers. This is
the closest *declarative* analog: a small rule grammar (method × path × maybe
headers) evaluated at a sidecar. Pattern to borrow: **the allowed call is a
data record, not code.**

**(c) Permission manifests co-located with the app.** Browser extensions
(Chrome/Firefox `manifest.json` `host_permissions` / match patterns) and
**Atlassian Forge** (`permissions.external.fetch.backend` in `manifest.yml`)
declare outbound reach in a manifest shipped *with* the app. Notably Forge is
**host-grained, not path-grained** — "Adding one domain allows access to any URL
on that domain" — so it has *exactly the limitation we are trying to fix*, and
is a useful negative example: a manifest alone doesn't buy you call-level
scoping unless its grammar reaches method+path. Emerging research on *permission
manifests for web agents* pushes manifests to cover "interactions with specific
resources and general actions" — resource-scoped, not just origin-scoped.

**(d) OAuth scopes / token exchange — bind a token to operations.** OAuth 2.0
scopes constrain a credential to declared operations (Forge's `scopes` list maps
to OAuth scopes for authenticated fetch); RFC 8693 **token exchange** mints a
narrowed token for a downstream call. This is the gold standard *when the
upstream API cooperates* — the credential is cryptographically bound to a
sub-operation by the issuer. Limitation: most third-party API keys (a single
bearer/`X-Api-Key`) carry no per-call scoping, so the *host* must enforce the
scope the token itself doesn't.

**(e) Capability-based security — unforgeable token ⇒ specific operation on a
specific resource.** The conceptual frame: a capability names *operation +
object*, not "reach this network destination." Tangram's `http-fetch` grant is
already a capability; today it is parameterized only by host. The move is to
parameterize it by the **exact call**.

**Synthesis for Tangram:** combine (c) a manifest co-located with the app that
reaches method+path (fixing Forge's host-only grain), enforced like (b) a small
declarative match grammar at the egress boundary, applied so that (e) the
injected credential is bound to the matched capability — and, where the upstream
supports it, layered with (d) real scoped tokens. Tangram is uniquely positioned
because it owns *both* sides of the boundary (the host runtime *and* the app
contract), so it can make the declaration ergonomic and the enforcement total.

---

## 4. Proposed call-level model

Extend ADR-0005's host-keyed inject rules into **call-level capability
declarations**. The unit of grant becomes a **declared call**: a
method + host + path-pattern (+ optional request-shape constraints), with the
credential injection attached *to that call*, not to the host.

### 4.1 Config / manifest shape (extends `[apps.<app>.inject]`)

Today (host-grained; `config.rs::InjectRule`, keyed by host):

```toml
[apps.nutrition]
allow_hosts = ["api.calorieninjas.com"]

[apps.nutrition.inject]
"api.calorieninjas.com" = { header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }
```

Proposed (call-grained; an array of declared calls per app). The credential
moves *inside* the call:

```toml
[apps.nutrition]
# allow_hosts stays as the coarse outer fence (host-level deny is cheap and
# is the first gate); calls are the inner, authoritative gate.
allow_hosts = ["api.calorieninjas.com"]

[[apps.nutrition.calls]]
method = "GET"
host   = "api.calorieninjas.com"
path   = "/v1/nutrition"            # exact, or a template: "/v1/items/{id}"
# inject moves onto the call: the credential is attached ONLY to THIS call.
inject = { header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }
# optional request-shape constraints (all checked host-side before sending):
query   = { required = ["query"], forbidden = ["callback"] }
max_body_bytes = 0                  # GET: forbid a body entirely
```

A richer example showing the multi-tenant-API hazard being closed:

```toml
[[apps.crm.calls]]
method = "GET"
host   = "api.vendor.com"
path   = "/v1/me/contacts"          # the app may READ its own contacts
inject = { bearer = true, secret = "age://vendor-token" }

[[apps.crm.calls]]
method = "POST"
host   = "api.vendor.com"
path   = "/v1/me/contacts"          # …and WRITE its own
inject = { bearer = true, secret = "age://vendor-token" }
headers = { required = { "content-type" = "application/json" } }
# NOTE: there is no declared call for POST /v1/accounts/*/import, so the
# host both DENIES that request and never injects the token onto it — the
# exfil path on the same allowlisted host is closed.
```

Match grammar (kept deliberately small — the §3(b) lesson):

- `method` — exact, case-insensitive (`GET`/`POST`/…); `"*"` allowed but
  discouraged (dev-mode warns).
- `host` — exact host, same space as `allow_hosts` (must also be allowlisted;
  the host fence composes, never bypassed — preserves ADR-0005's invariant).
- `path` — either an exact path, or an **RFC 6570-style template** with named
  segments (`/v1/items/{id}`) matching exactly one non-`/` segment; an explicit
  trailing `/**` may match a subtree (warned in dev as broadening). No regex —
  templates are predictable and canonicalizable.
- `query` — optional `{ required = [...], forbidden = [...] }` on parameter
  *names* (never values; values may carry data and matching on them invites the
  parser-differential class).
- `headers` — optional `{ required = { name = value | "*" } , forbidden = [...] }`.
- `max_body_bytes` — optional cap; `0` forbids a body.
- `inject` — the existing `InjectRule` (header / bearer / query + `secret`),
  now **scoped to this call**.

Multiple `calls` entries may target the same host; the host picks the
**first matching** declared call (declaration order is the precedence, like a
firewall rule list — explicit and auditable).

### 4.2 How it composes with the WIT `http-fetch` capability

**No WIT change.** This is the deliberate, load-bearing property — identical to
ADR-0005. The component still calls one host function:

```wit
http-fetch: func(request-json: string) -> result<string, string>;
```

It issues a **bare** request (`{method,url,headers,body-b64}`) and never names a
credential. Enforcement is entirely host-side, inside `HostState::http_fetch`
(`runtime.rs`). The component cannot observe which call matched, cannot read the
secret, and cannot widen its own grant — the grant lives in the spec, outside
the sandbox. (See §6 for the wasm-boundary argument.)

New `http_fetch` control flow (replacing the host-only match at
`runtime.rs:104-111`):

1. Parse + **canonicalize** the request once: method (upper), URL → parsed
   `(host, path, query)`, headers (lowercased names). Canonicalize *before*
   any matching — the single seam, per the §2 parser-differential lesson.
2. **Host fence (unchanged):** if `host ∉ allow_hosts` → deny (cheap first gate;
   keeps today's clear error).
3. **Call match:** find the first `calls` entry whose
   `method ∧ host ∧ path-template ∧ query ∧ headers ∧ body` constraints all
   match the canonicalized request. No match → **deny** with a precise error
   ("no declared call matches POST api.vendor.com/v1/accounts/9/import; declared
   calls for this host: GET /v1/me/contacts, POST /v1/me/contacts").
4. **Inject on the matched call only:** resolve that call's `inject.secret`
   host-side and attach it (header/bearer/query), exactly as today but keyed by
   the matched *call*, not the host. A call with no `inject` goes out
   un-credentialed (still allowed — declared public calls are fine).
5. Send; on the response, **strip auth-bearing headers** before handing the
   body back to the component (closes the ADR-0005 review's "API echoes the key"
   caveat — relevant now that the host owns the credential per-call).

Backward-compatible degrade: a host-level `inject` rule (today's shape) is
treated as a single implicit `calls` entry of `{ method="*", path="/**" }` for
that host — i.e. **today's behavior is the maximally-broad call** (see §5).

### 4.3 Internal representation

`AppSpec.inject: BTreeMap<String, InjectRule>` (host→rule) becomes
`AppSpec.calls: Vec<CallSpec>` where:

```rust
struct CallSpec {
    method: MethodMatch,          // Exact(Method) | Any
    host: String,                 // lowercased; must be in allow_hosts
    path: PathPattern,            // Exact(String) | Template(Vec<Seg>) | Subtree
    query: QueryConstraint,       // required/forbidden names
    headers: HeaderConstraint,    // required (name→value|*) / forbidden names
    max_body_bytes: Option<usize>,
    inject: Option<InjectRule>,   // reuse the existing rule + InjectKind
}
```

`validate_inject` (config.rs:305) generalizes to `validate_calls`: each call's
host must be in `allow_hosts`; each `inject` validated by the existing
`InjectRule::kind()`; path templates parsed once. `resolved_inject`
(config.rs:324) becomes `resolved_calls`. The runtime carries
`Vec<(CallMatcher, Option<(InjectKind, InjectRule)>)>` instead of
`Vec<(host, InjectKind, InjectRule)>`. The tenant `allow_hosts_ceiling`
intersection (config.rs:393) is unchanged — calls whose host falls outside the
ceiling are dropped, same as hosts today.

---

## 5. DevX / ergonomics — the crux, and the recommendation

The tension is real and must be named: **fine-grained = safer but more verbose
and more brittle when the upstream API changes.** A host allowlist is one line
and rarely changes; a call list is N entries and breaks when the vendor renames
a path. If declaring capabilities is tedious, people will write `path = "/**"`
(or skip the feature), and we are back to host-grained. The owner's goal —
*"trivially easy to vibe-code a WASM component and declare exactly the
capabilities you want"* — means the ergonomics decide whether the security
property is actually realized.

Design principles to hit both explicitness *and* DevX:

1. **Co-locate the declaration with the app, in the app's own source.** The
   most ergonomic place to declare "the calls I make" is *next to the code that
   makes them*. Because a Tangram app is the source of truth for its actions
   (the `#[actions]` macro already enumerates them) and already emits a
   `describe()` JSON over WIT (`guest.describe`, carrying an optional
   `capabilities` object), we can add a **declared-calls section to the
   component's `describe()` output** — an app declares its calls in Rust (e.g.
   a `const CALLS` / attribute on async actions) and they ride out in
   `describe()`. The host reads them at instantiation. *The manifest is then
   generated from code the developer already writes*, not hand-maintained TOML.

2. **Generate / infer, don't hand-write.** Two complementary lanes:
   - **Inference (dev mode):** run the app's integration smoke tests (or a
     record session) with the host in **observe mode**; the host logs every
     bare `http-fetch` as a *candidate* declared call (canonicalized method +
     host + path template, parameterizing numeric/uuid segments into `{id}`).
     `tangram dev` then prints a paste-ready `[[calls]]` block — vibe-code the
     fetch, run once, accept the generated declaration. This is the killer DevX
     path and the recommendation's backbone.
   - **Authoring helper:** a tiny declaration DSL in Rust so the fetch *and* its
     capability are written together and can't drift:
     ```rust
     ctx.call(Call::get("api.calorieninjas.com", "/v1/nutrition")
         .query_required(["query"]))
        .await?;            // host already knows this call is declared
     ```
     The macro emits the `CallSpec` into `describe()` *and* issues the fetch —
     one source, no separate file to forget.

3. **Sensible defaults that fail safe but not silently.** Default posture:
   `allow_hosts` empty ⇒ no egress (today's default — keep). A host in
   `allow_hosts` with **no** matching call ⇒ deny in prod. But a bare host with
   no `calls` at all (legacy shape) is treated as the broad implicit call
   (§4.2) so **nothing existing breaks on upgrade** — see §6 migration.

4. **Dev-mode warnings vs prod enforcement (the brittleness lever).** A single
   host-level mode toggle:
   - **`enforcement = "observe"` (dev default):** never deny; *log* every
     undeclared/over-broad call as a warning ("would deny in prod: POST
     api.vendor.com/v1/accounts/9/import"), and surface the suggested
     `[[calls]]` to add. Vibe-code freely; see exactly what to declare.
   - **`enforcement = "warn"`:** inject on declared calls, allow undeclared
     calls but loudly warn (migration aid).
   - **`enforcement = "enforce"` (prod default):** deny undeclared calls; inject
     only on matched calls. This is the §2 Anthropic posture — the deterministic
     boundary.
   Brittleness is then a *dev-time* signal (a failing smoke test / a warning),
   not a 3am prod outage with no diagnosis: when the vendor renames a path the
   warn-mode log names the exact call to update.

5. **Keep the grammar small and canonical** (§3(b), §4.1): method + path
   template + name-level query/header constraints. No value matching, no regex.
   Predictable to read, predictable to canonicalize, hard to footgun.

**Recommendation.** Adopt call-level capabilities with the credential bound to
the matched call, and make the declaration **(i) co-located in the app's source
and carried out via `describe()`** (no separate hand-maintained manifest as the
primary path), **(ii) generated by an observe-mode `tangram dev` pass** that
turns the fetches you actually make into paste-ready declarations, and **(iii)
enforced by a three-state mode** (`observe` → `warn` → `enforce`) so dev is
frictionless and prod is strict. The TOML `[[calls]]` shape (§4.1) remains the
explicit override / operator-facing form (and what the registry replicates), but
the *expected* authoring path is "write the fetch, run once, accept the
generated capability." This maximizes explicitness (the grant is exact and
auditable) *and* DevX (you almost never write it by hand), and it puts the
brittleness where it's cheap — a dev-time warning, not a prod incident.

---

## 6. Wasm-boundary interaction

The property that makes this safe is identical to ADR-0005 and worth stating
precisely:

- The **grant lives outside the sandbox** (spec / generated-from-`describe()`,
  held in `HostState`), and the **secret is resolved host-side** at request
  time. A component cannot read, widen, or forge its call list, and cannot read
  the credential — the WIT world (`http-fetch`, `log`, `now-ms`; empty WASI ctx,
  no fs/sockets/inbound-HTTP) gives it no path to any of those.
- `describe()`-carried declarations are a **request, not a grant**: the host
  treats the component's declared calls as an *upper bound to be intersected
  with the operator's spec*, never as authority on their own. (Same posture as
  registry specs being blocked from host-env expansion — `tenant.rs`.) For a
  fully-trusted first-party app the spec can simply *be* the generated calls;
  for an untrusted tenant the operator's `[[calls]]` is authoritative and the
  component's declaration can only narrow it. This keeps Tier-3 (ADR-0006)
  honest: an untrusted component cannot grant itself a call.
- Enforcement is **before the bytes leave the host process**, in
  `HostState::http_fetch`, so it holds regardless of in-process vs
  process-per-tenant isolation (ADR-0006) — it is an egress-boundary control,
  orthogonal to the microarchitectural tier.

---

## 7. Migration from the current domain allowlist

Strictly additive; no break on upgrade.

1. **Compat shim:** a host-keyed `[apps.<app>.inject]` rule (today's shape) is
   parsed into one implicit `CallSpec { method=Any, host, path=Subtree,
   inject }`. A host in `allow_hosts` with no inject and no calls remains
   "allowed, un-credentialed, any path." **Existing `apps.toml` files behave
   identically** — the new model is a strict generalization where today's config
   is the maximally-broad call.
2. **Default mode by build profile:** `observe` in dev, `enforce` in prod —
   but to avoid surprising existing prod deployments, the *initial* release
   defaults prod to `warn` for apps that declare no `calls` (legacy), and to
   `enforce` for apps that opt in by declaring at least one `[[calls]]`. An
   app declaring calls has signaled intent; a legacy app keeps working with a
   migration warning naming each call it should declare.
3. **Generate the migration:** ship the observe-mode pass (§5.2) so an operator
   runs the app once and gets a paste-ready `[[calls]]` block replacing the old
   host-keyed `inject`. The registry (`apps/registry`) replicates the new
   `calls` array the same way it replicates specs today.
4. **Deprecate slowly:** keep the compat shim indefinitely for `allow_hosts`
   without calls (it's a legitimate "public, un-credentialed host" grant); only
   *credential injection* migrates from host-keyed to call-keyed, with the
   host-keyed form warned as deprecated once tooling lands.

Sequence: ship the grammar + enforcement modes + compat shim first (behavior
identical by default), then the `describe()` declaration channel + macro, then
the observe-mode generator. Each step is independently shippable.

---

## 8. Security analysis

### What it closes

- **Same-host different-call exfil (the core threat).** The injected credential
  is attached *only* to declared calls. A compromised component issuing
  `POST api.vendor.com/v1/accounts/{victim}/import` matches **no** declared call
  → denied, and even if it targeted a *declared* host it gets **no credential**
  for the undeclared call. This is precisely the
  "every function on an allowlisted domain is an attack surface" class (§2),
  reduced from "every function on the host" to "exactly the declared calls."
- **Credential replay to a sibling endpoint.** The token for
  `GET /v1/me/contacts` is not attached to `POST /v1/accounts/*/import` even on
  the same host — closing the multi-tenant-API replay the prompt names.
- **Response credential echo (ADR-0005 review caveat).** §4.2 step 5 strips
  auth-bearing response headers before handing the body to the component.
- **Tightens the `(tenant, host)` binding** the tenant-isolation review asked
  for (Q4) into `(tenant, host, method, path, shape)`.

### What it does NOT close (residual risk vs ADR-0006 tiers)

- **Exfil *within* a declared call.** If the app legitimately declares
  `POST api.vendor.com/v1/me/contacts`, a compromised component can still put
  *exfiltrated data in that request body* to that legitimate endpoint. Call-level
  scoping shrinks the egress surface to the declared calls; it cannot make a
  declared write-call safe against being used to smuggle data the app already
  could send there. Mitigations are out of band: `max_body_bytes`, content
  constraints, or the model-layer / human-review tier (Anthropic's "probabilistic"
  layer). **This is the honest residual** — same shape as Anthropic's: the
  deterministic boundary narrows the surface, it does not read intent.
- **Microarchitectural side channels (ADR-0006, layer b).** Unchanged and
  orthogonal — this is an egress-content control, not a co-residency control.
  ADR-0006's tiering (SMT off, resource limits, process/core separation for
  untrusted tenants) still governs that axis.
- **Upstream API lacks per-call scoping.** When the credential is a single broad
  API key, the *host* enforces the call scope the token itself can't. If the
  token is stolen *out of the host* (a host-process compromise), per-call
  declarations don't help — but that is the privileged-broker threat ADR-0005
  already scoped, far smaller than an untrusted peer holding the key.
- **Parser-differential bypass (§2 SOCKS5 lesson).** Mitigated *by design*
  (canonicalize once, match on parsed components, no value/regex matching) but
  is the implementation's sharpest footgun — it must be tested adversarially
  (mixed-case host, `%2e`/`.`-encoded path, trailing-dot host, `..` segments,
  duplicate query keys). Reuse the canonicalization seam for *both* the host
  fence and the call match so the two layers can never disagree.

### Net posture by ADR-0006 tier

| Tier | Today (host allowlist + host-keyed inject) | With call-level capabilities |
|---|---|---|
| 1 — first-party | credential usable on any path of an allowed host | credential bound to declared calls; same-host exfil closed; near-zero authoring cost via generation |
| 2 — semi-trusted | tenant can replay its own injected credential to any endpoint on an allowed host | replay to undeclared endpoints denied; combine with ADR-0005 (no plaintext in component) |
| 3 — untrusted 3rd-party | host allowlist far too coarse to grant a marketplace app | call-level is a **prerequisite** for the marketplace: operator-authoritative `[[calls]]`, component declaration can only narrow; pairs with the hardware/co-residency controls ADR-0006 still mandates |

The change converts the egress grant from "reach this host" to "make exactly
these calls," closing the same-host exfil class Anthropic hit while ADR-0006's
co-residency controls remain the separate, orthogonal answer for the
microarchitectural axis.

---

## Sources

- Anthropic Engineering — *How we contain Claude across products* (deterministic
  boundary; "every function reachable through any domain on an allowlist is now
  an attack surface"; the api.anthropic.com Cowork incident; in-VM
  credential-validating proxy; battle-tested-primitives lesson):
  https://www.anthropic.com/engineering/how-we-contain-claude
- Anthropic Engineering — *Making Claude Code more secure and autonomous with
  sandboxing* (out-of-sandbox proxy enforcing domain allowlist; nothing
  pre-allowed): https://www.anthropic.com/engineering/claude-code-sandboxing
- anthropic-experimental/sandbox-runtime (HTTP + SOCKS5 proxy domain
  allow/deny; five threat classes incl. cross-tenant leakage; "allowing
  github.com lets a process push to any repository"):
  https://github.com/anthropic-experimental/sandbox-runtime
- *Second Time, Same Sandbox* — SOCKS5 hostname null-byte parser-differential
  bypass (`endsWith(".google.com")` vs libc truncation):
  https://oddguan.com/blog/second-time-same-sandbox-anthropic-claude-code-network-allowlist-bypass-data-exfiltration/
- AWS — *Securing Egress Architectures with Network Firewall Proxy* (L7
  inspection of HTTP method + URL path; multiple match conditions per rule):
  https://aws.amazon.com/blogs/networking-and-content-delivery/securing-egress-architectures-with-network-firewall-proxy/
- Google Cloud — *Introducing Secure Web Proxy* (granular egress by source,
  identities, destination, request types):
  https://cloud.google.com/blog/products/identity-security/introducing-secure-web-proxy-for-egress-traffic-protection
- Istio — *Authorization Policy* (allow/deny on HTTP methods, paths, headers,
  source identity): https://istio.io/latest/docs/reference/config/security/authorization-policy/
- Linkerd — *Managing egress traffic* (Gateway-API HTTPRoute matchers):
  https://linkerd.io/2-edge/tasks/managing-egress-traffic/
- Atlassian Forge — *Permissions* manifest reference
  (`permissions.external.fetch.backend`; host-grained: "adding one domain allows
  access to any URL on that domain" — the limitation we fix):
  https://developer.atlassian.com/platform/forge/manifest-reference/permissions/
- Chrome Extensions — *Declare permissions* (host_permissions / match patterns):
  https://developer.chrome.com/docs/extensions/develop/concepts/declare-permissions
- IETF RFC 8693 — OAuth 2.0 Token Exchange (minting narrowed downstream tokens):
  https://datatracker.ietf.org/doc/html/rfc8693
- *Permission Manifests for Web Agents* (resource- and action-scoped manifests
  for autonomous agents):
  https://www.researchgate.net/publication/399521931_Permission_Manifests_for_Web_Agents
- *How to Implement Capability-Based Security* (unforgeable token ⇒ specific
  operation on a specific resource):
  https://oneuptime.com/blog/post/2026-01-30-capability-based-security/view

### Codebase references grounding this design
- `crates/tangram-host/src/runtime.rs` — `HostState::http_fetch` host fence
  (`:84-95`) and host-keyed inject (`:104-151`); empty WASI ctx + closed world.
- `crates/tangram-host/src/config.rs` — `InjectRule` / `InjectKind`,
  `validate_inject` (`:305`), `resolved_inject` (`:324`), `allow_hosts`
  (`:138`), tenant `allow_hosts_ceiling` (`:393`).
- `crates/tangram-host/wit/tangram.wit` — `http-fetch` / `describe` (the
  declaration channel) / closed `host` world.
- `docs/adr/0005-egress-credential-injection.md`,
  `docs/adr/0006-tenant-isolation-posture.md`,
  `docs/security/tenant-isolation-review.md` (Q4 caveats this extends).
