//! tangram-host — run Tangram apps as WASM components (RUNTIME_PLAN Phase 2
//! + 3, ADR-0001 Track W).
//!
//! One native binary owns the platform: HTTP serving, the sync protocol,
//! MCP, persistence, and static UI files. Each app is a `wasm32-wasip2`
//! component holding ONLY app logic, kept alive as one instance and called
//! per action; its capabilities are exactly what the host implements for it
//! (outbound HTTP behind a per-app allowlist, log, wall clock) plus the env
//! vars granted in `apps.toml`. The host is also the only thing touching
//! `$HOME/.<app-name>` — the component cannot name a file at all.
//!
//! Desired state has two layers (Phase 3): `apps.toml` (bootstrap, watched
//! for edits) plus the replicated document of any app flagged
//! `registry = true` — install/remove/enable through the registry's actions
//! or MCP tools and the host converges exactly like a file change, with
//! registry entries winning name collisions. `TANGRAM_AUTH_TOKEN` gates the
//! mutating routes of registry (and `require_auth`) apps; without a token
//! the host refuses to run a registry app on a non-loopback bind.
//!
//! Usage: `tangram-host [apps.toml]` (or `APPS_TOML=...`); `BIND_ADDR`
//! defaults to 127.0.0.1:8080. The config file is watched: edits converge
//! live — apps appear, disappear, and reload (also when a component file is
//! rebuilt) without restarting the host.

mod accounts;
mod app;
mod audit;
mod auth;
mod authapi;
mod config;
mod doc;
mod egress;
mod fetch;
mod gateway;
mod mcp;
mod multitenant;
mod oauth;
mod policy;
mod registry;
mod routes;
mod runtime;
mod scheduler;
mod secrets;
mod tenant;
mod verify;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::IntoFuture;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use notify::Watcher as _;
use tangram::sync::DocHandle as _;
use tokio::sync::{Notify, RwLock};
use tracing_subscriber::EnvFilter;

use crate::app::{AppRuntime, component_mtime};
use crate::config::{AppSpec, HostConfig};
use crate::registry::{Desired, Source};
use crate::routes::AppEntry;
use crate::tenant::AppKey;

/// Fleet status of one desired app, refreshed by every converge pass and
/// served (with live running/healthy probes) at `GET /api/fleet`
/// (top-level apps) and `GET /t/<tenant>/api/fleet` (that tenant's apps,
/// bearer-gated).
#[derive(Debug, Clone)]
pub struct FleetStatus {
    pub source: Source,
    pub registry: bool,
    pub require_auth: bool,
    pub enabled: bool,
    /// The EFFECTIVE outbound grant (after the tenant ceiling intersection,
    /// for tenant apps). Reported in the tenant fleet so a tenant can see
    /// what their install actually got.
    pub allow_hosts: Vec<String>,
    /// Why the last converge could not (re)start this app, if it failed.
    pub error: Option<String>,
}

