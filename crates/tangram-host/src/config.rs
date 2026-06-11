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
/// URL-trivial. Shared by the file loader and the registry-entry parser.
pub fn validate_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "app name {name:?} must be alphanumeric/dash/underscore (it becomes a path prefix)"
    );
    Ok(())
}

impl AppSpec {
    /// The app's resolved env, with `${VAR}` values expanded from the host
    /// environment (missing host vars expand to empty, with a warning).
    pub fn resolved_env(&self, app: &str) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(key, value)| {
                let resolved = match value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
                    Some(var) => std::env::var(var).unwrap_or_else(|_| {
                        tracing::warn!("{app}: env {key} references unset host var ${{{var}}}");
                        String::new()
                    }),
                    None => value.clone(),
                };
                (key.clone(), resolved)
            })
            .collect()
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

#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    #[serde(default)]
    pub apps: BTreeMap<String, AppSpec>,
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
