# Handoff — drive the Whole Foods cart-fill MCP server (GC3) to working, locally

**Run + iterate this LOCALLY (residential IP).** The prod AWS host's datacenter
IP is blocked by Amazon/Whole Foods (402/403, like recipe import), so the live
browser run only works from a residential IP with your own 1Password session.

## Goal (acceptance)

An MCP server at `/grocery-cart/mcp` whose `fill_cart(grocery_list)` actually
adds items to a **real Whole Foods cart** via a headless browser using your
1Password session, and returns the **items added + the cart URL**, stopping at a
**reviewable cart (never checking out)**.

- `fill_cart([{item, quantity, preferences?}])` → `request_id`; poll
  `cart_fill_status(request_id)` → `{ added:[{item,product,qty}], not_added:[{item,reason}], cart_url, status:"done" }`.
- Items LLM-matched to WF products honoring quantity + preferences (e.g. "extra
  virgin", "organic"); unmatched → `not_added`.
- Login happens **once** (headed, you solve any CAPTCHA/2FA); the session
  persists and is reused — no repeat login.
- The run **halts at the filled cart** (StopGate); `/gp/buy/` is denied; nothing
  is ever purchased.

## What's already built (do NOT rebuild)

- **GC1** (`apps/grocery-cart`, `crates/tangram-host/src/cartfill.rs`): the
  `fill_cart`/`cart_fill_status`/`list_requests` MCP tools, the `CartFillResult`
  model, and the host `CartFillDispatcher` (picks up a PENDING `AutomationRequest`
  → `authorize()` → runner → writes the result back). Topology = return-a-handle
  + poll.
- **GC2** (`crates/tangram-automation/src/wholefoods.rs`): `wholefoods_cart_script()`
  — semantic-locator steps (navigate → `InjectCredential` (op://) gated by
  preflight/session-reuse → per item search + add-to-cart → **`StopGate`** before
  checkout → capture cart URL), the LLM matcher (`Matcher::Fixture|Live` over the
  `/llm` agentgateway proxy; rejects invented products), and the dispatcher wired
  to a `CartDriver` seam. `LiveCartRunner` currently returns `NeedsSignIn`.
- **The substrate** (`crates/tangram-automation`, ADR-0010, all shipped):
  supervised browser runner (`runner.rs`), browser egress gate (`egress.rs`,
  denies `/gp/buy/`), op:// 1Password broker (`broker.rs` + host `secrets.rs`),
  preflight signed-in check (`preflight.rs`), session persistence
  (`session.rs`, `storageState` reused, stored 0600 outside the repo), the
  interactive/LLM CAPTCHA decision (`decision.rs`).
- All of the above is **offline-tested** (340 tests). The offline harness:
  `OfflineFixtureRunner` (env `TANGRAM_CARTFILL_OFFLINE_FIXTURE=1`) + a mock
  `CartDriver` + fixture matcher + `SignedIn` preflight.

## What remains = GC3: the live `CartDriver`

Implement the Playwright-backed driver so `LiveCartRunner` actually drives a real
browser. The **seam**:

- `crates/tangram-automation/src/runner.rs` — supervise the browser (Playwright)
  and provide a live `CartDriver`/`FillSink` (navigate, type, click, read the
  a11y `Snapshot`, `locator.fill` for `InjectCredential`).
- `crates/tangram-automation/src/wholefoods.rs` — `run_fill` already orchestrates
  preflight → (login or skip) → per-item match+add → StopGate → cart URL against a
  `CartDriver`. Pass it the LIVE driver + `Matcher::Live`.
- `crates/tangram-host/src/cartfill.rs` — `LiveCartRunner`: launch the runner,
  build the `BrowserEgressGate` from the authorized domains (deny `/gp/buy/`),
  load/persist the session, run `wholefoods::run_fill`, return the `CartFillResult`.

The script is a best-effort blueprint; real-page divergences are healed by the
record→replay→validated-LLM-fallback (`script.rs`) — semantic role+name locators,
`Expect` post-conditions, the LLM proposes recovery, the runner validates it
(never skips the StopGate, never leaves the domain allowlist).

## Prereqs (local, one-time)

1. **Playwright:** `npx playwright install chromium` (the `[automation].driver`).
2. **`.env`:** `OP_SERVICE_ACCOUNT_TOKEN=<SA token scoped to the WF/Amazon login>`,
   plus `DEEPSEEK_API_KEY` (the `/llm` matcher). 
3. **`apps.toml` `[automation]`:** `enabled = true`, `headless = false` (first
   run, to solve login by hand), the real `op://` ref in
   `credential_grants."grocery-cart"` (replace `PLACEHOLDER`),
   `browser_domains_ceiling` narrowed to the WF/Amazon hosts, keep
   `denied_paths = ["/gp/buy/"]`.

## Run + iterate loop

```sh
# isolation (clean, no remote) — best for iterating:
cargo run -p tangram-host --release -- apps.toml          # serves :8080
# call the MCP tool:
RID=$(curl -s -X POST localhost:8080/grocery-cart/api/actions/fill_cart -H 'Content-Type: application/json' \
  -d '{"grocery_list":[{"item":"bananas","quantity":3},{"item":"organic milk","quantity":1}]}' | sed 's/.*"result":"//;s/".*//')
sleep 6; curl -s -X POST localhost:8080/grocery-cart/api/actions/cart_fill_status -d "{\"request_id\":\"$RID\"}"
# edit the live driver → rebuild → restart → re-call → repeat:
cargo build -p tangram-host --release && cargo build -p tangram-grocery-cart --lib --target wasm32-wasip2 --release
```

First run headed → solve login once → `session.rs` persists `storageState` →
later runs: preflight `SignedIn` → login skipped. Tune the WF search/add-to-cart
locators + timing against the real pages.

## Safety rails (must hold — do not weaken)

- **Never checkout:** the `StopGate` before order-submit (the runner never skips
  it) + the `/gp/buy/` `deny_path` (network backstop). The cart is left filled
  for you to review/checkout by hand.
- **Credential:** the SA token is the floor (scoped to ONLY the WF/Amazon login);
  the value is resolved→filled→dropped, never logged / persisted / in an LLM
  prompt (it's a `SecretString`, masked in snapshots).
- **Ingested page data** is web-sourced — keep it out of general LLM contexts
  beyond the once-only matcher call (the #117 opaque-to-LLM stance).

## Gate + push

```sh
cargo fmt && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo nextest run
```
Keep the offline tests green (NO live calls in CI — the live path is exercised
only by your local manual run). Commit + push; the remote rolling CI watcher
validates the merge.
