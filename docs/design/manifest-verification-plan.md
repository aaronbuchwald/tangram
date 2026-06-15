# Design: Mechanical capability-manifest verification

**Status:** IMPLEMENTED — the `granted ⊆ declared ⊆ audited` chain ships as
`crates/tangram-host/src/verify.rs` (function-level import audit at the converge
chokepoint, soft-flag/hard-fail dispositions, the `verified` fleet field;
pinned by `crates/tangram-host/tests/verification.rs`). This document is
retained as the design record; CP6 (call-grain converge consumption) remains
the one designed-for-but-deferred checkpoint (see verify.rs). Where the body
says "no production code", read it as the original plan.
**Date:** 2026-06-11
**Author:** Aaron (owner), with research + planning by Claude
**Related:** ADR-0006 (tenant isolation — this is the *mechanical* half of the
future third-party-submission pipeline), ADR-0005 (egress credential
injection), ADR-0001 (WASM-first runtime),
[`fine-grained-egress.md`](fine-grained-egress.md) (the call-level grant model
the manifest must be able to express),
`crates/tangram-host/src/{runtime.rs,app.rs,config.rs,registry.rs,fetch.rs}`,
`crates/tangram-host/wit/tangram.wit`, `apps/marketplace/src/lib.rs`.

---

## 0. Scope line (read this first)

This plan covers the **MECHANICAL gate only**: at install/converge time the
host extracts the component's *actual* imports programmatically and enforces a
subset chain over the declared manifest and the granted spec. It is a
deterministic, type-level check — "the bytes can only name what they claim, and
the operator granted no more than they claim."

**Explicitly OUT of scope** (the next layer, not built here): the LLM
*behavioral* sanity check — "does this app actually do what its description
says, and is the data it sends where it claims appropriate." That is a
probabilistic review layer (Anthropic's framing: the deterministic boundary is
hit when the probabilistic layer misses — see fine-grained-egress §2). The
third-party-submission pipeline (`apps/marketplace` TODO) needs *both*; this
plan delivers the deterministic one and leaves a clean seam for the other.

The mechanical gate answers: **does the component import a capability it did
not declare, and does the spec grant a capability the manifest did not
declare?** It cannot answer: is a *declared* call benign in intent. That
residual is the model/human layer, same shape as fine-grained-egress §8's
"exfil within a declared call."

---

## 1. The verification chain, stated precisely

The host enforces a three-link subset chain at every install/converge path:

```
granted (spec)  ⊆  declared (manifest)  ⊆  audited (component's real imports)
```

- **audited** — what the component's bytes can even *name*: its actual WIT
  imports, read programmatically from the compiled component. This is the
  ground truth (`wasm-tools component wit` produces the human-readable form
  today; we read it as types in-process).
- **declared** — the `CapabilityManifest` (marketplace) / the grant fields of
  the `AppSpec` the listing installs. What the app *says* it needs.
- **granted** — what the operator's effective spec actually hands the running
  component (`allow_hosts`, `inject`, `env`), after the tenant ceiling
  intersection.

Two violations, two dispositions:

1. **granted ⊄ declared → converge ERROR (hard fail).** The operator (or a
   registry/marketplace install) granted a host/inject/env the manifest never
   declared. The app does not run; the fleet shows a clear error. This is the
   operator-facing safety: you cannot silently widen an app past its published
   manifest.
2. **declared ⊄ audited → FLAGGED (unverified).** The manifest claims *less*
   than the component's real imports — e.g. the manifest says "no network" but
   the component imports `http-fetch`. The app may still run (it is not unsafe
   per se — the *grant* still gates reach), but it is marked `verified: false`
   with the specific discrepancy, surfaced in `/api/fleet` and as an
   unverified badge in the marketplace. A manifest that *under*-claims is a
   lie about the app's surface and must be visible.

