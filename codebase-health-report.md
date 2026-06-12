# Codebase Health Report — Tangram

_Read-only review (2026-06-12). Source untouched; this file is the only write._

## Executive summary

- **Themes:** the codebase is healthy. The bulk of opportunities are low-risk
  clippy-pedantic hygiene (visibility, `#[must_use]`, `Eq` derives, doc
  backticks/reflow) clustered in `crates/tangram` and `crates/tangram-core`,
  plus one real **test-helper duplication** debt (a `tests/support` module
  exists but ~8 test files still hand-roll `spawn_host`/`free_port`/
  `workspace_root`/`build_artifacts`), and a couple of **doc-drift** items.
- **Top 3:** (1) consolidate the duplicated tangram-host test harness onto the
  existing `tests/support/mod.rs` [DEFER — in-flight]; (2) tidy
  `crates/tangram` visibility + `#[must_use]` + doc lints [SAFE]; (3) fix the
  `crates/tangram-core` lint cluster (`Eq` derive, doc reflow, early-drop guard)
  [SAFE]. The frequently-flagged `id: String` → `&str` clippy hint on action
  methods is a **false positive** (action args are `serde::Deserialize`d into a
  generated struct) and is excluded.
- **Counts:** **6 SAFE-NOW groups** (independent, single-worker units) vs
  **4 DEFER findings** (all touch in-flight `tangram-host` files).

### Coverage I could NOT measure (missing tools)

- `cargo-llvm-cov` not installed → no line-coverage numbers; coverage gaps
  below are from reading `tests/` by hand.
- `cargo-audit` not installed → no vulnerability scan of the dep tree.
- `jscpd` not installed → cross-file duplication is from the fn-name heuristic
  + manual reading only.
- `cargo-machete`: ran clean (no unused deps). Duplicate transitive `bitflags`
  is entirely inside the `wasmtime`/`cap-std` subtree — not actionable by us.

---

## SAFE NOW — independent fan-out units

Each group below is one self-contained task for a single agent. Groups touch
disjoint file sets, so they can run in parallel without colliding. All are
quality/hygiene only — no behavior change.

### SAFE-1 · `crates/tangram` visibility, `#[must_use]`, and doc lints
- **Category:** code-quality / simplification · **Severity:** low · **Effort:** S
- **Files:** `crates/tangram/src/app.rs`, `crates/tangram/src/mcp.rs`,
  `crates/tangram/src/store.rs`, `crates/tangram/src/web.rs`,
  `crates/tangram/src/sync.rs`
- **Change (mechanical):**
  - `app.rs:62,68,76` — add `#[must_use]` to the builder methods returning
    `Self` (`ui_dir`, `instructions`, `remote`).
  - `mcp.rs:42`, `web.rs:49` — `pub(crate) fn router` inside a private module:
    narrow to `pub(super)` (or drop the qualifier; module is private already).
  - `store.rs:15` — `pub(crate) struct Store` inside a private module: narrow
    to `pub(super)`.
  - `sync.rs:55,62,65,68` — remove redundant `Store::` self-qualification in
    the impl block (clippy `use_self`/`unnecessary structure name repetition`);
    `sync.rs:28` — reflow the over-long first doc paragraph.
- **Safe to mechanize:** yes · **Collision:** SAFE NOW
- **Notes:** `mcp.rs:138 expect("valid response")` and `guest.rs:54/115` panics
  are intentional invariant guards (response builder is statically valid;
  guest panics are init/dispatch-contract only) — **leave as-is**, or at most
  add a `# Panics`/rationale comment; not part of the mechanical pass.
  `time.rs:13 u128 as i64` is the public `now_ms()` contract — out of scope.

### SAFE-2 · `crates/tangram-core` lint cluster
- **Category:** code-quality / simplification · **Severity:** low · **Effort:** S
- **Files:** `crates/tangram-core/src/mcp.rs`, `crates/tangram-core/src/store.rs`,
  `crates/tangram-core/src/sync.rs`
- **Change (mechanical):**
  - `mcp.rs:74` — the `#[derive(Debug, PartialEq)]` type has only `String`
    fields: add `Eq` (clippy `derive_partial_eq_without_eq`).
  - `mcp.rs:29,62`, `store.rs:20,84`, `sync.rs:52,96` — reflow the over-long
    first doc-comment paragraphs (clippy `too_long_first_doc_paragraph`).
  - `sync.rs:58` — `let mut map = sessions.map.lock()...`: the guard with a
    significant `Drop` is held longer than needed; `drop(map)` (or scope it)
    once the `entry()` work is done, before the trailing logic.
  - `sync.rs:116` — `bytes[..4].try_into().expect("4 bytes")` is guarded by the
    length check on the prior line; leave the logic, optionally tighten to a
    fixed-size array read. Low priority.
