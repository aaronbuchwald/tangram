//! The **Whole Foods cart-fill** automation (Build-3 / GC2;
//! `docs/design/grocery-cart-mcp.md`, `task-automation-browser.md` §9).
//!
//! This is the REAL automation behind the GC1 fixture skeleton, still
//! **offline/fixture-tested** (the LIVE run is GC3, owner-gated). It has three
//! pieces, each pure + offline-testable:
//!
//! 1. [`wholefoods_cart_script`] — the hand-authored `wholefoods-cart`
//!    [`AutomationScript`] template. **Semantic** [`Locator`]s (role + name,
//!    NOT CSS), an [`Step::InjectCredential`] login step that preflight skips
//!    when a session is reused, a search → choose → add-to-cart block PER item
//!    with [`Expect`] post-conditions, and a hard [`Step::StopGate`] before any
//!    checkout. The script is a best-effort blueprint — real-page divergences
//!    are healed at GC3 by the record→replay→LLM-fallback ([`crate::script`]).
//!
//! 2. [`Matcher`] — LLM item→product matching over the `/llm` agentgateway
//!    proxy (ADR-0012, host-injected key). Given a [`GroceryLine`] + a
//!    search-results [`Snapshot`], it asks the LLM to pick the best-matching
//!    product (honoring preferences) or report "no good match". A
//!    [`Matcher::Fixture`]/[`Matcher::Live`] seam (mirroring SO4 `ingest.rs`)
//!    keeps CI offline against a recorded reply — NO live LLM call here.
//!
//! 3. [`run_fill`] — the offline-capable runner. Given the authorized
//!    automation, the grocery list, the egress gate, a preflight outcome, a
//!    [`CartDriver`], a [`Matcher`], it replays the script: skips login when
//!    signed in, matches each item, lands the matches in `added` (or
//!    `not_added` with a reason), HALTS at the StopGate before checkout, and
//!    captures the cart URL. The browser I/O is behind the `CartDriver` seam,
//!    so the whole flow is driven by a mock driver + hand-authored snapshots
//!    WITHOUT a browser.
//!
//! The never-checkout rail is structural here: the script's last reachable step
//! is the StopGate, and the runner stops there; the [`crate::egress`] gate's
//! `/gp/buy/` path-deny is the network backstop. NOTHING in this module places
//! an order, and no test/dev path makes a live browser/1Password/LLM/network
//! call.

use serde::{Deserialize, Serialize};

use crate::egress::BrowserEgressGate;
use crate::preflight::PreflightOutcome;
use crate::script::{AutomationScript, Expect, Locator, Snapshot, Step, StepOutcome};

/// The template id the operator approves (matches the grocery-cart app's
/// `WHOLEFOODS_TEMPLATE` + `[automation].approved_templates`).
pub const TEMPLATE_ID: &str = "wholefoods-cart";

/// The Whole Foods storefront is reached through Amazon (Whole Foods groceries
/// are an Amazon storefront). The script navigates Amazon and filters to Whole
/// Foods; the cart URL anchors on Amazon.
pub const AMAZON_HOST: &str = "www.amazon.com";
/// The Whole Foods marketing host (kept on the allowlist for the storefront
/// landing). Real grocery operations happen on `www.amazon.com`.
pub const WHOLEFOODS_HOST: &str = "www.wholefoodsmarket.com";

/// The order-submit path prefix — the never-checkout rail. The egress gate
/// path-denies this on `AMAZON_HOST`, and the script's StopGate sits before it.
pub const ORDER_SUBMIT_PATH: &str = "/gp/buy/";

// ── 1. the wholefoods-cart AutomationScript template ─────────────────────────

/// One grocery line the runner matches + adds (the structured form the host
/// passes in). Mirrors the grocery-cart component's `GroceryItem`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroceryLine {
    pub item: String,
    #[serde(default = "default_qty")]
    pub quantity: i64,
    #[serde(default)]
    pub preferences: Option<String>,
}

fn default_qty() -> i64 {
    1
}

impl GroceryLine {
    pub fn new(item: impl Into<String>, quantity: i64, preferences: Option<&str>) -> Self {
        Self {
            item: item.into(),
            quantity,
            preferences: preferences.map(str::to_string),
        }
    }
}