The trivial-pass case the prompt names: a component importing **no**
`http-fetch` verifies for zero hosts automatically — audited∩network = ∅, so
any `allow_hosts`/`inject` grant is `granted ⊄ declared`/`audited` and errors,
and a manifest declaring no network is `declared ⊆ audited` trivially.

### 1.1 The granularity trap (load-bearing finding)

`wasm-tools component wit` at the **world** level is **too coarse to be the
audit**. The seed audits for `notes` and `nutrition` are *byte-identical*: both
import `tangram:app/host` and the same wasi:* set. notes makes no network call;
nutrition calls CalorieNinjas. The difference is **which functions of the
`host` interface the component imports** — `http-fetch` specifically — not which
*interfaces* it imports. (Both import the `host` interface; whether they import
its `http-fetch` *function* is what differs, and the component model records
that.)

Therefore the audit must inspect **the imported functions inside
`tangram:app/host`**, not just the world's imported interface names. The
existing `seed/*.wit` world dump stays useful as a coarse closed-world proof
(no `wasi:sockets`, no `wasi:http`) but is NOT sufficient for the http-fetch
distinction. The verifier reads the instance's exported function set. (This
finding is why the recommendation below is wasmtime in-process introspection,
which gives function-level types, over scraping the world-dump text.)

---

## 2. Implementation design

### 2.1 Reading a component's imports in Rust — RECOMMENDATION

**Recommendation: use wasmtime's in-process `Component` type introspection.
Do NOT shell out to `wasm-tools`.** Rationale and API surface follow; the
wasm-tools-subprocess and wasmparser-as-a-lib alternatives are weighed in §2.2.

The host already compiles every component with `Component::from_file` in
`runtime.rs::ComponentHandle::instantiate`. wasmtime 45 (the pinned version)
exposes the full import type graph off that same `Component`:

```rust
use wasmtime::component::types::{Component as CType, ComponentItem};

// `component` is the already-compiled wasmtime::component::Component.
let ct: CType = component.component_type();
for (import_name, item) in ct.imports(&engine) {
    // import_name is the interface id, e.g. "tangram:app/host",
    // "wasi:sockets/...", "wasi:http/...".
    if let ComponentItem::ComponentInstance(inst) = item {
        for (func_name, func_item) in inst.exports(&engine) {
            // func_name inside tangram:app/host is "http-fetch" | "log" | "now-ms".
            // (exports of an *imported instance* = the functions the component imports)
        }
    }
}
```

Confirmed against `wasmtime-45.0.1`:
`Component::component_type() -> types::Component` (`component.rs:392`);
`types::Component::imports(&Engine) -> impl ExactSizeIterator<Item=(&str,
ComponentItem)>` (`types.rs:995`); `ComponentItem::ComponentInstance` variant
(`types.rs:1117`) with `ComponentInstance::exports(&Engine)` enumerating the
interface's functions (`types.rs:1063`). This is exactly the function-level
granularity §1.1 requires, with **zero new dependency** and **no subprocess**.

The verifier reduces this graph to a small **`AuditedImports`** value:

```rust
pub struct AuditedImports {
    /// Interface ids the component imports (e.g. "tangram:app/host").
    interfaces: BTreeSet<String>,
    /// Functions imported from tangram:app/host (the capability surface).
    host_funcs: BTreeSet<String>,   // subset of {http-fetch, log, now-ms}
    /// Any import OUTSIDE the known-safe set (wasi std + tangram:app/host) —
    /// e.g. wasi:sockets, wasi:http. Non-empty ⇒ the closed world is broken.
    foreign: BTreeSet<String>,
}
```

The capability-relevant predicates the chain needs:
- `imports_http_fetch = host_funcs.contains("http-fetch")` — the network
  capability. If false, the component cannot make *any* outbound request and
  every `allow_hosts`/`inject` grant is vacuous → must be empty.
