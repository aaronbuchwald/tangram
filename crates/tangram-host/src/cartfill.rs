//! The grocery cart-fill request→runner **dispatch loop** (Build-3 / GC1, the
//! previously-unbuilt seam; `docs/design/grocery-cart-mcp.md`).
//!
//! The `grocery-cart` app's `fill_cart` MCP tool records a PENDING
//! [`AutomationRequest`]-shaped request in its replicated document and returns a
//! handle. This supervised host task picks those requests up, intersects each
//! with the operator [`OperatorPolicy`] built from `[automation]`
//! ([`tangram_automation::request::authorize`]), runs it, and writes the
//! [`CartFillResult`] back through the app's `record_cart_result` action — where
//! the `cart_fill_status` tool surfaces it (return-a-handle + poll topology).
//!
//! The loop mirrors `scheduler.rs` (the gateway-supervisor spawn/shutdown shape):
//! a `tokio::spawn`ed interval loop that `select!`s the tick against a `watch`
//! shutdown signal and stops cleanly on host shutdown.
//!
//! ## GC2 = the REAL `tangram-automation` runner (offline-tested)
//!
//! The dispatcher now drives the real Whole Foods automation via a
//! [`CartRunner`] seam ([`tangram_automation::wholefoods::run_fill`]): build the
//! [`BrowserEgressGate`] from the authorized domains (allow the WF/Amazon hosts;
//! DENY the order-submit `/gp/buy/` path), run the upfront preflight (reuse the
//! session or surface a sign-in decision), replay the `wholefoods-cart`
//! [`AutomationScript`] using the LLM item→product matcher per item, halt at the
//! `StopGate` before checkout, and capture the filled (never submitted)
//! `cart_url`. The single [`CartRunner::run`] call is the GC2 replacement for
//! GC1's `fixture_run`; the authorize/dispatch/write-back spine is unchanged.
//!
//! **Still offline in CI/dev.** The production [`LiveCartRunner`] makes NO live
//! browser/1Password/LLM/network call: with no live browser session configured
//! the preflight reports "no session", so [`run_fill`] returns
//! [`FillError::NeedsSignIn`] and the request is recorded `failed` with a
//! human-actionable assistance reason — the live browser session + 1Password
//! inject + LLM matcher land in **GC3 (owner-gated)**. The offline e2e test
//! injects a TEST runner (a mock driver + hand-authored snapshots + a fixture
//! LLM + a `SignedIn` preflight) so the WHOLE real-`run_fill` flow — session
//! reuse skips login, the matcher picks products, items land in `added`, the
//! StopGate halts before checkout, the cart URL is captured — is proven WITHOUT
//! a browser.
//!
//! ## The never-checkout rail
//!
//! Authorization fails closed: a request for an unapproved template (or an app
//! not allowed to request) is denied and recorded as `failed`, never run. The
//! order-submit path-deny (`[automation].denied_paths`) is wired into the
//! [`BrowserEgressGate`] the runner builds, and the template's `StopGate` is the
//! last reachable script step — `run_fill` halts there and never submits an
//! order.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tangram_automation::preflight::PreflightOutcome;
use tangram_automation::request::{AuthorizedAutomation, AutomationRequest, authorize};
use tangram_automation::wholefoods::{
    self, FillError, FillOutcome, GroceryLine, Matcher, wholefoods_gate,
};
use tokio::sync::watch;

use crate::Host;
use crate::config::HostConfig;
use crate::tenant::AppKey;

/// The app whose cart-fill requests this loop drives. Matches the
/// `grocery-cart` app name (and the request's `app` field / policy gate key).
const CART_APP: &str = "grocery-cart";

/// How often the loop scans for PENDING requests. A small fixed cadence (the
/// real automation is the slow part); a pending request is picked up within a
/// tick of being recorded.
const TICK: Duration = Duration::from_secs(2);

/// Lifecycle statuses, kept in sync with the `grocery-cart` component's
/// `status` module (plain strings so the host carries no component dependency).
mod status {
    pub const PENDING: &str = "pending";
    pub const RUNNING: &str = "running";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
}

// ── the request shape the component records (a subset of its state JSON) ──────

