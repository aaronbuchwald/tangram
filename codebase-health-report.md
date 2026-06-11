# Codebase Health Report — Tangram

_Lead review pass (read-only). Generated 2026-06-11. This report enumerates and
ranks opportunities; it changes no production code. Fan-out workers should be
scoped per the **file groups** below so they do not collide._

## Executive summary (3 lines)

1. **The dominant, highest-ROI finding is test-harness duplication**: the
   `workspace_root`/`component`/`HostProc`+`Drop`/`spawn_host`/`wait_for`/
   `free_port`/`status_of` helpers are copy-pasted near-verbatim across 6–7
   integration test files in `crates/tangram-host/tests/` — a shared
   `tests/support/` module is the clean fix.
2. **Secondary themes are small and safe**: the action-error → response
   mapping is hand-duplicated across three transports (host HTTP, host rmcp
   MCP, SDK sans-io MCP); a handful of pedantic-clippy idiom fixes; and one
   genuine **doc/ADR drift** (ADR-0006 says Wasmtime fuel/memory limits are
   "not yet configured" — still true, but should be cross-linked to the open
   gap, not left dangling).
3. **Invariants are clean**: no AGENTS.md convention violations found (actions
   sync/async split, `#[autosurgeon(missing)]` on all additive `Option`
   fields, relative UI fetches, `tangram-core` free of tokio/hyper/rmcp,
   no secrets in logs). Reachable `expect()`s in request paths are all
   lock-poison guards (acceptable). cargo-machete findings are **false
   positives** (macro-generated deps).

**Top 3 to action:** #1 (shared test-support module), #2 (centralize the
error-envelope mapping contract), #6 (ADR-0006 drift note).

### Coverage we could NOT measure (missing tools)
- `cargo-llvm-cov` not installed → no line/branch coverage numbers. Coverage
  gaps below are inferred by reading `tests/` by hand, not measured.
- `cargo-audit` not installed → **no vulnerability scan was run**. Recommend a
  follow-up `cargo install cargo-audit && cargo audit` as a separate gate.
- `jscpd` not available → cross-file duplication is from the fn-name heuristic
  + manual reading, not a token-level clone detector.
- `tokei` and `cargo-machete` were installed and run (results incorporated).

---

## Findings (ranked), grouped by file so workers don't collide

### GROUP A — `crates/tangram-host/tests/*.rs` (integration test harnesses)

> **Single owner recommended.** All findings here touch the same 6–7 test
> files; assign Finding A1 to one worker and fold A2 into the same PR. Do NOT
> split across workers — they will collide on every file.

#### A1 · Duplicated lifecycle-test harness → shared `tests/support` module
- **Category:** Duplication · **Severity:** high · **Effort:** M
- **Files:**
  `crates/tangram-host/tests/capabilities.rs`,
  `egress_injection.rs`, `federated_fleet.rs`, `gateway_lifecycle.rs`,
  `marketplace_lifecycle.rs`, `registry_lifecycle.rs`, `tenant_lifecycle.rs`
- **Evidence:** `workspace_root` defined in 7 files, `component`/`spawn_host`/
  `status_of` in 6, `wait_for`/`free_port`/`HostProc`+`Drop` in 6. The bodies
  are byte-identical for `workspace_root` (always `ancestors().nth(2)`),
  `component`, `free_port`, `wait_for`, and the `HostProc(Child)` /
  `impl Drop { kill+wait }` pattern. `spawn_host` has small per-file variation:
  most take `(home, apps_toml, bind, log)`; `egress_injection.rs:53` and
  `federated_fleet.rs:80` add an `extra_env: &[(&str,&str)]` arg and extra
  `.env_remove(...)` calls; token env vars differ per suite.
