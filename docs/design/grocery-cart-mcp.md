# Whole Foods grocery cart-fill MCP server

A Tangram app — `grocery-cart` — whose `fill_cart` MCP tool turns a structured
grocery list into a **filled (but never submitted) Whole Foods / Amazon cart**.
The app is sandboxed WASM; it never drives a browser. It only **requests** an
automation (an `AutomationRequest`, the request-not-grant channel of
[`task-automation-browser.md`](task-automation-browser.md) §4.3). A supervised
host task — the **request→runner dispatch loop** — picks the request up,
intersects it with operator policy, runs it, and writes a `CartFillResult` back
into the app's own document, where the `cart_fill_status` MCP tool surfaces it.

This is **Build-3** of the shopping automation, built **fixture-first**: GC1 (this
checkpoint) is the skeleton + the one previously-unbuilt seam (the dispatch loop),
with the runner execution stubbed to a deterministic dry-run. The live browser,
the real Whole Foods script, LLM item→product matching, and the live run all land
in later checkpoints. No code path exercised in CI or dev makes a live
network / browser / LLM / 1Password call.

## Why a separate app (not the `tangram` shell `fill_cart` stub)

`apps/tangram` already has a Smart-Objects SO5 `fill_cart` STUB: a pure,
review-only affordance that streams pipeline phase names and `purchased: false`.
That stub deliberately does **no** I/O and is the *UI demonstration* of the
cart-fill idea. The `grocery-cart` app is the *real automation surface*: a
first-class MCP server (`/grocery-cart/mcp`) whose tool emits an
`AutomationRequest` and is driven by the host dispatch loop against the
`tangram-automation` substrate. Keeping them separate means the shell demo stays
a zero-I/O mockup and the automation app carries the (later) live machinery and
its policy/egress gates without entangling the two.

## Topology decision: return-a-handle + a `cart_fill_status` poll tool

A cart fill is a **long, out-of-band browser automation** (login/preflight,
search, add-to-cart per item, possibly an LLM divergence call). Two shapes were
considered:

1. **Synchronous block-until-done** — `fill_cart` returns the `CartFillResult`
   directly. Cleanest call-shape, but it forces a single MCP `tools/call` to
   block for the full automation wall-clock (tens of seconds to minutes), holds
   the connection open across a browser run, and couples the MCP request lifetime
   to the runner lifetime. It also fights Tangram's action model: an action is a
   *pure state transition* (sync `&mut self`) or an `async fn` doing its OWN
   egress — but here the work happens in a **separate host task** outside the
   component, so the component literally cannot block on it.

2. **Return-a-handle + poll** (**CHOSEN**) — `fill_cart` records a PENDING
   request in the doc and returns a `request_id` (the handle) immediately. The
   host dispatch loop runs the automation asynchronously and writes the result
   back into the doc. `cart_fill_status(request_id)` returns the current
   status + (when done) the `CartFillResult`.

We pick **return-a-handle + poll** because:

- It matches Tangram's architecture exactly: `fill_cart` is a fast, pure
  `&mut self` action (append a request, no I/O, no await under the store lock);
  the actual work is a host-side supervised task (mirrors `scheduler.rs`), which
  writes back through the ordinary action-dispatch path.
- It is the cleaner MCP-client contract for a slow job: the client gets an
  immediate handle and polls, instead of staking one request on a multi-minute
  browser run that may time out at the transport layer.
- The handle is durable: the request + status live in the replicated document,
  so a client (or a different device) can poll the status of a fill it didn't
  start, and a host restart mid-fill leaves a visible `running`/`pending` record
  rather than a silently-dropped synchronous call.

The cost — the client must poll — is the standard ergonomics of any async job
API and is trivial for an MCP agent (call `fill_cart`, then `cart_fill_status`
in a loop until `done`/`failed`).

## The MCP tool surface (`/grocery-cart/mcp`)

Actions auto-become MCP tools (the host derives tools from `describe()`), so the
app exposes:

- **`fill_cart(grocery_list)`** — `grocery_list = [{ item, quantity,
  preferences? }]`. Builds + emits an `AutomationRequest` and records a PENDING
  request in the doc. Returns the `request_id` handle. *(mutating)*
- **`cart_fill_status(request_id)`** — returns `{ status, result? }` for the
  request: `pending` / `running` / `done` / `failed`, plus the `CartFillResult`
  once written. *(read-only)*
- **`record_cart_result(request_id, status, result?)`** — the WRITE-BACK action
  the host dispatch loop calls to set a request to `running`, then to `done`/
  `failed` with the result. *(mutating, operator-only)* The host loop calls it
  via **direct in-process dispatch** (it never goes over the HTTP/MCP surface),
  and the app sets `require_auth = true`, so the whole mutating surface
  (`fill_cart` + `record_cart_result`) requires the bearer on a tokened
  deployment — a public MCP client cannot forge a cart-fill result. On a
  self-hosted loopback bind (no token) the surface is loopback-trusted as usual.
- **`list_requests()`** — convenience read of all requests + statuses. *(read-only)*

## How it reuses each substrate piece