/// Build the hand-authored `wholefoods-cart` [`AutomationScript`] over a grocery
/// list, carrying only the `op://` credential **reference** (never a value).
///
/// Shape:
/// - navigate to the Amazon storefront (the Whole Foods grocery surface),
/// - an [`Step::InjectCredential`] login step (the password field) — preflight
///   SKIPS this when the session is reused (see [`run_fill`]),
/// - PER item: a search [`Step::Type`] (the box is the searchbox) → a
///   [`Step::Click`] on "Add to Cart" with an [`Expect`] post-condition
///   ("Added to Cart"),
/// - a hard [`Step::StopGate`] before any checkout/place-order, then
/// - a final navigate to the cart for the review URL capture.
///
/// The per-item search TEXT here is the item name; the runner's [`Matcher`]
/// chooses WHICH product to add from the live search-results snapshot (the
/// script is the skeleton; the match is the live decision).
#[must_use]
pub fn wholefoods_cart_script(items: &[GroceryLine], credential_ref: &str) -> AutomationScript {
    let mut script = AutomationScript::new(
        TEMPLATE_ID,
        vec![AMAZON_HOST.to_string(), WHOLEFOODS_HOST.to_string()],
    );

    // Navigate to the Amazon storefront (Whole Foods groceries live here).
    script.push(Step::Navigate {
        url: format!("https://{AMAZON_HOST}/"),
        expect: Expect {
            url_host: Some(AMAZON_HOST.to_string()),
            ..Default::default()
        },
    });

    // The login step: inject the resolved credential into the password field.
    // Preflight/session-reuse SKIPS this step when already signed in (the whole
    // point of durable session reuse — login once, reuse the session). The step
    // carries ONLY the op:// reference (the broker resolves it at the boundary).
    script.push(Step::InjectCredential {
        secret_ref: credential_ref.to_string(),
        target: Locator::new("textbox", "Password"),
    });

    // Per item: search → (the Matcher chooses) → add-to-cart with a post-cond.
    for line in items {
        script.push(Step::Type {
            target: Locator::new("searchbox", "Search Amazon"),
            text: line.item.clone(),
            expect: Expect::default(),
        });
        script.push(Step::Click {
            target: Locator::new("button", "Add to Cart"),
            expect: Expect {
                text_present: Some("Added to Cart".to_string()),
                ..Default::default()
            },
        });
    }

    // The hard StopGate — replay halts here; the LLM can NEVER pass it
    // (`validate_disposition`), and the egress gate denies the order-submit path.
    script.push(Step::StopGate {
        reason: "cart built — placing the order requires explicit owner approval".to_string(),
    });

    // After the stop-gate, navigating to the cart for the review URL is NOT
    // reached by replay (replay halts AT the gate). The runner builds the cart
    // URL directly (see `cart_review_url`); this step documents the intent.
    script.push(Step::Navigate {
        url: cart_review_url(),
        expect: Expect {
            url_host: Some(AMAZON_HOST.to_string()),
            ..Default::default()
        },
    });

    script
}

/// The filled (NEVER submitted) cart review URL the owner opens to check out
/// themselves. A cart-VIEW path (`/gp/cart/view.html`), never the order-submit
/// path — and it anchors on a ceiling domain.
#[must_use]
pub fn cart_review_url() -> String {
    format!("https://{AMAZON_HOST}/gp/cart/view.html")
}

// ── 2. LLM item→product matching ─────────────────────────────────────────────

/// One candidate product the search-results page surfaced (parsed from the
/// snapshot, or supplied in tests). The matcher chooses among these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductCandidate {
    /// The product title as shown on the results page.
    pub title: String,
    /// The price text, if surfaced (e.g. `"$4.99"`). Opaque to the matcher.
    #[serde(default)]
    pub price: Option<String>,
    /// The add-to-cart locator (role+name) for THIS candidate, used to add it.
    /// In a live run this is the per-result button; tests supply it directly.
    #[serde(default)]
    pub add_to_cart: Option<Locator>,
}

impl ProductCandidate {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            price: None,
            add_to_cart: None,
        }
    }
}

/// The matcher's decision for one grocery line: a chosen product, or no match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchDecision {
    /// The chosen product title (one of the candidates' titles).
    Chosen(String),
    /// No candidate is a good match; the item goes to `not_added` with `reason`.
    NoMatch(String),
}

/// A fixture-reply closure: given a grocery line + the search-results
/// candidates, return a CANNED LLM reply string (the same JSON shape the live
/// model is asked for), driven through the live parse path. Boxed so
/// [`Matcher::Fixture`] is constructible from any closure.
pub type FixtureReply = Box<dyn Fn(&GroceryLine, &[ProductCandidate]) -> String + Send + Sync>;

/// The LLM item→product matching seam, mirroring SO4 `ingest.rs`'s
/// `Llm::Fixture`/`Llm::Live`. [`Matcher::Fixture`] parses a CANNED LLM reply
/// (no network — CI/offline). [`Matcher::Live`] POSTs to the `/llm/<name>`
/// agentgateway proxy (host-injected key, ADR-0012) — never constructed in a
/// test/dev path.
pub enum Matcher {
    /// Offline / CI: a [`FixtureReply`] closure, driven through the SAME parse
    /// path the live call uses. Constructed only by tests + a future
    /// fixture-mode flag.
    Fixture(FixtureReply),
    /// Live: POST the matching prompt to the `/llm/<name>` proxy. The request
    /// carries NO key — the host injects it at the egress boundary.
    Live { proxy_url: String, model: String },
}

