# Tangram test-suite runtime report

## Executive summary

- **Full workspace test wall-clock: ~348s (`cargo test`) → ~200s today** by
  switching to `cargo nextest` + capping the host-spawning integration tests
  into one bounded `host-integration` group. That single change also **made
  the flaky `gateway_lifecycle` 502 disappear** (it was caused by CPU
  oversubscription starving the agentgateway child).
- **Top 3 levers:** (1) nextest + a bounded test-group for the 12 tangram-host
  integration binaries (landed); (2) a fast-fail unit lane — ~10s of test
  execution covers 96/112 tests, the real inner loop and a fast CI signal
  (landed); (3) tightening the per-test host startup + convergence inside the
  lifecycle tests, where the deep remaining time lives (recommended — touches
  test files owned by the in-flight agents).
- The integration tests are **IO/convergence-bound, not compile-bound**: they
  use the prebuilt `CARGO_BIN_EXE_tangram-host`, the wasmtime compile cache is
  hit (126 MB, persists across runs), so the cost is process spawn + sync
  rounds, multiplied by many hosts per test and amplified by parallelism.

## Measured baseline (16-core dev box, warm cache)

Built the wasm components first (so integration tests run, not self-skip):
```
cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
  -p tangram-marketplace -p tangram-app-tangram --lib \
  --target wasm32-wasip2 --release
```

| Run | Tool / config | Wall | Result |
|-----|---------------|------|--------|
| Build test binaries (`--no-run`, warm) | cargo | 41s | — |
| `cargo test --workspace` (warm) | cargo libtest | **348s** | pass |
| `cargo nextest run --workspace` (full fan-out) | nextest, no group | **244s** | 1 FAIL (gateway 502 flake) |
| `cargo nextest run` + host-integration cap=3 + retries | nextest, this branch | **~200-204s** | all pass, no flake |
| host-integration cap=4 | nextest | ~197-208s | all pass |
| host-integration cap=6 | nextest | ~229s | 1 flaky (retried) |
| **Unit-only lane** (`-E 'not (package(tangram-host) and kind(test))'`) | nextest | **~21s wall / ~9.5s exec** | 96 tests pass |

Cold build is dominated by compiling the dependency graph + the release wasm
components (the wasm build alone is a large chunk of any from-scratch CI job);
warm test execution is dominated entirely by the host integration tests.

### Top slowest tests (nextest, full fan-out — shows the contention)

| Duration | Test |
|----------|------|
| 244.3s | tangram-host::federated_fleet `federated_fleet_propagates_installs_removes_and_persists` |
| 235.0s | tangram-host::registry_lifecycle `registry_lifecycle_with_auth_and_restart_persistence` |
| 220.5s | tangram-host::tenant_lifecycle `tenants_are_isolated_authed_and_persistent` |
| 217.5s | tangram-host::marketplace_lifecycle `marketplace_to_registry_install_by_url_with_hash_verification` |
| 182.9s | tangram-host::frame_ancestors `host_served_app_emits_frame_ancestors_csp` |
| 178.3s | tangram-host::capabilities `capabilities_parity_native_vs_host` |
| 111.2s | tangram-host::default_view `root_redirects_to_shell_when_tangram_present` |
| 110.8s | tangram-host::artifact_upload `upload_computes_sha_serves_and_installs_and_rejects_garbage` |
| 109.2s | tangram-host::gateway_lifecycle `tenant_mcp_is_scoped_and_authed_through_the_gateway` |
| 109.1s | tangram-host::gateway_lifecycle `missing_binary_falls_back_to_direct_serving` |
| 105.7s | tangram-host::gateway_lifecycle `mcp_through_gateway...crash_recovery` (FLAKY 502) |
| 100.2s | tangram-host::bin fetch::tests `stores_a_real_component_and_computes_its_sha` |
| 81.4s  | tangram-host::egress_injection `host_injects_credential_at_egress...` |
| 67.2s  | tangram-host::artifact_upload `default_off_blocks_the_upload_route` |
| 66.5s  | tangram-host::default_view `root_falls_back_to_builtin_index_without_tangram` |

Everything else (all unit/bin tests, rmcp parity, SDK mcp) is **sub-second**.

### Where the time actually goes — the contention cliff

Run **in isolation** vs **under full fan-out**:

| Test | Alone | Full parallel | Inflation |
|------|-------|---------------|-----------|
| federated_fleet | **55s** | 244s | 4.4x |
| registry_lifecycle | **31s** | 235s | 7.5x |

The 12 tangram-host integration binaries each spawn multiple real host
processes (some spawn → kill → respawn for restart-persistence), and each host
instantiates several wasm components. At nextest's default one-thread-per-core
fan-out on a 16-core box, ~14 heavy tests launch at once → dozens of hosts
oversubscribe the CPU, every `wait_for` (100ms poll, 120s ceiling) stretches,
and the agentgateway child gets starved (the 502 flake). **Bounding this group
to 3 concurrent removes the cliff and the flake.** It does not slow the run
because the tests are IO-bound and still overlap each other's wait time.

