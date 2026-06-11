---
name: codebase-health
description: Periodic codebase health review — surface simplification, duplication, test-coverage, code-quality, dependency, project-invariant, and doc-drift opportunities, then produce a prioritized, scoped list ready to fan out as fix tasks. Use when asked to review code quality, find simplifications, check coverage, reduce duplication, or do a health/tech-debt pass.
argument-hint: "[--report <path>]   (default: codebase-health-report.md)"
allowed-tools: Bash, Read, Grep, Glob, Write
---

A breadth-first health sweep. Run the mechanical scan, then **reason over it
with judgment** — the tools surface candidates; you decide what's a real,
worthwhile, *scoped* improvement. Output a prioritized report another agent
(or you) can fan out into independent fix tasks.

## Procedure

1. `bash ${SKILL_DIR}/scan.sh > /tmp/health-scan.txt 2>&1` (run from repo root),
   then read it. It runs: tokei (size), clippy pedantic/nursery (suggestions
   beyond the `-D warnings` gate), allow()/unwrap()/panic! greps,
   cargo-machete (unused deps), duplicate transitive deps, cargo-audit,
   cargo-llvm-cov (coverage) + ignored-test scan, a duplication heuristic
   (+ jscpd if present), the project-invariant checks, and doc/ADR drift.
   Missing tools are skipped, not fatal — note coverage gaps in the report.
2. Go beyond the scan where it's shallow: open the files it flags, and read
   the hot spots by hand (the lifecycle test harnesses, the host converge
   loop, registry/tenant merge, the sync client) for the categories below.

## What to look for (the full set)

- **Simplification**: single-impl traits / single-caller indirection that
  could inline; needless `clone`/`to_string`/alloc; over-long or high-
  complexity fns; redundant error-wrapping boilerplate; options/flags never
  exercised; abstraction that doesn't earn its keep.
- **Duplication**: copy-pasted blocks across crates/apps (the per-test
  `spawn_host`/`wait_for`/`free_port` harnesses are a known offender — a
  shared test-support crate is the likely fix); repeated constants/strings;
  logic that belongs in `tangram-core` but is reimplemented in `tangram-host`.
- **Test coverage**: modules/paths with none (sync conflict resolution, auth
  edge cases, error-envelope mapping, converge failure modes); `#[ignore]`'d
  or self-skipping tests that quietly don't run; flaky/timeout-prone tests.
- **Code quality / idiom**: `unwrap`/`expect`/`panic!` reachable from request
  handling; anyhow-vs-thiserror boundary consistency; public-API doc coverage;
  module organization; pedantic-clippy suggestions worth adopting.
- **Dependencies**: unused (machete), vulnerable (audit), duplicate versions,
  feature bloat.
- **Project invariants** (AGENTS.md "conventions not obvious from code"):
  actions sync/no-IO or async-via-`Ctx`; additive model fields carry
  `#[autosurgeon(missing = ...)]`; `tangram-core` free of tokio/hyper/rmcp;
  UI fetches relative; deterministic `Default`; no secrets in logs (no
  `expose_secret` near logging). Flag any violation — generic linters miss these.
- **Doc / ADR drift**: README/AGENTS/RUNTIME_PLAN claims vs reality; ADRs that
  say "pending" for things now shipped; dead commands/paths.

## Output (write to `--report`, default `codebase-health-report.md`)

A ranked list. For EACH opportunity:
`title · category · severity (high/med/low) · effort (S/M/L) · files · concrete suggested change · safe-to-mechanize? (yes/no) · risk/notes`.
Dedupe overlapping findings; **group by which files they touch** so fan-out
workers don't collide. Put a 3-line executive summary on top (themes + the
top 3). Exclude anything speculative or behavior-changing-without-clear-win —
this is a quality pass, not a redesign. State what coverage you could NOT
measure (missing tools).
