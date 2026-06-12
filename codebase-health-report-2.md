# Codebase Health — Consolidation Pass 2

## Executive summary (3 lines)
- The repo is in good shape: the clippy `-D warnings` gate is already green, the prior consolidation pass cleaned the SDK facade / core / app `lib.rs` files / `cloud/cloudflare`, and the active churn lives in 8 open PRs whose files are off-limits.
- **Net result: mostly DEFER.** There were **no compelling, conflict-free SAFE-NOW code changes** to apply — the remaining candidates in the allowed areas are stylistic and would be manufactured churn (spreading call sites or adding cross-app coupling), so they were declined with reasons rather than applied.
- The substantive findings (test-harness duplication, the Wasmtime fuel/memory backlog item, secrets/manifest/egress work) all touch files owned by the open PRs and are queued below as a post-merge follow-up batch.

## What was applied
Nothing beyond this report. After surveying every allowed file/dir, no behavior-preserving change cleared the "clear win, conflict-free, not already-done" bar. An honest "little SAFE-NOW left" is the correct outcome here, per the task framing — the prior pass plus the 8 in-flight PRs already cover the live surface.

Gate was re-run and confirmed green before writing this (see Gate result).

## SAFE-NOW candidates examined and DECLINED (with reasons)
These touch only allowed files but are **not clear wins**, so they were intentionally not applied:

| Candidate | Files | Why declined |
| --- | --- | --- |
| Remove local `now_ms()` pass-through wrappers | `apps/notes/src/lib.rs:87`, `apps/nutrition/src/lib.rs:559`, `apps/tangram/src/lib.rs:250` | Each is a one-line convenience wrapper over `tangram::time::now_ms()` used at 2–5 call sites. Removing it spreads the fully-qualified path across many call sites and removes the single swap-point for the time source — a stylistic preference, not a behavior or clarity win. These `lib.rs` files were already in the prior pass's scope and left as-is. |
| Unify duplicated `validate_name()` | `apps/registry/src/lib.rs:104`, `apps/marketplace/src/lib.rs:120` | The two impls are byte-identical, but each is private to its own app's action contract. Extracting to a shared `tangram::validation` module introduces cross-app coupling for ~12 lines and is a (small) redesign, not a mechanical dedupe. Low ROI; the local duplication doubles as locally-clear documentation. |

Everything else in the allowed areas (`crates/tangram`, `crates/tangram-core`, `crates/tangram-macros`, `cloud/cloudflare/src/*`, the app `src/` trees) was clean: no unused imports/fns/fields, no commented-out code, no dead match arms, `cargo machete` finds no unused deps, and `tsc --noEmit` passes for the Cloudflare worker.

### Pedantic/nursery noise explicitly out of scope
The scan's clippy output is dominated by `pedantic`/`nursery` suggestions that are **not** part of the `-D warnings` gate and were correctly ignored: missing `# Errors`/`# Panics` doc sections, `doc_markdown` backticks, `needless_pass_by_value` on registered-action owned args (a deliberate leave-as-is), `missing_const_for_fn`, and `redundant_clone` hits that are all in test code (`crates/tangram/src/app.rs:139`, `apps/tangram/src/lib.rs:341`, `apps/marketplace/src/lib.rs:484`). Lock-poisoning `.expect("...lock")` calls are the documented leave-as-is.

## DEFER (in-flight) — follow-up batch for after the open PRs merge
Grouped by which open PR owns the files. Do NOT touch until merged; revisit then.

### `crates/tangram-host/tests/*` (PRs #1 egress, #2 manifest, #6 GL host test, #8 secrets)
- **Test-harness duplication** — the scan's top duplication signal: `spawn_host` defined in **9** test files, plus repeated `free_port`/`wait_healthy`/`write_apps_toml`/`build_artifacts`/`serve`/`sse_json`/`http_get`/`sha`/`post` helpers across `capabilities.rs`, `frame_ancestors.rs`, `federated_fleet.rs`, `gateway_lifecycle.rs`, `marketplace_lifecycle.rs`, etc. **Fix:** a shared `tangram-host` `tests/support/` (or a small `dev-dependencies` test-support crate) once the four host-test PRs have landed and the harness shape has stabilized. Doing it now would conflict with every one of those PRs.

### `crates/tangram-host/src/*` (PRs #1 egress, #2 manifest, #6, #8 secrets.rs)
- `runtime.rs:224` `#[allow(clippy::too_many_arguments)]` and the egress/secret-injection paths (`runtime.rs:119/145/148` `expose_secret` into request builders) — review for a params struct / consolidation only after #1 and #8 settle; these lines are exactly what those PRs edit.
- `auth.rs:96` `#[allow(dead_code)]` — confirm whether still needed after the auth/secrets PRs land.

### `docs/adr/0006-tenant-isolation-posture.md` (couples to tangram-host engine config)
- ADR-0006 (lines ~54/70/72) records "Wasmtime per-component fuel/memory limits **not yet configured**" as a tracked backlog item. If/when an in-flight PR adds those limits to `tangram-host`'s engine config, this ADR needs updating to match — defer the doc edit until the code lands so the doc doesn't drift the other way.

### Other protected paths (no findings, listed for completeness)
- `apps/marketplace/ui/index.html` (#2), root `Cargo.toml` / `apps.toml` / `AGENTS.md` (#5/#6/#7/#8 registration edits), `.github/`, `.config/nextest.toml`, `.cargo/config.toml` (#3): not inspected for changes — editing them conflicts with the named PRs.

## Coverage I could NOT measure (missing tools)
- `cargo-llvm-cov` not installed — no line-coverage numbers; assessed tests by reading instead.
- `cargo-audit` not installed — no vulnerability scan this pass.
- `jscpd` not installed — cross-file duplication assessed via the fn-name heuristic + manual reading.

## Gate result
Green (pre-existing, unchanged by this pass; no code was modified):
- `cargo clippy --workspace --all-targets -- -D warnings` → exit 0
- `tsc --noEmit` (cloud/cloudflare) → clean
- `cargo machete` → no unused dependencies