pub struct Host {
    engine: wasmtime::Engine,
    config_path: PathBuf,
    pub apps: RwLock<HashMap<AppKey, AppEntry>>,
    pub fleet: RwLock<BTreeMap<AppKey, FleetStatus>>,
    /// `TANGRAM_AUTH_TOKEN`: gates mutating routes on registry/require_auth
    /// apps. `None` = unauthenticated host (loopback-only for registries).
    pub auth_token: Option<String>,
    /// The deployment auth mode (docs/design/auth.md). `MultiTenant` activates
    /// the host-local credential store + scope-checked principal gating on the
    /// top-level surface; `SelfHosted` (the default) keeps byte-identical
    /// loopback-trusted behavior.
    pub auth_mode: config::AuthMode,
    /// Multi-tenant: gate reads behind `registry:read` too (auth.md §11.2).
    pub reads_gated: bool,
    /// The host-local account / credential store (auth.md §4) — `Some` only in
    /// multi-tenant mode. Hashed PATs + sessions; consulted per request for
    /// principal resolution. NEVER replicated (a replicated credential is a
    /// leaked credential).
    pub accounts: Option<Arc<accounts::AccountStore>>,
    /// The resolved OAuth/OIDC client config (auth.md §7 C6) — `Some` only in
    /// multi-tenant mode WITH a complete client id + secret. `None` is PAT-only
    /// bootstrap (still fully functional). A partial config fails startup, so a
    /// half-configured OAuth never silently runs.
    pub oauth: Option<oauth::OauthConfig>,
    /// The host-wide per-principal mutation rate limiter (auth.md §12, C7).
    /// Shared into every multi-tenant app gate so a principal counts against ONE
    /// budget across all apps. `0`/disabled in self-hosted unless opted in.
    pub rate_limiter: Arc<multitenant::RateLimiter>,
    /// Tenant → resolved bearer token (Phase 5), refreshed on every converge
    /// — the lookup table behind [`auth::resolve_principal`]. A tenant whose
    /// `${VAR}` token didn't resolve is absent: every request 401s.
    pub tenant_tokens: RwLock<BTreeMap<String, String>>,
    /// Whether `BIND_ADDR` is a loopback address — a registry app without a
    /// token refuses to run on a non-loopback bind.
    pub bind_loopback: bool,
    /// Nudges the converge loop (registry document changes use this channel;
    /// file edits arrive via the notify watcher).
    pub nudge: Arc<Notify>,
    /// The MCP gateway (agentgateway child), when `[gateway]` is enabled and
    /// the binary was found. `None` = direct per-app /mcp serving.
    pub gateway: Option<Arc<gateway::Gateway>>,
    /// Downloads + verifies `component_url` artifacts into the immutable
    /// content-addressed cache (Phase 8 install-from-URL).
    pub fetcher: fetch::Fetcher,
    /// The secret-resolution seam (ADR-0004 Phase 10a): scheme → resolver.
    /// Phase 10a registers exactly one — `env://` — and resolves every spec
    /// secret reference (`${VAR}` sugar included) through it at converge.
    /// `Arc` so it can be shared into each component's `HostState` for
    /// request-time egress injection (ADR-0005 / Phase 10b), where the value
    /// is resolved host-side and never enters the component.
    pub secrets: Arc<secrets::SecretRegistry>,
    /// `[artifacts] upload_enabled` (Phase S2b): when true the host hosts
    /// `POST /artifacts` (store an uploaded component, computing its sha) and
    /// `GET /artifacts/<sha>.wasm`. DEFAULT OFF — when false both routes 404.
    /// Read once at startup (not converged live, like `[gateway]`), behind
    /// the loopback/auth gate in `main`.
    pub artifacts_upload_enabled: bool,
}

impl Host {
    /// The shared Wasmtime engine — used to validate an UPLOADED component
    /// before it enters the content-addressed store (`POST /artifacts`).
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }
}

/// The bootstrap ("file") layer of one tenant's desired state: the explicit
/// `[tenants.<t>.apps.*]` template, or — when empty — a single registry
/// instance cloned from the file's own registry app spec (component + ui
/// only; no grants, no env), so every tenant starts with a control plane.
fn tenant_bootstrap(
    config: &HostConfig,
    tenant_spec: &config::TenantSpec,
) -> BTreeMap<String, AppSpec> {
    if !tenant_spec.apps.is_empty() {
        return tenant_spec.apps.clone();
    }
    let Some(template) = config
        .apps
        .values()
        .find(|spec| spec.registry && spec.enabled)
    else {
        // HostConfig::parse refuses this shape; be safe if it ever changes.
        return BTreeMap::new();
    };
    let registry = AppSpec {
        component: template.component.clone(),
        component_url: template.component_url.clone(),
        component_sha256: template.component_sha256.clone(),
        ui: template.ui.clone(),
        data_dir: None,
        allow_hosts: Vec::new(),
        env: BTreeMap::new(),
        inject: BTreeMap::new(),
        calls: Vec::new(),
        enforcement: None,
        policy: None,
        declared: None,
        remote: None,
        remote_token: None,
        registry: true,
        require_auth: false,
        enabled: true,
    };
    [("registry".to_string(), registry)].into_iter().collect()
}