/// One requested grocery line, as the component serialized it.
#[derive(Debug, Clone, Deserialize)]
pub struct GroceryItem {
    pub item: String,
    #[serde(default = "default_qty")]
    pub quantity: i64,
    #[serde(default)]
    pub preferences: Option<String>,
}

fn default_qty() -> i64 {
    1
}

/// A recorded cart-fill request, parsed from the app's state JSON. Carries the
/// [`AutomationRequest`] fields verbatim so we reconstruct the request without a
/// second schema (the request is a REQUEST, never a grant — `authorize` narrows
/// it against operator policy).
#[derive(Debug, Clone, Deserialize)]
pub struct PendingRequest {
    pub id: String,
    pub app: String,
    pub template_id: String,
    #[serde(default)]
    pub grocery_list: Vec<GroceryItem>,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub credential_refs: Vec<String>,
    pub status: String,
}

impl PendingRequest {
    /// Reconstruct the [`AutomationRequest`] the component emitted. `params`
    /// carries the grocery item names (opaque to the request channel — the
    /// runner reads the structured list).
    fn automation_request(&self) -> AutomationRequest {
        AutomationRequest {
            app: self.app.clone(),
            template_id: self.template_id.clone(),
            params: self.grocery_list.iter().map(|g| g.item.clone()).collect(),
            domains: self.domains.clone(),
            credential_refs: self.credential_refs.clone(),
        }
    }
}

/// The state-JSON envelope the component renders (`{ "requests": [...] }`).
#[derive(Debug, Default, Deserialize)]
struct CartState {
    #[serde(default)]
    requests: Vec<PendingRequest>,
}

/// Parse the PENDING requests out of a grocery-cart state JSON document. Pure +
/// total: malformed state ⇒ no requests (the loop logs and skips). Only
/// `pending` requests are returned — `running`/`done`/`failed` are skipped so a
/// request is dispatched exactly once.
pub fn pending_requests(state_json: &str) -> Vec<PendingRequest> {
    let state: CartState = serde_json::from_str(state_json).unwrap_or_default();
    state
        .requests
        .into_iter()
        .filter(|r| r.status == status::PENDING)
        .collect()
}

// ── the GC2 real runner seam ─────────────────────────────────────────────────

/// Convert the host's parsed grocery items into the automation crate's
/// [`GroceryLine`]s (the structured form `run_fill` matches over).
fn grocery_lines(list: &[GroceryItem]) -> Vec<GroceryLine> {
    list.iter()
        .map(|g| GroceryLine {
            item: g.item.clone(),
            quantity: g.quantity,
            preferences: g.preferences.clone(),
        })
        .collect()
}

/// Render a [`FillOutcome`] into the `CartFillResult` JSON the app's
/// `record_cart_result` action stores.
fn outcome_json(outcome: &FillOutcome) -> serde_json::Value {
    let added: Vec<serde_json::Value> = outcome
        .added
        .iter()
        .map(|a| json!({ "item": a.item, "product": a.product, "qty": a.qty }))
        .collect();
    let not_added: Vec<serde_json::Value> = outcome
        .not_added
        .iter()
        .map(|n| json!({ "item": n.item, "reason": n.reason }))
        .collect();
    json!({
        "added": added,
        "not_added": not_added,
        "cart_url": outcome.cart_url,
    })
}

/// The runner seam the dispatcher drives. GC2's [`LiveCartRunner`] builds the
/// egress gate + preflight + matcher and calls
/// [`tangram_automation::wholefoods::run_fill`]; the offline e2e test injects a
/// runner backed by a mock driver + fixture LLM. Either way the dispatcher's
/// authorize/dispatch/write-back spine is identical.
#[async_trait::async_trait]
pub trait CartRunner: Send + Sync {
    /// Run the Whole Foods cart fill for an authorized automation + its grocery
    /// list. Returns the result JSON (`added`/`not_added`/`cart_url`) on a
    /// completed (cart-built, StopGate-halted) fill, or a [`FillError`] when the
    /// run cannot proceed (e.g. not signed in / off-allowlist navigation).
    async fn run(
        &self,
        authorized: &AuthorizedAutomation,
        list: &[GroceryItem],
    ) -> Result<serde_json::Value, FillError>;
}