impl Matcher {
    /// A fixture matcher that replies with a JSON choice naming the candidate
    /// whose title best contains the item + every preference token (a
    /// deterministic stand-in for the LLM's judgment), else "no match". This is
    /// the recorded-reply path CI drives — it never calls a model.
    #[must_use]
    pub fn deterministic_fixture() -> Self {
        Matcher::Fixture(Box::new(|line, candidates| {
            // A simple, deterministic "best match": the first candidate whose
            // lower-cased title contains the (lower-cased) item word AND every
            // preference token. Emits the SAME JSON shape the live LLM is asked
            // for, so both paths share one parser.
            let item = line.item.to_lowercase();
            let prefs: Vec<String> = line
                .preferences
                .as_deref()
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_lowercase)
                .collect();
            let pick = candidates.iter().find(|c| {
                let t = c.title.to_lowercase();
                t.contains(&item) && prefs.iter().all(|p| t.contains(p))
            });
            match pick {
                Some(c) => format!(r#"{{"choice":{:?}}}"#, c.title),
                None => r#"{"choice":null,"reason":"no candidate matched the item + preferences"}"#
                    .to_string(),
            }
        }))
    }

    /// Match one grocery line against the search-results candidates. Returns the
    /// chosen product title or a no-match reason. Both seams share
    /// [`parse_match_reply`], so a recorded fixture reply exercises exactly the
    /// live parse path.
    pub async fn match_product(
        &self,
        line: &GroceryLine,
        candidates: &[ProductCandidate],
    ) -> MatchDecision {
        if candidates.is_empty() {
            return MatchDecision::NoMatch("search returned no products".to_string());
        }
        let raw = match self {
            Matcher::Fixture(f) => f(line, candidates),
            Matcher::Live { proxy_url, model } => {
                match match_live(proxy_url, model, line, candidates).await {
                    Ok(text) => text,
                    Err(e) => return MatchDecision::NoMatch(format!("LLM match failed: {e}")),
                }
            }
        };
        parse_match_reply(&raw, candidates)
    }
}

/// Build the item→product matching prompt. Asks the model to pick the
/// best-matching product title from the candidates, honoring the preferences,
/// and to return a strict JSON object `{"choice": "<title>"|null, "reason"?}`.
#[must_use]
pub fn build_match_prompt(line: &GroceryLine, candidates: &[ProductCandidate]) -> String {
    let listing = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| match &c.price {
            Some(p) => format!("{}. {} ({p})", i + 1, c.title),
            None => format!("{}. {}", i + 1, c.title),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prefs = line.preferences.as_deref().unwrap_or("(none)");
    format!(
        "You are choosing the best grocery product to add to a Whole Foods cart.\n\
         Grocery item: {item:?}\n\
         Quantity: {qty}\n\
         Preferences: {prefs}\n\n\
         Candidate products from the search results:\n{listing}\n\n\
         Pick the SINGLE best-matching product, honoring the preferences (e.g. \
         \"organic\", \"extra virgin\"). If NONE is a good match, choose null.\n\
         Output ONLY a JSON object with EXACTLY these keys:\n\
         - \"choice\": the EXACT product title string you picked, or null if no good match.\n\
         - \"reason\": a short reason (required when choice is null; optional otherwise).\n\
         No prose outside the JSON.",
        item = line.item,
        qty = line.quantity,
    )
}

/// Parse the model's match reply into a [`MatchDecision`]. Tolerant of a fenced
/// ```json block and leading prose. A `choice` must name one of the candidate
/// titles (the model may not invent a product) — an unknown title is treated as
/// no-match (fail safe). A `null`/absent choice is a no-match with the reason.
#[must_use]
pub fn parse_match_reply(text: &str, candidates: &[ProductCandidate]) -> MatchDecision {
    let Some(json) = extract_json_object(text) else {
        return MatchDecision::NoMatch("LLM reply was not a JSON object".to_string());
    };
    #[derive(Deserialize)]
    struct Reply {
        #[serde(default)]
        choice: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    }
    let Ok(reply) = serde_json::from_str::<Reply>(&json) else {
        return MatchDecision::NoMatch("LLM reply did not match the choice schema".to_string());
    };
    match reply.choice {
        Some(title) if !title.trim().is_empty() => {
            // The choice must be one of the offered candidates — the model
            // cannot conjure a product that wasn't on the page.
            if candidates.iter().any(|c| c.title == title) {
                MatchDecision::Chosen(title)
            } else {
                MatchDecision::NoMatch(format!(
                    "LLM chose a product not in the search results: {title:?}"
                ))
            }
        }
        _ => MatchDecision::NoMatch(
            reply
                .reason
                .filter(|r| !r.trim().is_empty())
                .unwrap_or_else(|| "no good match".to_string()),
        ),
    }
}

/// Pull the first top-level JSON object out of a model reply, tolerating a
/// surrounding ```json fence or leading prose.
fn extract_json_object(text: &str) -> Option<String> {
    let t = text.trim();
    let inner = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(str::trim_start)
        .and_then(|rest| rest.strip_suffix("```").map(str::trim))
        .unwrap_or(t);
    let start = inner.find('{')?;
    let end = inner.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(inner[start..=end].to_string())
}

/// Issue the live item→product match call to the `/llm/<name>` agentgateway
/// proxy (ADR-0012). The request carries NO key — the host injects the provider
/// bearer at the proxy. Returns the assistant message text (the JSON object the
/// parser reads). NEVER exercised in tests/dev (only [`Matcher::Live`] calls it).
async fn match_live(
    proxy_url: &str,
    model: &str,
    line: &GroceryLine,
    candidates: &[ProductCandidate],
) -> anyhow::Result<String> {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "user", "content": build_match_prompt(line, candidates) }
        ],
    });
    let client = reqwest::Client::new();
    let resp = client.post(proxy_url).json(&body).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("LLM proxy returned {}", resp.status());
    }
    let payload: serde_json::Value = resp.json().await?;
    payload
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("LLM proxy reply had no message content"))
}