- **Suggested change:** Add `crates/tangram-host/tests/support/mod.rs` (Rust's
  shared-test-module idiom: `mod support;` in each test file). Move the
  identical helpers verbatim: `workspace_root`, `component`, `free_port`,
  `wait_for`, `status_of`, and `HostProc` + its `Drop`. Make `spawn_host` the
  superset signature
  `spawn_host(home, apps_toml, bind, log, env: &[(&str,&str)])` (the
  4-arg callers pass `&[]`); keep per-suite token constants in their own files
  and pass them through `env`. Leave suite-specific helpers (`healthz`,
  `state_text`, `fleet_json`, `fleet_error`, `get_status`, artifact servers)
  where they are unless 2+ files share them verbatim.
- **Safe to mechanize?** Partly. The verbatim helpers (workspace_root,
  component, free_port, wait_for, HostProc) are mechanical lift-and-shift.
  `spawn_host` unification needs hand care (signature + env merge). Treat as a
  human-reviewed M, not a codemod.
- **Risk:** Low — test-only, no production change. Watch: each suite sets
  different env (HOST/ALICE/BOB tokens, `CALORIENINJAS_API_KEY`/
  `NUTRITION_STRATEGY` removals in federated_fleet) — the unified `spawn_host`
  must preserve each suite's exact env or tests will flip. Build/run with the
  existing wasm components on 19xxx ports only.

#### A2 · `#[ignore]`'d e2e test silently never runs in CI
- **Category:** Test coverage · **Severity:** low · **Effort:** S
- **Files:** `crates/tangram-host/tests/e2e_cloudflare_sync.rs:17`
- **Evidence:** `#[ignore = "spawns wrangler dev (miniflare) + two native
  instances; needs node >= 20.3 and npm"]`. Legitimately gated, but quietly
  excluded from the default `cargo test` gate.
- **Suggested change:** No code fix required; document in the report that this
  is intentionally manual. Optionally add a CI lane that runs
  `cargo test -p tangram-host --test e2e_cloudflare_sync -- --ignored` when
  node is present. (Out of scope for a quality pass — note only.)
- **Safe to mechanize?** no · **Risk:** none (informational).

---

### GROUP B — `crates/tangram-host/src/mcp.rs`, `crates/tangram-host/src/routes.rs`, `crates/tangram/src/mcp.rs` (+ optional `crates/tangram-core`)

> **Overlap flag:** B1 touches all three of these files at once. If B3
> (host mcp.rs schema clone) is also assigned, **serialize B1 then B3** since
> both edit `crates/tangram-host/src/mcp.rs`. B2 (SDK adapter) is in a
> different file (`crates/tangram/src/sync.rs`) and is independent.

#### B1 · Action-error → response mapping duplicated across 3 transports
- **Category:** Duplication / Simplification · **Severity:** med · **Effort:** M
- **Files:**
  `crates/tangram-host/src/routes.rs:433-436` (HTTP: Unknown→404, BadArgs→400,
  Failed→422, Internal→500),
  `crates/tangram-host/src/mcp.rs:82-90` (rmcp: BadArgs/Failed→tool-result,
  Unknown→invalid_params, Internal→internal_error),
  `crates/tangram/src/mcp.rs:94-98` (SDK sans-io: BadArgs/Failed→`call.fail`,
  Unknown→`call.unknown_tool`, Internal→`call.internal_error`).
  Classification lives at `crates/tangram-host/src/app.rs:66-78`
  (`DispatchError::from_guest`) and the enum mirrors
  `crates/tangram-core/src/action.rs` `ActionError`.
