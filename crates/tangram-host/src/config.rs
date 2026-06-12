//! Desired state, part 1: `apps.toml`. The file watcher re-reads it on every
//! change and the reconciler converges the running set of components toward
//! it. Since Phase 3 the file is the BOOTSTRAP half of the desired state:
//! an app flagged `registry = true` is itself a Tangram app whose replicated
//! document carries ADDITIONAL app specs, merged over this file by
//! [`crate::registry::merge`] (registry entries win on name collision).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

use secrecy::SecretString;

use crate::secrets::{SecretRef, SecretRegistry, resolve_value};

/// One egress credential-injection rule (ADR-0005, RUNTIME_PLAN Phase 10b):
/// the host attaches a credential to an outbound `http-fetch` request whose
/// URL host matches the rule's key, just before performing the real request.
/// The component issues the BARE (unauthenticated) request and never receives
/// the plaintext secret. Keyed by exact host in `[apps.<app>.inject]`:
///
/// ```toml
/// [apps.nutrition.inject]
/// "api.calorieninjas.com" = { header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }
/// ```
///
/// Exactly one of `header` / `bearer` / `query` selects the injection KIND;
/// `secret` is a `scheme://locator` reference resolved host-side through the
/// [`SecretRegistry`] (ADR-0004). The injected host must ALSO be in the app's
/// `allow_hosts` — injection composes with the allowlist, never bypasses it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectRule {
    /// Inject the secret as this request HEADER (e.g. `X-Api-Key`).
    #[serde(default)]
    pub header: Option<String>,
    /// Inject the secret as `Authorization: Bearer <secret>` when `true`.
    #[serde(default)]
    pub bearer: bool,
    /// Inject the secret as this URL QUERY parameter (optional kind).
    #[serde(default)]
    pub query: Option<String>,
    /// The `scheme://locator` secret reference (e.g.
    /// `env://CALORIENINJAS_API_KEY`); resolved host-side at request time.
    pub secret: String,
}

/// Where the credential goes on the outbound request — the resolved kind of
/// an [`InjectRule`], validated once so the egress path is total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectKind {
    /// `<name>: <secret>` request header.
    Header(String),
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// `?<name>=<secret>` URL query parameter.
    Query(String),
}

impl InjectRule {
    /// Validate that exactly one of `header` / `bearer` / `query` is set and
    /// the secret reference is non-empty. Returns the resolved [`InjectKind`].
    pub fn kind(&self) -> anyhow::Result<InjectKind> {
        anyhow::ensure!(
            !self.secret.trim().is_empty(),
            "inject rule: secret reference must be non-empty"
        );
        let header = self.header.as_deref().filter(|h| !h.trim().is_empty());
        let query = self.query.as_deref().filter(|q| !q.trim().is_empty());
        match (header, self.bearer, query) {
            (Some(name), false, None) => Ok(InjectKind::Header(name.to_string())),
            (None, true, None) => Ok(InjectKind::Bearer),
            (None, false, Some(name)) => Ok(InjectKind::Query(name.to_string())),
            (None, false, None) => {
                anyhow::bail!("inject rule: set exactly one of header / bearer / query (none set)")
            }
            _ => anyhow::bail!(
                "inject rule: set exactly one of header / bearer / query (multiple set)"
            ),
        }
    }

    /// Resolve the rule's secret reference through the registry. `Ok(None)`
    /// when the reference does not resolve (e.g. an unset env var) — the
    /// egress path then skips injection and the app runs degraded (nutrition
    /// → offline), never crashing and never logging the value.
    pub async fn resolve_secret(
        &self,
        registry: &SecretRegistry,
        context: &str,
    ) -> Option<SecretString> {
        match registry.resolve(&SecretRef::new(self.secret.clone())).await {
            Ok(secret) => Some(secret),
            Err(e) => {
                // Name the reference, never the value (matches the env-seam
                // warning shape). Missing/unresolvable → degraded, not fatal.
                tracing::warn!(
                    "{context}: inject secret {} did not resolve: {e:#}",
                    self.secret
                );
                None
            }
        }
    }
}

/// The egress enforcement posture for an app's call list
/// (docs/design/fine-grained-egress.md §5.4). A single host-level toggle:
///
/// - `observe` — never deny; log every undeclared/over-broad call as a
///   candidate `[[calls]]` to add (the dev default; vibe-code freely).
/// - `warn` — inject on declared calls, allow undeclared calls but loudly
///   warn (the migration aid / legacy default).
/// - `enforce` — deny undeclared calls; inject only on matched calls (the §2
///   Anthropic deterministic-boundary posture; the prod default for apps that
///   declare ≥1 call).
///
/// Absent (`None`) defers to the migration default (§7.2), computed at
/// converge from whether the app declares any `[[calls]]` (EC7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnforcementMode {
    Observe,
    Warn,
    Enforce,
}

impl EnforcementMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Warn => "warn",
            Self::Enforce => "enforce",
        }
    }
}

/// The constrained body matcher in TOML (§9.1, the JSON-RPC-method rung): a
/// fixed JSON-pointer selector plus the literal set the selected value must be
/// a member of. The common case `body = { json_method = ["tools/list"] }`
/// defaults the pointer to `/method`; an explicit `pointer` overrides it. The
/// grammar is CLOSED — no operators, no regex, no value-matching on arbitrary
/// fields.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyMatchToml {
    /// The literal set `$<pointer>` must be a member of (string equality).
    pub json_method: Vec<String>,
    /// The JSON pointer to select; defaults to `/method` (the JSON-RPC rung).
    #[serde(default = "default_json_pointer")]
    pub pointer: String,
}

fn default_json_pointer() -> String {
    "/method".to_string()
}

/// Name-level query/header constraint in TOML — parameter/header NAMES only
/// (never values; values may carry data and matching on them invites the
/// parser-differential class, §4.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameConstraintToml {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub forbidden: Vec<String>,
}

/// One declared call in TOML (`[[apps.<app>.calls]]`, §4.1). The credential
/// injection moves INSIDE the call: it is attached ONLY to this call, not to
/// the host. A call with no `inject` goes out un-credentialed (still allowed).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallSpecToml {
    /// HTTP method — exact, case-insensitive (`GET`/`POST`/…); `"*"` allowed
    /// but discouraged (the maximally-broad method, dev-mode warns).
    #[serde(default = "default_method_any")]
    pub method: String,
    /// Exact host, same space as `allow_hosts` (must ALSO be allowlisted — the
    /// host fence composes, never bypassed).
    pub host: String,
    /// Exact path, an RFC-6570-style template (`/v1/items/{id}`), or the
    /// subtree wildcard `/**`. Defaults to the subtree (maximally broad).
    #[serde(default = "default_path_subtree")]
    pub path: String,
    /// Optional name-level query constraint.
    #[serde(default)]
    pub query: NameConstraintToml,
    /// Optional name-level header constraint.
    #[serde(default)]
    pub headers: NameConstraintToml,
    /// Optional body-byte cap; `0` forbids a body entirely. Also bounds the
    /// body parsed for the `body` matcher.
    #[serde(default)]
    pub max_body_bytes: Option<usize>,
    /// The constrained JSON-RPC-method body rung (§9.1); the body is parsed
    /// ONLY when this is present.
    #[serde(default)]
    pub body: Option<BodyMatchToml>,
    /// The credential injection scoped to THIS call (reuses the existing
    /// host-keyed [`InjectRule`] shape; the `header`/`bearer`/`query` + `secret`
    /// selection is validated by [`InjectRule::kind`]).
    #[serde(default)]
    pub inject: Option<InjectRule>,
}

fn default_method_any() -> String {
    "*".to_string()
}

fn default_path_subtree() -> String {
    "/**".to_string()
}

impl CallSpecToml {
    /// Lower this TOML entry into the runtime [`crate::egress::CallSpec`]
    /// (canonical host, parsed method/path, classified inject kind). Errors
    /// carry the offending host/path so a bad entry is a clear config error.
    pub fn resolve(&self) -> anyhow::Result<crate::egress::CallSpec> {
        use crate::egress::{
            BodyMatch, CallSpec, HeaderConstraint, MethodMatch, PathPattern, QueryConstraint,
        };

        let host = crate::egress::canonical_host(&self.host).map_err(|e| anyhow::anyhow!(e))?;
        let method = match self.method.trim() {
            "*" => MethodMatch::Any,
            "" => anyhow::bail!("call for host {host:?}: method must be non-empty"),
            m => MethodMatch::Exact(m.to_ascii_uppercase()),
        };
        let path = PathPattern::parse(&self.path).map_err(|e| anyhow::anyhow!(e))?;
        // Header NAMES are matched lowercased (the canonical request lowercases
        // them); lower the declared names too so the constraint composes.
        let headers = HeaderConstraint {
            required: self
                .headers
                .required
                .iter()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
            forbidden: self
                .headers
                .forbidden
                .iter()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
        };
        let query = QueryConstraint {
            required: self.query.required.clone(),
            forbidden: self.query.forbidden.clone(),
        };
        let body = match &self.body {
            Some(bm) => {
                anyhow::ensure!(
                    !bm.json_method.is_empty(),
                    "call for host {host:?}: body.json_method must list at least one method \
                     (an empty set never matches)"
                );
                anyhow::ensure!(
                    bm.pointer.starts_with('/'),
                    "call for host {host:?}: body.pointer {:?} must be a JSON pointer \
                     (start with '/')",
                    bm.pointer
                );
                Some(BodyMatch {
                    pointer: bm.pointer.clone(),
                    allowed: bm.json_method.clone(),
                })
            }
            None => None,
        };
        let (inject, inject_kind) = match &self.inject {
            Some(rule) => {
                let kind = rule
                    .kind()
                    .with_context(|| format!("call inject rule for host {host:?}"))?;
                (Some(rule.clone()), Some(kind))
            }
            None => (None, None),
        };
        Ok(CallSpec {
            method,
            host,
            path,
            query,
            headers,
            max_body_bytes: self.max_body_bytes,
            body,
            inject,
            inject_kind,
        })
    }
}

