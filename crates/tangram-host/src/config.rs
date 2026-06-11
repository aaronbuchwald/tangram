//! Desired state, part 1: `apps.toml`. The file watcher re-reads it on every
//! change and the reconciler converges the running set of components toward
//! it. Since Phase 3 the file is the BOOTSTRAP half of the desired state:
//! an app flagged `registry = true` is itself a Tangram app whose replicated
//! document carries ADDITIONAL app specs, merged over this file by
//! [`crate::registry::merge`] (registry entries win on name collision).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// One app's spec: which component to run, what UI to serve, and what the
/// component is granted (data dir, outbound hosts, environment).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSpec {
    /// Path to the compiled `wasm32-wasip2` component.
    pub component: PathBuf,
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
    /// `${VAR}` is expanded from the HOST's environment at converge time, so
    /// secrets can stay in `.env` instead of `apps.toml`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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

/// Expand one config value: the exact form `${VAR}` is replaced from the
/// HOST's environment (missing host vars expand to empty, with a warning);
/// anything else passes through as-is. Shared by app `env`, `remote_token`,
/// and tenant `token` values, so secrets can stay in `.env`.
pub fn expand_value(context: &str, value: &str) -> String {
    match value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
        Some(var) => std::env::var(var).unwrap_or_else(|_| {
            tracing::warn!("{context}: references unset host var ${{{var}}}");
            String::new()
        }),
        None => value.to_string(),
    }
}

impl AppSpec {
    /// The app's resolved env, with `${VAR}` values expanded from the host
    /// environment (missing host vars expand to empty, with a warning).
    pub fn resolved_env(&self, app: &str) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(key, value)| {
                let resolved = expand_value(&format!("{app}: env {key}"), value);
                (key.clone(), resolved)
            })
            .collect()
    }

    /// The resolved `remote_token` (with `${VAR}` expansion); empty → None.
    pub fn resolved_remote_token(&self, app: &str) -> Option<String> {
        self.remote_token
            .as_deref()
            .map(|t| expand_value(&format!("{app}: remote_token"), t))
            .filter(|t| !t.trim().is_empty())
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
    /// The resolved bearer token (with `${VAR}` expansion); empty → None,
    /// which disables the tenant rather than running it open.
    pub fn resolved_token(&self, tenant: &str) -> Option<String> {
        let token = expand_value(&format!("tenant {tenant}: token"), &self.token);
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
}

impl HostConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(text)?;
        for name in config.apps.keys() {
            validate_name(name)?;
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

    #[test]
    fn parses_tenants_alongside_apps() {
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
        assert_eq!(alice.apps["notes"].component, PathBuf::from("notes.wasm"));
        let bob = &config.tenants.tenants["bob"];
        assert_eq!(bob.max_apps, 8, "max_apps defaults to 8");
        assert_eq!(bob.allow_hosts_ceiling, None);
        assert!(bob.apps.is_empty(), "default bootstrap: just a registry");

        // Token resolution: ${VAR} expands; unset → None (tenant disabled).
        assert_eq!(
            bob.resolved_token("bob").as_deref(),
            Some("literal-bob-token")
        );
        // Safety: test-local var name, nothing else reads it concurrently.
        unsafe { std::env::set_var("TANGRAM_TEST_ALICE_TOKEN_SET", "s3cret") };
        let mut alice2 = alice.clone();
        alice2.token = "${TANGRAM_TEST_ALICE_TOKEN_SET}".into();
        assert_eq!(alice2.resolved_token("alice").as_deref(), Some("s3cret"));
        alice2.token = "${TANGRAM_TEST_ALICE_TOKEN_UNSET}".into();
        assert_eq!(alice2.resolved_token("alice"), None);

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

    #[test]
    fn env_passthrough_expands_host_vars() {
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
        let env = config.apps["app"].resolved_env("app");
        assert!(env.contains(&("LITERAL".into(), "as-is".into())));
        assert!(env.contains(&("EXPANDED".into(), "secret-value".into())));
    }
}
