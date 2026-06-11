# ADR-0006: Tenant isolation posture for co-resident WASM

**Status:** accepted (2026-06-11)
**Deciders:** Aaron (owner), with research by Claude
**Related:** the full analysis in
[`docs/security/tenant-isolation-review.md`](../security/tenant-isolation-review.md);
ADR-0005 (egress credential injection — the load-bearing mitigation);
ADR-0001 (WASM-first runtime)

## Context

Tenants' (and apps') code runs as `wasm32-wasip2` components inside **one
shared `tangram-host` process** under one Wasmtime engine; multi-tenancy
(Phase 5) co-locates different tenants' apps in that same process. We needed
to know whether that is a sound isolation boundary, specifically against a
malicious component leaking a co-resident component's secrets via
microarchitectural (timing / cache / branch-predictor / SMT) side-channels,
and whether a hypervisor or gVisor would change the answer. The detailed,
sourced investigation is in the security review; this ADR records the
resulting policy decision.

## Findings (condensed)

- **WASM gives memory isolation, not microarchitectural isolation.** Separate
  linear memories + the closed WIT world (no fs/sockets/inbound-HTTP) +
  Wasmtime's Spectre-hardened bounds checks stop a component reading another's
  memory directly. They do **not** address shared L1/L2/LLC, branch
  predictors, TLB, or SMT sibling-thread contention — co-resident components
  share those physical resources, and Wasmtime makes no side-channel claim.
- **gVisor does not fix this.** Its own docs state it provides no protection
  against hardware side-channels; it is a *syscall* barrier (protects the host
  kernel from a guest), not a *cache* barrier. The same limit largely applies
  to plain process isolation against SMT/LLC channels.
- **Hardware separation is the only complete defense** — and it applies
  identically to "tenant vs tenant" and "component vs component," because both
  are just co-resident WASM instances; there is no isolation asymmetry between
  them.
- **But the asset usually isn't there to steal.** A side-channel can only leak
  what a victim holds and *computes on*. Tangram's high-value secrets are API
  keys that, after ADR-0005, never enter a component's address space (the host
  attaches them at the `http-fetch` egress boundary). Removing the secret from
  the tenant beats hardening the channel, and works independent of
  process/core/hardware isolation.

## Decision

Adopt a **tiered tenancy isolation policy**, with egress credential injection
(ADR-0005, delivered) as the baseline that neutralizes the dominant
key-leak concern at every tier:

| Trust tier | Isolation required |
|---|---|
| **First-party apps (today)** | In-process WASM is sufficient. Memory isolation + closed world + egress injection. No hardware separation needed. |
| **Semi-trusted tenants** | In-process WASM **only if** ADR-0005 holds for all secrets, **plus** disable SMT co-scheduling of distrusting tenants and set Wasmtime resource limits (fuel/memory — not yet configured; tracked as future work). |
| **Untrusted third-party (marketplace SaaS)** | In-process WASM is **insufficient**. Require process-per-tenant with no cross-tenant core co-residency, SMT off, LLC partitioning (CAT), ADR-0005 mandatory; separate physical cores/machines for the strongest guarantee. |

Corollary: **gVisor is not on this ladder** for the side-channel threat — it
was never the right lever (syscall, not microarchitectural). It remains
relevant only for *native* (non-WASM) untrusted apps as a kernel-attack-surface
reducer (Track G in RUNTIME_PLAN), not as a co-residency side-channel defense.

## Consequences

- The WASM-first decision (ADR-0001) stands; the isolation gap is addressed by
  *removing secrets from components* + *tenancy tiering*, not by adding a
  sandbox layer.
- ADR-0005 (egress injection) is reclassified from "hardening" to a
  **prerequisite** for any semi-trusted-or-beyond tenancy.
- Before opening the marketplace to untrusted third-party apps (the Phase 8
  TODO), the untrusted-tier requirements above are mandatory and must be
  designed in — process-per-tenant + core/SMT/CAT controls — not retrofitted.
- Open follow-up (not yet ticketed): set Wasmtime fuel/memory limits per
  component (a cheap robustness win independent of tier).