/// One rule of the OPT-IN egress policy engine in TOML (§9.2; ADR-0009 — the
/// deliberately-marked escape hatch, NOT the default). A rule fires when ALL of
/// its conditions hold (logical AND — the only combinator; no nesting), and its
/// `effect` (`allow` / `deny`) decides. Each field below lowers to one bounded
/// [`crate::policy::Condition`] over the SHARED canonicalization seam — there is
/// no second parser, no regex, no value-matching on arbitrary fields. An empty
/// rule (`{ effect = "deny" }`) is a catch-all default.
///
/// ```toml
/// [apps.mcpproxy.policy]
/// default = "deny"
/// rules = [
///   { effect = "allow", method = ["POST"], host = "api.vendor.com",
///     path_prefix = ["v1", "rpc"], json_method = ["tools/list", "tools/call"] },
///   { effect = "deny" },
/// ]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRuleToml {
    /// `allow` or `deny` — what a matching rule decides.
    pub effect: PolicyEffectToml,
    /// The canonical (upper-cased) method must be one of these. Empty/omitted →
    /// no method condition.
    #[serde(default)]
    pub method: Vec<String>,
    /// The canonical host must equal this. Omitted → no host condition. When
    /// set, the host must ALSO be in `allow_hosts` (the fence composes).
    #[serde(default)]
    pub host: Option<String>,
    /// The canonical path must equal this exact (canonicalized) path. Omitted →
    /// no exact-path condition. Mutually exclusive with `path_prefix`.
    #[serde(default)]
    pub path: Option<String>,
    /// The canonical path SEGMENTS must start with these (a parsed-segment
    /// prefix — never a string suffix/`endsWith`). Omitted → no prefix
    /// condition. Mutually exclusive with `path`.
    #[serde(default)]
    pub path_prefix: Vec<String>,
    /// These query parameter NAMES must each be present (never values).
    #[serde(default)]
    pub query_present: Vec<String>,
    /// These header NAMES (lowercased) must each be present (never values).
    #[serde(default)]
    pub header_present: Vec<String>,
    /// The JSON-RPC-method body rung: `$<pointer>` must be in this literal set.
    /// Reuses the SAME [`crate::egress::BodyMatch`] seam the declarative engine
    /// uses; the body is parsed only when this is set, bounded by the matched
    /// call's `max_body_bytes`.
    #[serde(default)]
    pub json_method: Vec<String>,
    /// The JSON pointer the `json_method` rung selects; defaults to `/method`.
    #[serde(default = "default_json_pointer")]
    pub pointer: String,
}

/// `allow` / `deny` in the policy TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyEffectToml {
    Allow,
    Deny,
}

impl PolicyEffectToml {
    fn to_effect(self) -> crate::policy::Effect {
        match self {
            Self::Allow => crate::policy::Effect::Allow,
            Self::Deny => crate::policy::Effect::Deny,
        }
    }
}

impl PolicyRuleToml {
    /// Lower this TOML rule into a bounded [`crate::policy::Rule`]: each set
    /// field becomes one condition over the canonical request. Canonicalizes
    /// the host/path the SAME way the seam does so the policy can never disagree
    /// with the declarative matcher. Errors carry the offending field.
    fn to_rule(&self) -> anyhow::Result<crate::policy::Rule> {
        use crate::policy::Condition;

        anyhow::ensure!(
            self.path.is_none() || self.path_prefix.is_empty(),
            "policy rule: set at most one of path / path_prefix"
        );
        let mut conditions = Vec::new();
        if !self.method.is_empty() {
            conditions.push(Condition::MethodIn(
                self.method.iter().map(|m| m.to_ascii_uppercase()).collect(),
            ));
        }
        if let Some(host) = &self.host {
            let host = crate::egress::canonical_host(host).map_err(|e| anyhow::anyhow!(e))?;
            conditions.push(Condition::HostEq(host));
        }
        if let Some(path) = &self.path {
            conditions.push(Condition::PathEq(crate::egress::canonical_path(path)));
        }
        if !self.path_prefix.is_empty() {
            // Canonicalize the declared prefix through the SAME path seam (so a
            // declared `%2e`/case/`.` cannot make the policy disagree), then
            // take its segments.
            let joined = format!("/{}", self.path_prefix.join("/"));
            let canon = crate::egress::canonical_path(&joined);
            let segs: Vec<String> = canon
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            conditions.push(Condition::PathPrefix(segs));
        }
        for name in &self.query_present {
            conditions.push(Condition::QueryNamePresent(name.clone()));
        }
        for name in &self.header_present {
            conditions.push(Condition::HeaderNamePresent(name.to_ascii_lowercase()));
        }
        if !self.json_method.is_empty() {
            anyhow::ensure!(
                self.pointer.starts_with('/'),
                "policy rule: pointer {:?} must be a JSON pointer (start with '/')",
                self.pointer
            );
            conditions.push(Condition::BodyJsonMethodIn(crate::egress::BodyMatch {
                pointer: self.pointer.clone(),
                allowed: self.json_method.clone(),
            }));
        }
        Ok(crate::policy::Rule {
            effect: self.effect.to_effect(),
            conditions,
        })
    }

    /// The host this rule names (canonicalized), if any — used to validate the
    /// rule's host composes with `allow_hosts` (the fence is never bypassed).
    fn canonical_host(&self) -> Option<String> {
        self.host
            .as_ref()
            .and_then(|h| crate::egress::canonical_host(h).ok())
    }
}

/// The OPT-IN egress policy in TOML (`[apps.<app>.policy]`, §9.2; ADR-0009): an
/// ordered rule list evaluated first-match-wins plus a `default` effect. This
/// is the explicit escape hatch — an app that attaches it is clearly marked as
/// "uses custom policy" (never silent). It runs AFTER the declarative call
/// match and can only NARROW (turn an allow into a deny), never widen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyToml {
    /// The ordered rules (first match wins). Bounded by
    /// [`crate::policy::MAX_RULES`]/[`MAX_CONDITIONS`] — over-budget is a parse
    /// error (the policy engine is deliberately bounded; fail closed at parse).
    pub rules: Vec<PolicyRuleToml>,
    /// The effect applied when no rule fires. Defaults to `deny` (fail closed —
    /// the conservative §9.2 choice: a policy that forgets a case denies).
    #[serde(default = "default_policy_deny")]
    pub default: PolicyEffectToml,
}

fn default_policy_deny() -> PolicyEffectToml {
    PolicyEffectToml::Deny
}

impl PolicyToml {
    /// Lower into the runtime [`crate::policy::Policy`], enforcing the latency
    /// budget at construction (the rule/condition caps). Returns a config error
    /// on a malformed rule or an over-budget policy.
    pub fn resolve(&self) -> anyhow::Result<crate::policy::Policy> {
        let rules = self
            .rules
            .iter()
            .map(PolicyRuleToml::to_rule)
            .collect::<anyhow::Result<Vec<_>>>()?;
        crate::policy::Policy::new(rules, self.default.to_effect()).map_err(|e| anyhow::anyhow!(e))
    }
}

/// What an app DECLARES it needs — the middle link of the verification chain
/// `granted ⊆ declared ⊆ audited` (design:
/// `docs/design/manifest-verification-plan.md` §2.4). This is a REQUEST, never
/// an authority: it is bounded above by the operator grant (`allow_hosts`/
/// `env`/`inject`) and below by the component's audited imports. The host
/// reads it but never trusts it as a grant.
///
/// Sourced from the marketplace `CapabilityManifest` on install, or written
/// directly under `[apps.<app>.declared]` for a file spec. ABSENT (the common
/// case for first-party apps) → the host derives the declaration from the
/// granted spec itself, so an honest spec verifies trivially (plan §2.4
/// back-compat).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredManifest {
    /// The declared outbound-network claim. Absent → derived from the granted
    /// `allow_hosts` (back-compat). `{ network = "none" }` is the explicit
    /// "no network" claim — verifies for zero hosts and must import no
    /// `http-fetch`.
    #[serde(default)]
    pub network: NetworkClaim,
    /// The environment-variable KEYS the app declares it reads. Absent →
    /// derived from the granted `env` keys (back-compat). env is gated
    /// manifest-side only (granted keys ⊆ declared keys); it carries data, not
    /// reach, so it has no import-level predicate (plan §2.1).
    #[serde(default)]
    pub env_keys: Option<Vec<String>>,
}

/// The declared outbound-network shape (plan §2.4, §2.6). Additive and
/// grain-agnostic: `Hosts` is today's host-level grain; `Calls` is the
/// fine-grained-egress call grain (DESIGNED-FOR, gated on that feature — see
/// [`crate::verify::CallSpec`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NetworkClaim {
    /// No outbound network at all — the component must import no `http-fetch`.
    None,
    /// A set of exact outbound host names (the existing `allow_hosts` grain).
    Hosts { hosts: Vec<String> },
    /// Fine-grained call-level claims (fine-grained-egress §4). Gated on that
    /// feature; present so the schema and the verifier's containment relation
    /// are designed for it (plan §2.6, CP6).
    Calls { calls: Vec<crate::verify::CallSpec> },
}

impl Default for NetworkClaim {
    /// A declaration with no explicit `network` defaults to "no network",
    /// which the chain then RELAXES to the granted hosts when the whole
    /// `declared` manifest is absent (see [`AppSpec::declared_capabilities`]).
    /// When a manifest IS present but omits `network`, "none" is the safe
    /// reading (declare nothing ⇒ claim nothing).
    fn default() -> Self {
        Self::None
    }
}