/// The production GC2 runner. Builds the [`wholefoods_gate`] from the authorized
/// domains (allow WF/Amazon; deny the `/gp/buy/` order-submit path), runs the
/// upfront preflight, and replays the `wholefoods-cart` script via `run_fill`
/// with the LLM matcher per item.
///
/// **Offline-by-default (GC2):** there is no configured live browser session, so
/// the preflight reports `NoSession`; `run_fill` then returns
/// [`FillError::NeedsSignIn`] (no browser/1Password/LLM/network call is made).
/// The live browser session, 1Password inject, and LLM matcher are wired here in
/// **GC3 (owner-gated)** by supplying a live `CartDriver`, a session-reuse
/// (`SignedIn`) preflight, and a [`Matcher::Live`] over the `/llm` proxy. The
/// egress gate + StopGate fences below are LIVE now.
pub struct LiveCartRunner;

#[async_trait::async_trait]
impl CartRunner for LiveCartRunner {
    async fn run(
        &self,
        authorized: &AuthorizedAutomation,
        _list: &[GroceryItem],
    ) -> Result<serde_json::Value, FillError> {
        // The never-checkout gate is built (and asserted in tests) even on the
        // offline path: allow the authorized (ceiling-trimmed) domains and
        // ALWAYS deny the order-submit path at the network layer.
        let _gate = wholefoods_gate(&authorized.domains);

        // GC2 is OFFLINE: there is no configured live browser session, so the
        // preflight is "no session" and the runner surfaces the sign-in decision
        // WITHOUT making any browser/1Password/LLM/network call. We short-circuit
        // to `NeedsSignIn` here rather than driving `run_fill` against a stub
        // browser — the live driver + a real (session-reuse) preflight + a
        // `Matcher::Live` over the `/llm` proxy are wired in GC3 (owner-gated).
        Err(FillError::NeedsSignIn(PreflightOutcome::NoSession))
    }
}

/// Thin wrapper around [`tangram_automation::wholefoods::run_fill`] so the
/// dispatcher and the offline test share one call site.
pub async fn run_fill_path(
    credential_ref: Option<&str>,
    lines: &[GroceryLine],
    gate: &tangram_automation::egress::BrowserEgressGate,
    preflight: &PreflightOutcome,
    driver: &mut dyn wholefoods::CartDriver,
    matcher: &Matcher,
) -> Result<FillOutcome, FillError> {
    wholefoods::run_fill(credential_ref, lines, gate, preflight, driver, matcher).await
}

/// Env var that selects the OFFLINE fixture runner ([`OfflineFixtureRunner`]) in
/// place of [`LiveCartRunner`]. CI's e2e test (`tests/cartfill_dispatch.rs`)
/// sets it so the spawned host binary drives the WHOLE real `run_fill` flow with
/// a mock driver + a fixture LLM (no browser/1Password/LLM/network), proving the
/// GC2 dispatcher→real-runner wiring end-to-end. Unset in production (the
/// default is [`LiveCartRunner`], which is offline-`NeedsSignIn` until GC3).
pub const OFFLINE_FIXTURE_ENV: &str = "TANGRAM_CARTFILL_OFFLINE_FIXTURE";

/// The OFFLINE fixture runner: drives the SAME real
/// [`tangram_automation::wholefoods::run_fill`] the live GC3 path uses, but with
/// a mock [`wholefoods::CartDriver`] (hand-authored search snapshots), a fixture
/// LLM [`Matcher::deterministic_fixture`], and a `SignedIn` (session-reuse)
/// preflight — NO browser/1Password/LLM/network call. This is how the e2e test
/// exercises the full flow through the spawned host binary.
pub struct OfflineFixtureRunner;

#[async_trait::async_trait]
impl CartRunner for OfflineFixtureRunner {
    async fn run(
        &self,
        authorized: &AuthorizedAutomation,
        list: &[GroceryItem],
    ) -> Result<serde_json::Value, FillError> {
        let gate = wholefoods_gate(&authorized.domains);
        let lines = grocery_lines(list);
        let mut driver = FixtureSearchDriver;
        let outcome = run_fill_path(
            authorized.credential_refs.first().map(String::as_str),
            &lines,
            &gate,
            // Session reuse: the fixture preflight reports signed-in, so the
            // login inject is skipped (login once, reuse the session).
            &PreflightOutcome::SignedIn,
            &mut driver,
            &Matcher::deterministic_fixture(),
        )
        .await?;
        Ok(outcome_json(&outcome))
    }
}

