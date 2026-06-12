# ADR-0009: Egress policy engine (the deferred imperative escape hatch — opt-in, latency-budgeted, never the default)

**Status:** proposed (2026-06-12) — on a pushed review branch
(`egress-policy-engine`), held for the owner's separate review, NOT merged.
**Deciders:** Aaron (owner), with design + implementation by Claude
**Related:** ADR-0008 (the declarative call-level engine this composes WITH —
the default and first gate), ADR-0005 (egress credential injection — the
exposure axis), ADR-0004 (the secret-resolver seam), ADR-0006 (tenant isolation
tiers),
[`docs/design/fine-grained-egress.md`](../design/fine-grained-egress.md) §9.2
(the policy-engine deferral and the constraints it must respect), §2 (the
"custom glue is where bugs appear" lesson), §8 (the parser-differential
discipline), `crates/tangram-host/src/{policy.rs,config.rs,runtime.rs,app.rs}`.

## Context

ADR-0008 shipped the **declarative** call-level egress engine: the grant is a
declared call `(method, host, path, shape)` matched against a single
canonicalization seam, regex-free, with the credential bound to the matched
call. The design (fine-grained-egress §9.2) deliberately deferred a **general
imperative policy engine** — the escape hatch for the rare case the declarative
`[[calls]]` grammar cannot express (e.g. a relationship between two
already-canonicalized fields, or a default-deny posture with a handful of
allow-shaped exceptions on one host). The deferral was explicit about the
shape of the eventual variant: it must be **bounded, not Turing-complete**,
**reuse the existing canonicalization seam** (no second parser — the SOCKS5
parser-differential lesson, §2/§8), carry a **hard latency budget** that fails
**closed**, and be **surfaced + opt-in** so "this app uses custom policy" is
never silent. It is built to the same merge-ready quality bar as everything
else, **pushed** to the remote for review, and left there rather than merged.

This ADR records that variant.

## Decision

Add an **opt-in, per-app egress policy engine** that runs at the host egress
boundary as an **additional gate, after** the declarative host-fence and call
match. The declarative `CallSpec` grammar (ADR-0008) **remains the default and
the first authoritative gate**; the policy engine is an explicit escape hatch
that **can only NARROW** — turn an allow into a deny. It never grants a call the
declarative engine denied, and never changes which credential is injected.

- **A small, bounded, auditable rule AST** (`crate::policy`): a `Policy` is an
  ordered list of `Rule`s evaluated **first-match-wins** plus a `default`
  effect. A `Rule` is `effect (allow|deny)` + an **AND** of leaf `Condition`s
  (the only combinator — no nesting, no OR/NOT tree to reason about). A
  `Condition` reads ONLY fields the shared `egress::CanonicalRequest` seam
  produced: `MethodIn`, `HostEq`, `PathEq`, `PathPrefix` (parsed-segment prefix,
  **never** a string suffix/`endsWith`), `QueryNamePresent`, `HeaderNamePresent`,
  and `BodyJsonMethodIn` (the **same** `egress::BodyMatch` JSON-RPC-method rung
  the declarative engine uses). There is **no regex, no value-matching on
  arbitrary fields, no second parser.**
- **The seam is reused, not re-implemented.** Every host/path a policy rule
  names is canonicalized through the *same* `egress::canonical_host` /
  `egress::canonical_path` at config-lowering time, and every request field a
  condition reads comes from the *same* `CanonicalRequest`/`BodyMatch` the
  declarative matcher consumes. The policy engine and the declarative matcher
  therefore can never disagree on what a host/path *means* — the §2/§8
  parser-differential discipline, satisfied by construction.
- **A hard latency budget, enforced at parse and at eval, failing CLOSED.**
  `MAX_RULES` (64) and `MAX_CONDITIONS` (256) are checked when the `Policy` is
  constructed (`Policy::new`) — an over-budget policy is a **config error**
  (the host refuses to load it). At evaluation a `MAX_EVAL_STEPS` (512) backstop
  charges one step per condition; if it is somehow exceeded the verdict is
  `FailClosed`, which the caller treats as a **deny**. There is no backtracking
  and no unbounded loop, so evaluation is `O(rules × conditions)` and
  terminates. `default` defaults to **deny** (a policy that forgets a case
  denies).
