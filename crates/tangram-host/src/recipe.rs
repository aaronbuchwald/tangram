//! Smart Objects SO4 — the host-mediated recipe-URL **fetch** plane
//! (`docs/design/smart-objects.md` §6 ingestion).
//!
//! A WASM component cannot fetch an arbitrary recipe URL: its egress is a
//! closed, operator-declared allow-list (`[[apps.tangram.calls]]`). So recipe
//! ingestion is **host-mediated**: the `tangram` component POSTs the pasted URL
//! to this loopback route, the HOST performs the read-only, user-initiated,
//! bounded fetch — *gated by the `tangram-automation` browser egress gate over
//! the shared `tangram-egress` canonicalizer* (ADR-0010, ADR-0008) — and
//! returns the page HTML. The component then extracts schema.org/Recipe JSON-LD
//! and LLM-normalizes the ingredients (its existing DeepSeek egress), entirely
//! in-component (`apps/tangram/src/ingest.rs`).
//!
//! ## The egress seam (NOT bypassed)
//!
//! The fetch is gated by [`tangram_automation::egress::BrowserEgressGate`] built
//! from `[automation].browser_domains_ceiling` — the operator policy ceiling.
//! Default-deny: with no ceiling configured the route refuses every fetch. This
//! is the SAME canonicalizer the component fence + the manifest verifier use, so
//! the recipe-fetch fence can never disagree with them on what a host means.
//!
//! ## What this slice does vs. defers (SO4 scope guard)
//!
//! This is the **smallest correct end-to-end slice**: a bounded HTTP GET of a
//! STATIC recipe page (schema.org JSON-LD in the served HTML — the common case
//! for major recipe sites). The full `tangram-automation` browser-driver path (a
//! supervised headless browser for JS-RENDERED pages, the `runner.rs` substrate)
//! is the **deferred seam**: [`fetch_recipe_html`] is the single choke point a
//! later change swaps for a browser-driver render when the static fetch yields no
//! JSON-LD. The egress gate, the request shape, and the operator ceiling are all
//! already wired here so that swap is local.

use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

use tangram_automation::egress::{BrowserEgressGate, Decision};
use tangram_automation::runner::AutomationSettings;

use crate::Host;

/// A coarse ceiling on a fetched recipe page so a hostile/huge URL can't blow up
/// host memory. Recipe pages with their JSON-LD are well under this.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// The per-fetch wall-clock timeout (a recipe page should resolve fast; a slow /
/// hanging target must not pin a host task).
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The query for `GET /recipe/fetch?url=<recipe url>`.
#[derive(Debug, Deserialize)]
pub struct RecipeFetchQuery {
    /// The recipe URL the user pasted (the component forwards it verbatim).
    pub url: String,
}

/// Build the recipe-fetch egress gate from the operator's `[automation]`
/// ceiling. The ceiling is the operator-policy intersection point (the
/// request-not-grant posture, `task-automation-browser.md` §5.3): a host that
/// declares no `browser_domains_ceiling` permits NO recipe fetch (default-deny).
///
/// Returns `None` when automation is disabled OR the ceiling is empty — both
/// mean "no recipe-fetch capability", which the route reports as a clear 403.
pub fn recipe_gate(settings: &AutomationSettings) -> Option<BrowserEgressGate> {
    if !settings.enabled || settings.browser_domains_ceiling.is_empty() {
        return None;
    }
    Some(BrowserEgressGate::new(
        settings.browser_domains_ceiling.clone(),
        std::iter::empty::<String>(),
    ))
}

