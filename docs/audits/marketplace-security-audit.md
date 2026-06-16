# Marketplace / Registry Artifact-Pipeline Security Audit

Issue: #29 — *AUDIT: Marketplace security, caching, storage, and credential handling*
Scope: READ-ONLY code review + threat model of the registry/marketplace artifact
pipeline across five seams (A–E). No audited code was changed; this document is
the only deliverable.

Date: 2026-06-16
Auditor: automated code review (file:line evidence below)
Commit audited: `4a400da` (main at audit start)

---

## Executive summary

The artifact pipeline's **core trust property is sound for the self-hosted /
loopback posture it ships in**: a `component_url` install never instantiates a
byte that does not hash to the operator-pinned sha-256, the cache is
content-addressed and write-once, secrets are resolved host-side per-call and
never enter the component or a replicated document, and federation derives
per-app remotes deterministically. The verification chain (`granted ⊆ declared
⊆ audited`) is real and enforced at converge for the host-grained links.

The gaps are concentrated where the issue itself predicted: **anything that would
face an untrusted/public uploader or an untrusted artifact source.** All three
prior findings are **confirmed**. Two additional real gaps were found (HTTPS not
enforced on `component_url`; `wasi:filesystem/` whitelisted in the closed-world
audit).

### MUST-FIX list (before any public / untrusted marketplace exposure), severity-ranked

> **Remediation status (issue #29 — branch `fix/marketplace-security`).** The two
> public-exposure blockers in the marketplace's own scope — **M1** (auth the
> artifact upload) and **M3** (enforce HTTPS on `component_url`) — are FIXED on
> this branch and verified by tests (see the per-finding ✅ notes below). **M2,
> M4, M5, M6 are NOT addressed here** — they are host-runtime hardening
> (Wasmtime limits, the closed-world `wasi:filesystem` whitelist, the call-grain
> verification arm, cache-dir perms) tracked as the separate public-marketplace
> hardening epic in "Cross-cutting recommendations" below, and none is a defect
> for the documented self-hosted / loopback posture.

| # | Severity | Finding | Seam |
|---|----------|---------|------|
| M1 | **High** | ✅ **FIXED (auth arm).** `POST /artifacts` now ALWAYS requires the bearer (`TANGRAM_AUTH_TOKEN`); a host with no token refuses every upload with 401 — anonymous upload is gone (`routes.rs::upload_artifact`). The remaining hardening (upload-time import audit, streaming size cap, per-principal rate limit, storage quota, GC/blocklist) is NOT in scope for this fix and stays in the hardening epic; the route remains default-off + startup-gated. | E / A |
| M2 | **High** | Wasmtime resource limits UNSET in `runtime.rs`: no fuel, no `StoreLimits` (memory/table/instance caps), no epoch interruption. A malicious/buggy component can OOM or spin the host (DoS). **NOT addressed in #29** (host-runtime hardening epic). | E |
| M3 | **Medium-High** | ✅ **FIXED.** `component_url` now requires `https://` at both validation boundaries — the marketplace listing (`apps/marketplace/src/lib.rs::add_listing` → `component_url_scheme_ok`) and the host installer (`tangram-host` `config.rs::component_url_scheme_ok`). Plaintext `http://` is rejected for every non-loopback host; the only exception is `http://` to a LOOPBACK host (`127.0.0.1`/`[::1]`/`localhost`/any loopback IP) for the self-hosted / dev / test posture, where there is no wire to sniff. All other schemes are rejected. | A |
| M4 | **Medium** | Closed-world import audit whitelists `wasi:filesystem/` (`verify.rs:56`). A component importing `wasi:filesystem` is NOT flagged as foreign, contradicting the documented invariant and the marketplace's displayed "no filesystem" claim. Not currently exploitable (empty WASI ctx grants no preopens), but it is a defense-in-depth + honesty gap that becomes load-bearing the moment any preopen is ever added. | E / A |
| M5 | **Medium** | Manifest verification's **calls arm is a no-op** on the live converge path: `granted.calls` is hard-coded empty (`config.rs:970`), and `NetworkClaim::Calls` is never produced by a registry/marketplace install. Host-grained subset is enforced; call-grained is not. Also: a marketplace listing's `CapabilityManifest` / `import_audit` string is NEVER mechanically checked against the real component imports at any install/converge path — it is displayed metadata only. | E |
| M6 | **Low-Medium** | Cache directory is created with `create_dir_all` (default umask perms, typically `0755`), not `0700`. On a shared host, world-readable component bytes + a predictable path. Low impact (artifacts are public-by-design content-addressed blobs) but worth tightening for multi-user hosts. | B |