impl Host {
    /// One reconciliation pass, in two stages: (1) converge the registry
    /// apps named in `apps.toml` — top-level AND each tenant's bootstrap
    /// registries, (2) read their replicated spec lists, merge them over
    /// their scope's file layer (registry wins name collisions; a tenant's
    /// registry only ever drives that tenant), apply tenant policy
    /// (data-dir confinement, the allow_hosts ceiling, max_apps), and
    /// converge the full set — build new/changed apps (spec changed, or the
    /// component file was rebuilt), drop removed/disabled/policy-blocked
    /// ones. A failing app is logged, reported in the fleet status, and
    /// skipped; a failing RELOAD keeps the old instance serving.
    async fn converge(&self) {
        let config = match HostConfig::load(&self.config_path) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!("not converging: {e:#}");
                return;
            }
        };

        // Refresh the tenant token table — the auth lookup behind every
        // `/t/<tenant>/` request. A tenant whose token didn't resolve stays
        // out of the table (uniform 401) and its apps don't run.
        let data_root = config.tenants.resolved_data_root();
        let mut tokens: BTreeMap<String, String> = BTreeMap::new();
        for (name, spec) in &config.tenants.tenants {
            match spec.resolved_token(&self.secrets, name).await {
                Some(token) => {
                    tokens.insert(name.clone(), token);
                }
                None => tracing::error!(
                    "tenant {name}: token did not resolve — /t/{name}/ answers 401 and its \
                     apps will not run (set the env var referenced by [tenants.{name}].token)"
                ),
            }
        }
        *self.tenant_tokens.write().await = tokens.clone();

        // Tenant→PAT convergence (auth.md §7, C7): in multi-tenant mode each
        // per-tenant static token becomes a seeded per-tenant PAT in the
        // host-local store, so the host & tenant credential models converge on
        // ONE account model. Idempotent — a no-op once seeded.
        if let Some(store) = &self.accounts {
            multitenant::converge_tenant_pats(store, &tokens);
        }

        let mut apps = self.apps.write().await;
        let mut errors: BTreeMap<AppKey, String> = BTreeMap::new();
        // Apps a POLICY blocked (cap, data-dir escape, unresolved token):
        // unlike start failures (where a failing reload keeps the old
        // instance serving), these must not run at all.
        let mut blocked: BTreeSet<AppKey> = BTreeSet::new();

        // Each tenant's bootstrap layer (template or default registry clone).
        let tenant_file: BTreeMap<String, BTreeMap<String, AppSpec>> = config
            .tenants
            .tenants
            .iter()
            .map(|(name, spec)| (name.clone(), tenant_bootstrap(&config, spec)))
            .collect();

        // Stage 1: registry apps — top-level ones from the file plus each
        // tenant's bootstrap registries — so their documents can be read
        // below. (They are re-checked as part of stage 2's full set —
        // `ensure_app` is idempotent for an up-to-date app.)
        for (name, spec) in config.apps.iter().filter(|(_, s)| s.registry && s.enabled) {
            let key = AppKey::top(name);
            let gate = self.auth_token.as_deref();
            // A registry app is file-controlled — its own component path is
            // local-authoritative, never a federated entry.
            if let Err(e) = self.ensure_app(&mut apps, &key, spec, gate, false).await {
                errors.insert(key, e);
            }
        }
        for (tname, layer) in &tenant_file {
            let Some(token) = tokens.get(tname) else {
                continue; // unresolved token: errors recorded in stage 2
            };
            let tenant_spec = &config.tenants.tenants[tname];
            let tenant_root = data_root.join(tname);
            // Respect max_apps even here: never start a registry the cap
            // will evict in stage 2.
            let bootstrap_desired: BTreeMap<String, Desired> = layer
                .iter()
                .map(|(n, s)| {
                    (
                        n.clone(),
                        Desired {
                            spec: s.clone(),
                            source: Source::File,
                            federated: false,
                        },
                    )
                })
                .collect();
            let over_cap: BTreeSet<String> =
                tenant::enforce_max_apps(&bootstrap_desired, tenant_spec.max_apps, &[])
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect();
            for (aname, spec) in layer
                .iter()
                .filter(|(n, s)| s.registry && s.enabled && !over_cap.contains(*n))
            {
                let key = AppKey::tenant(tname, aname);
                match tenant::effective_spec(
                    tname,
                    aname,
                    spec,
                    Source::File,
                    &tenant_root,
                    tenant_spec.allow_hosts_ceiling.as_deref(),
                ) {
                    Ok(effective) => {
                        if let Err(e) = self
                            .ensure_app(&mut apps, &key, &effective, Some(token), false)
                            .await
                        {
                            errors.insert(key, e);
                        }
                    }
                    Err(e) => {
                        errors.insert(key.clone(), e);
                        blocked.insert(key);
                    }
                }
            }
        }

        // The registry layer of desired state: every running registry app's
        // replicated spec list, scoped to where the registry lives — a
        // tenant's registry drives THAT tenant's desired state only, so app
        // names colliding across tenants are non-events. A registry whose
        // `remote` is set is FEDERATED: its document syncs with a peer, so
        // (Phase 9) every entry it carries is tagged `federated`, a
        // local-`component` PATH entry is flagged non-portable, and each
        // installed app gets a derived `<base>/<app>/sync` remote so its
        // OWN document replicates with the same peer (one `remote` syncs both
        // desired state and data).
        let mut top_entries: Vec<registry::RegistryDesired> = Vec::new();
        let mut tenant_entries: BTreeMap<String, Vec<registry::RegistryDesired>> = BTreeMap::new();
        // (key, federation coordinates) for every registry app, top-level and
        // per tenant. `Some` ⇒ federated; the sync base + token derive the
        // installed apps' per-document remotes.
        let federation_of = |spec: &AppSpec| -> Option<registry::Federation> {
            spec.remote.as_deref().map(|remote| registry::Federation {
                base: registry::sync_base(remote),
                token: spec.remote_token.clone(),
            })
        };
        let registry_keys: Vec<(AppKey, Option<registry::Federation>)> = config
            .apps
            .iter()
            .filter(|(_, s)| s.registry)
            .map(|(n, s)| (AppKey::top(n), federation_of(s)))
            .chain(tenant_file.iter().flat_map(|(tname, layer)| {
                layer
                    .iter()
                    .filter(|(_, s)| s.registry)
                    .map(move |(n, s)| (AppKey::tenant(tname, n), federation_of(s)))
            }))
            .collect();
        for (key, federation) in registry_keys {
            if let Some(entry) = apps.get(&key) {
                // state_json is verbatim component output (a String since the
                // float-rendering fix); the registry layer needs the parsed
                // tree to walk specs — exact now that float_roundtrip is on.
                match serde_json::from_str(&entry.runtime.state_json().await) {
                    Ok(state) => {
                        let parsed =
                            registry::parse_state(&key.to_string(), &state, federation.as_ref());
                        match &key.tenant {
                            Some(tname) => tenant_entries
                                .entry(tname.clone())
                                .or_default()
                                .extend(parsed),
                            None => top_entries.extend(parsed),
                        }
                    }
                    Err(e) => tracing::warn!("{key}: unparseable registry state: {e}"),
                }
            }
        }

        // Stage 2: merge per scope, apply tenant policy (data confinement,
        // ceiling intersection, max_apps), and converge the full set.
        let mut desired: BTreeMap<AppKey, Desired> = registry::merge(&config.apps, top_entries)
            .into_iter()
            .map(|(name, want)| (AppKey::top(name), want))
            .collect();
        for (tname, layer) in &tenant_file {
            let entries = tenant_entries.remove(tname).unwrap_or_default();
            // The registry doc's list order is install order — max_apps
            // evicts the NEWEST excess install, never an earlier one.
            let install_order: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
            let merged = registry::merge(layer, entries);
            let tenant_spec = &config.tenants.tenants[tname];
            let tenant_root = data_root.join(tname);
            for (name, message) in
                tenant::enforce_max_apps(&merged, tenant_spec.max_apps, &install_order)
            {
                let key = AppKey::tenant(tname, name);
                errors.insert(key.clone(), message);
                blocked.insert(key);
            }
            for (aname, mut want) in merged {
                let key = AppKey::tenant(tname, &aname);
                if !tokens.contains_key(tname) {
                    errors
                        .entry(key.clone())
                        .or_insert_with(|| "tenant token unresolved — app not run".to_string());
                    blocked.insert(key.clone());
                }
                match tenant::effective_spec(
                    tname,
                    &aname,
                    &want.spec,
                    want.source,
                    &tenant_root,
                    tenant_spec.allow_hosts_ceiling.as_deref(),
                ) {
                    Ok(effective) => want.spec = effective,
                    Err(e) => {
                        errors.insert(key.clone(), e);
                        blocked.insert(key.clone());
                    }
                }
                desired.insert(key, want);
            }
        }

        for (key, want) in &desired {
            if !want.spec.enabled || blocked.contains(key) {
                continue;
            }
            let gate = match &key.tenant {
                None => self.auth_token.as_deref(),
                Some(tname) => tokens.get(tname).map(String::as_str),
            };
            if let Err(e) = self
                .ensure_app(&mut apps, key, &want.spec, gate, want.federated)
                .await
            {
                errors.insert(key.clone(), e);
            }
        }
        apps.retain(|key, _| {
            let keep =
                desired.get(key).is_some_and(|want| want.spec.enabled) && !blocked.contains(key);
            if !keep {
                tracing::info!(
                    "{key}: removed (routes for {}/ are gone)",
                    key.route_prefix()
                );
            }
            keep
        });

        // The MCP gateway's config is generated from the same converged
        // state: regenerate (atomically; agentgateway hot-reloads the file)
        // whenever the running set changed.
        if let Some(gateway) = &self.gateway {
            let running: Vec<AppKey> = apps.keys().cloned().collect();
            gateway.sync_apps(&running);
        }

        *self.fleet.write().await = desired
            .into_iter()
            .map(|(key, want)| {
                let error = errors.remove(&key);
                let status = FleetStatus {
                    source: want.source,
                    registry: want.spec.registry,
                    require_auth: want.spec.require_auth,
                    enabled: want.spec.enabled,
                    allow_hosts: want.spec.allow_hosts.clone(),
                    error,
                };
                (key, status)
            })
            .collect();
    }

    /// Converge one app toward its spec: no-op when up to date, otherwise
    /// (re)instantiate. `gate_token` is the bearer gating the app's mutating
    /// routes when it is a registry/require_auth app — the host token for
    /// top-level apps, the tenant's token for tenant apps (whose WHOLE
    /// surface is additionally gated at dispatch). Returns the failure
    /// message for the fleet status.
    async fn ensure_app(
        &self,
        apps: &mut HashMap<AppKey, AppEntry>,
        key: &AppKey,
        spec: &AppSpec,
        gate_token: Option<&str>,
        federated: bool,
    ) -> Result<(), String> {
        if spec.registry && key.tenant.is_none() && self.auth_token.is_none() && !self.bind_loopback
        {
            let msg = "refusing to run a registry app on a non-loopback bind without \
                       TANGRAM_AUTH_TOKEN — self-hosted mode is loopback-trusted (set the \
                       token or bind 127.0.0.1)"
                .to_string();
            tracing::error!("{key}: {msg}");
            apps.remove(key);
            return Err(msg);
        }
        // Resolve the component to a local file first: a spec path as-is, a
        // `component_url` through the verified content-addressed cache (a
        // cache stat in steady state; the download + sha-256 gate runs only
        // on a miss). A fetch failure or hash mismatch is this app's
        // converge error — a running instance keeps serving, like a failed
        // reload.
        let component_path = match spec.component_source().map_err(|e| e.to_string())? {
            config::ComponentSource::Path(path) => {
                // Portability gate (Phase 9): a federated registry's entries
                // are seen by every peer, so a local component PATH is
                // host-local. A peer that lacks the path reports a clear
                // fleet error (artifact missing) — NOT a bare "file not
                // found" — and never mutates the shared document; it keeps
                // converging everything else. The host that wrote the entry
                // (where the path exists) runs it normally.
                if federated && !path.exists() {
                    let msg = format!(
                        "federated registry entry points at a local component path that does \
                         not exist on this host ({}) — local paths are not portable across a \
                         federated fleet; reinstall with component_url + component_sha256",
                        path.display()
                    );
                    tracing::error!("{key}: {msg}");
                    if apps.contains_key(key) {
                        return Err(format!("{msg} (old instance still serving)"));
                    }
                    return Err(msg);
                }
                path
            }
            config::ComponentSource::Url { url, sha256 } => self
                .fetcher
                .resolve(&key.to_string(), &url, &sha256)
                .await
                .map_err(|e| {
                    if apps.contains_key(key) {
                        format!("{e} (old instance still serving)")
                    } else {
                        e
                    }
                })?,
        };
        let up_to_date = apps.get(key).is_some_and(|entry| {
            entry.runtime.spec == *spec
                && entry.runtime.component_mtime == component_mtime(&component_path)
        });
        if up_to_date {
            return Ok(());
        }
        let started = std::time::Instant::now();
        match AppRuntime::build(&self.engine, &self.secrets, &key.app, spec, &component_path).await
        {
            Ok(runtime) => {
                // Multi-tenant mode gates TOP-LEVEL registry/require_auth apps
                // through the host-local credential store (scope-checked
                // principal resolution). Tenant apps keep the single-token
                // bearer gate (their `/t/<tenant>/` dispatch already gates the
                // whole surface). Self-hosted is unchanged.
                let multitenant = self.auth_mode == config::AuthMode::MultiTenant
                    && key.tenant.is_none()
                    && (spec.registry || spec.require_auth);
                let gate = match (multitenant, &self.accounts) {
                    (true, Some(store)) => routes::GatePolicy::MultiTenant {
                        store: store.clone(),
                        reads_gated: self.reads_gated,
                        limiter: self.rate_limiter.clone(),
                    },
                    _ => {
                        let token = gate_token
                            .filter(|_| spec.registry || spec.require_auth)
                            .map(str::to_string);
                        routes::GatePolicy::SingleToken(token)
                    }
                };
                let mut entry = AppEntry::new(runtime, gate);
                if spec.registry {
                    // Registry doc changes (actions, MCP calls, sync from a
                    // replica) re-trigger converge, just like a file edit.
                    let mut rx = entry.runtime.doc.subscribe();
                    let nudge = self.nudge.clone();
                    entry.watch_task = Some(tokio::spawn(async move {
                        while rx.changed().await.is_ok() {
                            nudge.notify_one();
                        }
                    }));
                }
                let verb = if apps.contains_key(key) {
                    "reloaded"
                } else {
                    "added"
                };
                tracing::info!(
                    "{key}: {verb} (serving {}/ after {:?})",
                    key.route_prefix(),
                    started.elapsed()
                );
                apps.insert(key.clone(), entry);
                Ok(())
            }
            Err(e) if apps.contains_key(key) => {
                tracing::error!("{key}: reload failed, keeping old instance: {e:#}");
                Err(format!("reload failed (old instance still serving): {e:#}"))
            }
            Err(e) => {
                tracing::error!("{key}: failed to start: {e:#}");
                Err(format!("failed to start: {e:#}"))
            }
        }
    }
}