/// `GET /recipe/fetch?url=<recipe url>` (Smart Objects SO4). Host-mediated,
/// read-only, user-initiated recipe-page fetch, gated by the automation egress
/// ceiling. Returns the page HTML (`text/html`) for the component to extract
/// JSON-LD from. Failures are plain-text status codes (the component surfaces
/// them as an ingest error):
///   * 403 — automation disabled / no ceiling, or the URL's host is not on the
///     operator ceiling (the egress gate denied it).
///   * 400 — a malformed/non-http(s) URL.
///   * 502 — the upstream fetch failed / the page was too large.
pub async fn recipe_fetch(
    State((host, _)): State<(Arc<Host>, bool)>,
    Query(query): Query<RecipeFetchQuery>,
) -> Response {
    let url = query.url.trim();
    if url.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing ?url=").into_response();
    }

    // The operator ceiling → the egress gate. Default-deny when unconfigured.
    let Some(gate) = host.recipe_gate.clone() else {
        return (
            StatusCode::FORBIDDEN,
            "recipe fetch is disabled: enable [automation] and declare a \
             browser_domains_ceiling in apps.toml (the operator policy that \
             bounds which hosts a recipe URL may be fetched from)",
        )
            .into_response();
    };

    // Gate on the PARSED method+URL through the shared canonicalizer — never a
    // string prefix. A host off the ceiling, or an unparseable URL, fails closed.
    match gate.decide("GET", url) {
        Decision::Allow => {}
        Decision::Deny(reason) => {
            tracing::warn!("recipe fetch denied ({reason:?}) for a pasted URL");
            return (
                StatusCode::FORBIDDEN,
                format!(
                    "recipe fetch denied: the URL's host is not on the operator \
                     [automation].browser_domains_ceiling ({reason:?})"
                ),
            )
                .into_response();
        }
    }

    match fetch_recipe_html(url).await {
        Ok(html) => ([(axum::http::header::CONTENT_TYPE, "text/html")], html).into_response(),
        Err(e) => {
            tracing::warn!("recipe fetch upstream error: {e}");
            (StatusCode::BAD_GATEWAY, format!("recipe fetch failed: {e}")).into_response()
        }
    }
}

/// Fetch a recipe page's HTML — the single I/O choke point.
///
/// SO4 slice: a bounded `reqwest` GET of the static page (schema.org JSON-LD in
/// the served HTML). **DEFERRED SEAM:** when the static page carries no JSON-LD
/// the component falls back to an LLM page-parse; a later change can route THIS
/// function through the `tangram-automation` browser-driver runner to render a
/// JS-heavy page first. The gate above already bounds which hosts are reachable,
/// so that swap needs no new policy.
async fn fetch_recipe_html(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported url scheme {other:?} (need http/https)"
            ));
        }
    }

    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        // A real browser UA — many recipe sites 403 a bare client. Read-only GET.
        .user_agent("Mozilla/5.0 (compatible; TangramRecipeImport/1.0)")
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client.get(parsed).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("upstream returned {status}"));
    }

    // Bound the body so a hostile target can't exhaust host memory.
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!(
            "page too large ({} bytes > {MAX_BODY_BYTES} cap)",
            bytes.len()
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_is_default_deny_when_unconfigured() {
        // Disabled automation → no gate.
        let settings = AutomationSettings::default();
        assert!(recipe_gate(&settings).is_none());

        // Enabled but empty ceiling → still no gate (default-deny).
        let settings = AutomationSettings {
            enabled: true,
            ..Default::default()
        };
        assert!(recipe_gate(&settings).is_none());
    }

    #[test]
    fn gate_allows_ceiling_hosts_only() {
        let settings = AutomationSettings {
            enabled: true,
            browser_domains_ceiling: vec!["example.com".to_string()],
            ..Default::default()
        };
        let gate = recipe_gate(&settings).expect("ceiling host → a gate");
        assert_eq!(
            gate.decide("GET", "https://example.com/recipes/pasta"),
            Decision::Allow
        );
        // A host NOT on the ceiling is denied (the operator policy intersection).
        assert!(matches!(
            gate.decide("GET", "https://evil.test/x"),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn gate_with_wildcard_ceiling_allows_any_host() {
        // The operator-accepted broad recipe-fetch egress: a non-empty ceiling
        // of just `["*"]` builds a gate (default-deny is NOT triggered — the
        // ceiling is non-empty), and that gate's any-host wildcard admits any
        // recipe host the operator never enumerated.
        let settings = AutomationSettings {
            enabled: true,
            browser_domains_ceiling: vec!["*".to_string()],
            ..Default::default()
        };
        let gate = recipe_gate(&settings).expect("non-empty ceiling → a gate");
        assert_eq!(
            gate.decide("GET", "https://cooking.nytimes.com/recipes/123"),
            Decision::Allow
        );
        assert_eq!(
            gate.decide("GET", "https://random-recipe-site.example/r/pasta"),
            Decision::Allow
        );
        // The wildcard widens the allowlist only — an unparseable URL still
        // fails closed (the canonicalizer is never disabled).
        assert!(matches!(
            gate.decide("GET", "https://attacker.com\u{0}.evil/x"),
            Decision::Deny(_)
        ));
    }
}