- `closed_world_ok = foreign.is_empty()` — the Phase-0 import-audit invariant
  (no sockets/fs-beyond-wasi/inbound-http) re-expressed as a programmatic
  check instead of a text assertion in the marketplace test.

Note env grants: env vars ride the wasi:cli/environment import (present in
*every* component — see the seed audits) and carry *data, not reach*
(runtime.rs comment). So `env` is NOT gated by an import predicate; it is gated
purely manifest-side (granted env keys ⊆ declared env keys). Only the network
capability (`http-fetch`) has a meaningful import-level predicate. This keeps
the audit honest: it constrains *reach*, and reach is exactly `http-fetch`.

### 2.2 Alternatives weighed (and why rejected)

- **Shell out to `wasm-tools component wit <path>` and parse the text.** This
  is what `seed/refresh.sh` does today for the *display* audit. Rejected as the
  enforcement mechanism: (a) it is text, and §1.1 shows the world-dump text
  does not distinguish http-fetch from no-network at the granularity we need
  without parsing the interface bodies too; (b) it adds a subprocess + a
  required external binary to the host's converge hot path (the 2s tick), with
  failure modes (binary missing, version skew, output-format churn) that turn a
  pure-Rust check into an ops dependency; (c) wasm-tools CLI output format is
  not a stability contract. KEEP it only for the human-readable `import_audit`
  string the marketplace already displays (it is good for humans; it is not the
  gate). The gate reads types in-process.
- **`wasmparser` / `wit-parser` / `wit-component` as a library.** Lower-level;
  would re-derive what wasmtime already computed during compile. The component
  is *already compiled* in the converge path, so wasmtime's type graph is free.
  Only reach for wasmparser if a future need arises to audit a component the
  host will NOT instantiate (not the case here — every audited component is
  about to be instantiated). Documented as the fallback if wasmtime's
  introspection API churns.

### 2.3 Where verification slots into the converge/install path

The single chokepoint is **`AppRuntime::build`** (`app.rs:170`), called by
`Host::ensure_app` (`main.rs:425`) for *every* converge path — apps.toml,
registry `install_app`, and marketplace (which installs via the registry).
`build` already (a) has the resolved local `component_path` (after
`fetch::Fetcher::resolve` verified the sha-256 for URL specs), and (b) compiles
the `Component`. Verification is one step right after instantiation, before the
app is inserted into the live table:

1. `ensure_app` resolves `component_path` (unchanged — sha-256 gate first).
2. `AppRuntime::build` compiles + instantiates (unchanged), then:
3. **NEW:** compute `AuditedImports` from the compiled `Component` (§2.1).
4. **NEW:** parse the component's *declared* manifest. Primary source: the
   component's own `describe()` output, extended with a `capabilities` block
   (the seam fine-grained-egress §5 already proposes — the app declares its
   calls/hosts in code, they ride out in `describe()`). Fallback for installs
   that carry a manifest out-of-band: the `AppSpec` grant fields themselves are
   the declared set for first-party apps (the spec IS the declaration). The
   marketplace `CapabilityManifest` is the declared set the registry passes
   through on install.
5. **NEW:** run the subset chain (§1):
   - `granted ⊄ declared` → return `Err` from `build` → `ensure_app` records it
     as the app's converge error (exactly like a sha-256 mismatch — the app
     does not run, fleet shows it). HARD FAIL.
   - `declared ⊄ audited` → build SUCCEEDS, but stamp `verified: false` +
     reason onto the `AppRuntime`. SOFT FLAG.
   - all subsets hold → `verified: true`.
6. Store the verification verdict on `AppRuntime` (a `Verification` field) and
   mirror it into `FleetStatus` on the converge pass that writes the fleet map.

Why `build`, not `ensure_app`: `build` is where the `Component` exists as a
typed value; threading it out to `ensure_app` would duplicate the compile. Why
not a separate pre-pass: the component is compiled exactly once already;
piggy-backing keeps converge a single instantiation per app.

