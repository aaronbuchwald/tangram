//! Primitive A.3 — the request-not-grant channel
//! (`task-automation-browser.md` §4.3).
//!
//! A WASM component can only **request** an automation; it never drives the
//! browser (the WIT world is unchanged — ADR-0010). An [`AutomationRequest`]
//! names a *template by id*, parameters, the domains, and the credential
//! *references* it wants. The host **intersects** that request with operator
//! policy: a request is an UPPER BOUND, never authority on its own (the same
//! posture as `describe()` egress declarations and tenant ceilings).
//!
//! A component cannot: name browser commands directly (only a template id),
//! widen its domain allowlist (∩ the operator ceiling), choose an arbitrary
//! credential (∩ the operator grant).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// What a component asks for. Data in the app's replicated doc / an action's
/// return — NOT a grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRequest {
    /// The requesting app/component (for the operator's per-app gate).
    pub app: String,
    /// A pre-approved template id — never raw browser commands.
    pub template_id: String,
    /// Free-form template parameters (e.g. the grocery items). Opaque here.
    #[serde(default)]
    pub params: Vec<String>,
    /// The domains the request wants — intersected with the ceiling.
    #[serde(default)]
    pub domains: Vec<String>,
    /// The credential references the request wants — intersected with grant.
    #[serde(default)]
    pub credential_refs: Vec<String>,
}

/// The operator's policy (from `apps.toml [automation]`): the absolute
/// ceilings a request is intersected against.
#[derive(Debug, Clone, Default)]
pub struct OperatorPolicy {
    /// Apps allowed to request automations at all (empty ⇒ none).
    pub allowed_apps: BTreeSet<String>,
    /// Template ids that exist and are review-approved (empty ⇒ none).
    pub approved_templates: BTreeSet<String>,
    /// The absolute maximum domains any automation may touch on this host
    /// (the `browser_domains_ceiling`).
    pub domains_ceiling: BTreeSet<String>,
    /// Per-app grant of which credential references each app may request.
    /// (app → allowed refs). Absent app ⇒ no credential grant.
    pub credential_grants: std::collections::BTreeMap<String, BTreeSet<String>>,
}

impl OperatorPolicy {
    /// Convenience builder for tests / simple configs.
    pub fn new() -> Self {
        Self::default()
    }
    pub fn allow_app(mut self, app: &str) -> Self {
        self.allowed_apps.insert(app.into());
        self
    }
    pub fn approve_template(mut self, id: &str) -> Self {
        self.approved_templates.insert(id.into());
        self
    }
    pub fn ceiling(mut self, domains: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.domains_ceiling
            .extend(domains.into_iter().map(Into::into));
        self
    }
    pub fn grant_credentials(
        mut self,
        app: &str,
        refs: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.credential_grants
            .entry(app.into())
            .or_default()
            .extend(refs.into_iter().map(Into::into));
        self
    }
}

/// An authorized automation: a request that passed the policy gate, with its
/// fields ALREADY narrowed to the intersection. The runner only ever acts on
/// one of these — never on a raw [`AutomationRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedAutomation {
    pub app: String,
    pub template_id: String,
    pub params: Vec<String>,
    /// domains ∩ ceiling (sorted, deduped).
    pub domains: Vec<String>,
    /// credential_refs ∩ grant (sorted, deduped).
    pub credential_refs: Vec<String>,
}

/// Why a request was denied outright (vs. merely narrowed).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("app {0:?} is not allowed to request automations")]
    AppNotAllowed(String),
    #[error("template {0:?} does not exist or is not review-approved")]
    TemplateNotApproved(String),
    #[error("request resolved to an empty domain set (none within the operator ceiling)")]
    NoDomainsInCeiling,
}