- **Surfaced, never silent.** A policy attaches as `[apps.<app>.policy]` in
  apps.toml; `AppSpec::uses_policy()` exposes the marker; `AppRuntime::build`
  logs a loud `CUSTOM POLICY` line naming the rule count at instantiation; and
  a policy denial returns an error that says the app "uses a custom egress
  policy (§9.2)". The default stays declarative.
- **Composition (narrows, never widens).** In `HostState::http_fetch` the policy
  is consulted on every request that would otherwise proceed (a matched
  declarative call, or an undeclared call allowed in observe/warn). In
  `observe` mode the policy only **logs** what it would do (the observe contract
  extended to the policy gate); in `warn`/`enforce` a policy `Deny` (or a
  fail-closed budget) blocks the request **before** the secret is resolved or
  injected.

### Strictly additive / behavior-identical by default

An app with no `[apps.<app>.policy]` block has `policy = None` and **no policy
gate at all** — its egress behavior is byte-identical to ADR-0008 (which is
itself byte-identical to the pre-call-level host allowlist). The section-1
guarantee holds: existing `apps.toml` files are unaffected. Registry-installed
specs deliberately **do not** carry a policy (a replicated policy would be
custom egress glue granted from the document plane; the §9.2/§6 posture keeps
that operator-authoritative — a policy lives in the operator's apps.toml).

## Consequences

- **Expresses what the declarative grammar can't, without leaving the
  deterministic boundary.** A default-deny-with-exceptions posture, or a
  combination of conditions across canonical fields, is now expressible —
  while staying bounded, regex-free, and on the same seam, so it does not
  reintroduce the parser-differential class the whole feature exists to avoid.
- **Auditability cost is real and is the trade-off** (the §2 glue lesson). Even
  bounded, a rule list is more to read and reason about than a `[[calls]]`
  table, and first-match-wins ordering is a footgun if rules overlap. This is
  *why it is the escape hatch, not the default*: the declarative grammar should
  cover the overwhelming majority, and the policy engine is reserved for the
  cases that genuinely need it — surfaced loudly so a reviewer always knows an
  app reached for it.
- **Latency.** Evaluation is bounded and fast (`O(rules × conditions)`, no
  allocation in the hot path beyond the body parse the declarative engine
  already does, only when a body condition is present and within
  `max_body_bytes`). The budget caps the worst case and fails closed.
- **It cannot widen, by construction.** The policy runs only on requests the
  declarative engine already allowed and can only deny them, so it can never be
  used to grant reach the operator spec withheld — the same "request, not a
  grant" posture as `describe()` declarations (§6).
- **Honest residual (unchanged from ADR-0008).** Like the declarative engine,
  the policy narrows the egress *surface*; it cannot make a *declared,
  policy-allowed* write-call safe against smuggling data the app could already
  send there. That remains the model-layer / human-review tier. Side-channels
  remain ADR-0006's separate axis.
- **Status: opt-in escape hatch, pushed for review, not the default and not
  merged.** Per §9.2, this branch is pushed to the remote and left for the
  owner's separate review rather than merged into main. The default egress
  contract stays the declarative ADR-0008 grammar.

## Open questions for review

- **Where in the pipeline the policy evaluates.** This implementation runs the
  policy on *every* request that would proceed (matched or warn/observe
  pass-through), which is the most conservative reading of "an additional gate
  that can only narrow." An alternative is to gate only *matched* calls and
  leave warn/observe purely declarative. The conservative choice was taken;
  flagged for the owner.
- **The condition vocabulary.** The leaves were chosen to mirror the
  declarative grammar exactly (so nothing the policy can express requires a new
  parser). If a future need arises (e.g. "exactly N segments"), it should be
  added as a new bounded `Condition` variant on the canonical fields — never as
  a regex or value match.
- **Budget constants.** `MAX_RULES`/`MAX_CONDITIONS`/`MAX_EVAL_STEPS` are set
  conservatively small; they are the auditable latency budget and can be tuned
  with data, but smaller is safer (the §9.2 "deliberately bounded" intent).