/// A mock [`wholefoods::CartDriver`] that fabricates plausible search results
/// from the search term itself — two candidates, one of which folds in a generic
/// "Organic"/preference token so the fixture matcher has a preference-honoring
/// pick. NO browser; deterministic. The fixture e2e runner uses this.
struct FixtureSearchDriver;

#[async_trait::async_trait]
impl wholefoods::CartDriver for FixtureSearchDriver {
    async fn navigate(
        &mut self,
        _url: &str,
    ) -> anyhow::Result<tangram_automation::script::Snapshot> {
        Ok(tangram_automation::script::Snapshot {
            url_host: Some(wholefoods::AMAZON_HOST.to_string()),
            ..Default::default()
        })
    }
    async fn inject_credential(&mut self, _secret_ref: &str, _target: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn search(&mut self, text: &str) -> anyhow::Result<tangram_automation::script::Snapshot> {
        Ok(tangram_automation::script::Snapshot {
            url_host: Some(wholefoods::AMAZON_HOST.to_string()),
            text: format!("fixture-results:{text}"),
            ..Default::default()
        })
    }
    fn candidates(
        &self,
        snap: &tangram_automation::script::Snapshot,
    ) -> Vec<wholefoods::ProductCandidate> {
        let term = snap
            .text
            .strip_prefix("fixture-results:")
            .unwrap_or_default();
        if term.is_empty() {
            return Vec::new();
        }
        // A generic store-brand candidate + an "Organic Whole" variant that
        // contains the common preference tokens, so a preference-bearing line
        // matches the second and a plain line matches the first.
        vec![
            wholefoods::ProductCandidate::new(format!("365 {term}")),
            wholefoods::ProductCandidate::new(format!("Organic Whole {term}")),
        ]
    }
    async fn add_to_cart(
        &mut self,
        _product_title: &str,
        _qty: i64,
    ) -> anyhow::Result<tangram_automation::script::Snapshot> {
        Ok(tangram_automation::script::Snapshot {
            text: "Added to Cart".to_string(),
            ..Default::default()
        })
    }
}

/// Pick the GC2 [`CartRunner`] for the binary: the [`OfflineFixtureRunner`] when
/// [`OFFLINE_FIXTURE_ENV`] is set (CI e2e), else the production
/// [`LiveCartRunner`].
fn default_runner() -> Arc<dyn CartRunner> {
    if std::env::var_os(OFFLINE_FIXTURE_ENV).is_some() {
        tracing::warn!(
            "cartfill: {OFFLINE_FIXTURE_ENV} set — using the OFFLINE fixture runner \
             (no browser/1Password/LLM); this is a test/CI mode, not the live run"
        );
        Arc::new(OfflineFixtureRunner)
    } else {
        Arc::new(LiveCartRunner)
    }
}

// ── the supervised loop ──────────────────────────────────────────────────────

/// The supervised cart-fill dispatcher: an interval loop + a shutdown channel,
/// in the shape of `scheduler.rs`. Holds the [`CartRunner`] seam (the production
/// [`LiveCartRunner`] by default; the offline test injects a mock-driver runner).
pub struct CartFillDispatcher {
    host: Arc<Host>,
    tick: Duration,
    shutdown: watch::Sender<bool>,
    runner: Arc<dyn CartRunner>,
}

impl CartFillDispatcher {
    pub fn new(host: Arc<Host>) -> Self {
        Self::with_runner(host, default_runner())
    }

    /// Build the dispatcher with an explicit [`CartRunner`] (the offline e2e
    /// test injects a mock-driver runner; production uses [`LiveCartRunner`]).
    pub fn with_runner(host: Arc<Host>, runner: Arc<dyn CartRunner>) -> Self {
        Self {
            host,
            tick: TICK,
            shutdown: watch::Sender::new(false),
            runner,
        }
    }