/// One app's spec: which component to run, what UI to serve, and what the
/// component is granted (data dir, outbound hosts, environment).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSpec {
    /// Path to the compiled `wasm32-wasip2` component on the host
    /// filesystem. Exactly one of `component` / `component_url` is required.
    #[serde(default)]
    pub component: Option<PathBuf>,
    /// Install-from-URL alternative to `component` (Phase 8): the host
    /// downloads the artifact, verifies `component_sha256` BEFORE
    /// instantiation, and caches it immutably under the host data root
    /// keyed by hash — re-converging with the same hash never refetches.
    #[serde(default)]
    pub component_url: Option<String>,
    /// REQUIRED with `component_url`: hex sha-256 of the artifact. A
    /// mismatch is a converge error (visible in the fleet status) and the
    /// app does not run.
    #[serde(default)]
    pub component_sha256: Option<String>,
    /// Directory of static UI files served at `/<app>/`.
    pub ui: PathBuf,
    /// Where the app's document lives. Default: `$HOME/.<app-name>` — and the
    /// host is the ONLY thing that touches it; the component has no
    /// filesystem capability at all.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    /// Outbound HTTP allowlist for the component's `http-fetch` import
    /// (exact host names). Empty = no outbound network at all.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    /// Environment variables handed to the component (e.g.
    /// NUTRITION_STRATEGY, CALORIENINJAS_API_KEY). A value of the exact form
    /// `${VAR}` is sugar for the secret reference `env://VAR`, resolved from
    /// the HOST's environment at converge time through the secret-resolver
    /// seam (ADR-0004, [`crate::secrets`]), so secrets stay in `.env` instead
    /// of `apps.toml`. The `scheme://locator` reference family is the
    /// extension point; Phase 10a ships only `env://`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Egress credential injection (ADR-0005, RUNTIME_PLAN Phase 10b): per
    /// outbound HOST, a rule the host applies to the component's `http-fetch`
    /// requests at the egress boundary — the component issues a BARE request
    /// and never holds the plaintext secret. Keyed by exact host name (same
    /// space as `allow_hosts`, and the host must ALSO be allowlisted). The
    /// dominant secret path now; `env` injection remains for the rare secret
    /// a component must compute on internally (which keeps in-sandbox
    /// exposure — see ADR-0005 scope note).
    #[serde(default)]
    pub inject: BTreeMap<String, InjectRule>,
    /// Call-level egress capabilities (docs/design/fine-grained-egress.md §4):
    /// an array of declared calls — method + host + path-pattern (+ optional
    /// name-level query/header constraints, a constrained JSON-RPC-method body
    /// rung, and the credential injection scoped to THAT call). When present,
    /// these are the AUTHORITATIVE inner gate: an undeclared call on an
    /// allowlisted host is denied (and un-credentialed) in `enforce` mode.
    /// STRICTLY ADDITIVE: an app with no `calls` desugars to the maximally-broad
    /// implicit call per host (`{ method = *, path = /** }`), carrying any
    /// host-keyed `inject`, so existing configs behave byte-identically (§7).
    #[serde(default)]
    pub calls: Vec<CallSpecToml>,
    /// The egress enforcement posture (`observe` / `warn` / `enforce`). Absent
    /// → the migration default computed at converge (§7.2): `warn` for legacy
    /// apps with no `calls`, `enforce` for apps declaring ≥1 call (EC7).
    #[serde(default)]
    pub enforcement: Option<EnforcementMode>,
    /// OPT-IN egress policy engine (`[apps.<app>.policy]`, §9.2; ADR-0009 — the
    /// deliberately-marked ESCAPE HATCH, NOT the default). When present, a
    /// bounded, auditable rule list runs at the egress boundary AFTER the
    /// declarative call match as an ADDITIONAL gate, for cases the declarative
    /// `[[calls]]` grammar can't express. It can only NARROW (deny a request the
    /// declarative engine allowed), never widen, and never changes which
    /// credential is injected. An app with no `policy` is behavior-identical
    /// (the section-1 guarantee). "This app uses custom policy" is surfaced, not
    /// silent.
    #[serde(default)]
    pub policy: Option<PolicyToml>,
    /// What the app DECLARES it needs (the middle link of the verification
    /// chain, plan §2.4). Optional and additive: absent → the declaration is
    /// derived from the granted fields above (an honest spec verifies
    /// trivially). When present, the host enforces `granted ⊆ declared` (a
    /// hard converge fail when violated). `[apps.<app>.declared]` in a file
    /// spec, or the marketplace manifest passed through on install.
    #[serde(default)]
    pub declared: Option<DeclaredManifest>,
    /// Optional sync base of a peer to replicate with (the host dials out,
    /// exactly like a native app's TANGRAM_REMOTE).
    #[serde(default)]
    pub remote: Option<String>,
    /// Bearer token the dial-out sync client presents to `remote` — needed
    /// when the remote's sync endpoints are private (a tangram-host tenant
    /// namespace). `${VAR}` expands from the host environment, like `env`.
    #[serde(default)]
    pub remote_token: Option<String>,
    /// This app IS a fleet registry: the host subscribes to its document and
    /// merges its replicated spec list into the desired state (Phase 3).
    /// Mutating routes on registry apps are gated behind TANGRAM_AUTH_TOKEN.
    #[serde(default)]
    pub registry: bool,
    /// Gate this app's mutating routes (`POST /api/actions/*` and MCP
    /// `tools/call` of mutating tools) behind `Authorization: Bearer
    /// $TANGRAM_AUTH_TOKEN`, like a registry app. No effect when the host
    /// has no token.
    #[serde(default)]
    pub require_auth: bool,
    /// Disabled apps stay on record but are not run.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// App names become path prefixes (and data-dir names), so keep them
/// URL-trivial. Shared by the file loader and the registry-entry parser, and
/// by tenant names (which become the `/t/<tenant>/` prefix). `t` is reserved
/// for the tenant namespace itself and `mcp` for the aggregate endpoint.
pub fn validate_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "app name {name:?} must be alphanumeric/dash/underscore (it becomes a path prefix)"
    );
    anyhow::ensure!(
        name != "t" && name != "mcp",
        "name {name:?} is reserved ({} routes live there)",
        if name == "t" {
            "tenant namespace /t/<tenant>/"
        } else {
            "the aggregate /mcp endpoint"
        }
    );
    Ok(())
}

/// Expand one config value through the secret-resolution seam (ADR-0004): the
/// exact form `${VAR}` is sugar for `env://VAR` and the `scheme://…` family of
/// references resolve through the [`SecretRegistry`]; anything else passes
/// through as-is. A reference that fails to resolve (e.g. an unset host var)
/// expands to empty with a warning — byte-identical to the pre-seam `${VAR}`
/// behavior. Shared by app `env`, `remote_token`, and tenant `token` values,
/// so secrets can stay in `.env`. The resolved value is never logged.
pub async fn expand_value(registry: &SecretRegistry, context: &str, value: &str) -> String {
    resolve_value(registry, context, value).await
}

/// Where an app's component bytes come from: a local path, or a URL whose
/// artifact must hash to the pinned sha-256 (Phase 8 install-from-URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentSource {
    Path(PathBuf),
    Url { url: String, sha256: String },
}

/// `component_sha256` format check: exactly 64 hex characters. Returns the
/// lowercased digest so cache keys are canonical.
pub fn validate_sha256(digest: &str) -> anyhow::Result<String> {
    let digest = digest.trim().to_ascii_lowercase();
    anyhow::ensure!(
        digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
        "component_sha256 must be 64 hex characters (a sha-256 digest), got {digest:?}"
    );
    Ok(digest)
}

