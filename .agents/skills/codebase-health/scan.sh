#!/usr/bin/env bash
# Mechanical half of the codebase-health review: run every cheap signal we
# have and print a sectioned report to stdout. Tools that aren't installed are
# noted and skipped (never fatal) — the point is breadth, and the agent
# reasons over whatever is present. Run from the repo root.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1
have() { command -v "$1" >/dev/null 2>&1; }
section() { printf '\n========== %s ==========\n' "$1"; }

section "SIZE / SHAPE (tokei)"
have tokei && tokei --sort code || echo "skip: tokei not installed (cargo install tokei)"

section "CLIPPY — pedantic + nursery (suggestions, NOT the -D warnings gate)"
cargo clippy --workspace --all-targets --quiet -- \
  -W clippy::pedantic -W clippy::nursery -A clippy::missing_errors_doc \
  2>&1 | grep -E "^(warning|  -->|note: )" | head -120 || true

section "DEAD CODE / SUPPRESSIONS (grep heuristics)"
echo "-- #[allow(...)] suppressions:"
grep -rn "#\[allow(" --include="*.rs" crates apps | grep -v "/target/" | head -40
echo "-- unwrap()/expect()/panic! in NON-test src (reachable-panic smell):"
grep -rn "\.unwrap()\|\.expect(\|panic!(" --include="*.rs" crates apps \
  | grep -vE "/tests/|/target/|#\[test\]|mod tests" | head -40

section "UNUSED DEPENDENCIES (cargo-machete)"
have cargo-machete && cargo machete 2>&1 | head -40 || echo "skip: cargo-machete (cargo install cargo-machete)"

section "DUPLICATE TRANSITIVE DEPS"
cargo tree --duplicates 2>/dev/null | head -40 || true

section "DEPENDENCY AUDIT (cargo-audit)"
have cargo-audit && cargo audit 2>&1 | tail -30 || echo "skip: cargo-audit (cargo install cargo-audit)"

section "TEST COVERAGE (cargo-llvm-cov, summary)"
have cargo-llvm-cov && cargo llvm-cov --workspace --summary-only 2>/dev/null | tail -40 \
  || echo "skip: cargo-llvm-cov (cargo install cargo-llvm-cov) — fall back to reading tests/ by hand"
echo "-- #[ignore]'d tests (silently skipped):"
grep -rn "#\[ignore" --include="*.rs" crates apps | grep -v /target/ | head

section "CODE DUPLICATION (heuristic: repeated fn names across test files)"
echo "-- helper fns defined in multiple test files (copy-paste candidates):"
grep -rhoE "^\s*(async )?fn [a-z_]+" --include="*.rs" crates/*/tests apps/*/tests 2>/dev/null \
  | sed -E 's/^\s*(async )?fn //' | sort | uniq -c | sort -rn | awk '$1>1' | head -30
echo "(jscpd for real cross-file duplication if available:)"
have jscpd && npx --yes jscpd --silent --min-lines 12 --reporters consolefull crates apps 2>&1 | tail -25 \
  || echo "skip: jscpd (npx jscpd) — use the fn-name heuristic above + judgment"

section "PROJECT INVARIANTS (AGENTS.md 'conventions not obvious from code')"
echo "-- additive model fields missing the autosurgeon attr (Option<T> w/o #[autosurgeon(missing)]):"
echo "   (manual: grep models for Option fields, confirm each has #[autosurgeon(missing = ...)])"
echo "-- tangram-core must stay free of tokio/hyper/rmcp/axum/reqwest:"
grep -nE "tokio|hyper|rmcp|axum|reqwest" crates/tangram-core/Cargo.toml || echo "   OK: none in tangram-core deps"
echo "-- secrets in logs? (expose_secret near a tracing/println — should be none):"
grep -rn "expose_secret" --include="*.rs" crates apps | grep -v /target/ | head
echo "-- absolute /api or /sync fetches in UI (should be relative for prefix-mounting):"
grep -rn 'fetch("/\|EventSource("/\|href="/api\|"/sync' --include="*.html" apps | head

section "DOC / ADR DRIFT"
echo "-- ADRs that still say 'pending'/'not yet'/'no file exists' but may now be done:"
grep -rniE "pending|not yet|no file exists|TODO" docs/adr/*.md | head -20
echo "-- README/AGENTS commands — spot-check these still run:"
grep -oE "cargo (run|build|test)[^\`\"]*" README.md AGENTS.md 2>/dev/null | sort -u | head

section "TODO / FIXME / HACK markers"
grep -rniE "TODO|FIXME|HACK|XXX" --include="*.rs" --include="*.ts" crates apps cloud | grep -v /target/ | head -30

echo; echo "scan complete — reason over the above, dedupe, prioritize."
