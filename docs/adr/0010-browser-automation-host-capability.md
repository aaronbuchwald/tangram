# ADR-0010: Browser automation is a host capability, never a WASM-component WIT import

**Status:** accepted (2026-06-12)
**Deciders:** Aaron (owner), with analysis by Claude
**Related:** ADR-0001 (WASM-first runtime — what the browser is *not*),
ADR-0004 (the secret-resolver seam — the `op://` scheme this implements),
ADR-0005 (egress credential injection — the *exposure* axis this mirrors onto
the browser), ADR-0006 (tenant isolation / gVisor — where the browser process
runs); `docs/design/task-automation-browser.md` (the spec, AC0-AC8); ADR-0008
fine-grained egress (the canonicalization seam this consumes, now the shared
`crates/tangram-egress` leaf crate); RUNTIME_PLAN.

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
  endpoint. *The browser gate (`crates/tangram-automation/src/egress.rs`)
  consumes the shared `crates/tangram-egress` canonicalizer — the same one the
  component fence and the manifest verifier use — so they can never disagree on
  what a host or path means.*
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
- **Most-dangerous-workstream posture.** The substrate
  (`crates/tangram-automation`) was reviewed separately and merged; the live
  Amazon run (AC8) remains gated behind explicit owner approval. Building the
  cart and stopping before checkout is the entire deliverable; placing an order
  is never autonomous.

## Session reuse: make login + CAPTCHA a one-time cost

The first AC8 run paused at Amazon's CAPTCHA — the expected, designed behavior
(we never bypass a human-verification challenge). To stop paying that cost on
*every* run, the substrate adds an **upfront preflight → decision → persist**
flow. The crux: an authenticated session is persisted and reused, so after the
first solve the common path skips login entirely.

### The flow (run first, every time)

1. **Preflight — "are we already signed in?"** (`preflight.rs`). Before any
   login attempt, the runner loads the *persisted session* into the browser
   context, navigates to an auth-gated page (Amazon account/cart), and
   classifies one a11y snapshot: a **signed-in indicator** (the account menu /
   "Hello,") vs a **sign-in form** (the password field). This is cheap and
   deterministic. Outcomes: `SignedIn` (→ straight to the task, NO login, NO
   CAPTCHA, NO credential fetch), `NoSession` (first run), `Expired` (had a
   session, cookies lapsed), `NotSignedIn` (session present but the server
   rejected it — soft invalidation). The live page is authoritative: a
   signed-in indicator wins regardless of the cookie clock.

2. **Decision point** (`decision.rs`) — only when NOT signed in. The runner
   does not silently start a login; it surfaces a structured `AssistanceRequest`
   through the same request/approval channel as the rest of the project
   (`request.rs`), stating the two paths, the credential references the headed
   path would use (references, never values), and the LLM-path token budget:

   - **(a) Interactive headed solve** (one-time, human-assisted). A *headed*
     browser with the persistent profile; credentials filled from `op`
     in-process (the existing broker discipline — never in LLM context); then
     PAUSE for the human to solve the CAPTCHA / 2FA; detect auth success;
     persist the session. **Headless-box reality:** this box has no display, so
     the interactive solve realistically runs on the **owner's machine** (or via
     a VNC bridge) and the resulting `storageState` JSON is carried back here —
     see the handoff below.
   - **(b) LLM-assisted CAPTCHA solve** (bounded). The multimodal Anthropic
     model (the `CLAUDE_CODE_OAUTH_TOKEN` bearer) reads the challenge and
     proposes a solution under a **hard `LlmCaptchaBudget`** (`max_attempts` +
     `max_tokens`, both parameters). The loop refuses an attempt that would
     overshoot the token ceiling *before* spending it, and on exhaustion **falls
     back to path (a)** — fail safe. This can burn significant tokens and often
     fails (Amazon image puzzles are hard); the UX says so.

3. **Both paths converge** on a verified authenticated session, which is
   persisted (#4 below). The CART-ONLY / hard-stop-before-checkout gate is
   unchanged and still holds.

### Durable session reuse (the persisted session IS a credential)

Two shapes, both implemented (`session.rs`):

- **Persistent `userDataDir`** (`launchPersistentContext`) — the full profile
  including a stable device fingerprint, which **reduces Amazon re-challenges**.
  Site-local, not portable across machines.
- **`storageState` export** (`context.storageState({path})`) — portable cookies
  + localStorage JSON. This is the **handoff format** from an interactive solve
  done elsewhere.

**Recommendation:** use the **persistent `userDataDir`** on the box that runs
replays (fewer re-challenges from the stable fingerprint), and use
**`storageState`** as the portable handoff to seed it from an interactive solve
done on the owner's machine. They compose: solve interactively → export
`storageState` → carry it back → import into the persistent profile here.

A persisted session is an **auth bearer**, so it is handled like a credential:
stored OUTSIDE the repo (default `~/.tangram-automation/profiles/<site>/`),
gitignored by location, `0o600` files / `0o700` dirs, never logged (only a
redacted `summary()` of counts + earliest expiry), never embedded in a recorded
script. `PersistedSession::assert_outside_repo` refuses any root under a git
working tree. The session may optionally be **sealed into 1Password** via an
`op://` reference (`SealedSessionRef`) and restored on demand through the broker
— the crate carries only the reference, never the blob.

**Expiry / invalidation** is detected in the preflight: `StorageState::is_expired`
treats an artifact as expired when every persistent cookie has lapsed (and a
session-cookie-only artifact as already expired, since those don't survive a
reload). Amazon cookies *do* expire (weeks, not forever), so re-auth recurs —
just rarely. An expired/rejected session falls back to the decision point and
the stale artifact is invalidated before re-auth.

### Headless-box → owner-machine handoff (interactive solve)

Because this box is headless, the interactive (human-assisted) solve runs where
a human and a display are: the **owner's machine**, or a **VNC bridge** to a
headed browser here. The portable artifact makes this clean:

1. On the owner's machine, launch a headed browser, sign in to Amazon, solve the
   CAPTCHA/2FA by hand.
2. Export the session: `context.storageState({ path: 'amazon-storage-state.json' })`.
3. Copy that JSON to this box at
   `~/.tangram-automation/profiles/www.amazon.com/storage-state.json` (perms are
   re-restricted to `0o600` on save) — treat it like any credential in transit.
4. The next preflight here loads it, sees the signed-in page, and proceeds to
   the task with no login. (Cookies eventually expire → repeat, rarely.)

The same artifact can be sealed into 1Password (`op item create`/`edit`) and
pulled with `op read` on the box, so the bearer lives in the vault rather than
only on disk.

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