- **What it is:** The same 4-variant semantic contract ("domain errors
  (BadArgs/Failed) are tool results the agent reads; system errors
  (Unknown/Internal) are protocol errors") is re-encoded by hand in 3 places
  over **two different enums** (`DispatchError` in host, `ActionError` in
  core/SDK). If a 5th variant is added or the HTTP/MCP semantics shift, all 3
  sites must change in lockstep — drift is silent.
- **Suggested change (conservative):** Do NOT force a single trait across two
  crates (host's `DispatchError` vs core's `ActionError` differ for good
  reasons). Instead: (a) add a doc-comment cross-reference on each of the 3
  match sites naming the other two so the contract is discoverable; and/or
  (b) within `tangram-host`, the rmcp `call_tool` already duplicates the
  classification that `routes.rs` does — give `DispatchError` two small
  inherent helpers (`fn is_domain_error(&self) -> bool`, or
  `fn http_status(&self) -> StatusCode`) so the host's two sites share one
  source of truth. Keep the SDK site as-is (different crate/enum).
- **Safe to mechanize?** no — requires judgment about which sites legitimately
  differ vs. should share.
- **Risk:** Low if scoped to host-internal helpers; the mappings are
  behavior-defining (status codes, JSON-RPC error codes) so any change must
  preserve the exact current outputs — covered by the parity/lifecycle suites.

#### B2 · `AsSyncDoc` single-use adapter wraps a near-identical trait
- **Category:** Simplification · **Severity:** low · **Effort:** S
- **Files:** `crates/tangram/src/sync.rs:72-91` (def), single call site ~`:103`
- **What it is:** `AsSyncDoc<'a, D>` exists only to bridge `DocHandle` to
  `tangram_core::sync::SyncDoc` (the two differ only in `DocHandle::subscribe`,
  unused by the sync loop). One definition, one caller.
- **Suggested change:** Either implement `tangram_core::sync::SyncDoc` for
  `D: DocHandle` directly (blanket impl) and drop the wrapper, or inline the
  adapter at its single call site. Prefer the blanket impl if the orphan rule
  allows (both traits/types may be local to the `tangram` crate — verify).
- **Safe to mechanize?** no (orphan-rule check needed) · **Risk:** Low,
  compile-checked, no behavior change.

#### B3 · Eager `input_schema.clone()` in `McpBridge::new`
- **Category:** Simplification (minor alloc) · **Severity:** low · **Effort:** S
- **Files:** `crates/tangram-host/src/mcp.rs:30-34`
- **What it is:** `match a.input_schema.clone() { Object(map)=>map, _=>{} }`
  clones the whole schema before the match; clone only needed in the `Object`
  arm. Once-per-app (not per-request), so impact is tiny.
- **Suggested change:** `match &a.input_schema { Object(map)=>map.clone(),
  _=>Map::new() }`.
- **Safe to mechanize?** yes · **Risk:** none. **Collision:** same file as B1.

---

### GROUP C — `crates/tangram-core/src/*.rs` & `crates/tangram/src/*.rs` (pedantic-clippy idiom)

> These are scattered one-liners across many files; bundle as a **single
> "clippy pedantic sweep" PR** so workers don't each touch overlapping files.
> Independent of Groups A and B except B-files; if a B-worker is active on
> `crates/tangram-host/src/mcp.rs`/`crates/tangram/src/mcp.rs`, exclude those
> two files from the sweep or serialize.

#### C1 · Adopt low-risk pedantic-clippy suggestions
- **Category:** Code quality / idiom · **Severity:** low · **Effort:** S–M
- **Files (representative, from the pedantic/nursery scan):**
  `crates/tangram-core/src/mcp.rs:74` (derive `Eq` alongside `PartialEq`),
  `crates/tangram-core/src/store.rs:41` (`const fn`), `:52` (`#[must_use]`),
  `crates/tangram-core/src/sync.rs` (`filter_map(..).next()` → `find_map`;
  temporary-with-significant-Drop early drop at `:58`),
  `crates/tangram/src/app.rs:62,68,76` & `crates/tangram/src/http.rs:41,47`
  (missing `#[must_use]` on builder methods returning `Self`),
  `crates/tangram/src/sync.rs:55-68` (unnecessary struct-name repetition),
  `crates/tangram/src/web.rs:94` (`map(..).unwrap_or_else(..)` on Option →
  `map_or_else`).
- **Suggested change:** Apply the mechanical subset: `find_map`, `map_or_else`,
  `#[must_use]` on builders, `Eq` derive, `const fn` where it compiles. Skip
  the doc-paragraph-length and `# Panics`-section warnings unless doing a
  dedicated docs pass (noise, not bugs).
- **Safe to mechanize?** mostly yes (`cargo clippy --fix` for the idiom subset),
  but review each — `const fn`/`Eq` can fail to compile on some types.
- **Risk:** Low; compile + existing tests catch regressions. Note: these are
  beyond the repo's `-D warnings` gate (pedantic/nursery), so adopting them is
  optional polish, not a gate fix.

#### C2 · `cargo-macros` over-long fn (110/100 lines)
- **Category:** Simplification · **Severity:** low · **Effort:** M
- **Files:** `crates/tangram-macros/src/lib.rs:65` (and `:126`
  `#[allow(non_camel_case_types)]` on generated arg structs, which is correct).
- **Suggested change:** Extract the field/arg-struct generation into a helper
  to drop under the line threshold. Low priority — proc-macro code, well-
  contained.
- **Safe to mechanize?** no · **Risk:** Low but macro changes are
  high-blast-radius; only worth it if touching this file anyway.

---

### GROUP D — Docs / ADRs (no source files)

> Fully independent of all code groups. Single doc-worker.

#### D1 · ADR-0006 fuel/memory limits drift (still pending, dangling)
- **Category:** Doc / ADR drift · **Severity:** med · **Effort:** S
- **Files:** `docs/adr/0006-tenant-isolation-posture.md:54,70,72`
- **Evidence:** ADR says Wasmtime fuel/memory limits are "not yet configured;
  tracked as future work" and "Open follow-up (not yet ticketed)". Verified by
  grep: **no `fuel` / `StoreLimits` / `ResourceLimiter` / memory-limit code
  exists in `crates/tangram-host/src/`** — the claim is accurate but the gap is
  untracked. For a "semi-trusted/untrusted tenant" posture this is a real
  security-relevant gap, not just docs.
- **Suggested change:** Keep the ADR statement (it's true) but file/ticket the
  follow-up and link it, so the limitation isn't silently load-bearing.
  Optionally add a one-line note in `docs/RUNTIME_PLAN.md` where tenant
  isolation is described. (No production code change in this pass.)
- **Safe to mechanize?** no · **Risk:** none (docs).

#### D2 · ADR-0002 "pending" markers — verify, mostly informational
- **Category:** Doc / ADR drift · **Severity:** low · **Effort:** S
- **Files:** `docs/adr/0002-cloudflare-app-runtime.md:39,138`
- **Evidence:** Lines mention `--async-mode jspi` wrapping and "MCP sessions +
  pending tool calls live in component memory". The scan's "pending" hit at
  `:138` is the word "pending tool calls" (a design statement), not a
  status-pending marker — **not actual drift**. Note in report; no change.
- **Safe to mechanize?** no · **Risk:** none.

#### D3 · `apps/marketplace` index check — NOT drift (resolved)
- The mechanical scan's invariant section did not list marketplace, but
  `AGENTS.md:38-41` **does** index `apps/marketplace`, and all five apps
  (`marketplace`, `notes`, `nutrition`, `registry`, `shell`) appear. No action.
  (The CLAUDE.md surfaced in the session context was a stale copy; the
  on-disk AGENTS.md is current.)

---

### GROUP E — Dependencies (Cargo.toml metadata only)

> Independent. Low value; include only if a worker is idle.

#### E1 · cargo-machete "unused deps" are FALSE POSITIVES
- **Category:** Dependencies · **Severity:** low (do-not-remove) · **Effort:** S
- **Files:** `apps/notes/Cargo.toml`, `apps/nutrition/Cargo.toml`,
  `apps/registry/Cargo.toml`, `apps/marketplace/Cargo.toml`
- **Evidence:** machete flags `automerge`/`autosurgeon`/`schemars`/`serde` as
  unused, but `crates/tangram-macros/src/lib.rs` emits absolute paths
  `::serde`, `::schemars`, `::autosurgeon`, `::automerge` (lines 25-28, 125,
  149-174) in generated code — each app crate genuinely needs them as **direct**
  deps for `#[model]`/`#[actions]` expansion to resolve. **Do not remove.**
- **Suggested change (optional):** Add
  `[package.metadata.cargo-machete]` with
  `ignored = ["automerge","autosurgeon","schemars","serde"]` to each app
  Cargo.toml to silence the false positive in future scans. Purely cosmetic.
- **Safe to mechanize?** yes (metadata add only) · **Risk:** none.

#### E2 · Duplicate transitive dep versions — not actionable
- **Category:** Dependencies · **Severity:** low · **Effort:** L (mostly N/A)
- **Evidence:** `cargo tree --duplicates` shows split versions
  (sha2 0.10/0.11, thiserror 1/2, digest 0.10/0.11, toml 0.8/0.9, plus the
  whole wasmtime/cap-std/rustix tree). These are pulled by upstream crates
  (wasmtime ecosystem, notify, etc.), not by first-party direct deps — not
  fixable without upstream bumps.
- **Suggested change:** None now. Re-evaluate after a `cargo update`/wasmtime
  bump. Note only.
- **Safe to mechanize?** no · **Risk:** N/A.

---

## Items explicitly NOT flagged (checked, clean)
- **Reachable panics in request paths:** every `expect()` in
  `crates/tangram-core/src/{store,sync,mcp}.rs` and host `doc.rs` is a
  lock-poison guard (`.lock().expect("...")`) — acceptable (a poisoned lock
  means a prior panic; aborting is correct). `into_axum`'s
  `builder.body(body).expect("valid response")` (`crates/tangram/src/mcp.rs:138`)
  builds from server-controlled bytes, not user input — not reachable on bad
  input. The remaining `unwrap`/`panic!` hits are all in `#[cfg(test)]`/`tests/`.
- **Invariants:** actions sync/async split honored (only `nutrition::log_meal`
  is async, resolves then `ctx.mutate`); all additive `Option` model fields
  carry `#[autosurgeon(missing = ...)]`; no HashMap/BTreeMap in models; UI
  fetches are relative; `tangram-core` has no tokio/hyper/rmcp/axum/reqwest;
  secrets only `expose_secret()` into request builders, never into a log macro
  (`secrets.rs:182-184` logs the reference name only).
- **`#[allow(...)]` suppressions:** the 5 in src are justified
  (`too_many_arguments` on builders, `dead_code` on a genuinely-future field,
  `non_camel_case_types` on macro-generated arg structs).

---

## Fan-out plan: independent work items

There are **5 independent (non-file-overlapping) work items**:

| Item | Group(s) | Files touched | Notes |
|------|----------|---------------|-------|
| W1 | A1 + A2 | `crates/tangram-host/tests/*.rs` | One owner; A2 is note-only |
| W2 | B1 + B3 | `crates/tangram-host/src/{mcp,routes,app}.rs`, `crates/tangram/src/mcp.rs` | **Serialize B1 before B3** (both edit host `mcp.rs`) |
| W3 | B2 | `crates/tangram/src/sync.rs` | Independent of W2 |
| W4 | C1 + C2 | `crates/tangram-core/src/*`, `crates/tangram/src/*` (excl. the two mcp.rs files if W2 active), `crates/tangram-macros/src/lib.rs` | Clippy sweep; **exclude/serialize the mcp.rs files vs W2** |
| W5 | D1 + D2 + E1 + E2 | `docs/adr/*`, `docs/RUNTIME_PLAN.md`, app `Cargo.toml`s | Docs + Cargo metadata; no source overlap |

**Serialization flags:**
- W2 internal: B1 → B3 (shared `crates/tangram-host/src/mcp.rs`).
- W2 ↔ W4: both can touch `crates/tangram-host/src/mcp.rs` and
  `crates/tangram/src/mcp.rs` — give those two files to W2 only, or run W4
  after W2 merges.
- All other pairs are file-disjoint and safe to run in parallel.