// ── 3. the offline-capable runner ────────────────────────────────────────────

/// One item the runner added to the cart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedLine {
    pub item: String,
    pub product: String,
    pub qty: i64,
}

/// One item the runner could not add, with a reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotAddedLine {
    pub item: String,
    pub reason: String,
}

/// The outcome of a cart fill — the shape written back as the `CartFillResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillOutcome {
    pub added: Vec<AddedLine>,
    pub not_added: Vec<NotAddedLine>,
    pub cart_url: String,
    /// Whether the runner halted at the StopGate (the never-checkout proof).
    /// Always true on a normal completion — the cart is built, never submitted.
    pub stopped_at_gate: bool,
}

/// The browser-side I/O the runner needs, behind a trait so the whole flow is
/// driven offline by a mock. A live implementation wraps the supervised
/// browser-driver ([`crate::runner`]) + the [`crate::broker`] credential broker;
/// tests supply a mock that returns hand-authored snapshots and records the
/// fills.
///
/// Every method is the runner's ONLY contact with the page — and every URL the
/// runner would navigate to is checked against the [`BrowserEgressGate`] BEFORE
/// the driver sees it (the gate is the network fence; this is the in-runner
/// belt over it).
#[async_trait::async_trait]
pub trait CartDriver: Send {
    /// Navigate to `url` and return the resulting page snapshot.
    async fn navigate(&mut self, url: &str) -> anyhow::Result<Snapshot>;
    /// Resolve `secret_ref` and inject it into `target` (the login). The value
    /// never returns here — the driver fills it into the page. Delegates to a
    /// [`crate::broker::FillSink`] in the live runner.
    async fn inject_credential(&mut self, secret_ref: &str, target: &str) -> anyhow::Result<()>;
    /// Type `text` into the search box and return the search-results snapshot
    /// (the page the [`Matcher`] reads candidates from).
    async fn search(&mut self, text: &str) -> anyhow::Result<Snapshot>;
    /// Read the candidate products off the current search-results snapshot.
    fn candidates(&self, snap: &Snapshot) -> Vec<ProductCandidate>;
    /// Add the chosen product (by title) to the cart; return the post-add
    /// snapshot (whose `expect` the runner checks: "Added to Cart").
    async fn add_to_cart(&mut self, product_title: &str, qty: i64) -> anyhow::Result<Snapshot>;
}

/// Why a fill aborted before completing (distinct from per-item no-matches,
/// which are recorded in `not_added` and do NOT abort the run).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FillError {
    #[error("egress gate denied navigation to {url:?}: {reason}")]
    EgressDenied { url: String, reason: String },
    #[error("not signed in and no session to reuse ({0:?}); surface the decision point")]
    NeedsSignIn(PreflightOutcome),
    #[error("a browser step failed: {0}")]
    Driver(String),
}