    /// Spawn the interval loop. Every `tick`, dispatch any PENDING grocery-cart
    /// requests; log and continue on any error. Stops cleanly on shutdown.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let disp = self.clone();
        tokio::spawn(async move {
            let mut shutdown = disp.shutdown.subscribe();
            let mut interval = tokio::time::interval(disp.tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => disp.run_once().await,
                    _ = shutdown.changed() => {
                        tracing::info!("cartfill: stopped");
                        return;
                    }
                }
            }
        })
    }

    /// One scan: find the running grocery-cart app, read its pending requests,
    /// and dispatch each. Absent app → silent no-op (the app may not be
    /// installed). A per-request error is logged; the loop continues.
    async fn run_once(&self) {
        let runtime = {
            let apps = self.host.apps.read().await;
            apps.get(&AppKey::top(CART_APP)).map(|e| e.runtime.clone())
        };
        let Some(runtime) = runtime else { return };

        let pending = pending_requests(&runtime.state_json().await);
        if pending.is_empty() {
            return;
        }

        // The operator policy for THIS app, built from `[automation]`. Re-read
        // per scan so a config edit (a newly-approved template, a credential
        // grant) takes effect without a restart — cheap, and the loop is slow.
        let policy = match HostConfig::load(&self.host.config_path()) {
            Ok(config) => config.automation.operator_policy([CART_APP]),
            Err(e) => {
                tracing::warn!("cartfill: cannot load config, skipping scan: {e:#}");
                return;
            }
        };

        for req in pending {
            self.dispatch_one(&runtime, &policy, &req).await;
        }
    }

    /// Dispatch one PENDING request: mark it running, authorize it against the
    /// operator policy, run the (GC1 fixture) runner, and write the result back.
    /// A policy denial → a `failed` result with the reason (default-deny: never
    /// run an unapproved template). The write-backs go through the app's
    /// `record_cart_result` action (the ordinary dispatch path).
    async fn dispatch_one(
        &self,
        runtime: &crate::app::AppRuntime,
        policy: &tangram_automation::request::OperatorPolicy,
        req: &PendingRequest,
    ) {
        // 1) Mark running so a slow run isn't re-picked-up on the next tick.
        if let Err(e) = self.record(runtime, &req.id, status::RUNNING, None).await {
            tracing::warn!("cartfill: {}: cannot mark running: {e}", req.id);
            return;
        }

        // 2) Authorize (∩ operator policy). A denial fails closed.
        let authorized = match authorize(&req.automation_request(), policy) {
            Ok(authorized) => authorized,
            Err(e) => {
                tracing::warn!("cartfill: {}: denied by policy: {e}", req.id);
                self.fail(runtime, &req.id, &format!("denied by operator policy: {e}"))
                    .await;
                return;
            }
        };

        // 3) Run the real `tangram-automation` Whole Foods runner (GC2). The
        // gate denies the order-submit path; the script halts at the StopGate.
        // A run that can't proceed (e.g. not signed in — GC2 is offline, the
        // live session is GC3) is recorded `failed` with a human-actionable
        // reason, never run live.
        match self.runner.run(&authorized, &req.grocery_list).await {
            Ok(result) => {
                tracing::info!(
                    "cartfill: {}: run completed over domains {:?}",
                    req.id,
                    authorized.domains,
                );
                if let Err(e) = self
                    .record(runtime, &req.id, status::DONE, Some(result))
                    .await
                {
                    tracing::warn!("cartfill: {}: cannot write result: {e}", req.id);
                }
            }
            Err(e) => {
                tracing::warn!("cartfill: {}: run could not proceed: {e}", req.id);
                self.fail(runtime, &req.id, &e.to_string()).await;
            }
        }
    }

    /// Dispatch a `record_cart_result` action onto the app.
    async fn record(
        &self,
        runtime: &crate::app::AppRuntime,
        request_id: &str,
        status: &str,
        result: Option<serde_json::Value>,
    ) -> Result<(), String> {
        let args = json!({
            "request_id": request_id,
            "status": status,
            "result": result,
        });
        runtime
            .dispatch("record_cart_result", args)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Record a `failed` status with a reason (a result carrying the reason as a
    /// not_added line so the poll surfaces WHY it failed).
    async fn fail(&self, runtime: &crate::app::AppRuntime, request_id: &str, reason: &str) {
        let result = json!({
            "added": [],
            "not_added": [{ "item": "(request)", "reason": reason }],
            "cart_url": "",
        });
        if let Err(e) = self
            .record(runtime, request_id, status::FAILED, Some(result))
            .await
        {
            tracing::warn!("cartfill: {request_id}: cannot record failure: {e}");
        }
    }

    /// Stop the loop (used on Ctrl-C, like the scheduler).
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tangram_automation::runner::AutomationSettings;

    fn state(reqs: &str) -> String {
        format!("{{\"requests\":{reqs}}}")
    }

    fn one_pending() -> String {
        state(
            r#"[{
                "id": "r1",
                "app": "grocery-cart",
                "template_id": "wholefoods-cart",
                "grocery_list": [
                    { "item": "milk", "quantity": 1, "preferences": "organic" },
                    { "item": "eggs", "quantity": 2, "preferences": null }
                ],
                "domains": ["www.amazon.com", "attacker.com"],
                "credential_refs": ["op://Private/Amazon/password", "op://Private/Bank/pw"],
                "status": "pending"
            }]"#,
        )
    }

    #[test]
    fn pending_requests_filters_to_pending_only() {
        let s = state(
            r#"[
                { "id": "a", "app": "grocery-cart", "template_id": "wholefoods-cart", "status": "pending" },
                { "id": "b", "app": "grocery-cart", "template_id": "wholefoods-cart", "status": "running" },
                { "id": "c", "app": "grocery-cart", "template_id": "wholefoods-cart", "status": "done" }
            ]"#,
        );
        let pending = pending_requests(&s);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "a");
    }

    #[test]
    fn malformed_state_yields_no_requests() {
        assert!(pending_requests("not json").is_empty());
        assert!(pending_requests("{}").is_empty());
        assert!(pending_requests(r#"{"requests": "oops"}"#).is_empty());
    }

    fn policy() -> tangram_automation::request::OperatorPolicy {
        let s: AutomationSettings = toml::from_str(
            "enabled = true\n\
             browser_domains_ceiling = [\"www.amazon.com\", \"www.wholefoodsmarket.com\"]\n\
             approved_templates = [\"wholefoods-cart\"]\n\
             [credential_grants]\n\
             \"grocery-cart\" = [\"op://Private/Amazon/password\"]",
        )
        .unwrap();
        s.operator_policy(["grocery-cart"])
    }

    #[test]
    fn authorize_narrows_to_the_ceiling_and_grant() {
        let req = &pending_requests(&one_pending())[0];
        let authorized = authorize(&req.automation_request(), &policy()).unwrap();
        // The never-widen rail: off-ceiling domain + ungranted credential dropped.
        assert_eq!(authorized.domains, vec!["www.amazon.com"]);
        assert_eq!(
            authorized.credential_refs,
            vec!["op://Private/Amazon/password"]
        );
    }

    /// A mock [`wholefoods::CartDriver`] for the offline GC2 round-trip: returns
    /// hand-authored search snapshots, records what was injected/added — NO
    /// browser.
    struct TestDriver {
        results: std::collections::HashMap<String, Vec<wholefoods::ProductCandidate>>,
    }

    #[async_trait::async_trait]
    impl wholefoods::CartDriver for TestDriver {
        async fn navigate(
            &mut self,
            _url: &str,
        ) -> anyhow::Result<tangram_automation::script::Snapshot> {
            Ok(tangram_automation::script::Snapshot {
                url_host: Some("www.amazon.com".into()),
                ..Default::default()
            })
        }
        async fn inject_credential(
            &mut self,
            _secret_ref: &str,
            _target: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn search(
            &mut self,
            text: &str,
        ) -> anyhow::Result<tangram_automation::script::Snapshot> {
            Ok(tangram_automation::script::Snapshot {
                url_host: Some("www.amazon.com".into()),
                text: format!("results:{text}"),
                ..Default::default()
            })
        }
        fn candidates(
            &self,
            snap: &tangram_automation::script::Snapshot,
        ) -> Vec<wholefoods::ProductCandidate> {
            let term = snap.text.strip_prefix("results:").unwrap_or("");
            self.results.get(term).cloned().unwrap_or_default()
        }
        async fn add_to_cart(
            &mut self,
            _product_title: &str,
            _qty: i64,
        ) -> anyhow::Result<tangram_automation::script::Snapshot> {
            Ok(tangram_automation::script::Snapshot {
                text: "Added to Cart".into(),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn authorize_then_real_run_round_trips_offline() {
        let req = &pending_requests(&one_pending())[0];
        let authorized = authorize(&req.automation_request(), &policy()).unwrap();

        // The OFFLINE real-runner path: a SignedIn preflight (session reuse) +
        // a mock driver + the fixture LLM matcher — the SAME `run_fill` the live
        // GC3 runner uses, with no browser/1Password/LLM/network call.
        let mut driver = TestDriver {
            results: std::collections::HashMap::from([
                (
                    "milk".to_string(),
                    vec![
                        wholefoods::ProductCandidate::new("Whole Milk"),
                        wholefoods::ProductCandidate::new("Organic Whole Milk"),
                    ],
                ),
                (
                    "eggs".to_string(),
                    vec![wholefoods::ProductCandidate::new("Large Eggs")],
                ),
            ]),
        };
        let gate = wholefoods_gate(&authorized.domains);
        let lines = grocery_lines(&req.grocery_list);
        let outcome = run_fill_path(
            authorized.credential_refs.first().map(String::as_str),
            &lines,
            &gate,
            &PreflightOutcome::SignedIn,
            &mut driver,
            &Matcher::deterministic_fixture(),
        )
        .await
        .expect("offline fill succeeds");

        let result = outcome_json(&outcome);
        let added = result["added"].as_array().unwrap();
        assert_eq!(added.len(), 2);
        assert_eq!(added[0]["item"], "milk");
        // The matcher honored the "organic" preference.
        assert_eq!(added[0]["product"], "Organic Whole Milk");
        assert_eq!(added[0]["qty"], 1);
        assert_eq!(added[1]["item"], "eggs");
        assert_eq!(added[1]["qty"], 2);
        assert!(result["not_added"].as_array().unwrap().is_empty());
        // The never-checkout proof: halted at the gate; cart URL is a VIEW path.
        assert!(outcome.stopped_at_gate);
        let cart_url = result["cart_url"].as_str().unwrap();
        assert!(cart_url.starts_with("https://www.amazon.com/"));
        assert!(!cart_url.contains("/gp/buy/"));
    }

    #[tokio::test]
    async fn live_runner_is_offline_and_surfaces_signin() {
        // The production runner with no live browser session reports "not signed
        // in" WITHOUT any live call (GC3 supplies the live session).
        let req = &pending_requests(&one_pending())[0];
        let authorized = authorize(&req.automation_request(), &policy()).unwrap();
        let err = LiveCartRunner
            .run(&authorized, &req.grocery_list)
            .await
            .unwrap_err();
        assert!(matches!(err, FillError::NeedsSignIn(_)));
    }

    #[test]
    fn default_deny_when_template_not_approved() {
        // An empty `[automation]` (no approved templates) denies every request.
        let s: AutomationSettings = toml::from_str("enabled = true").unwrap();
        let policy = s.operator_policy(["grocery-cart"]);
        let req = &pending_requests(&one_pending())[0];
        assert!(
            authorize(&req.automation_request(), &policy).is_err(),
            "an unapproved template must be denied (default-deny)"
        );
    }

    #[test]
    fn unlisted_app_is_denied() {
        // The app is approved-template-OK but not in `allowed_apps`.
        let s: AutomationSettings = toml::from_str(
            "enabled = true\n\
             browser_domains_ceiling = [\"www.amazon.com\"]\n\
             approved_templates = [\"wholefoods-cart\"]",
        )
        .unwrap();
        let policy = s.operator_policy(["some-other-app"]); // grocery-cart NOT allowed
        let req = &pending_requests(&one_pending())[0];
        assert!(authorize(&req.automation_request(), &policy).is_err());
    }
}