What is NOT the problem: per-test compilation (binaries are prebuilt via
`CARGO_BIN_EXE_*`); wasm compilation (the `Config::cache` at
`$HOME/.tangram-host/wasmtime-cache` is hit — 126 MB, survives across runs);
fixed sleeps (only a couple of 100ms polls and one 5s sleep in marketplace).

### Parallelism / skip facts

- Separate `tests/*.rs` binaries DO run in parallel (both libtest and nextest);
  nextest additionally parallelizes across binaries by default, which is why it
  beat `cargo test` (244 vs 348) even before tuning.
- `e2e_cloudflare_sync.rs` is `#[ignore]`d in the default run (covered by the
  miniflare CI jobs) — correctly not counted.
- The host lifecycle tests `self-skip` (via `component(name).exists()`) when the
  wasm components aren't built — so a naive `cargo test` silently runs almost
  nothing. The CI pre-build step (and our wasm build) is what makes them run.

## Prioritized opportunities

Legend — Effort S/M/L; "Mechanizable" = safe to auto-apply; "Conflicts" = must
wait for the two in-flight tangram-host branches (manifest verification +
fine-grained egress) to merge.

### Landed on this branch now (conflict-free)

1. **`.config/nextest.toml` — bounded `host-integration` group + retries.**
   Saves ~40s and removes the gateway flake. Effort S. Mechanizable: yes.
   Conflicts: no (new file, no agent touches it). Caps the 12 tangram-host
   integration binaries to 3 concurrent; `retries = 2` reports transient passes
   as FLAKY rather than silently green.
2. **CI: adopt nextest in `check` + a fast-fail `unit` job.** `check` now runs
   `cargo nextest run --profile ci` (+ `cargo test --doc`, since nextest skips
   doctests). New `unit` job runs only the non-host tests (no wasm build) — a
   ~1-2 min fail-fast signal in parallel with the slow `check`. Uses
   `taiki-e/install-action@nextest` (prebuilt, no compile cost). Effort S.
   Mechanizable: yes. Conflicts: no (`.github/workflows/ci.yml` is shared, but
   the agents work under `crates/tangram-host`).

### Recommended — apply AFTER the build branches merge (touches owned files)

3. **Drop the in-test `cargo build` from `capabilities.rs` and
   `frame_ancestors.rs`.** Both shell out to `cargo build ...` *inside the
   test* to (re)build artifacts the CI pre-step already produced. Even warm,
   this pays cargo fingerprint-check overhead and serializes against nextest's
   own build, and it is why these two ranked among the slowest under contention.
   Replace with the same `component(name).exists()` self-skip the other
   lifecycle tests use (assert-built, don't build). Est. save: several seconds
   each + less contention. Effort S. Mechanizable: no (judgment). **Conflicts:
   yes** — these are `tests/*.rs` files. Never lose the coverage; only remove
   the redundant build.
4. **A shared host fixture for read-only happy-path assertions.** Several
   assertions (CSP header present, default-view redirect, capability probe)
   each spin up a *fresh* host purely to read one response. A
   `once_cell`/`OnceLock` shared host (or a single test that makes several
   assertions against one host) would amortize the ~spawn+instantiate cost
   across them. Est. save: 60-120s aggregate. Effort M. Mechanizable: no.
   **Conflicts: yes** (harness + test files). Flag for human design — do not
   merge coverage that needs isolation (restart-persistence, multi-host
   convergence) into a shared fixture.
5. **Tighten `wait_for` ergonomics in `support/mod.rs`.** The 120s ceiling is
   correct as a ceiling, but the happy path waits in 100ms steps and some tests
   chain many sequential `wait_for`s. A short initial fast-poll (e.g. 20ms for
   the first second, then 100ms) shaves startup latency off every host spawn
   across the suite without weakening the ceiling. Est. save: 10-30s aggregate.
   Effort S. Mechanizable: no. **Conflicts: yes** (`tests/support/mod.rs` is
   explicitly an agent-owned file).
6. **Cache the release wasm components as a CI artifact.** They are rebuilt in
   `check` and again in all three e2e jobs. Build once, `actions/upload-artifact`
   + `download-artifact` into the jobs that only consume them. Est. CI save:
   one wasm-release build per dependent job (minutes of CI, not local). Effort
   M. Mechanizable: partly. Conflicts: no (CI only) — but sequence after #2 to
   keep the workflow diff reviewable.
7. **`sccache` for compile time.** Marginal here: the dev loop is warm-cache
   (incremental rebuilds are already fast) and CI already uses
   `Swatinem/rust-cache`. Worth it only if cold CI compile becomes the
   bottleneck after the test-execution wins above land. Effort M. Don't
   prioritize.

## How to run the faster suite

```sh
# one-time: build the wasm components so integration tests don't self-skip
cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
  -p tangram-marketplace -p tangram-app-tangram --lib \
  --target wasm32-wasip2 --release

# fast inner loop (~10s exec): everything except the host-spawning tests
cargo nextest run -E 'not (package(tangram-host) and kind(test))'

# full suite (bounded group + retries, ~200s, no flake)
cargo nextest run

# what CI runs
cargo nextest run --profile ci && cargo test --doc
```