/// Is this bind address loopback? Unparseable hosts count as non-loopback
/// (fail safe — the check guards running an unauthenticated registry).
fn bind_is_loopback(bind_addr: &str) -> bool {
    if let Ok(addr) = bind_addr.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }
    matches!(
        bind_addr.rsplit_once(':').map(|(host, _)| host),
        Some("localhost")
    )
}

/// The host-local credential store path (multi-tenant mode). Lives under the
/// host data root (`$HOME/.tangram/accounts.sqlite`), never inside any app's
/// replicated document. `TANGRAM_DATA_DIR` overrides the root (tests set it to
/// a scratch dir); else `$HOME/.tangram`; else `./data`.
fn host_account_store_path() -> PathBuf {
    let root = std::env::var("TANGRAM_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".tangram")))
        .unwrap_or_else(|_| PathBuf::from("data"));
    root.join("accounts.sqlite")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    let config_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("APPS_TOML").ok())
        .unwrap_or_else(|| "apps.toml".into());
    let config_path = std::fs::canonicalize(&config_path)
        .with_context(|| format!("apps config {config_path} not found"))?;

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let bind_loopback = bind_is_loopback(&bind_addr);
    let auth_token = std::env::var("TANGRAM_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());

    // The deployment auth mode (docs/design/auth.md). Absent `[auth]` → the
    // self-hosted, loopback-trusted default. `multi-tenant` preserves today's
    // tenant/token machinery; its identity layer (PATs/sessions/OAuth) lands
    // in checkpoints C2–C6.
    let auth_config = HostConfig::load(&config_path)
        .map(|config| config.auth)
        .unwrap_or_default();
    let auth_mode = auth_config.mode;
    let reads_gated = auth_config.reads_gated;
    // The per-principal mutation rate limit (auth.md §12, C7). The limiter is
    // only ever consulted by the multi-tenant `PrincipalGate`, so self-hosted
    // (which uses the single-token gate) is unaffected regardless of value.
    // Multi-tenant default is ~60/min; `0` disables it.
    let rate_limiter = Arc::new(multitenant::RateLimiter::new(
        auth_config.rate_limit_per_min,
    ));

    // The secret-resolution seam (ADR-0004). Built here (not inline in the Host
    // literal) so the OAuth client secret can resolve through it at startup.
    let secrets = Arc::new(secrets::SecretRegistry::default());

    // Resolve the OAuth/OIDC client config (auth.md §7 C6) in multi-tenant mode.
    // A PARTIAL or unresolvable config fails startup (no silent open-mode
    // fallback, auth.md §2); a fully-absent one is PAT-only bootstrap (None).
    let oauth = if auth_mode == config::AuthMode::MultiTenant {
        oauth::OauthConfig::resolve(&auth_config, &secrets)
            .await
            .context("resolving the [auth] OAuth config")?
    } else {
        None
    };
    if let Some(cfg) = &oauth {
        tracing::info!(
            "multi-tenant mode: OAuth sign-in enabled (provider {}, authorize {})",
            cfg.provider,
            cfg.authorize_url
        );
    }

    // Multi-tenant mode (auth.md §4): open the host-local credential store and,
    // on a zero-accounts boot, mint a local-admin PAT printed ONCE. The store
    // lives at the host data root, NEVER inside any app's replicated document.
    let accounts = if auth_mode == config::AuthMode::MultiTenant {
        let store_path = host_account_store_path();
        let store = accounts::AccountStore::open(&store_path)
            .with_context(|| format!("opening the account store at {}", store_path.display()))?;
        if let Some(token) = multitenant::bootstrap_admin(&store)? {
            tracing::warn!(
                "multi-tenant mode: no accounts existed — minted a local-admin PAT (Admin + \
                 registry:write + registry:read). THIS IS SHOWN ONCE; store it now:\n\n    \
                 {token}\n\n(use it as `Authorization: Bearer <token>` or to bootstrap the UI \
                 login). Account store: {}",
                store_path.display()
            );
        }
        tracing::info!(
            "multi-tenant mode: host-local credential store at {} — hashed PATs + sessions; \
             reads_gated = {reads_gated} (auth.md §4)",
            store_path.display()
        );
        Some(Arc::new(store))
    } else {
        None
    };

    if auth_token.is_none() {
        if bind_loopback {
            // The self-hosted happy path: loopback trust IS the model.
            tracing::info!(
                "self-hosted mode: loopback-trusted — local connections may use all routes; \
                 no token required (docs/design/auth.md). Bind beyond loopback or set [auth] \
                 mode=\"multi-tenant\" to require credentials."
            );
        } else if HostConfig::load(&config_path).is_ok_and(|config| config.has_registry()) {
            // Non-loopback bind with no token: preserve the safety guarantee —
            // refuse to expose a registry app's mutating surface unauthenticated.
            anyhow::bail!(
                "refusing to bind {bind_addr}: self-hosted mode is loopback-trusted, but \
                 apps.toml contains a registry app and TANGRAM_AUTH_TOKEN is not set — bind \
                 127.0.0.1 (loopback trust) or set the token to expose it"
            );
        } else {
            tracing::warn!(
                "binding beyond loopback with no TANGRAM_AUTH_TOKEN — mutating routes on any \
                 require_auth app are UNAUTHENTICATED; bind 127.0.0.1 (self-hosted loopback \
                 trust) or set the token before exposing the host"
            );
        }
    }

    // Open artifact upload (Phase S2b) — DEFAULT OFF. When enabled it is a
    // dev/demo capability: arbitrary-blob storage that MUST NOT face the
    // public internet until the MUST-FIX checklist (auth, per-upload size
    // cap, rate limiting, content/abuse controls, quota — see
    // crates/tangram-host/README.md) is met. The wasm-validity check is the
    // only checklist item already done. Mirror the registry posture: refuse a
    // non-loopback bind without a token, and log a loud warning when on.
    let artifacts_upload_enabled = HostConfig::load(&config_path)
        .map(|config| config.artifacts.upload_enabled)
        .unwrap_or(false);
    if artifacts_upload_enabled {
        if !bind_loopback && auth_token.is_none() {
            anyhow::bail!(
                "refusing to bind {bind_addr}: [artifacts] upload_enabled = true exposes open \
                 artifact upload (arbitrary blob storage) and TANGRAM_AUTH_TOKEN is not set — \
                 set the token or bind 127.0.0.1 (see the MUST-FIX checklist in \
                 crates/tangram-host/README.md before exposing this publicly)"
            );
        }
        tracing::warn!(
            "⚠️  OPEN ARTIFACT UPLOAD IS ENABLED (POST /artifacts) — DEV/DEMO ONLY. This is \
             arbitrary-blob storage; do NOT expose it publicly until the MUST-FIX checklist is \
             met (auth, per-upload size cap, rate limiting, content/abuse controls, quota — \
             see crates/tangram-host/README.md). Uploads are validated as wasm components and \
             {}.",
            if auth_token.is_some() {
                "require the bearer token"
            } else {
                "are UNAUTHENTICATED (loopback-only bind)"
            }
        );
    }

    // The MCP gateway (RUNTIME_PLAN D3): with `[gateway] enabled = true` and
    // an agentgateway binary, the host binds an INTERNAL loopback listener
    // for direct per-app serving and routes public MCP through a supervised
    // agentgateway child. The host stays the single public entry point.
    let gateway_settings = HostConfig::load(&config_path)
        .map(|config| config.gateway)
        .unwrap_or_default();
    let mut internal_listener = None;
    let mcp_gateway = if gateway_settings.enabled {
        match gateway_settings.resolve_binary() {
            Some(binary) => {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .context("binding the internal MCP listener")?;
                let internal_port = listener.local_addr()?.port();
                let port = match gateway_settings.port {
                    Some(port) => port,
                    None => gateway::free_port()?,
                };
                let config_file = gateway::default_config_file();
                tracing::info!(
                    "gateway: MCP through agentgateway ({}) on 127.0.0.1:{port} — internal \
                     app listener 127.0.0.1:{internal_port}, generated config {}",
                    binary.display(),
                    config_file.display()
                );
                internal_listener = Some(listener);
                Some(Arc::new(gateway::Gateway::new(
                    binary,
                    port,
                    internal_port,
                    gateway_settings.llm.clone(),
                    config_file,
                )))
            }
            None => {
                tracing::warn!(
                    "[gateway] is enabled but no agentgateway binary was found — falling back \
                     to direct per-app /mcp serving (install agentgateway or set \
                     [gateway].binary in apps.toml; the aggregate /mcp endpoint is unavailable)"
                );
                None
            }
        }
    } else {
        None
    };

    let host = Arc::new(Host {
        engine: runtime::engine()?,
        config_path: config_path.clone(),
        apps: RwLock::new(HashMap::new()),
        fleet: RwLock::new(BTreeMap::new()),
        auth_token,
        auth_mode,
        reads_gated,
        accounts,
        oauth,
        rate_limiter,
        tenant_tokens: RwLock::new(BTreeMap::new()),
        bind_loopback,
        nudge: Arc::new(Notify::new()),
        gateway: mcp_gateway,
        fetcher: fetch::Fetcher::new(fetch::default_cache_dir()),
        secrets,
        artifacts_upload_enabled,
    });
    host.converge().await;

    // Start the single host-wide epoch ticker (M2): it advances the shared
    // engine's epoch counter so every component store's CPU deadline elapses.
    // Without this, `set_epoch_deadline` would never fire. Shut down cleanly
    // (like the gateway supervisor) at host exit.
    let (epoch_ticker, epoch_task) = runtime::EpochTicker::spawn(host.engine().clone());

    // Start the gateway child (its config was generated by the converge
    // above) and the internal listener it targets — direct serving, no
    // gateway hop, loopback only.
    let mut supervisor = None;
    if let (Some(gateway), Some(listener)) = (host.gateway.clone(), internal_listener.take()) {
        supervisor = Some(gateway.spawn_supervisor());
        let internal_router = routes::root_router(host.clone(), false);
        tokio::spawn(axum::serve(listener, internal_router).into_future());
    }

    // The agent scheduler (Triggers v1): a supervised ~60s interval that drives
    // the `tangram` shell app's `tick_agents` action, so cron-triggered agent
    // notes run with no browser open. Stopped cleanly on shutdown alongside the
    // gateway child.
    let scheduler = Arc::new(scheduler::Scheduler::new(host.clone()));
    let scheduler_task = scheduler.spawn();

    // Watch the config file's DIRECTORY (editors replace the file, which
    // would orphan a file watch) and nudge the converge loop on any event
    // there; a slow tick also picks up component rebuilds (mtime changes).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = tx.send(());
        }
    })?;
    let watch_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    watcher.watch(&watch_dir, notify::RecursiveMode::NonRecursive)?;

    let converge_host = host.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = rx.recv() => {
                    if event.is_none() {
                        return; // watcher gone
                    }
                    // Debounce editor write bursts, then drain.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    while rx.try_recv().is_ok() {}
                }
                // A registry document changed (install_app & co.) —
                // converge immediately, no debounce.
                _ = converge_host.nudge.notified() => {}
                _ = tick.tick() => {}
            }
            converge_host.converge().await;
        }
    });

    let router = routes::root_router(host.clone(), host.gateway.is_some());
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    tracing::info!(
        "tangram-host — index http://{bind_addr}/ (apps from {}, fleet status /api/fleet)",
        config_path.display()
    );
    if host.gateway.is_some() {
        tracing::info!("mcp — aggregate http://{bind_addr}/mcp (all apps, tools <app>_<tool>)");
    }
    for key in host.apps.read().await.keys() {
        let prefix = key.route_prefix();
        tracing::info!("{key} — web UI http://{bind_addr}{prefix}/");
        tracing::info!("{key} — mcp    http://{bind_addr}{prefix}/mcp");
        tracing::info!("{key} — sync   http://{bind_addr}{prefix}/sync");
    }

    // Race the server against Ctrl-C instead of graceful shutdown: SSE
    // state/poke streams and MCP sessions never close on their own, and
    // persistence is synchronous on every change — same pattern as
    // App::serve. The gateway child is the exception: it must be killed
    // explicitly (and is kill_on_drop as a backstop), so the select! is
    // followed by a bounded supervisor shutdown instead of returning
    // straight out of the race.
    tokio::select! {
        result = axum::serve(listener, router).into_future() => result?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("tangram-host — shutting down");
        }
    }
    scheduler.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(2), scheduler_task).await;
    if let Some(gateway) = &host.gateway {
        gateway.shutdown();
    }
    if let Some(task) = supervisor {
        // The supervisor kills the child and returns; don't hang shutdown if
        // something is wedged.
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    // Stop the epoch ticker (M2) cleanly; bounded so shutdown never hangs.
    epoch_ticker.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), epoch_task).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bind_is_loopback;

    #[test]
    fn loopback_detection() {
        assert!(bind_is_loopback("127.0.0.1:8080"));
        assert!(bind_is_loopback("[::1]:8080"));
        assert!(bind_is_loopback("localhost:8080"));
        assert!(!bind_is_loopback("0.0.0.0:8080"));
        assert!(!bind_is_loopback("[::]:8080"));
        assert!(!bind_is_loopback("192.168.1.4:8080"));
        assert!(!bind_is_loopback("example.com:8080"));
        assert!(!bind_is_loopback("garbage"));
    }
}
