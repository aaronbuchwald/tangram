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
    pub async fn any_inject_resolves(&self, registry: &SecretRegistry, app: &str) -> bool {
        for (host, _kind, rule) in self.resolved_inject() {
            if rule
                .resolve_secret(registry, &format!("{app}: inject {host}"))
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
}