- **Safe to mechanize:** yes · **Collision:** SAFE NOW
- **Notes — DO NOT TOUCH:** the `.expect("store lock")` / `.expect("sessions
  lock")` calls in `store.rs` and `mcp.rs` are `Mutex` lock-poisoning expects.
  Converting them to fallible returns is a **behavior-changing API redesign**
  across the dispatch path — excluded from this quality pass (would also widen
  every caller's error type). Flagged for a deliberate future decision, not the
  fan-out.

### SAFE-3 · App `#[must_use]` + `Self` idiom
- **Category:** code-quality · **Severity:** low · **Effort:** S
- **Files:** `apps/notes/src/lib.rs`, `apps/tangram/src/lib.rs`
- **Change (mechanical):**
  - `notes/src/lib.rs:79` (`list_notes`) and `:98` (`app()`) — add `#[must_use]`.
  - `tangram/src/lib.rs:38` — `Default` impl: `Vault { files: ... }` →
    `Self { files: ... }` (clippy `use_self`).
- **Safe to mechanize:** yes · **Collision:** SAFE NOW
- **EXCLUDE (false positive):** `notes/src/lib.rs:56,68` clippy "argument passed
  by value but not consumed" on `id: String` in `update_note`/`delete_note`.
  These are **registered actions** — `tangram-macros` deserializes args into a
  generated `#[derive(serde::Deserialize)]` struct via `serde_json::from_value`,
  so params must be owned `Deserialize` types. Changing to `&str` breaks the
  macro. Do not change.

### SAFE-4 · Doc-backtick cleanup in app doc-comments
- **Category:** doc-drift (style) · **Severity:** low · **Effort:** S
- **Files:** `apps/tangram/src/lib.rs`, `apps/marketplace/src/lib.rs`
- **Change (mechanical):** wrap bare code/identifier tokens in backticks where
  clippy flags `doc_markdown`:
  - `tangram/src/lib.rs:22,62` and the over-long first paragraph at `:50`.
  - `marketplace/src/lib.rs:61` (e.g. `wasm32-wasip2`).
- **Safe to mechanize:** yes · **Collision:** SAFE NOW
- **Notes:** the `#[allow(clippy::too_many_arguments)]` at
  `marketplace/src/lib.rs:154` and `registry/src/lib.rs:230` are justified
  (multi-field install/listing actions) — leave them.

### SAFE-5 · `tangram-core` test reflow
- **Category:** code-quality · **Severity:** low · **Effort:** S
- **Files:** `crates/tangram-core/tests/rmcp_parity.rs`
- **Change (mechanical):** `rmcp_golden_flows_replay_semantically` at line 39 is
  flagged `too_many_lines` (111/100); also one `unnested or-patterns` and a
  `u64 as u16` / `usize as u32` cast hint in this crate's test/fixture code.
  Split the test body into a small helper (or `#[allow]` with a one-line
  rationale) and collapse the or-pattern. Cosmetic; no assertion changes.
- **Safe to mechanize:** yes · **Collision:** SAFE NOW

### SAFE-6 · Cloudflare SSE enqueue de-dup
- **Category:** duplication / simplification · **Severity:** low · **Effort:** S
- **Files:** `cloud/cloudflare/src/index.ts`
- **Change (mechanical):** the two near-identical `try { controller.enqueue(...) }
  catch { <remove from set> }` blocks in the `api/events` and `sync` SSE streams
  (around lines 213–217 and 255–259) can be hoisted into a
  `safeEnqueue(controller, payload, set)` helper. Pure refactor; behavior
  identical.
- **Safe to mechanize:** yes · **Collision:** SAFE NOW
- **Notes:** two larger cloud items are deliberately **excluded** as
  feature-gaps, not quality fixes: (a) no idle session-eviction in the DO vs
  the native host, (b) no ADR-0005 egress credential injection in `shim.ts`.
  Both are design/feature work, out of scope for a mechanical pass — record as
  backlog.

---

## DEFER — touch in-flight files (follow-up batch)

These overlap branches #40 (manifest verification, `tangram-host/src/{verify,
config,runtime,app,routes,registry,tenant,main}` + `tests/verification.rs`),
#41 (fine-grained egress, `tangram-host/src/*` + `tests/*`), and #42 (infra:
`.github/workflows/ci.yml`, `.config/nextest.toml`, `.cargo/config.toml`). Hold
until those merge to avoid conflicts.

### DEFER-1 · Consolidate the tangram-host test harness onto `tests/support`
- **Category:** duplication · **Severity:** med · **Effort:** M
- **Files:** `crates/tangram-host/tests/{artifact_upload,default_view,
  egress_injection,federated_fleet,frame_ancestors,gateway_lifecycle,
  marketplace_lifecycle,registry_lifecycle,tenant_lifecycle,capabilities}.rs`
  → fold into `crates/tangram-host/tests/support/mod.rs`
- **Change:** a `tests/support/mod.rs` already exists with `workspace_root`,
  `component`, `HostProc`, `wait_for`, `status_of`, `free_port`. But ~8 test
  files still define their **own** `spawn_host` (9 copies), `workspace_root`,
  `free_port`, `build_artifacts`, `http_get`, `wait_healthy`, `write_apps_toml`,
  `serve`/`url`/`hits` (the fake-artifact server in `federated_fleet.rs` and
  `marketplace_lifecycle.rs` is duplicated). Promote the common variants into
  `support/` and delete the per-file copies.
- **Safe to mechanize:** no (needs care to unify slightly-divergent signatures)
- **Collision:** DEFER (in-flight: tangram-host) — #40 and #41 both rewrite
  these test files; consolidating now would conflict hard.

### DEFER-2 · tangram-host test-file clippy hits
- **Category:** code-quality · **Severity:** low · **Effort:** S
- **Files:** `crates/tangram-host/tests/{egress_injection.rs:97,
  registry_lifecycle.rs:45, artifact_upload.rs:67, frame_ancestors.rs:90,
  e2e_cloudflare_sync.rs:10, gateway_lifecycle.rs, federated_fleet.rs:15,
  tenant_lifecycle.rs:10}`
- **Change:** `too_many_lines` test fns, `map().unwrap_or_else()` →
  `map_or_else`, `Debug` formatting in `panic!`, missing doc backticks. Largely
  subsumed by DEFER-1 once helpers move out.
- **Safe to mechanize:** yes (but in files being rewritten)
- **Collision:** DEFER (in-flight: tangram-host)

### DEFER-3 · tangram-host src clippy / suppressions
- **Category:** code-quality · **Severity:** low · **Effort:** S
- **Files:** `crates/tangram-host/src/{runtime.rs:224, auth.rs:96, gateway.rs}`
- **Change:** review `#[allow(clippy::too_many_arguments)]` (runtime.rs:224)
  and `#[allow(dead_code)]` (auth.rs:96 — confirm still needed); gateway.rs
  `expect`/`unwrap` are all in `#[cfg(test)]` blocks (fine). Mostly confirm-and-
  leave; defer because every one of these files is in #40/#41's edit set.
- **Safe to mechanize:** partial
- **Collision:** DEFER (in-flight: tangram-host)

### DEFER-4 · `runtime.rs` egress secret-in-log invariant check
- **Category:** project-invariant · **Severity:** low (verify) · **Effort:** S
- **Files:** `crates/tangram-host/src/runtime.rs:119,145,148`,
  `crates/tangram-host/src/secrets.rs:178,206`
- **Change:** the invariant scan flags `expose_secret()` calls; on reading,
  these attach credentials to outbound request builders / headers — **not** to
  any `tracing`/`println` sink, so the "no secrets in logs" invariant holds.
  No fix needed; recorded so a reviewer doesn't re-flag it. Defer because #41
  (egress) is actively rewriting exactly this code — re-verify post-merge.
- **Safe to mechanize:** no (verification only)
- **Collision:** DEFER (in-flight: tangram-host)

---

## Validated NON-findings (checked, nothing to do)

- **Project invariants — all PASS** across `apps/*`: additive `Option<T>` model
  fields carry `#[autosurgeon(missing = "Option::default")]`
  (`notes:Note.updated_at_ms`, `tangram:MdFile.updated_at_ms`,
  `registry:AppSpec.{component_url,component_sha256,data_dir}`); every `Default`
  is deterministic (`Vec`, seeded from `include_str!`); actions are
  sync-or-`Ctx`-async with no lock held across `await` (nutrition's `log_meal`
  resolves outside the lock then `ctx.mutate`); all UI fetches are relative.
- **`tangram-core` stays portable** — no tokio/hyper/axum/reqwest/rmcp in its
  `Cargo.toml`; the wasm32-wasip2 constraint holds.
- **Doc/ADR drift — mostly clean.** ADR-0006's "Wasmtime fuel/memory limits not
  yet configured" is **accurate** (grep for `fuel`/`StoreLimits`/
  `ResourceLimiter` in `tangram-host/src` is empty). ADR-0002's in-memory-MCP-
  session claim is accurate. AGENTS.md:37's `registry::Federation` reference is
  **correct** — the type lives in `crates/tangram-host/src/registry.rs` (the
  host's `registry` module), not the `apps/registry` crate; Phase 9 federation
  is genuinely delivered (`RUNTIME_PLAN.md:530`). No drift fix needed here.
- **`cargo-machete`** clean; documented package names (`tangram-notes`,
  `-shell`, `-host`, `-nutrition`, `-registry`) all exist.
