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
//! ## GC1 = a FIXTURE runner
//!
//! [`fixture_run`] is a deterministic stand-in for the real automation: it
//! echoes the grocery list as "added" with stub product names + a stub
//! `cart_url`, WITHOUT launching a browser, calling 1Password, or the LLM. It
//! proves the full MCP-tool → request → authorize → dispatch → result round-trip
//! offline. GC2 replaces the single [`fixture_run`] call with the real
//! `tangram-automation` runner (the Whole Foods script replay); the
//! authorize/dispatch/write-back spine here is unchanged.
//!
//! ## The never-checkout rail
//!
//! Authorization fails closed: a request for an unapproved template (or an app
//! not allowed to request) is denied and recorded as `failed`, never run. The
//! order-submit path-deny (`[automation].denied_paths`) and the template's
//! `StopGate` (GC2) keep the runner from ever submitting an order.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tangram_automation::request::{AuthorizedAutomation, AutomationRequest, authorize};
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

// ── the GC1 fixture runner ───────────────────────────────────────────────────

/// GC1 deterministic fixture runner: turn an [`AuthorizedAutomation`] + its
/// grocery list into a canned [`CartFillResult`] WITHOUT any browser, 1Password,
/// or LLM call. Each item is echoed as "added" with a stub product name (a
/// placeholder for GC2's LLM-matched product) and a stub `cart_url`. This proves
/// the authorize→dispatch→write-back spine end-to-end offline.
///
/// GC2 replaces this with the real `tangram-automation` runner (the Whole Foods
/// `AutomationScript` replay over the authorized domains + the `op://` inject),
/// keeping the same `(authorized, list) -> CartFillResult` signature.
pub fn fixture_run(authorized: &AuthorizedAutomation, list: &[GroceryItem]) -> serde_json::Value {
    let added: Vec<serde_json::Value> = list
        .iter()
        .map(|g| {
            json!({
                "item": g.item,
                // A deterministic STUB product name (GC2 = LLM-matched). The
                // preference, when present, is folded into the stub label so the
                // round-trip carries it visibly.
                "product": stub_product(&g.item, g.preferences.as_deref()),
                "qty": g.quantity,
            })
        })
        .collect();
    json!({
        "added": added,
        "not_added": [],
        // A STUB review URL — the filled (never submitted) cart. The first
        // authorized domain anchors it so the rail (ceiling ∩) is visible.
        "cart_url": stub_cart_url(authorized),
    })
}

/// A deterministic stub product label for a requested item (GC1 stand-in for the
/// GC2 LLM item→product match).
fn stub_product(item: &str, preferences: Option<&str>) -> String {
    match preferences {
        Some(pref) if !pref.trim().is_empty() => format!("[stub] {pref} {item}"),
        _ => format!("[stub] {item}"),
    }
}

/// A deterministic stub cart-review URL on the first authorized domain (or a
/// generic Amazon cart if none — authorize would have denied an empty domain set
/// before we got here, so this is belt-and-braces).
fn stub_cart_url(authorized: &AuthorizedAutomation) -> String {
    let host = authorized
        .domains
        .first()
        .map_or("www.amazon.com", String::as_str);
    format!("https://{host}/cart?fixture=gc1")
}

// ── the supervised loop ──────────────────────────────────────────────────────

/// The supervised cart-fill dispatcher: an interval loop + a shutdown channel,
/// in the shape of `scheduler.rs`.
pub struct CartFillDispatcher {
    host: Arc<Host>,
    tick: Duration,
    shutdown: watch::Sender<bool>,
}

impl CartFillDispatcher {
    pub fn new(host: Arc<Host>) -> Self {
        Self {
            host,
            tick: TICK,
            shutdown: watch::Sender::new(false),
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

        // 3) Run. GC1 = the deterministic fixture (no browser/op/LLM). GC2
        // swaps in the real tangram-automation runner here.
        let result = fixture_run(&authorized, &req.grocery_list);
        tracing::info!(
            "cartfill: {}: fixture run added {} item(s) over domains {:?}",
            req.id,
            req.grocery_list.len(),
            authorized.domains,
        );

        // 4) Write the result back (done).
        if let Err(e) = self
            .record(runtime, &req.id, status::DONE, Some(result))
            .await
        {
            tracing::warn!("cartfill: {}: cannot write result: {e}", req.id);
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
    fn authorize_then_fixture_run_round_trips() {
        let req = &pending_requests(&one_pending())[0];
        let authorized = authorize(&req.automation_request(), &policy()).unwrap();

        // The never-widen rail: off-ceiling domain + ungranted credential dropped.
        assert_eq!(authorized.domains, vec!["www.amazon.com"]);
        assert_eq!(
            authorized.credential_refs,
            vec!["op://Private/Amazon/password"]
        );

        let result = fixture_run(&authorized, &req.grocery_list);
        let added = result["added"].as_array().unwrap();
        assert_eq!(added.len(), 2);
        assert_eq!(added[0]["item"], "milk");
        // The stub product folds in the preference; the qty round-trips.
        assert_eq!(added[0]["product"], "[stub] organic milk");
        assert_eq!(added[0]["qty"], 1);
        assert_eq!(added[1]["product"], "[stub] eggs");
        assert_eq!(added[1]["qty"], 2);
        assert!(result["not_added"].as_array().unwrap().is_empty());
        // The stub cart URL anchors on the authorized (ceiling-trimmed) domain.
        assert_eq!(
            result["cart_url"],
            "https://www.amazon.com/cart?fixture=gc1"
        );
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