impl AppSpec {
    /// Validate and classify the spec's component source: exactly one of
    /// `component` (local path) and `component_url` (+ a well-formed
    /// `component_sha256`) must be set. Shared by the file loader, the
    /// registry-entry parser, and the converge loop.
    pub fn component_source(&self) -> anyhow::Result<ComponentSource> {
        match (&self.component, &self.component_url) {
            (Some(path), None) => {
                anyhow::ensure!(
                    self.component_sha256.is_none(),
                    "component_sha256 is only valid with component_url \
                     (local component paths are not hash-verified)"
                );
                anyhow::ensure!(
                    !path.as_os_str().is_empty(),
                    "component must be a non-empty path"
                );
                Ok(ComponentSource::Path(path.clone()))
            }
            (None, Some(url)) => {
                anyhow::ensure!(
                    url.starts_with("https://") || url.starts_with("http://"),
                    "component_url must be http(s), got {url:?}"
                );
                let digest = self.component_sha256.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "component_url requires component_sha256 (the artifact is \
                         verified before instantiation)"
                    )
                })?;
                Ok(ComponentSource::Url {
                    url: url.clone(),
                    sha256: validate_sha256(digest)?,
                })
            }
            (Some(_), Some(_)) => anyhow::bail!(
                "set exactly one of component (local path) and component_url, not both"
            ),
            (None, None) => {
                anyhow::bail!("set exactly one of component (local path) and component_url")
            }
        }
    }

    /// The app's resolved env, with each value resolved through the secret
    /// seam (`${VAR}` sugar for `env://VAR`; unresolved → empty + warning).
    pub async fn resolved_env(
        &self,
        registry: &SecretRegistry,
        app: &str,
    ) -> Vec<(String, String)> {
        let mut resolved = Vec::with_capacity(self.env.len());
        for (key, value) in &self.env {
            let value = expand_value(registry, &format!("{app}: env {key}"), value).await;
            resolved.push((key.clone(), value));
        }
        resolved
    }

    /// Validate every injection rule: each rule must name exactly one kind
    /// with a non-empty secret reference, and its host must ALSO be in
    /// `allow_hosts` — injection composes with the allowlist (ADR-0005), it
    /// never grants reach the allowlist withheld. Called by the file/registry
    /// loaders so a bad rule is a clear config error, not a silent miss.
    pub fn validate_inject(&self) -> anyhow::Result<()> {
        for (host, rule) in &self.inject {
            rule.kind()
                .with_context(|| format!("inject rule for host {host:?}"))?;
            anyhow::ensure!(
                self.allow_hosts
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(host)),
                "inject rule for host {host:?} targets a host that is not in allow_hosts \
                 (injection composes with the allowlist — add {host:?} to allow_hosts)"
            );
        }
        Ok(())
    }

    /// The validated injection rules, keyed by lowercased host for matching
    /// against an outbound request. Skips (warns on) any malformed rule so a
    /// single bad entry can't take the app's egress down — `validate_inject`
    /// already rejected those at load, this is the runtime safety net.
    pub fn resolved_inject(&self) -> Vec<(String, InjectKind, InjectRule)> {
        self.inject
            .iter()
            .filter_map(|(host, rule)| match rule.kind() {
                Ok(kind) => Some((host.to_ascii_lowercase(), kind, rule.clone())),
                Err(e) => {
                    tracing::warn!("ignoring inject rule for host {host:?}: {e:#}");
                    None
                }
            })
            .collect()
    }

    /// Validate every declared `[[calls]]` entry: each call's host must be in
    /// `allow_hosts` (the host fence composes, never bypassed — §4.2 step 2),
    /// its method/path-template/body parse, and its `inject` (if any) names
    /// exactly one kind. Called by the file/registry loaders alongside
    /// [`Self::validate_inject`] so a bad call is a clear config error.
    pub fn validate_calls(&self) -> anyhow::Result<()> {
        for (i, call) in self.calls.iter().enumerate() {
            let resolved = call
                .resolve()
                .with_context(|| format!("[[calls]] #{} (host {:?})", i + 1, call.host))?;
            anyhow::ensure!(
                self.allow_hosts
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&resolved.host)),
                "[[calls]] #{} targets host {:?} which is not in allow_hosts \
                 (the call grant composes with the allowlist — add {:?} to allow_hosts)",
                i + 1,
                resolved.host,
                resolved.host
            );
        }
        Ok(())
    }

    /// The effective call list for egress enforcement, as runtime
    /// [`crate::egress::CallSpec`]s.
    ///
    /// - When the app declares `[[calls]]`, those are the authoritative inner
    ///   gate (malformed entries are skipped with a warning — `validate_calls`
    ///   already rejected them at load, this is the runtime safety net).
    /// - When it declares NONE, the list is the COMPAT SHIM (§4.2 / §7.1): one
    ///   maximally-broad implicit call per allowlisted host
    ///   (`{ method = *, path = /** }`), carrying that host's host-keyed
    ///   `inject` if any — so a bare `allow_hosts` / host-keyed
    ///   `[apps.X.inject]` behaves BYTE-IDENTICALLY to before.
    pub fn resolved_calls(&self) -> Vec<crate::egress::CallSpec> {
        if !self.calls.is_empty() {
            return self
                .calls
                .iter()
                .filter_map(|call| match call.resolve() {
                    Ok(spec) => Some(spec),
                    Err(e) => {
                        tracing::warn!("ignoring [[calls]] entry for host {:?}: {e:#}", call.host);
                        None
                    }
                })
                .collect();
        }
        // Compat shim: desugar each allowlisted host to the broad implicit
        // call, moving its host-keyed inject onto it. The host-keyed inject map
        // is lowercased here to match the canonical host.
        let inject = self.resolved_inject();
        self.allow_hosts
            .iter()
            .map(|host| {
                let host_lc = host.to_ascii_lowercase();
                let on_call = inject
                    .iter()
                    .find(|(h, _, _)| *h == host_lc)
                    .map(|(_, kind, rule)| (kind.clone(), rule.clone()));
                crate::egress::CallSpec::implicit_subtree(&host_lc, on_call)
            })
            .collect()
    }

    /// The effective egress enforcement mode for this app. An explicit
    /// `enforcement` wins; otherwise the migration default (§7.2): `warn` for a
    /// legacy app declaring no `[[calls]]`, `enforce` for an app that has
    /// signaled intent by declaring ≥1 call. This keeps existing configs
    /// (which declare none) non-blocking while an app that opts into the call
    /// grammar gets the strict deterministic boundary.
    pub fn effective_enforcement(&self) -> EnforcementMode {
        self.enforcement.unwrap_or(if self.calls.is_empty() {
            EnforcementMode::Warn
        } else {
            EnforcementMode::Enforce
        })
    }

    /// Validate the OPT-IN egress policy (§9.2; ADR-0009), if any: it must lower
    /// without error (well-formed rules) and stay within the latency budget (the
    /// rule/condition caps — over-budget fails closed AT PARSE), and any host a
    /// rule names must be in `allow_hosts` (the host fence composes, never
    /// bypassed — the policy can only narrow, so a rule cannot name a host the
    /// allowlist withheld). Called by the file/registry loaders alongside
    /// [`Self::validate_calls`] so a bad policy is a clear config error.
    pub fn validate_policy(&self) -> anyhow::Result<()> {
        let Some(policy) = &self.policy else {
            return Ok(());
        };
        // Budget + rule well-formedness (fail closed at parse).
        policy.resolve().context("egress policy")?;
        // Each rule's host (if any) must compose with allow_hosts.
        for (i, rule) in policy.rules.iter().enumerate() {
            if let Some(host) = rule.canonical_host() {
                anyhow::ensure!(
                    self.allow_hosts
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(&host)),
                    "egress policy rule #{} names host {host:?} which is not in allow_hosts \
                     (the policy composes with the allowlist — add {host:?} to allow_hosts)",
                    i + 1
                );
            }
        }
        Ok(())
    }

    /// The resolved OPT-IN egress policy (§9.2; ADR-0009), if any — the bounded
    /// runtime [`crate::policy::Policy`] the egress gate evaluates AFTER the
    /// declarative call match. `None` for the overwhelming majority of apps
    /// (the declarative grammar is the default); `validate_policy` already
    /// rejected a malformed/over-budget policy at load, so a lowering error here
    /// is logged and treated as "no policy gate" only as a runtime safety net —
    /// but the caller (`AppRuntime::build`) FAILS the app build on a policy that
    /// won't resolve, so a surfaced policy is never silently dropped.
    pub fn resolved_policy(&self) -> anyhow::Result<Option<crate::policy::Policy>> {
        match &self.policy {
            Some(p) => Ok(Some(p.resolve()?)),
            None => Ok(None),
        }
    }

    /// Whether this app attaches the OPT-IN egress policy engine — the surfaced
    /// "uses custom policy" marker (never silent, §9.2).
    pub fn uses_policy(&self) -> bool {
        self.policy.is_some()
    }

    /// Whether this app declares ANY egress injection — host-keyed
    /// `[apps.X.inject]` OR a call-level `inject` on a `[[calls]]` entry. The
    /// capabilities-probe gate uses this to decide whether to derive the
    /// "configured" signal host-side (ADR-0005); a call-grained app must be
    /// treated the same as a host-keyed one.
    pub fn has_any_inject(&self) -> bool {
        !self.inject.is_empty() || self.calls.iter().any(|c| c.inject.is_some())
    }

    /// The EFFECTIVE declared capabilities for the verification chain (plan
    /// §2.3 step 4). When the spec carries an explicit `declared` manifest it
    /// is used verbatim; otherwise the declaration is DERIVED from the granted
    /// fields — `Hosts(allow_hosts)` (or `None` when empty) plus the granted
    /// env keys — so an honest, un-annotated spec verifies trivially
    /// (`granted == declared`). This is the back-compat default plan §2.4
    /// names: never widen past the operator grant, and an app that says
    /// nothing is taken to declare exactly what it was granted.
    pub fn declared_capabilities(&self) -> crate::verify::DeclaredCapabilities {
        use crate::verify::DeclaredCapabilities;
        match &self.declared {
            Some(manifest) => DeclaredCapabilities::from_manifest(manifest, &self.allow_hosts),
            None => DeclaredCapabilities::derived_from_grant(
                &self.allow_hosts,
                self.env.keys().cloned(),
            ),
        }
    }

    /// The EFFECTIVE granted capabilities for the verification chain — the
    /// POST-CEILING values already on this spec (plan §3.1: the tenant ceiling
    /// intersection has already been applied to `allow_hosts` by
    /// `tenant::effective_spec`, so verifying the spec here verifies the
    /// effective grant, never the raw pre-ceiling request).
    pub fn granted_capabilities(&self) -> crate::verify::GrantedCapabilities {
        crate::verify::GrantedCapabilities {
            allow_hosts: self
                .allow_hosts
                .iter()
                .map(|h| h.to_ascii_lowercase())
                .collect(),
            inject_hosts: self.inject.keys().map(|h| h.to_ascii_lowercase()).collect(),
            env_keys: self.env.keys().cloned().collect(),
            // Host-grained today; call-grained grants arrive with
            // fine-grained-egress (plan §2.6, CP6).
            calls: Vec::new(),
        }
    }

    /// Whether this app has at least one injection rule whose secret resolves
    /// — i.e. an egress credential is genuinely configured (ADR-0005). The
    /// capabilities probe derives "configured" from this (host-side) instead
    /// of from the component seeing an env var. `false` when there are no
    /// rules or none resolve (→ the app stays offline/degraded, cleanly).
    /// Considers BOTH host-keyed inject and call-level inject (the effective
    /// `resolved_calls`), so a call-grained credential counts as configured.
    pub async fn any_inject_resolves(&self, registry: &SecretRegistry, app: &str) -> bool {
        for call in self.resolved_calls() {
            if let Some(rule) = &call.inject
                && rule
                    .resolve_secret(registry, &format!("{app}: call inject {}", call.host))
                    .await
                    .is_some()
            {
                return true;
            }
        }
        false
    }

    /// The resolved `remote_token` (through the secret seam); empty → None.
    pub async fn resolved_remote_token(
        &self,
        registry: &SecretRegistry,
        app: &str,
    ) -> Option<String> {
        let token = self.remote_token.as_deref()?;
        let token = expand_value(registry, &format!("{app}: remote_token"), token).await;
        (!token.trim().is_empty()).then_some(token)
    }

    /// The app's data directory: explicit `data_dir`, else `$HOME/.<name>`
    /// (the ADR-0001 capability-grant root), else `./data/<name>`.
    pub fn resolved_data_dir(&self, app: &str) -> PathBuf {
        match (&self.data_dir, std::env::var("HOME")) {
            (Some(dir), _) => dir.clone(),
            (None, Ok(home)) => PathBuf::from(home).join(format!(".{app}")),
            (None, Err(_)) => PathBuf::from("data").join(app),
        }
    }
}

/// One tenant's config: `[tenants.<name>]` (RUNTIME_PLAN Phase 5). A tenant
/// is an isolated app set with its own data tree, grants, and control plane,
/// served under `/t/<name>/` — every request there requires the tenant's
/// bearer token.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantSpec {
    /// REQUIRED bearer token for everything under `/t/<name>/`. `${VAR}`
    /// expands from the host environment so the secret stays in `.env`; an
    /// unresolved token disables the tenant (all requests 401).
    pub token: String,
    /// Cap on the tenant's enabled apps (bootstrap + registry-installed).
    /// Apps beyond the cap are not run and error in the tenant's fleet.
    #[serde(default = "default_max_apps")]
    pub max_apps: usize,
    /// Tenant-wide outbound ceiling: a tenant app's effective `allow_hosts`
    /// is the INTERSECTION of its spec and this list. `None` = no ceiling.
    #[serde(default)]
    pub allow_hosts_ceiling: Option<Vec<String>>,
    /// Bootstrap apps the tenant starts with (same schema as `[apps.*]`).
    /// Empty/omitted → the tenant starts with just a registry instance,
    /// cloned from the file's own registry app spec.
    #[serde(default)]
    pub apps: BTreeMap<String, AppSpec>,
}

fn default_max_apps() -> usize {
    8
}

impl TenantSpec {
    /// The resolved bearer token (through the secret seam); empty → None,
    /// which disables the tenant rather than running it open.
    pub async fn resolved_token(&self, registry: &SecretRegistry, tenant: &str) -> Option<String> {
        let token = expand_value(registry, &format!("tenant {tenant}: token"), &self.token).await;
        (!token.trim().is_empty()).then_some(token)
    }
}

/// `[tenants]`: the section's own keys (data_root) plus one sub-table per
/// tenant. Present and non-empty → multi-tenancy mode is on, ALONGSIDE the
/// top-level apps (which keep serving unauthenticated exactly as before).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct TenantsConfig {
    /// Root of every tenant's data tree:
    /// `<data_root>/<tenant>/<app>/<app>.automerge`.
    /// Default: `$HOME/.tangram-tenants`.
    #[serde(default)]
    pub data_root: Option<PathBuf>,
    #[serde(flatten)]
    pub tenants: BTreeMap<String, TenantSpec>,
}

impl TenantsConfig {
    /// The resolved tenant data root (ADR-0001-style default under `$HOME`).
    pub fn resolved_data_root(&self) -> PathBuf {
        match (&self.data_root, std::env::var("HOME")) {
            (Some(dir), _) => dir.clone(),
            (None, Ok(home)) => PathBuf::from(home).join(".tangram-tenants"),
            (None, Err(_)) => PathBuf::from("data").join("tangram-tenants"),
        }
    }
}