Items M1–M3 are the public-exposure blockers (**M1 + M3 are now fixed on
`fix/marketplace-security`, #29; M2 remains**). M4–M5 are correctness/honesty
gaps that matter most once untrusted *components* (not just untrusted operators)
are in scope. None of M1–M6 is a defect for the documented **self-hosted,
loopback / single-operator** deployment.

### Top 3 most important findings

1. **M2 — no Wasmtime resource limits.** This is the broadest exposure: it applies
   to EVERY component the host runs (first-party included), not just uploaded ones,
   and a single component can starve the whole host process. `runtime.rs:626-643`
   sets only the compilation cache on the `Config` — no `consume_fuel`, no
   `epoch_interruption`, and `Store::new` (`runtime.rs:555`) installs no
   `ResourceLimiter`. Confirms prior finding (1).
2. **M1 — open upload is unhardened.** ✅ Auth arm FIXED (#29): the upload route
   now ALWAYS requires the bearer (no anonymous upload, even on loopback). The
   route is otherwise honestly documented as dev/demo-only and default-off, but the
   body is still buffered whole up to 64 MiB (`routes.rs:42,241`), there is no
   per-host quota or rate limit, and **no upload-time closed-world import audit** —
   `store_artifact` validates *shape* (`Component::new`) but not *imports*
   (`fetch.rs:111-132`). Those remain for the hardening epic.
3. **M3 — plaintext `http://` artifact fetch permitted.** ✅ FIXED (#29).
   `component_source()` (and `add_listing`) now require `https://` via
   `component_url_scheme_ok`, with a loopback-only `http://` exception for local
   dev/test. Plaintext fetch of executable artifacts over a sniffable wire is
   rejected.

---

## Seam A — Artifact fetch & verification (`crates/tangram-host/src/fetch.rs`)

`Fetcher::resolve()` (`fetch.rs:139`), `fetch_verified()` (`fetch.rs:172`),
`validate_wasm_component()` (`fetch.rs:49`), `store_artifact()` (`fetch.rs:111`).

| Check | Verdict | Evidence |
|-------|---------|----------|
| HTTPS enforced on `component_url` | ✅ FIXED (#29) | Was `config.rs:721` accepting any `http://`. Now `config::component_url_scheme_ok` requires `https://`, allowing `http://` only to a loopback host; `add_listing` applies the same rule at the catalog boundary. Plaintext fetch of an executable artifact over a sniffable wire is rejected. (`fetch.rs:172` still uses a plain `reqwest::Client::new()` — fine now that only https or loopback-http reaches it.) **M3 — done.** |
| SHA-256 computed & checked BEFORE cache placement | ✅ holds | `fetch.rs:192-197` computes `Sha256::digest(&body)` and `ensure!(actual == sha256, …)` BEFORE the write at `204` and rename at `206`. Test `mismatched_digest_is_rejected_and_not_cached` (`fetch.rs:282`) proves nothing unverified reaches the cache. |
| Write-then-rename atomicity (`.tmp` → final) | ✅ holds | `fetch.rs:203-207` writes `.<sha>.tmp` then `rename` to the content address. Upload path mirrors it with a distinct `.<sha>.upload` suffix to avoid racing the fetch tmp slot (`fetch.rs:128-130`). |
| Cache immutability (write-once) | ✅ holds | `resolve` short-circuits on `path.exists()` (`fetch.rs:141`); `store_artifact` dedups on `path.exists()` (`fetch.rs:119`). A present `<sha>.wasm` is never rewritten. Content-addressing makes overwrite a no-op even if attempted. Test `fetch_verifies_caches_and_never_refetches` (`fetch.rs:256`). |
| Failed-fetch memory (~30s) blunts retry-DOS | ✅ holds | `RETRY_AFTER = 30s` (`fetch.rs:20`); failures keyed `(url, sha256)` in a mutex map (`fetch.rs:70`), re-reported without refetch while fresh (`fetch.rs:147-151`). Test asserts the immediate retry does not hit the network (`fetch.rs:305-311`). |
| WASM validation — magic bytes + `Component::new` | ✅ holds | `validate_wasm_component` checks `\0asm` magic (`fetch.rs:50`) then `wasmtime::component::Component::new` (`fetch.rs:56`), which rejects core modules and corrupt binaries. Tests `rejects_non_wasm_garbage`, `rejects_garbage_upload`. NOTE: validation runs on the UPLOAD path (`store_artifact`) but **NOT on the URL-fetch path** — `fetch_verified` only hashes; the bytes are validated later at `Component::from_file` during instantiation (`runtime.rs:526`). That is acceptable (instantiation will reject a non-component), but means the URL cache can briefly hold a hash-matching non-component blob. ⚠️ partial. |
| Closed-world import audit (reject `wasi:sockets`/`wasi:http`/`wasi:filesystem`) at fetch/store | ❌ gap | Neither `fetch_verified` nor `store_artifact` runs `AuditedImports`. The audit happens only at instantiation in `app.rs:313` (and there `wasi:filesystem` is whitelisted — see Seam E / M4). No import audit gates entry to the content-addressed store. **M1 item 4, M4.** |

### Threat-model notes (A)

- **Race on hash mismatch:** None. The verify-then-rename ordering (`fetch.rs:192`
  → `206`) means a partial/wrong artifact is never visible at the content address.
  `resolve` holds the `failures` mutex across the whole fetch (`fetch.rs:146-169`),
  serializing concurrent fetches of the same `(url, sha)` — no two writers race the
  same tmp/final pair. ✅
- **DOS via constant retries:** Mitigated by the 30s failure memory (✅), but the
  converge loop is the only caller and is single-flight; an *external* retry-DOS
  vector does not exist here. The real DOS surface is M1 (upload) and M2 (runtime).
- **Bypass of verification:** No path instantiates an unverified artifact — both
  `ComponentSource::Path` (operator-local, trusted) and `ComponentSource::Url`
  (hash-gated) flow through `main.rs:504-539`. A present cache slot is trusted
  as-is, which is correct because it can only have been placed post-verification.
  ✅

---

## Seam B — Caching (`$HOME/.tangram-host/components/<sha256>.wasm`)

| Check | Verdict | Evidence |
|-------|---------|----------|
| Directory permissions secure | ⚠️ partial | `create_dir_all(&self.cache_dir)` (`fetch.rs:123,199`) with no explicit mode → inherits umask (commonly `0755`). World-readable on a shared host. Artifacts are content-addressed public blobs so confidentiality impact is low, but **M6** flags tightening to `0700` for multi-user hosts. The default root is under `$HOME` (`fetch.rs:27-32`), so single-user hosts are fine. |
| Deduplication (same sha → same file) | ✅ holds | Content-addressed path (`cache_path`, `fetch.rs:37`); both `resolve` and `store_artifact` early-return on `exists()`. Identical bytes never re-stored. |
| Cleanup / unbounded growth | ❌ gap | No GC, no LRU, no quota anywhere. The cache grows monotonically with every distinct artifact ever installed/uploaded. Bounded in practice for a curated fleet, unbounded under open upload. Part of **M1 (item 6 — operator delete/GC)**. |
| Upload-time validation before caching | ⚠️ partial | `store_artifact` validates wasm SHAPE before writing (`fetch.rs:116`) ✅, but NOT imports/size/quota/rate (M1). Validation IS before the cache write, so garbage never lands; the gap is the *depth* of validation, not its ordering. |

---

## Seam C — Credential handling (`secrets.rs`, egress injection in `runtime.rs`, `tangram-egress`)

| Check | Verdict | Evidence |
|-------|---------|----------|
| `secret="env://VAR"` resolved at dispatch-time only | ✅ holds | Inject secrets are resolved inside `http_fetch`, per-request, only on the matched call (`runtime.rs:242-251` → `rule.resolve_secret`). The value is a `SecretString` (`secrets.rs:18`, redacted Debug + zeroize-on-drop). Resolution flows through `SecretRegistry::resolve` (`secrets.rs:223`). |
| Never logged | ✅ holds | `SecretString` Debug is redacted (test `secret_string_debug_is_redacted`, `secrets.rs:448`). `resolve_value` warns with the REFERENCE, never the value (`secrets.rs:283-287`). Egress logs name `injected = <bool>` and host/method/path only (`runtime.rs:449-452`), never the credential. `op://` resolver keeps the SA token in inherited env and never logs argv/value (`secrets.rs:107-200`). The only `expose_secret()` calls are at the actual reqwest builder (`runtime.rs:411,438,441`) and the immediate `.to_string()` in `resolve_value` (`secrets.rs:279`, fed straight to the WASI env builder, not logged). |
| Never in replicated docs | ✅ holds | Specs carry `scheme://locator` REFERENCES, not values (`secrets.rs:20-23`). The marketplace listing's `inject.secret` is a reference passed through to the registry (`marketplace/src/lib.rs:104-106`), and the registry document stores the reference, never the resolved value. ADR-0005: the component never sees the plaintext (it is attached host-side in `send_and_strip`). |
| Response auth headers stripped before the component sees the response | ✅ holds | `STRIPPED_RESPONSE_HEADERS` = authorization, proxy-authorization, www-authenticate, proxy-authenticate, set-cookie (`runtime.rs:290-296`); filtered case-insensitively before the body is returned (`runtime.rs:461-475`). |
| Injection can't target hosts outside `allow_hosts` (canonicalizer seam) | ✅ holds | The host fence runs FIRST against the single canonicalized host (`runtime.rs:131-142`) before any call match or inject. Inject is attached only to a matched declared call (`runtime.rs:242`), and a call's host MUST be in `allow_hosts`. `validate_inject` (referenced `verify.rs:339-340`, enforced at config load) requires inject targets ⊆ allow_hosts. Host canonicalization is the shared `tangram-egress` seam (`egress.rs:43,86`) — lowercased, trailing-dot-stripped, null-byte-rejected (tests `host_is_lowercased`, `null_byte_host_is_refused`, `egress.rs:739-767`). |

### Threat-model notes (C)

Credential handling is the strongest seam. The component literally cannot read a
host credential: it is resolved host-side, attached after the component's own
headers so the component cannot pre-seed/override the injected name
(`runtime.rs:433-444`), and any echoed auth header is stripped from the response.
Query-injection mutates the URL before the builder consumes it (`runtime.rs:408`).
No plaintext-in-logs path was found.

---

## Seam D — Federation & replication (`apps/registry`, `tangram-host/src/registry.rs`)

| Check | Verdict | Evidence |
|-------|---------|----------|
| Registry sync converges across peers | ✅ holds | Registry is a normal Tangram app whose document the host merges over `apps.toml` (`registry.rs:286-324` `merge`). Convergence is Automerge CRDT merge of the spec list; `merge` is deterministic (BTreeMap, registry-wins-except-file-`registry=true`). |
| Per-app remotes as `<base>/<app>/sync` | ✅ holds | `Federation::app_remote` = `format!("{base}/{app}/sync")` (`registry.rs:132-134`); `sync_base` strips `/registry/sync` (or `/registry`) from the registry remote (`registry.rs:140-147`). Tests at `registry.rs:490-526` assert the derivation incl. tenant-prefixed bases. |
| Federated specs with local component paths report clear errors (not silent) | ✅ holds | Two layers: `parse_state` warns at parse (`registry.rs:192-199`) but does NOT skip (the writing host runs it); the converge path in `main.rs:513-525` returns a CLEAR fleet error ("local paths are not portable … reinstall with component_url + component_sha256") on a peer where the path is absent, and never mutates the shared doc. ✅ |
| Byte parity when peers converge on the same hash | ✅ holds | A `component_url` + `component_sha256` install is portable: every peer fetches independently and the sha-256 gate (`fetch.rs:192`) guarantees byte-identical artifacts or a hard converge error. Genesis is the component's deterministic `genesis()` so documents share one root and merge (`app.rs:364-368` comment; ADR-0001). |

### Threat-model notes (D)

- **Federation desync:** Not observed. The only divergence vector is a local-path
  entry on a federated registry, which is surfaced as an explicit per-app fleet
  error rather than a silent skip — peers stay converged on everything else.
- **Trust note (not a code defect):** A federated registry's document is authority
  for desired state across the fleet. A compromised / malicious peer that writes the
  registry doc can install any `component_url`+sha it likes on every peer — but
  each peer still hash-verifies and capability-verifies locally, and the registry's
  mutating actions are bearer-gated (`TANGRAM_AUTH_TOKEN`, README "registry app").
  This is the intended trust model (operator-controlled registry), worth stating
  explicitly for any public-federation future.

---

## Seam E — Marketplace metadata, manifest verification, upload route, runtime limits

### E.1 Manifest verification chain (`verify.rs`)

| Check | Verdict | Evidence |
|-------|---------|----------|
| `granted ⊆ declared` enforced at converge (host grain) | ✅ holds | `verify::verify` (`verify.rs:324`) hard-fails on a granted host/inject-host/env-key not in the declaration (`verify.rs:331-356`); called at `app.rs:313` for every app build, error mapped to the app's converge error (does not run). Tests `over_grant_is_hard_fail`, etc. |
| `declared ⊆ audited` enforced (soft flag) | ✅ holds | Under-claim / broken closed world → `Unverified` (app runs, flagged) (`verify.rs:391-421`); surfaced via `tracing::warn` "running UNVERIFIED" (`app.rs:319-321`). |
| Vacuous-grant guard | ✅ holds | Granting reach to a component that imports no `http-fetch` is a HARD fail (`verify.rs:382-389`). |
| **calls arm** actually implemented | ⚠️ partial / no-op | The containment relation exists and is correct (`verify.rs:366-376` → `CallSpec::contains` → `egress::CallSpec::covers`), routed through the single egress seam. BUT `granted.calls` is hard-coded `Vec::new()` (`config.rs:970`) and `NetworkClaim::Calls` is never produced by a registry/marketplace install (`registry.rs:237` sets `calls: Vec::new()`). The loop is dead on the live converge path; the activating test is `#[ignore]`d (`verify.rs:578`). **M5.** Confirms prior finding (3). |
| `CapabilityManifest` mechanically checked against real component imports | ❌ gap | The marketplace `import_audit` is a free-text string (`wasm-tools` output) shown in the UI (`marketplace/src/lib.rs:57-62`), NEVER parsed or compared to `AuditedImports`. `add_listing` validates only non-emptiness + sha/url shape (`marketplace/src/lib.rs:167-187`). The host-side `granted ⊆ declared` chain uses the SPEC grant as the declaration for registry installs (`declared: None` → `derived_from_grant`, `config.rs:947`, `registry.rs:249`), so an honest install verifies trivially but a listing's *claimed* manifest is never proven against the artifact's imports. The pipeline that would do this is documented as future work (`marketplace/src/lib.rs:151-153`). **M5.** |
| Import audit rejects `wasi:sockets`/`wasi:http`/`wasi:filesystem` | ⚠️ partial | `is_known_safe_interface` (`verify.rs:51-58`) flags `wasi:sockets`/`wasi:http` as `foreign` (closed-world break, soft-flagged ✅) — but **whitelists `wasi:filesystem/`** (`verify.rs:56`). A component importing `wasi:filesystem` is treated as known-safe and NOT flagged, contradicting the module doc ("no sockets/fs-beyond-wasi/inbound-http", `verify.rs:119-120`) and the marketplace's "no filesystem" claim (`marketplace/src/lib.rs:58-61`). **M4.** Mitigating fact: the runtime links WASI with an EMPTY context — no preopens (`runtime.rs:533-539`) — so an imported `wasi:filesystem` has no directories to act on; the import is inert *today*. The gap is honesty + defense-in-depth, and becomes exploitable if any preopen is ever introduced. |

### E.2 Blob upload route (`POST /artifacts`, `ArtifactsConfig`, `routes.rs`)

| Check | Verdict | Evidence |
|-------|---------|----------|
| Default-off | ✅ holds | `ArtifactsConfig.upload_enabled` defaults false (`config.rs:1163-1166`); both routes 404 when off (`routes.rs:279-281,313,331`). |
| Auth-gated | ✅ FIXED (#29) | Upload now ALWAYS requires the bearer: `upload_artifact` returns 401 when no `TANGRAM_AUTH_TOKEN` is configured (no more anonymous-on-loopback) and 401 on a wrong/absent bearer otherwise (`routes.rs::upload_artifact`). Startup still REFUSES a non-loopback bind with upload on and no token, and now warns that a no-token host will 401 every upload. Test `upload_requires_a_token_even_on_loopback`. **M1 (auth arm) — done.** |
| Size limit | ⚠️ partial | A coarse 64 MiB `DefaultBodyLimit` (`routes.rs:42,241`) — but the body is buffered WHOLE into `Bytes` (`routes.rs:277`), not streamed-and-rejected. No per-host aggregate cap. **M1.** |
| Rate / frequency limit | ❌ gap | None on this route. **M1.** |
| Storage quota | ❌ gap | None (see Seam B cleanup). **M1.** |
| Upload-time import-audit reject | ❌ gap | `store_artifact` runs `validate_wasm_component` (shape) but NOT `AuditedImports`/closed-world (`fetch.rs:116`). A valid component importing `wasi:sockets` would be stored and served, deferring the (soft) flag to install-time. **M1 item 4.** |

The route is HONESTLY self-documented as dev/demo-only with the exact MUST-FIX
checklist inline (`routes.rs:255-273`) and a loud startup warning
(`main.rs:764-776`). The disposition is correct for self-hosted; the gaps are real
for public exposure.

### E.3 Wasmtime resource limits (`runtime.rs`)

| Check | Verdict | Evidence |
|-------|---------|----------|
| Fuel / epoch interruption (CPU bound) | ❌ gap | `engine()` (`runtime.rs:626-643`) sets ONLY the compilation cache on `Config` — no `config.consume_fuel(true)`, no `config.epoch_interruption(true)`. No `store.set_fuel` / `set_epoch_deadline` anywhere. A component can spin a guest call indefinitely (the per-app mutex `runtime.rs:491` then blocks that app, and the await holds a tokio task). **M2.** |
| `StoreLimits` (memory/table/instance bound) | ❌ gap | `Store::new(engine, state)` (`runtime.rs:555`) installs no `ResourceLimiter`; `HostState` carries none. A component can grow memory until the host OOMs. **M2.** |
| Per-call network timeout | ✅ holds (partial mitigation) | Outbound fetches are bounded (30s, `runtime.rs:420`) and the artifact fetch is 120s (`fetch.rs:177`) — but these bound I/O, not guest CPU/memory. |

Confirms prior finding (1). This is the single broadest exposure because it
affects every component, not only uploaded/untrusted ones.

---

## Issue #29 security checklist

- [x] **No plaintext credentials in logs** — ✅ Confirmed. `SecretString` redaction
  (`secrets.rs:18,448`), warnings name references not values (`secrets.rs:283`),
  egress logs carry `injected=<bool>` only (`runtime.rs:449`). `op://` keeps token
  in inherited env, never argv/value (`secrets.rs:173-200`).
- [x] **No credentials persisted in replicated docs** — ✅ Confirmed. Specs and
  marketplace listings carry `scheme://locator` references only
  (`secrets.rs:20-23`, `marketplace/src/lib.rs:104-106`); values resolved
  host-side at dispatch (`runtime.rs:242`).
- [x] **No MITM (HTTPS + hash verify)** — ✅ FIXED (#29). Hash verify is solid
  (`fetch.rs:192-197`) and HTTPS is now enforced at both boundaries
  (`config::component_url_scheme_ok`, `add_listing`): `https://` required,
  `http://` allowed only to a loopback host, all other schemes rejected.
  Integrity ✅, confidentiality/availability ✅ (off-loopback). **M3 — done.**
- [x] **No cache bypass** — ✅ Confirmed. Every instantiation path resolves through
  the hash-gated cache (`main.rs:504-539`); a present slot is post-verification
  only; verify-then-rename prevents a partial/wrong artifact at the address.
- [x] **No federation desync** — ✅ Confirmed. Deterministic per-app remote
  derivation (`registry.rs:132`), clear non-portable-path fleet errors
  (`main.rs:513-525`), byte parity via the sha pin.
- [~] **No unvalidated upload** — ⚠️ PARTIAL (auth arm FIXED #29). Upload now
  ALWAYS requires the bearer — no anonymous upload (`routes.rs::upload_artifact`,
  **M1 auth arm done**). It still validates SHAPE but not imports/size-streaming/
  rate/quota (`fetch.rs:116`, `routes.rs:42`) — those stay in the hardening epic.
  Default-off + startup gate + mandatory auth make this safe for the shipped
  posture; the remaining depth is needed before public exposure.
- [ ] **Closed-world audit before public marketplace** — ❌ GAP. No import audit at
  fetch or upload; the install-time audit whitelists `wasi:filesystem`
  (`verify.rs:56`); listing manifests are never mechanically verified against
  imports (`marketplace/src/lib.rs:151-153`). **M1, M4, M5.**

---

## Appendix — code paths audited

- `crates/tangram-host/src/fetch.rs` — `Fetcher::resolve`, `fetch_verified`,
  `store_artifact`, `validate_wasm_component`, `cache_path`, `artifact_path`,
  `RETRY_AFTER`, failure memory.
- `crates/tangram-host/src/runtime.rs` — `HostState`, `http_fetch`,
  `send_and_strip`, `STRIPPED_RESPONSE_HEADERS`, `ComponentHandle::instantiate`,
  `engine()` (resource-limit review).
- `crates/tangram-host/src/secrets.rs` — `SecretRef`, `EnvResolver`,
  `OnePasswordResolver`, `SecretRegistry`, `resolve_value`, `SecretString` use.
- `crates/tangram-host/src/verify.rs` — `AuditedImports::from_component`,
  `is_known_safe_interface`, `verify`, `CallSpec::contains`, the chain links and
  call-grain no-op.
- `crates/tangram-host/src/egress.rs` — `CanonicalRequest::from_request`,
  `CallSpec::matches`/`covers`, the canonicalization seam, `intersect_with_declared`.
- `crates/tangram-host/src/config.rs` — `AppSpec::component_source`,
  `validate_sha256`, `granted_capabilities`, `declared_capabilities`,
  `ArtifactsConfig`.
- `crates/tangram-host/src/routes.rs` — `upload_artifact`, `serve_artifact`,
  `artifacts_disabled_get`, `MAX_UPLOAD_BYTES`, `root_router` artifact routes.
- `crates/tangram-host/src/registry.rs` — `Federation`, `app_remote`, `sync_base`,
  `parse_state`, `merge`.
- `crates/tangram-host/src/main.rs` — converge component resolution
  (`504-539`), federated local-path gate, artifacts startup gate (`745-776`).
- `crates/tangram-host/wit/tangram.wit` — the component world (import surface).
- `apps/marketplace/src/lib.rs` — `Listing`, `CapabilityManifest`, `InjectGrant`,
  `add_listing`, seed catalog.
- `apps/marketplace/seed/` — seeded sha256 / wit import-audit data.

### Cross-cutting recommendations (for the public-marketplace hardening epic)

1. ~~**M3:** Default `component_url` to HTTPS-only; gate `http://` behind an explicit
   `[artifacts] allow_insecure_url = true` dev flag (mirror the upload posture).~~
   ✅ DONE (#29) — implemented as HTTPS-required with a loopback-`http://`
   exception (`component_url_scheme_ok` in both `tangram-host::config` and the
   marketplace `add_listing`), rather than a config flag; loopback has no wire to
   sniff so it covers the self-hosted/dev/test posture without a knob.
2. **M2:** In `engine()`, enable `epoch_interruption` (or `consume_fuel`) and set a
   per-dispatch deadline; install a `StoreLimits` ResourceLimiter on every
   `Store::new` (memory + instance caps). Applies fleet-wide, low risk, high value.
3. **M4:** Remove `wasi:filesystem/` from `is_known_safe_interface` so it is
   flagged `foreign` — aligns the audit with the documented invariant and the
   marketplace claim; harmless today (no preopens) and forward-safe.
4. **M1:** ✅ Auth arm DONE (#29) — `POST /artifacts` always requires the bearer.
   STILL TO DO for public exposure: add to `store_artifact` a closed-world
   `AuditedImports` reject (hard, at upload), a streaming size cap, a per-principal
   rate limit, and a per-host quota + GC/delete + blocklist; stream the body rather
   than buffering whole.
5. **M5:** Wire a marketplace `import_audit`/`CapabilityManifest` → `AuditedImports`
   mechanical check at `add_listing` (or at install in the registry), so a listing
   that lies about its imports is rejected, not merely displayed; activate the
   call-grain arm by carrying `NetworkClaim::Calls` through registry installs.
6. **M6:** Create the cache dir with `0700` on unix (a `DirBuilder.mode(0o700)`),
   for multi-user hosts.