**Granted ⊄ declared is the hard gate; declared ⊄ audited is the flag.** The
asymmetry is deliberate: over-granting is an *operator/install* error we can
refuse safely (the app simply doesn't run); under-declaring is an *app honesty*
problem where the grant still gates reach, so we surface rather than break a
possibly-load-bearing first-party app mid-fleet. (A future strict mode could
promote the flag to a hard fail for untrusted-tier installs — see §6.)

### 2.4 Manifest schema (extending what exists)

Two declaration shapes already exist; the plan unifies them behind one
`DeclaredCapabilities` the verifier consumes:

- **Marketplace `CapabilityManifest`** (`apps/marketplace/src/lib.rs`):
  `allow_hosts: Vec<String>`, `env_keys: Vec<String>`,
  `inject: Vec<InjectGrant>`, `data_note`. Today DISPLAYED, passed through to
  the registry's `install_app`. Becomes the *declared* set for a marketplace
  install. Add one field, additive (`#[serde(default)]` +
  `#[autosurgeon(missing = ...)]` per the CLAUDE.md model-evolution rule):
  - `network: NetworkClaim` — `{ none }` | `{ hosts }` (the existing
    allow_hosts becomes the body of the `hosts` variant) | `{ calls }` (the
    fine-grained-egress call list — see §2.6). `none` is the explicit
    "verifies for zero hosts, must import no http-fetch" claim. Older catalog
    docs without it default to `hosts` derived from `allow_hosts` (back-compat).
- **`AppSpec` grants** (`config.rs`): `allow_hosts`, `inject:
  BTreeMap<String, InjectRule>`, `env: BTreeMap<String,String>`. This is the
  *granted* set. No schema change needed for the chain — the verifier reads
  these directly.

The verifier's internal value (host-side, not serialized):

```rust
struct DeclaredCapabilities {
    network: NetworkClaim,        // None | Hosts(BTreeSet<String>) | Calls(Vec<CallSpec>)
    env_keys: BTreeSet<String>,
}
struct GrantedCapabilities {
    allow_hosts: BTreeSet<String>,   // effective, post-ceiling
    inject_hosts: BTreeSet<String>,  // hosts (or call keys) inject targets
    env_keys: BTreeSet<String>,
}
```

The subset checks are then plain set/`CallSpec`-containment:

- `granted.allow_hosts ⊆ declared.network.hosts()` (where `None` ⇒ ∅).
- `granted.inject_hosts ⊆ declared.network.hosts()` (inject already must be in
  allow_hosts — `validate_inject`, config.rs:305 — so this composes with the
  existing invariant).
- `granted.env_keys ⊆ declared.env_keys`.
- `declared.network` requires `http-fetch` to be imported iff it is non-`None`:
  `declared.network != None ⟹ audited.imports_http_fetch` (else FLAG: declares
  network but the component can't reach the network — a stale/wrong manifest)
  AND `granted.allow_hosts non-empty ⟹ audited.imports_http_fetch` (else HARD
  FAIL: granting reach to a component that cannot reach — vacuous, refuse).
- `audited.closed_world_ok` (no foreign imports) — FLAG if broken (a component
  that imports `wasi:sockets` escaped the closed world; surface loudly).

### 2.5 Surfacing: fleet + marketplace UI

**Fleet (`/api/fleet`, `main.rs::FleetStatus` + `routes.rs::fleet`).** Add a
first-class field to `FleetStatus`:

```rust
pub verified: Verification,   // Verified | Unverified { reasons: Vec<String> }
```

`routes.rs::fleet` (and `tenant_fleet`) serialize it:

```json
{ "name": "nutrition", "running": true, "verified": true, ... }
{ "name": "evil",      "running": false, "verified": false,
  "verify_reasons": ["spec grants api.evil.com which the manifest does not declare"],
  "error": "manifest verification failed: granted ⊄ declared", ... }
```

The hard-fail case (`granted ⊄ declared`) already lands in the existing
`error` field (the app didn't run); `verified:false` + `verify_reasons` carries
the soft-flag (`declared ⊄ audited`) where the app DID run. So `verified` is a
first-class boolean sourced from the **host's own audit of the running bytes**,
not from any listing claim.

**Marketplace UI.** Today the listing's `import_audit` + `CapabilityManifest`
are displayed *from the listing* (self-asserted by whoever added it). The badge
must instead be **sourced from the host** that ran the verification — i.e. the
marketplace UI reads the *local host's* `/api/fleet` for an installed app's
`verified` field, and for not-yet-installed listings shows "unverified until
installed" rather than a green badge derived from the listing's own text. The
listing's displayed manifest/audit remains (good for humans pre-install) but
the *trust badge* comes from the host's mechanical verdict, closing the
"displayed, not verified" gap the prompt names. (A pre-install verification —
fetch + audit a listing's artifact without running it — is a possible
enhancement; see §3 CP7 as optional.)

### 2.6 Interaction with fine-grained-egress (call-level grain)

The manifest/chain must be **expressible at whatever grain the grants are**.
fine-grained-egress moves grants from `(host)` to `(host, method, path, shape)`
`CallSpec`s, declared in the app's `describe()` and intersected with the
operator spec. The verifier is grain-agnostic by construction:

- `NetworkClaim::Hosts` (today) → subset = set containment on hosts.
- `NetworkClaim::Calls` (fine-grained) → subset = "every granted `CallSpec` is
  matched by some declared `CallSpec`" (a `CallSpec ⊆ CallSpec` relation:
  same/narrower method, host, path-template, query/header/body constraints).
  This is a structural containment on the small, canonical grammar
  fine-grained-egress §4.1 deliberately keeps regex-free, so containment is
  decidable and cheap.

Crucially, the **audited** side does NOT get finer with calls: the component
still only imports `http-fetch` (one function, no WIT change —
fine-grained-egress §4.2). Method/path are runtime values, not types, so the
*audit* link stays "imports http-fetch: yes/no," and the *granted ⊆ declared*
link is where call-grain containment lives. The verifier therefore needs no
change to its audit step when calls land — only the `NetworkClaim::Calls`
containment arm. Build the host-grained chain first; the call arm slots in when
fine-grained-egress ships, sharing the *same canonicalization seam* (§2.1 of
that doc) so the verifier and the egress enforcer never disagree on what a host
or path means (the SOCKS5 parser-differential lesson).

---

## 3. Effort estimate (framed for Claude Fable doing the work)

Sizing convention: **1 agent-session** ≈ one focused `/dev`-style implement +
test + verify loop ending green on the build/clippy/fmt/test gates;
**wall-clock band** assumes the owner reviews between sessions.
Complexity is relative within this plan.

| Phase | What | Size | Wall-clock | Complexity | Risk |
|---|---|---|---|---|---|
| **P1 — Audit reader** | `AuditedImports` from a compiled `Component` (§2.1), as a standalone module + unit tests over real fixture components (notes vs nutrition). | 1 session | half-day | **Low–Med** | wasmtime introspection API shape (mitigated: verified against 45.0.1 below) |
| **P2 — Manifest + chain** | `DeclaredCapabilities`/`GrantedCapabilities`, the subset checks (§2.4), the `Verification` verdict type, exhaustive unit tests of each chain link. Pure logic, no I/O. | 1 session | half-day | **Low** | grain edge cases (mixed-case hosts, inject⊆allow_hosts composition) |
| **P3 — Converge wiring** | Slot P1+P2 into `AppRuntime::build` → `ensure_app`; hard-fail vs soft-flag dispositions; thread the verdict into `FleetStatus`; integration test through a real converge. | 1–2 sessions | 1 day | **Med** | touching the converge hot path; not regressing existing fleet/error semantics |
| **P4 — Surfacing** | `verified` + `verify_reasons` in `/api/fleet` + `tenant_fleet`; marketplace UI badge sourced from host fleet (not listing). | 1 session | half-day | **Low–Med** | UI plumbing; relative-path fetch convention (CLAUDE.md) |
| **P5 — Manifest channel** | Extend `describe()`/`CapabilityManifest` with `network: NetworkClaim` (additive, model-evolution rules); make `describe()` the primary declared source. | 1 session | half-day | **Med** | additive model field discipline (`Option`+`autosurgeon(missing)`); keeping tangram-core compiling for wasip2 |
| **P6 (optional) — Pre-install audit** | Fetch + compile + audit a marketplace listing's artifact WITHOUT instantiating, so the badge is live before install. | 1 session | half-day | **Med** | compiling-but-not-running an untrusted artifact; resource limits (ties to ADR-0006 fuel/memory backlog) |

**Total (P1–P5, the mechanical gate): ~5–7 agent-sessions / ~3–4 working
days** of Fable time with owner review between phases. P6 optional, +1 session.

### 3.1 Realistic risk callouts (what could blow up)

- **Component-model introspection gaps / API churn (P1, the sharpest).** The
  recommendation rests on `types::Component::imports` + drilling
  `ComponentItem::ComponentInstance::exports` giving function-level granularity.
  This is *confirmed present in wasmtime 45.0.1* (api citations in §2.1), but
  it is a less-traveled corner of the API than instantiation; a wasmtime major
  bump could rename/reshape it. **Mitigation:** isolate all introspection in
  the one P1 module behind the `AuditedImports` boundary so a churn is a
  one-file change; keep the wasmparser-as-lib fallback documented (§2.2). The
  CP1 test (below) is the canary — it breaks loudly on any granularity change.
- **The granularity trap itself (§1.1).** If P1 is built against the *world*
  dump rather than the *function* set, notes and nutrition look identical and
  the whole gate is a no-op that *appears* to pass. **This is the most likely
  silent failure.** CP1's assertion (`nutrition` imports http-fetch, `notes`
  does not) is specifically designed to catch a verifier that only reads the
  world.
- **wasm-tools subprocess (only if the recommendation is overridden).** Version
  skew, output-format drift, missing binary on a converge tick. The
  recommendation avoids this entirely; flagged so a reviewer doesn't
  "simplify" the in-process reader back into a shell-out.
- **Converge hot-path regressions (P3).** `ensure_app`/`build` are load-bearing
  and already subtle (reload-keeps-old-instance, federated portability,
  sha-256 gate, tenant ceiling). Adding a failure mode must not change those.
  **Mitigation:** verification runs *after* a successful instantiation, so it
  can only add a verdict, never perturb the existing instantiate/error paths;
  the hard-fail reuses the existing `Err(String)` → fleet-error channel verbatim.
- **describe()-as-declaration trust (P5).** The component declaring its own
  manifest is a *request, not a grant* (fine-grained-egress §6): a malicious
  component could declare `network: none` while importing http-fetch — which is
  exactly the `declared ⊄ audited` FLAG case, so the audit catches the lie.
  But the inverse (declare a lot, import a lot, operator grants nothing) is fine
  — the grant gates reach. The discipline: **never trust `declared` as
  authority; it is only the middle of the chain, bounded above by the operator
  grant and below by the audited bytes.** Get this backwards and the gate is
  decorative.
- **Multi-tenant ceiling interaction (P3).** `granted` must be the
  *post-ceiling-intersection* effective allow_hosts (the value already in
  `FleetStatus.allow_hosts`), not the raw spec — verifying the raw spec would
  flag a host the tenant ceiling already removed. Use the effective set.

---

## 4. Testable checkpoints

Each checkpoint = a concrete assertion + how to run it. Ordered so the owner
and Fable can confirm correct progress at each step, not only at the end. Tests
live in `tangram-host` (unit) and its integration suite; run with
`cargo test -p tangram-host` unless noted.

- **CP1 — The host can dump a component's *function-level* imports, and notes
  ≠ nutrition.** A test builds (or uses fixture builds of) the notes and
  nutrition components and asserts:
  - `audit(notes).imports_http_fetch == false`
  - `audit(nutrition).imports_http_fetch == true`
  - `audit(notes).host_funcs ⊇ {log, now-ms}` and `audit(notes).foreign == ∅`
  - both `closed_world_ok` (no `wasi:sockets`/`wasi:http` in `foreign`).
  **This is the canary for the granularity trap (§1.1):** a verifier that reads
  only the world dump cannot make notes≠nutrition pass and fails here.
  *Run:* `cargo test -p tangram-host audit_reader` (needs the wasm32-wasip2
  fixture components built — reuse the existing integration-test build harness).

- **CP2 — A spec over-granting a host fails converge with a clear error.** An
  apps.toml (or registry entry) granting `allow_hosts = ["api.evil.com"]` for a
  manifest/describe that declares no such host produces an `ensure_app` error
  whose message names the offending host and the rule
  ("granted api.evil.com which the manifest does not declare"), and the app is
  NOT in the live table.
  *Run:* `cargo test -p tangram-host over_grant_fails_converge` (a converge
  integration test asserting `fleet[app].error` contains the host name and
  `running == false`).

- **CP3 — A component that imports no http-fetch cannot be granted any host.**
  Granting `allow_hosts` to the notes component (no http-fetch) is a HARD FAIL
  with "grants outbound reach to a component that imports no http-fetch."
  *Run:* `cargo test -p tangram-host vacuous_grant_fails`. Conversely, notes
  with empty `allow_hosts` verifies `true` (the trivial-pass case).

- **CP4 — A manifest under-claiming the real imports is FLAGGED, not failed.** A
  spec that grants nothing but whose component imports http-fetch while its
  `describe()` declares `network: none` builds successfully, runs, and lands in
  the fleet with `verified == false` and a reason naming the http-fetch
  discrepancy. The app is `running == true` (soft flag, not a block).
  *Run:* `cargo test -p tangram-host under_claim_is_flagged`.

- **CP5 — `verified` is a first-class fleet field, sourced from the host.** A
  converge integration test hits `GET /api/fleet` and asserts each app's JSON
  carries a boolean `verified` (and `verify_reasons` when false), independent of
  any marketplace listing text. notes/registry/nutrition (first-party, honest
  manifests) report `verified == true`.
  *Run:* the existing fleet integration test extended; assert on the parsed
  JSON body.

- **CP6 — The subset chain holds at the call grain (gated on
  fine-grained-egress).** When `NetworkClaim::Calls` exists: a granted
  `POST api.vendor.com/v1/accounts/{x}/import` against a manifest declaring only
  `GET /v1/me/contacts` HARD-FAILS (`granted ⊄ declared`), and a granted
  `GET /v1/me/contacts` against that manifest passes. Asserts the `CallSpec ⊆
  CallSpec` containment, including a parser-differential row (mixed-case host,
  `%2e` path, trailing-dot host) sharing the egress canonicalization seam.
  *Run:* `cargo test -p tangram-host call_grain_subset` — **only after
  fine-grained-egress ships**; until then this checkpoint is the explicit
  "deferred, but designed-for" marker.

- **CP7 (optional, P6) — Pre-install badge is honest.** The marketplace UI shows
  a green "verified" badge for an installed app ONLY when the local host's
  `/api/fleet` reports `verified == true`, and shows "unverified" / "verify on
  install" for listings whose `verified` verdict the host has not produced.
  *Run:* a UI/integration check that the badge state is read from `/api/fleet`,
  not from the listing's `import_audit`/`CapabilityManifest` fields.

The ordering lets the owner sign off incrementally: CP1 proves the ground truth
is readable at the right granularity (the make-or-break finding); CP2–CP3 prove
the hard gate; CP4–CP5 prove the soft flag and its surfacing; CP6 proves the
grain-agnostic design when calls arrive; CP7 closes the "displayed-not-verified"
UI gap.

---

## 5. What this does NOT do (honest residual)

- It does NOT judge whether a *declared* call is benign — that an app declaring
  `POST .../contacts` uses it to sync contacts and not to exfiltrate. That is
  the **LLM behavioral sanity check**, the next layer (the marketplace TODO's
  item 3), explicitly out of scope here. The mechanical gate shrinks the
  surface to "exactly the declared capabilities"; reading intent within that
  surface is the probabilistic layer.
- It does NOT address microarchitectural side-channels (ADR-0006) — orthogonal;
  this is a type/grant-level egress-surface control, not a co-residency control.
- It does NOT re-verify the component's *behavior* matches its description, nor
  perform the sandboxed smoke-run (marketplace TODO item 2) — a smoke-run is a
  natural companion (and P6's pre-install audit is a step toward it) but is a
  distinct effort.

The mechanical gate is the **deterministic boundary**: it is what holds when the
behavioral review misses, and it is a *prerequisite* for opening the marketplace
to untrusted third-party submissions (ADR-0006 untrusted tier), not the whole
of it.

---

## 6. Future: strict mode for the untrusted tier

For ADR-0006's untrusted third-party tier, the `declared ⊄ audited` SOFT FLAG
should be promotable to a HARD FAIL (an untrusted app that under-declares its
surface must not run at all), and the operator-authoritative `[[calls]]` of
fine-grained-egress §6 becomes the *declared* set the component can only narrow.
That is a one-line policy change in the disposition (§2.3 step 5) keyed on the
tenant trust tier, not new mechanism — recorded here so the seam is intentional.

---

## 7. Codebase references grounding this plan

- `crates/tangram-host/src/runtime.rs` — `ComponentHandle::instantiate`
  (`:225`) compiles the `Component` (the introspection input); the closed-world
  WASI ctx + host-fence comments.
- `crates/tangram-host/src/app.rs` — `AppRuntime::build` (`:170`), the single
  verification chokepoint; `Describe` (`:20`) carrying the optional
  `capabilities` block (the declaration channel).
- `crates/tangram-host/src/main.rs` — `FleetStatus` (`:61`), `ensure_app`
  (`:425`) and its sha-256 / federated-portability error channel the hard-fail
  reuses; the converge loop.
- `crates/tangram-host/src/routes.rs` — `fleet` (`:157`) / `tenant_fleet`
  (`:358`) JSON serialization where `verified` surfaces.
- `crates/tangram-host/src/config.rs` — `AppSpec` grant fields (`allow_hosts`
  `:138`, `inject` `:157`, `env` `:147`), `validate_inject` (`:305`,
  inject⊆allow_hosts), tenant `allow_hosts_ceiling` (the effective-grant source).
- `crates/tangram-host/src/registry.rs` — `parse_state`/`merge`: the
  registry/marketplace install path that flows through `ensure_app`.
- `crates/tangram-host/src/fetch.rs` — `Fetcher::resolve`: the sha-256 gate that
  runs BEFORE verification (verification trusts the bytes are the pinned bytes).
- `crates/tangram-host/wit/tangram.wit` — the `host` interface (`http-fetch`,
  `log`, `now-ms`): the function set the audit reads.
- `apps/marketplace/src/lib.rs` — `CapabilityManifest` / `InjectGrant` /
  `import_audit` (`:69`): today DISPLAYED; becomes the declared set + the
  badge moves to the host verdict.
- `apps/marketplace/seed/refresh.sh` — the `wasm-tools component wit` world dump
  kept for human display, NOT the gate (§1.1, §2.2).
- `docs/design/fine-grained-egress.md` §4–§6 — the call-level grain the chain
  must express; the canonicalization seam to share.