/// Run the Whole Foods cart fill OFFLINE-capably. The browser is entirely behind
/// `driver`; with a mock driver + hand-authored snapshots this runs the full
/// flow WITHOUT a browser, 1Password, or LLM.
///
/// Flow:
/// 1. **Preflight gate** — if `preflight` is not `SignedIn` and there is no
///    session to reuse, return [`FillError::NeedsSignIn`] (the host surfaces the
///    decision point). When `SignedIn`, the login [`Step::InjectCredential`] is
///    SKIPPED (session reuse — login once).
/// 2. **Egress-gate every navigation** — the storefront navigate is checked
///    against `gate` before the driver sees the URL (fail closed on deny).
/// 3. **Per item** — search, read candidates, ask the `matcher` to choose,
///    add-to-cart the choice (checking the "Added to Cart" post-condition).
///    A no-match or a failed add → `not_added` (the run continues).
/// 4. **StopGate** — the script's StopGate halts the run BEFORE any checkout;
///    `stopped_at_gate` is set. The runner NEVER navigates the order-submit
///    path (the gate would deny it anyway).
/// 5. **Cart URL** — the filled (never submitted) cart-review URL is captured.
pub async fn run_fill(
    authorized_credential_ref: Option<&str>,
    items: &[GroceryLine],
    gate: &BrowserEgressGate,
    preflight: &PreflightOutcome,
    driver: &mut dyn CartDriver,
    matcher: &Matcher,
) -> Result<FillOutcome, FillError> {
    // The login step is skipped when the session is reused (signed in). When
    // NOT signed in, we need a credential to log in — if there's none granted,
    // surface the decision point.
    let signed_in = preflight.is_signed_in();

    // The script (the blueprint). The credential ref is only used for the login
    // step, which we skip when signed in.
    let credential_ref = authorized_credential_ref.unwrap_or("");
    let script = wholefoods_cart_script(items, credential_ref);

    // 1) Storefront navigate — egress-gated before the driver sees it.
    let storefront = format!("https://{AMAZON_HOST}/");
    egress_check(gate, &storefront)?;
    let _landing = driver
        .navigate(&storefront)
        .await
        .map_err(|e| FillError::Driver(e.to_string()))?;

    // 2) Login (skipped on session reuse). When not signed in, we must inject.
    if signed_in {
        tracing::info!("wholefoods: session reused — skipping login");
    } else {
        let Some((secret_ref, target)) = script.steps.iter().find_map(|s| match s {
            Step::InjectCredential { secret_ref, target } if !secret_ref.is_empty() => {
                Some((secret_ref.clone(), target.clone()))
            }
            _ => None,
        }) else {
            return Err(FillError::NeedsSignIn(preflight.clone()));
        };
        driver
            .inject_credential(&secret_ref, &locator_handle(&target))
            .await
            .map_err(|e| FillError::Driver(e.to_string()))?;
    }

    // 3) Per item: search → match → add-to-cart.
    let mut added = Vec::new();
    let mut not_added = Vec::new();
    for line in items {
        let results = match driver.search(&line.item).await {
            Ok(snap) => snap,
            Err(e) => {
                not_added.push(NotAddedLine {
                    item: line.item.clone(),
                    reason: format!("search failed: {e}"),
                });
                continue;
            }
        };
        let candidates = driver.candidates(&results);
        match matcher.match_product(line, &candidates).await {
            MatchDecision::NoMatch(reason) => {
                not_added.push(NotAddedLine {
                    item: line.item.clone(),
                    reason,
                });
            }
            MatchDecision::Chosen(product) => {
                match driver.add_to_cart(&product, line.quantity).await {
                    Ok(post) => {
                        // Check the add-to-cart post-condition ("Added to Cart").
                        let added_ok = post.text.contains("Added to Cart")
                            || post.has_locator(&Locator::new("button", "Proceed to checkout"));
                        if added_ok {
                            added.push(AddedLine {
                                item: line.item.clone(),
                                product,
                                qty: line.quantity,
                            });
                        } else {
                            not_added.push(NotAddedLine {
                                item: line.item.clone(),
                                reason: "add-to-cart post-condition not observed".to_string(),
                            });
                        }
                    }
                    Err(e) => not_added.push(NotAddedLine {
                        item: line.item.clone(),
                        reason: format!("add-to-cart failed: {e}"),
                    }),
                }
            }
        }
    }

    // 4) The StopGate is the script's last reachable step. We assert it is
    // present (the never-checkout rail) — the runner halts here, never
    // navigating to checkout. The script ALWAYS carries exactly one StopGate.
    let stopped_at_gate = script.steps.iter().any(Step::is_stop_gate);
    debug_assert!(
        stopped_at_gate,
        "the wholefoods-cart script must carry a StopGate"
    );

    // 5) The filled (never submitted) cart-review URL — egress-checked too (it
    // is a cart-VIEW path, allowed; the order-submit path is denied).
    let cart_url = cart_review_url();
    egress_check(gate, &cart_url)?;

    Ok(FillOutcome {
        added,
        not_added,
        cart_url,
        stopped_at_gate,
    })
}

/// Check a navigation URL against the egress gate (GET, as a top-level
/// navigation). A deny fails the fill closed — the runner never lets the driver
/// touch a denied URL.
fn egress_check(gate: &BrowserEgressGate, url: &str) -> Result<(), FillError> {
    match gate.decide("GET", url) {
        crate::egress::Decision::Allow => Ok(()),
        crate::egress::Decision::Deny(reason) => Err(FillError::EgressDenied {
            url: url.to_string(),
            reason: format!("{reason:?}"),
        }),
    }
}

/// The stable handle a fill targets — the locator's `ref` hint when present,
/// else its `role:name` (the semantic handle). The live runner resolves this to
/// a Playwright locator; the mock records it.
fn locator_handle(loc: &Locator) -> String {
    loc.r#ref
        .clone()
        .unwrap_or_else(|| format!("{}:{}", loc.role, loc.name))
}

/// Build a [`BrowserEgressGate`] for the Whole Foods cart fill from the
/// authorized domains: allow the (ceiling-trimmed) domains, and ALWAYS path-deny
/// the order-submit subtree on Amazon (the never-checkout network backstop — it
/// applies even under a `*` ceiling, see [`BrowserEgressGate`]).
#[must_use]
pub fn wholefoods_gate(authorized_domains: &[String]) -> BrowserEgressGate {
    BrowserEgressGate::new(
        authorized_domains.iter().cloned(),
        std::iter::empty::<String>(),
    )
    .deny_path(AMAZON_HOST, "*", ORDER_SUBMIT_PATH)
}

