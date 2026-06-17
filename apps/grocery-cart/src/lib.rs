//! Grocery cart-fill MCP server (Build-3 / GC1) — a sandboxed Tangram app whose
//! `fill_cart` MCP tool turns a structured grocery list into a **filled (never
//! submitted) Whole Foods / Amazon cart**.
//!
//! The component NEVER drives a browser. It only **requests** an automation: an
//! [`AutomationRequest`]-shaped record (the request-not-grant channel of
//! `docs/design/task-automation-browser.md` §4.3) appended to the app's
//! replicated document. A supervised host task — the request→runner dispatch
//! loop (`tangram-host/src/cartfill.rs`) — picks the request up, intersects it
//! with operator policy, runs it (a deterministic FIXTURE runner in GC1), and
//! writes the [`CartFillResult`] back through `record_cart_result`, where
//! `cart_fill_status` surfaces it.
//!
//! Topology (see `docs/design/grocery-cart-mcp.md`): **return-a-handle + poll**.
//! `fill_cart` is a fast, pure `&mut self` action that records a PENDING request
//! and returns its `request_id`; the slow browser work happens in the host task,
//! not in this action. Clients poll `cart_fill_status(request_id)`.

use tangram::prelude::*;
use tangram::time::now_ms;

/// The storefront template the host dispatch loop runs (`approved_templates` in
/// `[automation]`). A request naming a different template is denied (default-deny).
pub const WHOLEFOODS_TEMPLATE: &str = "wholefoods-cart";

/// The requesting app name — the per-app gate key in the operator policy.
pub const APP_NAME: &str = "grocery-cart";

#[model]
#[derive(Default)]
pub struct GroceryCart {
    /// Every cart-fill request ever made, newest last. The dispatch loop reads
    /// the PENDING ones and writes results back into them.
    requests: Vec<CartFillRequest>,
}

/// One line of a requested grocery list: a free-text item, a quantity, and an
/// optional free-text preference (brand/size/organic/…). The host (GC2) matches
/// `item` to a concrete Whole Foods product via the `/llm` proxy.
#[model]
pub struct GroceryItem {
    item: String,
    quantity: i64,
    /// Optional free-text preference. `None` on older documents (the `missing`
    /// attribute hydrates the absent key — the field-addition convention).
    #[autosurgeon(missing = "Option::default")]
    preferences: Option<String>,
}

/// A recorded cart-fill request: the durable handle a client polls. Carries the
/// emitted [`AutomationRequest`]-shaped fields (`template_id` / `domains` /
/// `credential_refs`) verbatim so the host reconstructs the request without a
/// second schema, the grocery list it was built from, the lifecycle `status`,
/// and the written-back [`CartFillResult`] (once the run finishes).
#[model]
pub struct CartFillRequest {
    /// The handle returned by `fill_cart` and passed to `cart_fill_status`.
    id: String,
    /// The requesting app (always [`APP_NAME`]) — the operator per-app gate key.
    app: String,
    /// The pre-approved template id (always [`WHOLEFOODS_TEMPLATE`]) — never raw
    /// browser commands. A request for an unapproved template is denied.
    template_id: String,
    /// The grocery list this fill is over.
    grocery_list: Vec<GroceryItem>,
    /// The domains the request wants (the WF/Amazon hosts) — the host
    /// intersects these with the operator ceiling (never widens).
    domains: Vec<String>,
    /// The `op://` credential references the request wants — the host intersects
    /// these with the per-app grant. NEVER a value; always a reference.
    credential_refs: Vec<String>,
    /// Lifecycle: `pending` → `running` → `done` | `failed`. The host moves it.
    status: String,
    created_at_ms: i64,
    /// The result, written back by the host's `record_cart_result`. `None` until
    /// the run finishes. (`missing`: an older doc may predate the field.)
    #[autosurgeon(missing = "Option::default")]
    result: Option<CartFillResult>,
}

/// The outcome of a cart fill, written back into the request by the host.
#[model]
pub struct CartFillResult {
    /// Items the runner added to the cart, with the matched product + quantity.
    added: Vec<AddedItem>,
    /// Items that could NOT be added, with a reason (out of stock, no match, …).
    not_added: Vec<NotAddedItem>,
    /// A link to the filled (NOT submitted) cart for the owner to review +
    /// check out themselves. Never an order-confirmation URL.
    cart_url: String,
}

/// One successfully added cart line.
#[model]
pub struct AddedItem {
    /// The requested grocery item (free text).
    item: String,
    /// The concrete product the runner matched + added (GC2 = LLM-matched).
    product: String,
    qty: i64,
}