/// `[artifacts]` — host-side WASM blob upload + hosting (Phase S2b). When
/// `upload_enabled` is true the host exposes `POST /artifacts` (store an
/// uploaded component, computing its sha-256) and `GET /artifacts/<sha>.wasm`
/// (serve it). DEFAULT OFF: open upload is arbitrary-blob storage — an abuse,
/// DoS, and malware-hosting magnet on a public bind. See the MUST-FIX
/// checklist at the route in `routes.rs` and in `crates/tangram-host/README.md`.
/// This is a dev/demo capability until that checklist is met.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactsConfig {
    /// Open artifact upload. DEFAULT OFF. When false, `POST /artifacts`
    /// 404s and `GET /artifacts/<sha>.wasm` serves nothing. When true, the
    /// host REFUSES to start on a non-loopback bind without
    /// `TANGRAM_AUTH_TOKEN` (mirrors the registry posture) and logs a loud
    /// startup warning.
    #[serde(default)]
    pub upload_enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    #[serde(default)]
    pub apps: BTreeMap<String, AppSpec>,
    /// `[tenants]` — multi-tenancy mode (RUNTIME_PLAN Phase 5). Absent (the
    /// default) → single-tenant behavior, byte-identical to before.
    #[serde(default)]
    pub tenants: TenantsConfig,
    /// `[gateway]` — route MCP through a host-managed agentgateway child
    /// (see `crate::gateway`). Applied at startup, not converged live.
    #[serde(default)]
    pub gateway: crate::gateway::GatewaySettings,
    /// `[artifacts]` — WASM blob upload + hosting (Phase S2b). DEFAULT OFF.
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
}