/// Replay the script's steps deterministically against observed snapshots,
/// asserting it halts at the StopGate and never reaches checkout. This is the
/// never-checkout proof over the SCRIPT (independent of the runner): replay
/// stops at the gate, so the post-gate cart-navigate step is never executed.
#[must_use]
pub fn replay_halts_before_checkout(script: &AutomationScript) -> bool {
    // Drive replay with always-satisfied snapshots so it would proceed as far
    // as it possibly can; it must stop at the StopGate.
    let outcomes = crate::script::replay(script, |_, step| satisfying_snapshot(step));
    matches!(outcomes.last(), Some(StepOutcome::StoppedAtGate(_)))
        && outcomes
            .iter()
            .filter(|o| matches!(o, StepOutcome::StoppedAtGate(_)))
            .count()
            == 1
}

/// A snapshot that satisfies whatever post-condition `step` carries (used to
/// drive replay as far as it can go, proving the StopGate is the hard stop).
fn satisfying_snapshot(step: &Step) -> Snapshot {
    let expect = step.expect();
    Snapshot {
        url_host: expect.url_host.clone(),
        text: expect.text_present.clone().unwrap_or_default(),
        locators: expect.locator_present.clone().into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Cookie, StorageState};

    fn list() -> Vec<GroceryLine> {
        vec![
            GroceryLine::new("olive oil", 1, Some("extra virgin")),
            GroceryLine::new("milk", 2, Some("organic")),
        ]
    }

    const REF: &str = "op://Private/Amazon/password";

    // ── 1. the script template ──

    #[test]
    fn script_is_semantic_and_carries_only_a_reference() {
        let s = wholefoods_cart_script(&list(), REF);
        assert_eq!(s.template_id, TEMPLATE_ID);
        // The credential step holds only the op:// reference, never a value.
        s.assert_no_secret_values().unwrap();
        let json = s.to_json();
        assert!(json.contains(REF));
        assert!(!json.contains("hunter2"));
        // Locators are semantic (role/name), present in the serialized form.
        assert!(json.contains("\"role\""));
        assert!(json.contains("\"name\""));
    }

    #[test]
    fn script_has_a_stop_gate_before_any_checkout() {
        let s = wholefoods_cart_script(&list(), REF);
        let gate_idx = s
            .steps
            .iter()
            .position(Step::is_stop_gate)
            .expect("a StopGate");
        // No step AFTER the gate navigates to an order-submit path.
        for step in &s.steps[gate_idx + 1..] {
            if let Step::Navigate { url, .. } = step {
                assert!(
                    !url.contains(ORDER_SUBMIT_PATH),
                    "no order-submit navigation may follow the StopGate"
                );
            }
        }
        // Exactly one StopGate.
        assert_eq!(s.steps.iter().filter(|s| s.is_stop_gate()).count(), 1);
    }

    #[test]
    fn script_has_search_add_per_item() {
        let s = wholefoods_cart_script(&list(), REF);
        let types = s
            .steps
            .iter()
            .filter(|s| matches!(s, Step::Type { .. }))
            .count();
        let clicks = s
            .steps
            .iter()
            .filter(|s| matches!(s, Step::Click { .. }))
            .count();
        assert_eq!(types, 2, "one search per item");
        assert_eq!(clicks, 2, "one add-to-cart per item");
    }

    #[test]
    fn replay_halts_at_the_stop_gate() {
        let s = wholefoods_cart_script(&list(), REF);
        assert!(
            replay_halts_before_checkout(&s),
            "replay must stop at the StopGate and never reach the post-gate cart navigate"
        );
    }

    // ── 2. the matcher ──

    #[tokio::test]
    async fn fixture_matcher_picks_preference_match() {
        let line = GroceryLine::new("olive oil", 1, Some("extra virgin"));
        let candidates = vec![
            ProductCandidate::new("Pure Olive Oil 500ml"),
            ProductCandidate::new("Organic Extra Virgin Olive Oil 750ml"),
            ProductCandidate::new("Vegetable Oil"),
        ];
        let m = Matcher::deterministic_fixture();
        let decision = m.match_product(&line, &candidates).await;
        assert_eq!(
            decision,
            MatchDecision::Chosen("Organic Extra Virgin Olive Oil 750ml".to_string())
        );
    }

    #[tokio::test]
    async fn fixture_matcher_reports_no_match() {
        let line = GroceryLine::new("saffron", 1, Some("Spanish"));
        let candidates = vec![
            ProductCandidate::new("Black Pepper"),
            ProductCandidate::new("Salt"),
        ];
        let m = Matcher::deterministic_fixture();
        assert!(matches!(
            m.match_product(&line, &candidates).await,
            MatchDecision::NoMatch(_)
        ));
    }

    #[tokio::test]
    async fn empty_candidates_is_no_match() {
        let m = Matcher::deterministic_fixture();
        assert!(matches!(
            m.match_product(&GroceryLine::new("milk", 1, None), &[])
                .await,
            MatchDecision::NoMatch(_)
        ));
    }

    #[test]
    fn parse_reply_rejects_an_invented_product() {
        let candidates = vec![ProductCandidate::new("Real Product")];
        // The model named a product NOT in the results — fail safe to no-match.
        let d = parse_match_reply(r#"{"choice":"Phantom Product"}"#, &candidates);
        assert!(matches!(d, MatchDecision::NoMatch(_)));
    }

    #[test]
    fn parse_reply_handles_fenced_json_and_null() {
        let candidates = vec![ProductCandidate::new("Organic Milk")];
        let d = parse_match_reply("```json\n{\"choice\":\"Organic Milk\"}\n```", &candidates);
        assert_eq!(d, MatchDecision::Chosen("Organic Milk".to_string()));
        let n = parse_match_reply(r#"{"choice":null,"reason":"out of stock"}"#, &candidates);
        assert_eq!(n, MatchDecision::NoMatch("out of stock".to_string()));
    }

    #[test]
    fn build_match_prompt_lists_candidates_and_prefs() {
        let line = GroceryLine::new("milk", 2, Some("organic whole"));
        let candidates = vec![ProductCandidate {
            title: "Organic Whole Milk".into(),
            price: Some("$4.99".into()),
            add_to_cart: None,
        }];
        let p = build_match_prompt(&line, &candidates);
        assert!(p.contains("organic whole"));
        assert!(p.contains("Organic Whole Milk"));
        assert!(p.contains("$4.99"));
        assert!(p.contains("choice"));
    }

    // ── 3. the offline runner (mock driver) ──

    /// A mock [`CartDriver`] driven by hand-authored snapshots — NO browser.
    /// Records the fills (credential injection target, products added) so the
    /// test can prove what reached the "page".
    struct MockDriver {
        /// item word → the search-results candidates to surface.
        results: std::collections::HashMap<String, Vec<ProductCandidate>>,
        /// Recorded credential injections (target only — never the value).
        injected: Vec<String>,
        /// Recorded (product, qty) add-to-cart calls.
        added: Vec<(String, i64)>,
        /// Whether add-to-cart should report the "Added to Cart" success text.
        add_succeeds: bool,
    }

    impl MockDriver {
        fn new() -> Self {
            Self {
                results: std::collections::HashMap::new(),
                injected: Vec::new(),
                added: Vec::new(),
                add_succeeds: true,
            }
        }
        fn with_results(mut self, item: &str, candidates: Vec<ProductCandidate>) -> Self {
            self.results.insert(item.to_string(), candidates);
            self
        }
    }

    #[async_trait::async_trait]
    impl CartDriver for MockDriver {
        async fn navigate(&mut self, _url: &str) -> anyhow::Result<Snapshot> {
            Ok(Snapshot {
                url_host: Some(AMAZON_HOST.to_string()),
                ..Default::default()
            })
        }
        async fn inject_credential(
            &mut self,
            _secret_ref: &str,
            target: &str,
        ) -> anyhow::Result<()> {
            self.injected.push(target.to_string());
            Ok(())
        }
        async fn search(&mut self, text: &str) -> anyhow::Result<Snapshot> {
            // The snapshot's text carries the search term so `candidates()` can
            // recover it and surface the hand-authored results for that term.
            Ok(Snapshot {
                url_host: Some(AMAZON_HOST.to_string()),
                text: format!("search results for {text}"),
                ..Default::default()
            })
        }
        fn candidates(&self, snap: &Snapshot) -> Vec<ProductCandidate> {
            let term = snap.text.strip_prefix("search results for ").unwrap_or("");
            self.results.get(term).cloned().unwrap_or_default()
        }
        async fn add_to_cart(&mut self, product_title: &str, qty: i64) -> anyhow::Result<Snapshot> {
            self.added.push((product_title.to_string(), qty));
            Ok(Snapshot {
                url_host: Some(AMAZON_HOST.to_string()),
                text: if self.add_succeeds {
                    "Added to Cart".to_string()
                } else {
                    "Out of stock".to_string()
                },
                ..Default::default()
            })
        }
    }

    fn signed_in_state() -> StorageState {
        let mut s = StorageState::new(AMAZON_HOST);
        s.cookies.push(Cookie {
            name: "at-main".into(),
            value: "v".into(),
            domain: AMAZON_HOST.into(),
            path: "/".into(),
            expires: 9_999_999_999.0,
        });
        s
    }

    #[tokio::test]
    async fn run_fill_session_reuse_skips_login_and_adds_items() {
        // The signed-in session is modeled by the SignedIn preflight outcome
        // (the preflight module tests cover the storage-state → outcome path).
        assert!(!signed_in_state().cookies.is_empty());
        let mut driver = MockDriver::new()
            .with_results(
                "olive oil",
                vec![
                    ProductCandidate::new("Pure Olive Oil"),
                    ProductCandidate::new("Organic Extra Virgin Olive Oil"),
                ],
            )
            .with_results(
                "milk",
                vec![
                    ProductCandidate::new("Whole Milk"),
                    ProductCandidate::new("Organic Whole Milk"),
                ],
            );
        let gate = wholefoods_gate(&[AMAZON_HOST.to_string(), WHOLEFOODS_HOST.to_string()]);
        let outcome = run_fill(
            Some(REF),
            &list(),
            &gate,
            &PreflightOutcome::SignedIn,
            &mut driver,
            &Matcher::deterministic_fixture(),
        )
        .await
        .expect("fill succeeds offline");

        // Session reuse: the login inject was SKIPPED.
        assert!(
            driver.injected.is_empty(),
            "signed in ⇒ no credential inject"
        );
        // Both items matched their preference and landed in `added`.
        assert_eq!(outcome.added.len(), 2);
        assert_eq!(outcome.added[0].product, "Organic Extra Virgin Olive Oil");
        assert_eq!(outcome.added[0].qty, 1);
        assert_eq!(outcome.added[1].product, "Organic Whole Milk");
        assert_eq!(outcome.added[1].qty, 2);
        assert!(outcome.not_added.is_empty());
        // The never-checkout proof: halted at the gate, cart URL is a VIEW path.
        assert!(outcome.stopped_at_gate);
        assert_eq!(outcome.cart_url, cart_review_url());
        assert!(!outcome.cart_url.contains(ORDER_SUBMIT_PATH));
    }

    #[tokio::test]
    async fn run_fill_not_signed_in_injects_login_once() {
        let mut driver = MockDriver::new()
            .with_results("milk", vec![ProductCandidate::new("Organic Whole Milk")]);
        let gate = wholefoods_gate(&[AMAZON_HOST.to_string()]);
        let outcome = run_fill(
            Some(REF),
            &[GroceryLine::new("milk", 1, Some("organic"))],
            &gate,
            &PreflightOutcome::NoSession,
            &mut driver,
            &Matcher::deterministic_fixture(),
        )
        .await
        .expect("fill with login");
        // Not signed in ⇒ the login inject ran exactly once (login-once).
        assert_eq!(driver.injected.len(), 1);
        assert_eq!(driver.injected[0], "textbox:Password");
        assert_eq!(outcome.added.len(), 1);
    }

    #[tokio::test]
    async fn run_fill_no_credential_when_not_signed_in_needs_signin() {
        let mut driver = MockDriver::new();
        let gate = wholefoods_gate(&[AMAZON_HOST.to_string()]);
        // Not signed in AND no granted credential ⇒ surface the decision point.
        let err = run_fill(
            None,
            &[GroceryLine::new("milk", 1, None)],
            &gate,
            &PreflightOutcome::NoSession,
            &mut driver,
            &Matcher::deterministic_fixture(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, FillError::NeedsSignIn(_)));
    }

    #[tokio::test]
    async fn run_fill_no_match_goes_to_not_added_and_continues() {
        let mut driver = MockDriver::new()
            .with_results("milk", vec![ProductCandidate::new("Organic Whole Milk")])
            // saffron's only candidate doesn't match the item word → no match.
            .with_results("saffron", vec![ProductCandidate::new("Black Pepper")]);
        let gate = wholefoods_gate(&[AMAZON_HOST.to_string()]);
        let outcome = run_fill(
            Some(REF),
            &[
                GroceryLine::new("milk", 1, Some("organic")),
                GroceryLine::new("saffron", 1, None),
            ],
            &gate,
            &PreflightOutcome::SignedIn,
            &mut driver,
            &Matcher::deterministic_fixture(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.added.len(), 1);
        assert_eq!(outcome.added[0].item, "milk");
        assert_eq!(outcome.not_added.len(), 1);
        assert_eq!(outcome.not_added[0].item, "saffron");
        // The run still completed at the gate despite the one no-match.
        assert!(outcome.stopped_at_gate);
    }

    #[tokio::test]
    async fn run_fill_denies_offlist_navigation() {
        // A gate that allows NEITHER amazon nor wholefoods ⇒ the storefront
        // navigate is denied, the fill fails closed BEFORE any driver contact.
        let mut driver = MockDriver::new();
        let gate = wholefoods_gate(&["example.com".to_string()]);
        let err = run_fill(
            Some(REF),
            &list(),
            &gate,
            &PreflightOutcome::SignedIn,
            &mut driver,
            &Matcher::deterministic_fixture(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, FillError::EgressDenied { .. }));
        assert!(driver.added.is_empty(), "no item added on a denied fill");
    }

    #[test]
    fn gate_denies_the_order_submit_path() {
        let gate = wholefoods_gate(&[AMAZON_HOST.to_string()]);
        // The cart-view URL the runner captures is allowed…
        assert_eq!(
            gate.decide("GET", &cart_review_url()),
            crate::egress::Decision::Allow
        );
        // …but the order-submit path is denied at the network layer (the rail).
        assert_eq!(
            gate.decide(
                "POST",
                &format!("https://{AMAZON_HOST}/gp/buy/spc/handlers/display.html")
            ),
            crate::egress::Decision::Deny(crate::egress::DenyReason::PathDenied)
        );
    }
}