/// One grocery item the runner could not add, with a human-readable reason.
#[model]
pub struct NotAddedItem {
    item: String,
    reason: String,
}

/// The lifecycle statuses, as the strings stored in the doc + returned by the
/// status tool. Shared with the host loop's expectations (kept as plain &str so
/// the WASM component carries no host dependency).
pub mod status {
    pub const PENDING: &str = "pending";
    pub const RUNNING: &str = "running";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
}

#[actions]
impl GroceryCart {
    /// Request a cart fill over `grocery_list` (`[{ item, quantity,
    /// preferences? }]`). Builds + records a PENDING [`AutomationRequest`]-shaped
    /// request naming the `wholefoods-cart` template, the WF/Amazon domains, and
    /// the configured `op://` credential reference, and returns its `request_id`
    /// handle. The actual browser run happens out-of-band in the host dispatch
    /// loop; poll `cart_fill_status(request_id)` for progress + the result.
    ///
    /// This action does NO I/O: it is a pure state transition (the request is a
    /// REQUEST, never a grant — the host intersects it with operator policy).
    pub fn fill_cart(&mut self, grocery_list: Vec<GroceryItem>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.requests.push(CartFillRequest {
            id: id.clone(),
            app: APP_NAME.to_string(),
            template_id: WHOLEFOODS_TEMPLATE.to_string(),
            grocery_list,
            // The domains the request WANTS — the host trims these to the
            // operator `browser_domains_ceiling` (∩, never widened).
            domains: wholefoods_domains(),
            // The op:// reference the request WANTS — the host trims this to the
            // per-app credential grant. A clearly-marked placeholder the operator
            // sets in apps.toml before the live run (GC3); GC1 never resolves it.
            credential_refs: vec![credential_ref_placeholder()],
            status: status::PENDING.to_string(),
            created_at_ms: now_ms(),
            result: None,
        });
        id
    }

    /// Poll a request's status + (once finished) its [`CartFillResult`]. Errors
    /// if no request has the given id.
    pub fn cart_fill_status(&self, request_id: String) -> Result<CartFillStatus, String> {
        let req = self
            .requests
            .iter()
            .find(|r| r.id == request_id)
            .ok_or_else(|| format!("no cart-fill request with id {request_id}"))?;
        Ok(CartFillStatus {
            request_id: req.id.clone(),
            status: req.status.clone(),
            result: req.result.clone(),
        })
    }

    /// Write a request's lifecycle state back into the doc — the host dispatch
    /// loop's seam. The loop calls this to move a request to `running`, then to
    /// `done`/`failed` with the run's [`CartFillResult`]. `require_auth` gates
    /// this action (operator-only) so a cart-fill RESULT cannot be forged by an
    /// MCP client — only the host loop (bearer-authenticated over loopback) can
    /// advance a request. Errors if the id is unknown or the status is invalid.
    pub fn record_cart_result(
        &mut self,
        request_id: String,
        status: String,
        result: Option<CartFillResult>,
    ) -> Result<(), String> {
        if !matches!(
            status.as_str(),
            status::PENDING | status::RUNNING | status::DONE | status::FAILED
        ) {
            return Err(format!("invalid cart-fill status {status:?}"));
        }
        let req = self
            .requests
            .iter_mut()
            .find(|r| r.id == request_id)
            .ok_or_else(|| format!("no cart-fill request with id {request_id}"))?;
        req.status = status;
        if let Some(result) = result {
            req.result = Some(result);
        }
        Ok(())
    }

    /// List every cart-fill request (newest first) with its status — the
    /// convenience read for a UI / an agent surveying outstanding fills.
    #[must_use]
    pub fn list_requests(&self) -> Vec<CartFillStatus> {
        let mut out: Vec<CartFillStatus> = self
            .requests
            .iter()
            .map(|r| CartFillStatus {
                request_id: r.id.clone(),
                status: r.status.clone(),
                result: r.result.clone(),
            })
            .collect();
        out.reverse();
        out
    }
}

/// The status-poll shape returned by `cart_fill_status` / `list_requests`.
#[model]
pub struct CartFillStatus {
    request_id: String,
    status: String,
    /// Present once the run finished (`done`/`failed`); `None` while pending /
    /// running.
    #[autosurgeon(missing = "Option::default")]
    result: Option<CartFillResult>,
}

/// The Whole Foods / Amazon hosts a cart fill wants to touch — the request's
/// upper bound, trimmed by the host to `[automation].browser_domains_ceiling`.
#[must_use]
pub fn wholefoods_domains() -> Vec<String> {
    vec![
        "www.amazon.com".to_string(),
        "www.wholefoodsmarket.com".to_string(),
    ]
}