impl HostConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text)?;
        for (name, spec) in &config.apps {
            validate_name(name)?;
            spec.component_source()
                .map(|_| ())
                .with_context(|| format!("app {name:?}"))?;
            spec.validate_inject()
                .with_context(|| format!("app {name:?}"))?;
            spec.validate_calls()
                .with_context(|| format!("app {name:?}"))?;
            spec.validate_policy()
                .with_context(|| format!("app {name:?}"))?;
        }
        for (tenant, spec) in &config.tenants.tenants {
            validate_name(tenant).with_context(|| format!("tenant name {tenant:?}"))?;
            anyhow::ensure!(
                !spec.token.trim().is_empty(),
                "tenant {tenant:?}: token must be set (use \"${{VAR}}\" to read it from the \
                 host environment)"
            );
            for (app, app_spec) in &spec.apps {
                validate_name(app).with_context(|| format!("tenant {tenant:?} app {app:?}"))?;
                app_spec
                    .component_source()
                    .map(|_| ())
                    .with_context(|| format!("tenant {tenant:?} app {app:?}"))?;
                app_spec
                    .validate_inject()
                    .with_context(|| format!("tenant {tenant:?} app {app:?}"))?;
                app_spec
                    .validate_calls()
                    .with_context(|| format!("tenant {tenant:?} app {app:?}"))?;
                app_spec
                    .validate_policy()
                    .with_context(|| format!("tenant {tenant:?} app {app:?}"))?;
                if let Some(dir) = &app_spec.data_dir {
                    crate::tenant::validate_tenant_data_dir(dir).with_context(|| {
                        format!("tenant {tenant:?} app {app:?}: data_dir {}", dir.display())
                    })?;
                }
            }
            // The default bootstrap (no apps template) clones the file's own
            // registry app — require one so the tenant isn't born empty.
            anyhow::ensure!(
                !spec.apps.is_empty() || config.apps.values().any(|s| s.registry && s.enabled),
                "tenant {tenant:?} has no [tenants.{tenant}.apps.*] and apps.toml has no \
                 registry app to clone as its default bootstrap"
            );
        }
        Ok(config)
    }

    /// True if any (enabled) app in the file is a registry.
    pub fn has_registry(&self) -> bool {
        self.apps.values().any(|spec| spec.registry && spec.enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_declared_manifest_and_defaults_to_derived() {
        use crate::verify::DeclaredNetwork;
        // No `declared` block → declaration derived from the grant.
        let config = HostConfig::parse(
            r#"
            [apps.notes]
            component = "notes.wasm"
            ui = "ui"
            allow_hosts = ["api.example.com"]
            [apps.notes.env]
            K = "v"
            "#,
        )
        .unwrap();
        let derived = config.apps["notes"].declared_capabilities();
        assert_eq!(
            derived.network,
            DeclaredNetwork::Hosts(["api.example.com".into()].into_iter().collect())
        );
        assert!(derived.env_keys.contains("K"));

        // An explicit `declared` block is used verbatim.
        let config = HostConfig::parse(
            r#"
            [apps.app]
            component = "a.wasm"
            ui = "ui"
            allow_hosts = ["api.calorieninjas.com"]
            [apps.app.declared.network]
            kind = "hosts"
            hosts = ["api.calorieninjas.com"]
            [apps.app.declared]
            env_keys = ["NUTRITION_STRATEGY"]
            "#,
        )
        .unwrap();
        let declared = config.apps["app"].declared_capabilities();
        assert_eq!(
            declared.network,
            DeclaredNetwork::Hosts(["api.calorieninjas.com".into()].into_iter().collect())
        );
        assert!(declared.env_keys.contains("NUTRITION_STRATEGY"));

        // `network = none` declares no outbound network.
        let config = HostConfig::parse(
            r#"
            [apps.app]
            component = "a.wasm"
            ui = "ui"
            [apps.app.declared.network]
            kind = "none"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.apps["app"].declared_capabilities().network,
            DeclaredNetwork::None
        );
    }

    #[test]
    fn parses_registry_and_auth_flags() {
        let config = HostConfig::parse(
            r#"
            [apps.registry]
            component = "registry.wasm"
            ui = "ui"
            registry = true

            [apps.notes]
            component = "notes.wasm"
            ui = "notes-ui"
            require_auth = true
            enabled = false
            "#,
        )
        .unwrap();
        let registry = &config.apps["registry"];
        assert!(registry.registry);
        assert!(!registry.require_auth);
        assert!(registry.enabled, "enabled defaults to true");
        let notes = &config.apps["notes"];
        assert!(!notes.registry);
        assert!(notes.require_auth);
        assert!(!notes.enabled);
        assert!(config.has_registry());
    }

    #[test]
    fn parses_gateway_section_and_defaults_off() {
        let config = HostConfig::parse(
            r#"
            [gateway]
            enabled = true
            binary = "/usr/local/bin/agentgateway"
            port = 19200

            [apps.notes]
            component = "notes.wasm"
            ui = "ui"
            "#,
        )
        .unwrap();
        assert!(config.gateway.enabled);
        assert_eq!(
            config.gateway.binary.as_deref(),
            Some(std::path::Path::new("/usr/local/bin/agentgateway"))
        );
        assert_eq!(config.gateway.port, Some(19200));
        // No [gateway] section → disabled (direct serving, today's behavior).
        let config = HostConfig::parse("[apps.a]\ncomponent = \"a\"\nui = \"u\"").unwrap();
        assert!(!config.gateway.enabled);
    }

    #[test]
    fn parses_artifacts_section_and_defaults_off() {
        // No [artifacts] section → upload is OFF (the safe default).
        let config = HostConfig::parse("[apps.a]\ncomponent = \"a\"\nui = \"u\"").unwrap();
        assert!(
            !config.artifacts.upload_enabled,
            "open upload must default OFF"
        );
        // Explicit opt-in.
        let config = HostConfig::parse(
            "[artifacts]\nupload_enabled = true\n[apps.a]\ncomponent = \"a\"\nui = \"u\"",
        )
        .unwrap();
        assert!(config.artifacts.upload_enabled);
        // Unknown keys in the section are rejected (deny_unknown_fields).
        assert!(HostConfig::parse("[artifacts]\nbogus = true").is_err());
    }

    #[test]
    fn rejects_bad_names_and_unknown_fields() {
        assert!(HostConfig::parse("[apps.\"bad name\"]\ncomponent = \"a\"\nui = \"b\"").is_err());
        assert!(HostConfig::parse("[apps.ok]\ncomponent = \"a\"\nui = \"b\"\nbogus = 1").is_err());
        // `t` (the tenant namespace) and `mcp` (the aggregate endpoint) are
        // reserved as app names.
        for reserved in ["t", "mcp"] {
            let err =
                HostConfig::parse(&format!("[apps.{reserved}]\ncomponent = \"a\"\nui = \"b\""))
                    .unwrap_err();
            assert!(err.to_string().contains("reserved"), "{reserved}: {err}");
        }
    }

    const TENANTED: &str = r#"
        [apps.registry]
        component = "registry.wasm"
        ui = "registry-ui"
        registry = true

        [tenants]
        data_root = "/srv/tenants"

        [tenants.alice]
        token = "${ALICE_TOKEN}"
        max_apps = 3
        allow_hosts_ceiling = ["api.calorieninjas.com"]

        [tenants.alice.apps.notes]
        component = "notes.wasm"
        ui = "notes-ui"

        [tenants.bob]
        token = "literal-bob-token"
    "#;

    #[tokio::test]
    async fn parses_tenants_alongside_apps() {
        let registry = SecretRegistry::default();
        let config = HostConfig::parse(TENANTED).unwrap();
        assert_eq!(
            config.tenants.data_root,
            Some(PathBuf::from("/srv/tenants"))
        );
        assert_eq!(
            config.tenants.resolved_data_root(),
            PathBuf::from("/srv/tenants")
        );
        let alice = &config.tenants.tenants["alice"];
        assert_eq!(alice.token, "${ALICE_TOKEN}");
        assert_eq!(alice.max_apps, 3);
        assert_eq!(
            alice.allow_hosts_ceiling,
            Some(vec!["api.calorieninjas.com".to_string()])
        );
        assert_eq!(
            alice.apps["notes"].component,
            Some(PathBuf::from("notes.wasm"))
        );
        let bob = &config.tenants.tenants["bob"];
        assert_eq!(bob.max_apps, 8, "max_apps defaults to 8");
        assert_eq!(bob.allow_hosts_ceiling, None);
        assert!(bob.apps.is_empty(), "default bootstrap: just a registry");

        // Token resolution: ${VAR} expands; unset → None (tenant disabled).
        assert_eq!(
            bob.resolved_token(&registry, "bob").await.as_deref(),
            Some("literal-bob-token")
        );
        // Safety: test-local var name, nothing else reads it concurrently.
        unsafe { std::env::set_var("TANGRAM_TEST_ALICE_TOKEN_SET", "s3cret") };
        let mut alice2 = alice.clone();
        alice2.token = "${TANGRAM_TEST_ALICE_TOKEN_SET}".into();
        assert_eq!(
            alice2.resolved_token(&registry, "alice").await.as_deref(),
            Some("s3cret")
        );
        alice2.token = "${TANGRAM_TEST_ALICE_TOKEN_UNSET}".into();
        assert_eq!(alice2.resolved_token(&registry, "alice").await, None);

        // No [tenants] section → empty map, single-tenant mode.
        let config = HostConfig::parse("[apps.a]\ncomponent = \"c\"\nui = \"u\"").unwrap();
        assert!(config.tenants.tenants.is_empty());
        assert_eq!(config.tenants.data_root, None);
    }

    #[test]
    fn rejects_invalid_tenants() {
        // Missing token.
        let err = HostConfig::parse("[tenants.alice]\nmax_apps = 2").unwrap_err();
        assert!(err.to_string().contains("token"), "{err}");
        // Empty token.
        let err = HostConfig::parse("[tenants.alice]\ntoken = \" \"").unwrap_err();
        assert!(err.to_string().contains("token"), "{err}");
        // Bad tenant name.
        assert!(HostConfig::parse("[tenants.\"bad name\"]\ntoken = \"x\"").is_err());
        // Tenant app with an escaping data_dir.
        for escape in ["/etc/evil", "../bob"] {
            let err = HostConfig::parse(&format!(
                r#"
                [tenants.alice]
                token = "x"
                [tenants.alice.apps.notes]
                component = "c"
                ui = "u"
                data_dir = "{escape}"
                "#
            ))
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("relative path"),
                "{escape}: {err:#}"
            );
        }
        // Default bootstrap (no apps) requires a registry app in the file.
        let err = HostConfig::parse("[tenants.alice]\ntoken = \"x\"").unwrap_err();
        assert!(format!("{err:#}").contains("registry"), "{err:#}");
    }

    const GOOD_SHA: &str = "a665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3";

    #[test]
    fn component_source_requires_exactly_one_of_path_and_url() {
        // Local path: ok, and sha256 alongside it is rejected.
        let config = HostConfig::parse("[apps.a]\ncomponent = \"a.wasm\"\nui = \"u\"").unwrap();
        assert_eq!(
            config.apps["a"].component_source().unwrap(),
            ComponentSource::Path(PathBuf::from("a.wasm"))
        );
        let err = HostConfig::parse(&format!(
            "[apps.a]\ncomponent = \"a.wasm\"\ncomponent_sha256 = \"{GOOD_SHA}\"\nui = \"u\""
        ))
        .unwrap_err();
        assert!(format!("{err:#}").contains("component_url"), "{err:#}");

        // URL + sha256: ok (the digest is canonicalized to lowercase).
        let config = HostConfig::parse(&format!(
            "[apps.a]\ncomponent_url = \"https://x.test/a.wasm\"\n\
             component_sha256 = \"{}\"\nui = \"u\"",
            GOOD_SHA.to_ascii_uppercase()
        ))
        .unwrap();
        assert_eq!(
            config.apps["a"].component_source().unwrap(),
            ComponentSource::Url {
                url: "https://x.test/a.wasm".into(),
                sha256: GOOD_SHA.into()
            }
        );

        // URL without sha256, both sources, neither source, bad scheme: all
        // parse errors (the whole file is rejected, like other bad specs).
        for (toml, needle) in [
            (
                "[apps.a]\ncomponent_url = \"https://x.test/a.wasm\"\nui = \"u\"".to_string(),
                "requires component_sha256",
            ),
            (
                format!(
                    "[apps.a]\ncomponent = \"a.wasm\"\n\
                     component_url = \"https://x.test/a.wasm\"\n\
                     component_sha256 = \"{GOOD_SHA}\"\nui = \"u\""
                ),
                "not both",
            ),
            ("[apps.a]\nui = \"u\"".to_string(), "exactly one"),
            (
                format!(
                    "[apps.a]\ncomponent_url = \"ftp://x.test/a.wasm\"\n\
                     component_sha256 = \"{GOOD_SHA}\"\nui = \"u\""
                ),
                "http(s)",
            ),
        ] {
            let err = HostConfig::parse(&toml).unwrap_err();
            assert!(format!("{err:#}").contains(needle), "{toml}: {err:#}");
        }

        // Tenant apps get the same validation.
        let err =
            HostConfig::parse("[tenants.alice]\ntoken = \"x\"\n[tenants.alice.apps.a]\nui = \"u\"")
                .unwrap_err();
        assert!(format!("{err:#}").contains("exactly one"), "{err:#}");
    }

    #[test]
    fn sha256_format_is_validated() {
        assert_eq!(validate_sha256(GOOD_SHA).unwrap(), GOOD_SHA);
        assert_eq!(
            validate_sha256(&format!(" {} ", GOOD_SHA.to_ascii_uppercase())).unwrap(),
            GOOD_SHA,
            "trimmed and lowercased"
        );
        for bad in [
            "",
            "abc",
            &GOOD_SHA[..63],
            &format!("{GOOD_SHA}0"),
            "g665a45920422f9d417e4867efdc4fb8a04a1f3fff1fa07e998e86f7f7a27ae3",
        ] {
            assert!(validate_sha256(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[tokio::test]
    async fn env_passthrough_expands_host_vars() {
        let registry = SecretRegistry::default();
        let config = HostConfig::parse(
            r#"
            [apps.app]
            component = "a.wasm"
            ui = "ui"
            [apps.app.env]
            LITERAL = "as-is"
            EXPANDED = "${TANGRAM_TEST_EXPANSION_VAR}"
            "#,
        )
        .unwrap();
        // Safety: test-local var name, nothing else reads it concurrently.
        unsafe { std::env::set_var("TANGRAM_TEST_EXPANSION_VAR", "secret-value") };
        let env = config.apps["app"].resolved_env(&registry, "app").await;
        assert!(env.contains(&("LITERAL".into(), "as-is".into())));
        assert!(env.contains(&("EXPANDED".into(), "secret-value".into())));
    }

    #[tokio::test]
    async fn injected_secret_is_not_in_the_component_env() {
        // ADR-0005: the injected credential lives in the inject rule (applied
        // host-side at egress), NEVER in the component's env. This pins the
        // apps.toml-shaped nutrition spec: even with the key set in the HOST
        // environment, `resolved_env` (what the component receives) does not
        // carry CALORIENINJAS_API_KEY.
        let registry = SecretRegistry::default();
        let config = HostConfig::parse(
            r#"
            [apps.nutrition]
            component = "n.wasm"
            ui = "ui"
            allow_hosts = ["api.calorieninjas.com"]
            [apps.nutrition.env]
            NUTRITION_STRATEGY = "calorieninjas"
            [apps.nutrition.inject]
            "api.calorieninjas.com" = { header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }
            "#,
        )
        .unwrap();
        // Key present in the HOST env (where the resolver reads it).
        unsafe { std::env::set_var("CALORIENINJAS_API_KEY", "host-only-secret") };
        let env = config.apps["nutrition"]
            .resolved_env(&registry, "nutrition")
            .await;
        assert!(
            env.iter().all(|(k, _)| k != "CALORIENINJAS_API_KEY"),
            "the API key must NOT be in the component env: {env:?}"
        );
        // The non-secret strategy selector IS still an env var.
        assert!(env.contains(&("NUTRITION_STRATEGY".into(), "calorieninjas".into())));
        // And the credential is genuinely configured (resolves) for egress.
        assert!(
            config.apps["nutrition"]
                .any_inject_resolves(&registry, "nutrition")
                .await
        );
        unsafe { std::env::remove_var("CALORIENINJAS_API_KEY") };
    }

    #[test]
    fn parses_inject_rules_and_classifies_kinds() {
        let config = HostConfig::parse(
            r#"
            [apps.nutrition]
            component = "n.wasm"
            ui = "ui"
            allow_hosts = ["api.calorieninjas.com", "api.example.com", "q.example.com"]
            [apps.nutrition.inject]
            "api.calorieninjas.com" = { header = "X-Api-Key", secret = "env://CN_KEY" }
            "api.example.com" = { bearer = true, secret = "env://TOK" }
            "q.example.com" = { query = "api_key", secret = "env://QK" }
            "#,
        )
        .unwrap();
        let inject = &config.apps["nutrition"].inject;
        assert_eq!(
            inject["api.calorieninjas.com"].kind().unwrap(),
            InjectKind::Header("X-Api-Key".into())
        );
        assert_eq!(
            inject["api.example.com"].kind().unwrap(),
            InjectKind::Bearer
        );
        assert_eq!(
            inject["q.example.com"].kind().unwrap(),
            InjectKind::Query("api_key".into())
        );
    }

    #[test]
    fn inject_host_must_be_allowlisted() {
        // A rule targeting a host NOT in allow_hosts is a parse error —
        // injection composes with the allowlist (ADR-0005), never bypasses it.
        let err = HostConfig::parse(
            r#"
            [apps.n]
            component = "n.wasm"
            ui = "ui"
            allow_hosts = ["api.example.com"]
            [apps.n.inject]
            "api.calorieninjas.com" = { header = "X-Api-Key", secret = "env://K" }
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not in allow_hosts"), "{err:#}");
    }

    #[test]
    fn inject_requires_exactly_one_kind() {
        for body in [
            // none
            r#""api.example.com" = { secret = "env://K" }"#,
            // two
            r#""api.example.com" = { header = "X", bearer = true, secret = "env://K" }"#,
            // empty secret
            r#""api.example.com" = { header = "X", secret = "  " }"#,
        ] {
            let err = HostConfig::parse(&format!(
                "[apps.n]\ncomponent = \"n.wasm\"\nui = \"ui\"\n\
                 allow_hosts = [\"api.example.com\"]\n[apps.n.inject]\n{body}"
            ))
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("inject rule"),
                "{body}: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn any_inject_resolves_reflects_secret_presence() {
        let registry = SecretRegistry::default();
        let config = HostConfig::parse(
            r#"
            [apps.n]
            component = "n.wasm"
            ui = "ui"
            allow_hosts = ["api.calorieninjas.com"]
            [apps.n.inject]
            "api.calorieninjas.com" = { header = "X-Api-Key", secret = "env://TANGRAM_TEST_INJECT_KEY" }
            "#,
        )
        .unwrap();
        let spec = &config.apps["n"];
        // Unset → does not resolve → not configured (app stays offline).
        unsafe { std::env::remove_var("TANGRAM_TEST_INJECT_KEY") };
        assert!(!spec.any_inject_resolves(&registry, "n").await);
        // Set → resolves → configured.
        unsafe { std::env::set_var("TANGRAM_TEST_INJECT_KEY", "live-key") };
        assert!(spec.any_inject_resolves(&registry, "n").await);
        // The resolved rule is the X-Api-Key header on the lowercased host.
        let resolved = spec.resolved_inject();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "api.calorieninjas.com");
        assert_eq!(resolved[0].1, InjectKind::Header("X-Api-Key".into()));
        unsafe { std::env::remove_var("TANGRAM_TEST_INJECT_KEY") };
    }

    // ── EC2: [[calls]] config parse + validate_calls + the compat shim. ──────
    // The test module name carries `calls_config` so the build plan's filter
    // `cargo test -p tangram-host calls_config` selects exactly this suite.
    mod calls_config {
        use super::*;
        use crate::egress::{MethodMatch, PathPattern};

        // EC7: the migration default modes (§7.2). An app declaring no
        // `[[calls]]` (legacy) defaults to `warn` — never a surprise prod
        // denial; an app declaring >=1 call has signaled intent and defaults to
        // `enforce` — the strict deterministic boundary. An explicit
        // `enforcement` always wins.
        #[test]
        fn migration_default_enforcement_modes() {
            let legacy = HostConfig::parse(
                "[apps.a]\ncomponent = \"a.wasm\"\nui = \"ui\"\nallow_hosts = [\"h.com\"]",
            )
            .unwrap();
            assert_eq!(
                legacy.apps["a"].effective_enforcement(),
                EnforcementMode::Warn,
                "a legacy app (no [[calls]]) defaults to warn — never a surprise prod deny"
            );

            let declared = HostConfig::parse(
                "[apps.a]\ncomponent = \"a.wasm\"\nui = \"ui\"\n\
                 allow_hosts = [\"h.com\"]\n[[apps.a.calls]]\nhost = \"h.com\"",
            )
            .unwrap();
            assert_eq!(
                declared.apps["a"].effective_enforcement(),
                EnforcementMode::Enforce,
                "an app declaring >=1 call signals intent and defaults to enforce"
            );

            let explicit = HostConfig::parse(
                "[apps.a]\ncomponent = \"a.wasm\"\nui = \"ui\"\n\
                 allow_hosts = [\"h.com\"]\nenforcement = \"observe\"\n\
                 [[apps.a.calls]]\nhost = \"h.com\"",
            )
            .unwrap();
            assert_eq!(
                explicit.apps["a"].effective_enforcement(),
                EnforcementMode::Observe,
                "an explicit enforcement always wins over the migration default"
            );
        }

        #[test]
        fn parses_a_calls_block_with_all_constraints() {
            let config = HostConfig::parse(
                r#"
                [apps.mcpproxy]
                component = "m.wasm"
                ui = "ui"
                allow_hosts = ["api.vendor.com"]
                enforcement = "enforce"

                [[apps.mcpproxy.calls]]
                method = "GET"
                host = "api.vendor.com"
                path = "/v1/items/{id}"
                query = { required = ["query"], forbidden = ["callback"] }
                headers = { required = ["content-type"] }
                max_body_bytes = 0

                [[apps.mcpproxy.calls]]
                method = "POST"
                host = "api.vendor.com"
                path = "/rpc"
                body = { json_method = ["tools/list", "tools/call"] }
                inject = { bearer = true, secret = "env://VENDOR_TOKEN" }
                "#,
            )
            .unwrap();
            let spec = &config.apps["mcpproxy"];
            assert_eq!(spec.enforcement, Some(EnforcementMode::Enforce));
            assert_eq!(spec.calls.len(), 2);

            let calls = spec.resolved_calls();
            assert_eq!(calls.len(), 2);
            // First call: GET template, name-level constraints, no body, no
            // inject; max_body_bytes = 0 forbids a body.
            assert_eq!(calls[0].method, MethodMatch::Exact("GET".into()));
            assert_eq!(calls[0].host, "api.vendor.com");
            assert!(matches!(calls[0].path, PathPattern::Template(_)));
            assert_eq!(calls[0].query.required, vec!["query".to_string()]);
            assert_eq!(calls[0].query.forbidden, vec!["callback".to_string()]);
            assert_eq!(calls[0].headers.required, vec!["content-type".to_string()]);
            assert_eq!(calls[0].max_body_bytes, Some(0));
            assert!(calls[0].inject.is_none());
            // Second call: POST /rpc, JSON-RPC body rung (default pointer
            // /method), bearer inject classified.
            assert_eq!(calls[1].method, MethodMatch::Exact("POST".into()));
            assert_eq!(calls[1].path, PathPattern::Exact("/rpc".into()));
            let body = calls[1].body.as_ref().expect("body matcher");
            assert_eq!(body.pointer, "/method");
            assert_eq!(body.allowed, vec!["tools/list", "tools/call"]);
            assert_eq!(calls[1].inject_kind, Some(InjectKind::Bearer));
        }

        #[test]
        fn default_method_and_path_are_maximally_broad() {
            // A call with only a host declared is the broad implicit call.
            let config = HostConfig::parse(
                r#"
                [apps.a]
                component = "a.wasm"
                ui = "ui"
                allow_hosts = ["h.com"]
                [[apps.a.calls]]
                host = "h.com"
                "#,
            )
            .unwrap();
            let calls = config.apps["a"].resolved_calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].method, MethodMatch::Any);
            assert_eq!(calls[0].path, PathPattern::Subtree);
        }

        /// THE behavior-identical-by-default guarantee (§4.2 / §7.1): the live
        /// nutrition host-keyed inject (NO `[[calls]]`) desugars to one
        /// maximally-broad implicit call per allowlisted host, carrying that
        /// host's inject. This is what makes the live fleet byte-identical.
        #[test]
        fn host_keyed_inject_desugars_to_the_broad_implicit_call() {
            let config = HostConfig::parse(
                r#"
                [apps.nutrition]
                component = "n.wasm"
                ui = "ui"
                allow_hosts = ["api.calorieninjas.com"]
                [apps.nutrition.inject]
                "api.calorieninjas.com" = { header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }
                "#,
            )
            .unwrap();
            let calls = config.apps["nutrition"].resolved_calls();
            assert_eq!(calls.len(), 1, "one implicit call per allowlisted host");
            let call = &calls[0];
            assert_eq!(call.method, MethodMatch::Any, "any method (broad)");
            assert_eq!(call.path, PathPattern::Subtree, "any path (broad)");
            assert_eq!(call.host, "api.calorieninjas.com");
            // The host-keyed inject moved ONTO the implicit call.
            assert_eq!(
                call.inject_kind,
                Some(InjectKind::Header("X-Api-Key".into())),
                "the host-keyed credential is attached to the broad call"
            );
            assert_eq!(
                call.inject.as_ref().map(|r| r.secret.as_str()),
                Some("env://CALORIENINJAS_API_KEY")
            );
        }

        /// A bare `allow_hosts` host with no inject and no calls desugars to a
        /// broad, un-credentialed call (allowed, any path).
        #[test]
        fn bare_allow_host_desugars_to_uncredentialed_broad_call() {
            let config = HostConfig::parse(
                r#"
                [apps.a]
                component = "a.wasm"
                ui = "ui"
                allow_hosts = ["public.example.com", "API.Other.com"]
                "#,
            )
            .unwrap();
            let calls = config.apps["a"].resolved_calls();
            assert_eq!(calls.len(), 2);
            assert!(calls.iter().all(|c| c.inject.is_none()));
            // Hosts are canonicalized (lowercased) on the implicit call.
            let hosts: Vec<&str> = calls.iter().map(|c| c.host.as_str()).collect();
            assert!(hosts.contains(&"public.example.com"));
            assert!(hosts.contains(&"api.other.com"));
        }

        #[test]
        fn call_host_must_be_in_allow_hosts() {
            // The host fence composes — a call targeting a host outside
            // allow_hosts is a parse error (never bypasses the allowlist).
            let err = HostConfig::parse(
                r#"
                [apps.a]
                component = "a.wasm"
                ui = "ui"
                allow_hosts = ["api.allowed.com"]
                [[apps.a.calls]]
                method = "GET"
                host = "api.evil.com"
                path = "/x"
                "#,
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("not in allow_hosts"), "{err:#}");
        }

        #[test]
        fn call_inject_must_name_exactly_one_kind() {
            let err = HostConfig::parse(
                r#"
                [apps.a]
                component = "a.wasm"
                ui = "ui"
                allow_hosts = ["h.com"]
                [[apps.a.calls]]
                host = "h.com"
                inject = { header = "X", bearer = true, secret = "env://K" }
                "#,
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("inject rule"), "{err:#}");
        }

        #[test]
        fn empty_json_method_set_is_rejected() {
            // A body matcher with an empty literal set never matches — reject
            // it as a config error rather than silently denying everything.
            let err = HostConfig::parse(
                r#"
                [apps.a]
                component = "a.wasm"
                ui = "ui"
                allow_hosts = ["h.com"]
                [[apps.a.calls]]
                method = "POST"
                host = "h.com"
                path = "/rpc"
                body = { json_method = [] }
                "#,
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("json_method"), "{err:#}");
        }

        #[test]
        fn bad_path_template_is_rejected() {
            let err = HostConfig::parse(
                r#"
                [apps.a]
                component = "a.wasm"
                ui = "ui"
                allow_hosts = ["h.com"]
                [[apps.a.calls]]
                host = "h.com"
                path = "/v1/**/x"
                "#,
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("**"), "{err:#}");
        }

        // ── EC4: the JSON-RPC-method body rung (§9.1) end-to-end. The test
        //    module name carries `jsonrpc_method_match` so the build plan's
        //    filter `cargo test -p tangram-host jsonrpc_method_match` selects it.
        mod jsonrpc_method_match {
            use super::*;
            use crate::egress::CanonicalRequest;

            fn one_call(toml_body: &str) -> crate::egress::CallSpec {
                let config = HostConfig::parse(&format!(
                    r#"
                    [apps.mcpproxy]
                    component = "m.wasm"
                    ui = "ui"
                    allow_hosts = ["api.vendor.com"]
                    enforcement = "enforce"
                    [[apps.mcpproxy.calls]]
                    method = "POST"
                    host   = "api.vendor.com"
                    path   = "/rpc"
                    {toml_body}
                    "#
                ))
                .unwrap();
                let calls = config.apps["mcpproxy"].resolved_calls();
                assert_eq!(calls.len(), 1);
                calls.into_iter().next().unwrap()
            }

            fn rpc_req() -> CanonicalRequest {
                let url = reqwest::Url::parse("https://api.vendor.com/rpc").unwrap();
                CanonicalRequest::from_request("POST", &url, ["content-type"]).unwrap()
            }

            #[test]
            fn json_method_membership_allows_only_declared_methods() {
                let call = one_call(r#"body = { json_method = ["tools/list", "tools/call"] }"#);
                let req = rpc_req();
                // Declared methods match.
                assert!(call.matches(&req, br#"{"method":"tools/list","id":1}"#));
                assert!(call.matches(&req, br#"{"method":"tools/call","params":{}}"#));
                // An undeclared JSON-RPC method does NOT match (→ denied).
                assert!(!call.matches(&req, br#"{"method":"resources/read"}"#));
                assert!(!call.matches(&req, br#"{"method":"tools/delete"}"#));
            }

            #[test]
            fn body_is_parsed_only_when_a_matcher_is_declared() {
                // No body matcher: the call matches regardless of body content
                // (the body is NOT parsed). Even invalid JSON is fine.
                let call = one_call("");
                let req = rpc_req();
                assert!(call.matches(&req, b"this is not json at all"));
                assert!(call.matches(&req, br#"{"method":"anything"}"#));
            }

            #[test]
            fn adversarial_bodies_are_denied_not_panicked() {
                let call = one_call(r#"body = { json_method = ["tools/list"] }"#);
                let req = rpc_req();
                // non-JSON body → no match (deny), never a parse panic.
                assert!(!call.matches(&req, b"\xff\xfe not json"));
                // missing /method pointer → no match.
                assert!(!call.matches(&req, br#"{"other":"x"}"#));
                // /method present but wrong type (not a string) → no match.
                assert!(!call.matches(&req, br#"{"method":123}"#));
            }

            #[test]
            fn oversized_body_is_rejected_before_parse() {
                // max_body_bytes caps the body the matcher will parse: a body
                // over the cap is rejected (no match) BEFORE any JSON parse,
                // even though it IS valid JSON with the declared method.
                let call = one_call(
                    r#"max_body_bytes = 16
                       body = { json_method = ["tools/list"] }"#,
                );
                let req = rpc_req();
                let small = br#"{"method":"tools/list"}"#; // 23 bytes > 16
                assert!(
                    !call.matches(&req, small),
                    "a body over max_body_bytes must be rejected before parse"
                );
                // A body within the cap with the declared method matches.
                let tiny = one_call(
                    r#"max_body_bytes = 64
                       body = { json_method = ["tools/list"] }"#,
                );
                assert!(tiny.matches(&req, br#"{"method":"tools/list"}"#));
            }

            #[test]
            fn explicit_pointer_overrides_the_default_method_pointer() {
                // The default pointer is /method; an explicit pointer selects a
                // different field (still pointer + literal-set only — no
                // operators, no value-matching on arbitrary fields).
                let call = one_call(r#"body = { json_method = ["v2"], pointer = "/jsonrpc" }"#);
                let req = rpc_req();
                assert!(call.matches(&req, br#"{"jsonrpc":"v2","method":"x"}"#));
                assert!(!call.matches(&req, br#"{"jsonrpc":"v1","method":"x"}"#));
            }
        }

        // ── §9.2 / ADR-0009: the OPT-IN egress policy engine config. The test
        //    module name carries `policy_config` so the filter
        //    `cargo test -p tangram-host policy_config` selects exactly this. ──
        mod policy_config {
            use super::*;
            use crate::egress::CanonicalRequest;
            use crate::policy::{Condition, PolicyVerdict};

            fn req(method: &str, url: &str) -> CanonicalRequest {
                let parsed = reqwest::Url::parse(url).unwrap();
                CanonicalRequest::from_request(method, &parsed, std::iter::empty()).unwrap()
            }

            #[test]
            fn parses_and_lowers_a_policy_block() {
                let config = HostConfig::parse(
                    r#"
                    [apps.mcpproxy]
                    component = "m.wasm"
                    ui = "ui"
                    allow_hosts = ["api.vendor.com"]

                    [apps.mcpproxy.policy]
                    default = "deny"
                    rules = [
                      { effect = "allow", method = ["POST"], host = "api.vendor.com", path_prefix = ["rpc"], json_method = ["tools/list", "tools/call"] },
                      { effect = "deny" },
                    ]
                    "#,
                )
                .unwrap();
                let spec = &config.apps["mcpproxy"];
                assert!(spec.uses_policy(), "the policy marker is surfaced");
                let policy = spec.resolved_policy().unwrap().expect("a policy");
                assert_eq!(policy.rule_count(), 2);

                // The lowered policy allows the declared JSON-RPC methods on the
                // rpc subtree and denies everything else (default deny).
                let r = req("POST", "https://api.vendor.com/rpc");
                assert_eq!(
                    policy.evaluate(&r, br#"{"method":"tools/list"}"#),
                    PolicyVerdict::Allow
                );
                assert!(matches!(
                    policy.evaluate(&r, br#"{"method":"resources/read"}"#),
                    PolicyVerdict::Deny(_)
                ));
                // A GET (not POST) misses the allow rule → default deny.
                assert!(matches!(
                    policy.evaluate(&req("GET", "https://api.vendor.com/rpc"), b""),
                    PolicyVerdict::Deny(_)
                ));
            }

            #[test]
            fn default_effect_defaults_to_deny() {
                // Omitting `default` fails CLOSED (deny) — the conservative §9.2
                // choice.
                let config = HostConfig::parse(
                    r#"
                    [apps.a]
                    component = "a.wasm"
                    ui = "ui"
                    allow_hosts = ["h.com"]
                    [apps.a.policy]
                    rules = [ { effect = "allow", host = "h.com", path = "/ok" } ]
                    "#,
                )
                .unwrap();
                let policy = config.apps["a"].resolved_policy().unwrap().unwrap();
                // Declared path allowed; anything else default-denied.
                assert_eq!(
                    policy.evaluate(&req("GET", "https://h.com/ok"), b""),
                    PolicyVerdict::Allow
                );
                assert!(matches!(
                    policy.evaluate(&req("GET", "https://h.com/elsewhere"), b""),
                    PolicyVerdict::Deny(_)
                ));
            }

            #[test]
            fn path_and_path_prefix_are_mutually_exclusive() {
                let err = HostConfig::parse(
                    r#"
                    [apps.a]
                    component = "a.wasm"
                    ui = "ui"
                    allow_hosts = ["h.com"]
                    [apps.a.policy]
                    rules = [ { effect = "allow", path = "/x", path_prefix = ["x"] } ]
                    "#,
                )
                .unwrap_err();
                assert!(format!("{err:#}").contains("at most one"), "{err:#}");
            }

            #[test]
            fn policy_rule_host_must_be_in_allow_hosts() {
                // The fence composes: a rule naming a host outside allow_hosts is
                // a parse error (the policy can only narrow, never grant reach).
                let err = HostConfig::parse(
                    r#"
                    [apps.a]
                    component = "a.wasm"
                    ui = "ui"
                    allow_hosts = ["api.allowed.com"]
                    [apps.a.policy]
                    rules = [ { effect = "allow", host = "api.evil.com" } ]
                    "#,
                )
                .unwrap_err();
                assert!(format!("{err:#}").contains("not in allow_hosts"), "{err:#}");
            }

            #[test]
            fn over_budget_policy_fails_closed_at_parse() {
                // More than MAX_RULES rules is a parse error (the latency budget
                // is enforced at parse — fail closed).
                let rules: String = (0..crate::policy::MAX_RULES + 1)
                    .map(|_| r#"{ effect = "deny" },"#.to_string())
                    .collect();
                let err = HostConfig::parse(&format!(
                    r#"
                    [apps.a]
                    component = "a.wasm"
                    ui = "ui"
                    allow_hosts = ["h.com"]
                    [apps.a.policy]
                    rules = [ {rules} ]
                    "#
                ))
                .unwrap_err();
                assert!(format!("{err:#}").contains("over the budget"), "{err:#}");
            }

            #[test]
            fn empty_rules_is_rejected() {
                let err = HostConfig::parse(
                    r#"
                    [apps.a]
                    component = "a.wasm"
                    ui = "ui"
                    allow_hosts = ["h.com"]
                    [apps.a.policy]
                    rules = []
                    "#,
                )
                .unwrap_err();
                assert!(format!("{err:#}").contains("no rules"), "{err:#}");
            }

            #[test]
            fn no_policy_is_the_default_and_behavior_identical() {
                // The section-1 guarantee: an app with no [policy] block has no
                // policy gate at all (None) and is unaffected.
                let config = HostConfig::parse(
                    "[apps.a]\ncomponent = \"a.wasm\"\nui = \"ui\"\nallow_hosts = [\"h.com\"]",
                )
                .unwrap();
                assert!(!config.apps["a"].uses_policy());
                assert!(config.apps["a"].resolved_policy().unwrap().is_none());
            }

            #[test]
            fn declared_prefix_is_canonicalized_through_the_same_seam() {
                // A declared prefix carrying `.`/`%2e` lowers through the SAME
                // path seam, so the policy can't disagree with the matcher.
                let config = HostConfig::parse(
                    r#"
                    [apps.a]
                    component = "a.wasm"
                    ui = "ui"
                    allow_hosts = ["h.com"]
                    [apps.a.policy]
                    rules = [ { effect = "deny", path_prefix = ["v1", ".", "accounts"] }, { effect = "allow" } ]
                    "#,
                )
                .unwrap();
                let policy = config.apps["a"].resolved_policy().unwrap().unwrap();
                // The `.` segment was normalized away → prefix is ["v1","accounts"].
                assert!(matches!(
                    policy.evaluate(&req("GET", "https://h.com/v1/accounts/9"), b""),
                    PolicyVerdict::Deny(_)
                ));
                assert_eq!(
                    policy.evaluate(&req("GET", "https://h.com/v1/me"), b""),
                    PolicyVerdict::Allow
                );
            }

            #[test]
            fn rule_fields_lower_to_the_expected_conditions() {
                // A direct check that each TOML field becomes the right Condition
                // (so a future field rename can't silently drop a constraint).
                let rule = PolicyRuleToml {
                    effect: PolicyEffectToml::Allow,
                    method: vec!["get".into()],
                    host: Some("H.com".into()),
                    path: None,
                    path_prefix: vec!["v1".into()],
                    query_present: vec!["q".into()],
                    header_present: vec!["X-Trace".into()],
                    json_method: vec!["tools/list".into()],
                    pointer: "/method".into(),
                }
                .to_rule()
                .unwrap();
                assert!(
                    rule.conditions
                        .contains(&Condition::MethodIn(vec!["GET".into()])),
                    "method upper-cased"
                );
                assert!(
                    rule.conditions.contains(&Condition::HostEq("h.com".into())),
                    "host canonicalized"
                );
                assert!(
                    rule.conditions
                        .contains(&Condition::HeaderNamePresent("x-trace".into())),
                    "header name lowercased"
                );
            }
        }

        #[test]
        fn tenant_ceiling_drops_out_of_ceiling_calls() {
            use crate::registry::Source;
            use std::path::Path;
            let config = HostConfig::parse(
                r#"
                [apps.a]
                component = "a.wasm"
                ui = "ui"
                allow_hosts = ["api.in.com", "api.out.com"]
                [[apps.a.calls]]
                method = "GET"
                host = "api.in.com"
                path = "/x"
                [[apps.a.calls]]
                method = "GET"
                host = "api.out.com"
                path = "/y"
                "#,
            )
            .unwrap();
            let spec = &config.apps["a"];
            let ceiling = vec!["api.in.com".to_string()];
            let eff = crate::tenant::effective_spec(
                "t",
                "a",
                spec,
                Source::File,
                Path::new("/r"),
                Some(&ceiling),
            )
            .unwrap();
            // Both allow_hosts and calls are intersected with the ceiling.
            assert_eq!(eff.allow_hosts, vec!["api.in.com".to_string()]);
            assert_eq!(eff.calls.len(), 1);
            assert_eq!(eff.calls[0].host, "api.in.com");
            // And resolved_calls on the effective spec carries only the kept call.
            let resolved = eff.resolved_calls();
            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].host, "api.in.com");
        }
    }
}
