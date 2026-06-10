//! Desired state: `apps.toml`. The file watcher re-reads it on every change
//! and the reconciler converges the running set of components toward it —
//! Phase 3 replaces this file with the registry app as the source of truth,
//! keeping it as import/export.

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
}

impl HostConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        for name in config.apps.keys() {
            anyhow::ensure!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "app name {name:?} must be alphanumeric/dash/underscore (it becomes a path prefix)"
            );
        }
        Ok(config)
    }
}