The dispatch loop and (in GC2) the runner reuse the fully-built
`tangram-automation` substrate; GC1 wires the request→authorize→dispatch→result
spine and stubs the run:

- **`request.rs` (`AutomationRequest` + `authorize` + `OperatorPolicy`)** — the
  app emits the request; the dispatch loop calls `authorize(request, policy)`
  against the `[automation]` operator policy. The request is an UPPER BOUND:
  authorize denies an unapproved template / unlisted app outright, and TRIMS
  out-of-ceiling domains and ungranted credential refs (never widens). GC1
  exercises this fully offline.
- **`preflight.rs` + `session.rs` (login-once / reuse-session)** — GC2: the
  runner loads the persisted Amazon session, runs the upfront "are we signed in?"
  preflight, and only routes to the decision point (`decision.rs`) when not.
- **`broker.rs` + `secrets.rs` `OnePasswordResolver` (`op://` inject)** — GC2:
  the WF login step is an `InjectCredential` carrying only the **`op://`
  reference** (config-driven, from the per-app credential grant); the host
  resolves it through the SA-scoped 1Password resolver at the egress boundary.
  The credential never enters the component or the script artifact.
- **`egress.rs` gate + `script.rs` `StopGate` (never-checkout)** — GC2: the WF
  `AutomationScript` carries a `StopGate` immediately before checkout, and the
  browser egress gate + operator policy DENY the order-submit path
  (`POST /gp/buy/…`). `validate_disposition` guarantees the LLM can never pass a
  stop-gate or steer off the domain allowlist.
- **`/llm` via agentgateway (item→product matching)** — GC2: mapping a free-text
  grocery `item` to a concrete Whole Foods product goes through the host
  agentgateway `/llm/<name>` proxy (host-injected key, ADR-0012), with the result
  validated before any add-to-cart.

## The host request→runner dispatch loop (the unbuilt seam)

A supervised host task — `crates/tangram-host/src/cartfill.rs`,
`CartFillDispatcher` — built in the spawn/shutdown shape of `scheduler.rs` (a
`tokio::spawn`ed interval loop that `select!`s against a `watch` shutdown). Each
tick:

1. Reads the `grocery-cart` app's state JSON (`AppRuntime::state_json`) and finds
   PENDING requests (`CartFillDispatcher::pending_requests`, a pure parse).
2. Marks the request `running` (a `record_cart_result` dispatch).
3. Calls `authorize(request, policy)` against the operator `OperatorPolicy` built
   from `[automation]`. A denial → writes a `failed` result with the policy
   reason (default-deny: a request for an unapproved template fails closed).
4. **GC1 fixture runner** — `fixture_run(authorized)` (in `cartfill.rs`) returns
   a deterministic canned `CartFillResult` WITHOUT launching a browser, calling
   1Password, or the LLM: it echoes each grocery item as "added" with a stub
   product name + qty, and a stub `cart_url`. **GC2** replaces this single call
   with the real `tangram-automation` runner (the WF script replay).
5. Writes the result back (`record_cart_result` → `done`/`failed`).

The fixture runner makes the FULL round-trip — MCP tool → request → authorize →
dispatch → result — provable offline. The pure pieces (`pending_requests`,
`authorize`, `fixture_run`) are unit-tested in `cartfill.rs`; an integration
test drives the live host binary end-to-end.

## The never-checkout rail (preserved from day one)

Even though GC1 drives no browser, the rail is configured and asserted now:

- **Operator policy / egress ceiling** — `[automation].browser_domains_ceiling`
  bounds the WF/Amazon hosts; the per-app credential grant is a single
  `op://` reference. The `wholefoods-cart` template must be `approved_templates`
  for any request to authorize at all (default-deny otherwise).
- **Order-submit path-deny** — the operator config denies the checkout/order
  path (`POST /gp/buy/…`) so the browser egress gate fails closed on it. GC1
  documents + asserts this intent; GC2 wires the live gate + the template's
  `StopGate` before checkout. The `purchased`/submit path is never reachable.

## GC1 → GC2 → GC3 roadmap

- **GC1 (this checkpoint)** — the `grocery-cart` app + `fill_cart` /
  `cart_fill_status` MCP tools + the `CartFillResult` model + the host
  request→runner **dispatch loop** with a **fixture/dry-run runner** + the design
  doc + the policy/egress config (template approval, WF ceiling, `op://`
  placeholder, order-submit deny). All offline-tested.
- **GC2** — the real Whole Foods `AutomationScript` template (semantic-locator
  steps, the `StopGate` before checkout, the `op://` `InjectCredential` login),
  the LLM item→product matching over `/llm`, and a realistic fixture e2e
  (recorded snapshots replayed without a live browser). The fixture runner call
  is replaced by the live `tangram-automation` runner; CI stays fixture-driven.
- **GC3 (owner-gated)** — the live run: a real headed/headless browser session,
  the live 1Password inject, the live add-to-cart. Owner-gated; requires the
  operator to set the real `op://` reference in `apps.toml` (the placeholder
  below) and configure the SA-scoped token. Never submits an order.