/// Intersect a request with operator policy (§4.3). The result is the request
/// NARROWED — never widened. A request can be denied outright (app/template
/// gate) or narrowed (domains/credentials beyond policy are dropped, not
/// honored). A request asking for a domain or credential outside policy does
/// NOT fail for that reason — it is simply trimmed, exactly like a tenant
/// spec intersected with `allow_hosts_ceiling`.
pub fn authorize(
    request: &AutomationRequest,
    policy: &OperatorPolicy,
) -> Result<AuthorizedAutomation, PolicyError> {
    if !policy.allowed_apps.contains(&request.app) {
        return Err(PolicyError::AppNotAllowed(request.app.clone()));
    }
    if !policy.approved_templates.contains(&request.template_id) {
        return Err(PolicyError::TemplateNotApproved(
            request.template_id.clone(),
        ));
    }

    // domains ∩ ceiling. A `"*"` in the ceiling means ANY host — mirroring the
    // browser egress gate's wildcard (`BrowserEgressGate`), so a `["*"]` ceiling
    // authorizes the request's domains as-is rather than trimming every real host
    // away (they never literally equal "*"). When the operator narrows the
    // ceiling to explicit hosts, the exact-match intersection applies.
    let any_host = policy.domains_ceiling.iter().any(|d| d == "*");
    let domains: BTreeSet<String> = request
        .domains
        .iter()
        .filter(|d| any_host || policy.domains_ceiling.contains(*d))
        .cloned()
        .collect();
    if domains.is_empty() {
        return Err(PolicyError::NoDomainsInCeiling);
    }

    // credential_refs ∩ this app's grant
    let granted = policy.credential_grants.get(&request.app);
    let credential_refs: BTreeSet<String> = request
        .credential_refs
        .iter()
        .filter(|r| granted.is_some_and(|g| g.contains(*r)))
        .cloned()
        .collect();

    Ok(AuthorizedAutomation {
        app: request.app.clone(),
        template_id: request.template_id.clone(),
        params: request.params.clone(),
        domains: domains.into_iter().collect(),
        credential_refs: credential_refs.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> OperatorPolicy {
        OperatorPolicy::new()
            .allow_app("grocery")
            .approve_template("amazon-grocery-cart")
            .ceiling(["www.amazon.com", "fls-na.amazon.com"])
            .grant_credentials(
                "grocery",
                [
                    "op://Private/Amazon/username",
                    "op://Private/Amazon/password",
                ],
            )
    }

    fn request() -> AutomationRequest {
        AutomationRequest {
            app: "grocery".into(),
            template_id: "amazon-grocery-cart".into(),
            params: vec!["milk".into(), "eggs".into()],
            domains: vec!["www.amazon.com".into()],
            credential_refs: vec!["op://Private/Amazon/password".into()],
        }
    }

    #[test]
    fn valid_request_is_authorized() {
        let auth = authorize(&request(), &policy()).unwrap();
        assert_eq!(auth.template_id, "amazon-grocery-cart");
        assert_eq!(auth.domains, vec!["www.amazon.com"]);
        assert_eq!(auth.credential_refs, vec!["op://Private/Amazon/password"]);
        assert_eq!(auth.params, vec!["milk", "eggs"]);
    }

    #[test]
    fn unlisted_app_is_denied() {
        let mut r = request();
        r.app = "malware".into();
        assert!(matches!(
            authorize(&r, &policy()),
            Err(PolicyError::AppNotAllowed(_))
        ));
    }

    #[test]
    fn unapproved_template_is_denied() {
        let mut r = request();
        r.template_id = "place-order-now".into();
        assert!(matches!(
            authorize(&r, &policy()),
            Err(PolicyError::TemplateNotApproved(_))
        ));
    }

    #[test]
    fn overbroad_domains_are_narrowed_not_widened() {
        let mut r = request();
        r.domains = vec![
            "www.amazon.com".into(),
            "attacker.com".into(), // off-ceiling — dropped, not honored
        ];
        let auth = authorize(&r, &policy()).unwrap();
        assert_eq!(auth.domains, vec!["www.amazon.com"]);
        assert!(!auth.domains.iter().any(|d| d == "attacker.com"));
    }

    #[test]
    fn wildcard_ceiling_authorizes_any_domain() {
        // A `"*"` ceiling (the operator's any-host setting) authorizes the
        // request's domains as-is, mirroring the browser egress gate — instead
        // of trimming every real host away (none literally equals "*"). GC1
        // regression: the grocery-cart request was denied "empty domain set"
        // under the `["*"]` ceiling that recipe-import enabled.
        let policy = OperatorPolicy::new()
            .allow_app("grocery")
            .approve_template("amazon-grocery-cart")
            .ceiling(["*"])
            .grant_credentials("grocery", ["op://Private/Amazon/password"]);
        let mut r = request();
        r.domains = vec!["www.wholefoods.com".into(), "www.amazon.com".into()];
        let auth = authorize(&r, &policy).unwrap();
        assert_eq!(auth.domains, vec!["www.amazon.com", "www.wholefoods.com"]);
    }

    #[test]
    fn request_with_only_offlist_domains_is_denied() {
        let mut r = request();
        r.domains = vec!["attacker.com".into()];
        assert!(matches!(
            authorize(&r, &policy()),
            Err(PolicyError::NoDomainsInCeiling)
        ));
    }

    #[test]
    fn ungranted_credential_is_dropped() {
        let mut r = request();
        r.credential_refs = vec![
            "op://Private/Amazon/password".into(),
            "op://Private/Bank/password".into(), // not granted — dropped
        ];
        let auth = authorize(&r, &policy()).unwrap();
        assert_eq!(auth.credential_refs, vec!["op://Private/Amazon/password"]);
    }

    #[test]
    fn app_with_no_grant_gets_no_credentials() {
        let policy = OperatorPolicy::new()
            .allow_app("grocery")
            .approve_template("amazon-grocery-cart")
            .ceiling(["www.amazon.com"]);
        let auth = authorize(&request(), &policy).unwrap();
        assert!(auth.credential_refs.is_empty());
    }
}
