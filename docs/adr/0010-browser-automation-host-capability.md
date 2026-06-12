# ADR-0010: Browser automation is a host capability, never a WASM-component WIT import

**Status:** accepted (2026-06-12)
**Deciders:** Aaron (owner), with analysis by Claude
**Related:** ADR-0001 (WASM-first runtime — what the browser is *not*),
ADR-0004 (the secret-resolver seam — the `op://` scheme this implements),
ADR-0005 (egress credential injection — the *exposure* axis this mirrors onto
the browser), ADR-0006 (tenant isolation / gVisor — where the browser process
runs); `docs/design/task-automation-browser.md` (the spec, AC0-AC8); PR #1
fine-grained egress (the canonicalization seam this will consume); RUNTIME_PLAN.

## Context

We want to complete real-world tasks by driving a web browser under LLM
guidance (the first consumer: turn a grocery list into an Amazon cart and
stop before checkout). A browser is a far larger, more dangerous surface than
`http-fetch`: it executes adversarial JS, loads cross-origin sub-resources,
follows redirects, and — uniquely — **feeds page content into the LLM's
context**, opening a prompt-injection path `http-fetch` never had. It also
needs a real credential (the Amazon sign-in) at the login step.

The architectural question: does a WASM component get a "drive a browser"
capability, or does the browser live elsewhere?

## Decision

**Browser automation is a *host* capability. No new WIT import is added; a
WASM component cannot drive a browser.** A component can only *request* an
automation through the existing action/data plane (an `AutomationRequest`
naming a pre-approved **template by id**, parameters, and credential
*references*); the host's runner picks it up, intersects it with operator
policy, and executes it out-of-band. The browser, the LLM-planner, the
1Password token, and the recorded scripts all live host-side.

Concretely, a new native-only crate `crates/tangram-automation` (NOT
`wasm32-wasip2`-clean — `tangram-core` and every component stay
browser-unaware) implements four reusable primitives, supervised by
`tangram-host` the way `gateway.rs` supervises agentgateway:

- **A. Host-side runner** — a supervised browser-driver child (the
  `gateway.rs` `Backoff`/shutdown/`kill_on_drop` pattern, reused), gated
  behind `[automation]` in `apps.toml`, default-off, missing-driver →
  warn-and-disable.
- **B. Browser egress gate** — Allow/Deny on **every** browser request,
  decided on **parsed** `(host, path)` components (canonicalize once; never
  string-suffix match — the SOCKS5 parser-differential lesson). Default-deny.
  A call-level path-deny is the network-layer backstop for the order-submit
  endpoint. *Implemented here as a focused canonicalizer with a `TODO` to
  unify with PR #1's shared seam when it merges, so the browser fence and the
  component fence can never disagree on what a host means.*
- **C. Credential broker** — the `op://` `SecretResolver` (ADR-0004 follow-on,
  via the `op` CLI + `OP_SERVICE_ACCOUNT_TOKEN`) resolves a credential
  *reference* host-side at the point of use and `fill`s it into the field; the
  value never enters the LLM snapshot (field masked), the recorded script (it
  stores the reference), or any log — the ADR-0005 inject-at-the-boundary
  model, applied to the browser.
- **D. Record → replay → validated LLM-fallback** — a run records an
  `AutomationScript` of small declarative steps with **semantic** locators
  (a11y role+name, not CSS) and per-step `expect` post-conditions. Replay is
  deterministic and LLM-free; on divergence the LLM returns
  continue/recover/hand-off/abort, but **the runner validates that choice** —
  it can never skip a `stop_gate` nor navigate off the domain allowlist (those
  are enforced by host code *around* the model, not by asking it).

The request is an **upper bound intersected with operator policy**, never
authority on its own: a component cannot name browser commands directly, widen
its domains beyond the operator ceiling, or choose a credential outside its
grant — the same posture as `describe()` egress declarations and tenant
ceilings.

## Consequences

- **The sandbox invariant stays crisp.** The WASM world is unchanged; the most
  dangerous machinery (adversarial-content processing, a real credential, the
  LLM planner) is host-side, gated, and out of every component's reach. This
  is the single most important property and the reason browser control is not
  a WIT import.
- **Prompt injection cannot escalate.** The domain gate is deterministic and
  out-of-band, so no page content can steer the browser off-allowlist; the
  credential is never in LLM context; irreversible actions sit behind
  `stop_gate`s a human must pass. The model has no privileged action the gate
  doesn't independently authorize.
- **Where the browser runs (ADR-0006).** The browser is exactly the *native*,
  untrusted-input-processing workload ADR-0006 retained gVisor (Track G) for:
  run it under tighter OS confinement (dedicated low-priv user, seccomp/ns,
  ephemeral profile, gVisor for the untrusted tier). The secret is in the
  browser's address space only for the duration of one `fill`.
- **Most-dangerous-workstream merge posture.** This lands on a dedicated
  branch, **held for human review, not merged**, and the live Amazon run (AC8)
  is gated separately behind explicit owner approval even after the code is
  reviewed. Building the cart and stopping before checkout is the entire
  deliverable; placing an order is never autonomous.

## Alternatives considered

- **A `browser` WIT import for components.** Rejected: it would put
  adversarial-content processing, a real credential, and an LLM loop *inside*
  the sandbox's reach and make every component a potential browser driver —
  the opposite of the containment ADR-0001/0006 buy us.
- **Driving the browser purely through the Playwright MCP server.** Useful as
  the a11y-snapshot *vocabulary* and a dev driver, but the egress gate and the
  credential `fill` must be enforced by host code, not delegated one process
  away across a tool boundary (the secret would transit the MCP `type` call).
  The host owns the `BrowserContext`; the MCP snapshot format is the LLM-facing
  representation only.
- **General computer-use / arbitrary desktop control.** Out of scope: one
  headless(-or-headed-for-debug) browser tab on the allowlisted domains.