/// A clearly-marked PLACEHOLDER `op://` credential reference. The operator sets
/// the real SA-scoped reference in `apps.toml` (the per-app credential grant)
/// before the live run (GC3). GC1 NEVER resolves it — the fixture runner makes
/// no 1Password call.
#[must_use]
pub fn credential_ref_placeholder() -> String {
    "op://Private/PLACEHOLDER-amazon-login/password".to_string()
}

/// MCP instructions, shared between the native app builder and the WASM
/// component's `describe()` export.
const INSTRUCTIONS: &str = "Fill a Whole Foods / Amazon grocery cart from a structured list. \
     `fill_cart` requests a cart fill and returns a request_id handle; poll \
     `cart_fill_status(request_id)` for progress and the result. The cart is filled \
     for your review and NEVER submitted — you confirm checkout yourself.";

/// The grocery-cart app, fully configured. Call `.serve()` to run it standalone
/// or `.build()` to mount it in a multi-app host.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub fn app() -> App<GroceryCart> {
    App::<GroceryCart>::new(APP_NAME).instructions(INSTRUCTIONS)
}

// Compiled for wasm32-wasip2, the same model + actions become a Tangram
// component (`tangram-host` owns the platform around it).
#[cfg(target_family = "wasm")]
tangram::export_component!(GroceryCart {
    name: "grocery-cart",
    instructions: INSTRUCTIONS,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Vec<GroceryItem> {
        vec![
            GroceryItem {
                item: "milk".into(),
                quantity: 1,
                preferences: Some("organic whole".into()),
            },
            GroceryItem {
                item: "eggs".into(),
                quantity: 2,
                preferences: None,
            },
        ]
    }

    #[test]
    fn fill_cart_records_a_wellformed_pending_request() {
        let mut cart = GroceryCart::default();
        let id = cart.fill_cart(list());
        assert_eq!(cart.requests.len(), 1);
        let req = &cart.requests[0];
        assert_eq!(req.id, id);
        // The emitted request is the AutomationRequest upper bound: the app
        // name, the approved template id, the WF domains, and an op:// ref.
        assert_eq!(req.app, APP_NAME);
        assert_eq!(req.template_id, WHOLEFOODS_TEMPLATE);
        assert_eq!(req.status, status::PENDING);
        assert_eq!(req.grocery_list.len(), 2);
        assert!(req.domains.contains(&"www.amazon.com".to_string()));
        // The credential ref is an op:// REFERENCE, never a value.
        assert_eq!(req.credential_refs.len(), 1);
        assert!(req.credential_refs[0].starts_with("op://"));
        assert!(req.result.is_none());
    }

    #[test]
    fn status_poll_tracks_lifecycle() {
        let mut cart = GroceryCart::default();
        let id = cart.fill_cart(list());

        // Pending right after fill_cart.
        let s = cart.cart_fill_status(id.clone()).unwrap();
        assert_eq!(s.status, status::PENDING);
        assert!(s.result.is_none());

        // The host loop moves it to running, then done with a result.
        cart.record_cart_result(id.clone(), status::RUNNING.into(), None)
            .unwrap();
        assert_eq!(
            cart.cart_fill_status(id.clone()).unwrap().status,
            status::RUNNING
        );

        let result = CartFillResult {
            added: vec![AddedItem {
                item: "milk".into(),
                product: "Organic Whole Milk".into(),
                qty: 1,
            }],
            not_added: vec![],
            cart_url: "https://www.amazon.com/cart".into(),
        };
        cart.record_cart_result(id.clone(), status::DONE.into(), Some(result))
            .unwrap();
        let s = cart.cart_fill_status(id).unwrap();
        assert_eq!(s.status, status::DONE);
        let r = s.result.expect("result written back");
        assert_eq!(r.added.len(), 1);
        assert_eq!(r.added[0].product, "Organic Whole Milk");
    }

    #[test]
    fn record_result_rejects_unknown_id_and_bad_status() {
        let mut cart = GroceryCart::default();
        let id = cart.fill_cart(list());
        assert!(
            cart.record_cart_result("nope".into(), status::DONE.into(), None)
                .is_err()
        );
        assert!(cart.record_cart_result(id, "garbage".into(), None).is_err());
    }

    #[test]
    fn status_poll_unknown_id_errors() {
        let cart = GroceryCart::default();
        assert!(cart.cart_fill_status("missing".into()).is_err());
    }

    #[test]
    fn list_requests_is_newest_first() {
        let mut cart = GroceryCart::default();
        let a = cart.fill_cart(list());
        let b = cart.fill_cart(list());
        let listed = cart.list_requests();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].request_id, b);
        assert_eq!(listed[1].request_id, a);
    }
}
