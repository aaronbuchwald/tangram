//! Desired state, part 2: the registry app (RUNTIME_PLAN Phase 3). An app
//! flagged `registry = true` in `apps.toml` carries a replicated list of app
//! specs (`apps/registry`); the host parses that list out of the app's
//! `state-json` and merges it OVER the file config — `apps.toml` stays the
//! bootstrap (and the registry's own entry), registry entries win on name
//! collision. Because the list lives in the registry's automerge document,
//! registry-installed apps persist across host restarts and replicate.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::{AppSpec, validate_name};

/// Where a desired app came from — reported by `GET /api/fleet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    File,
    Registry,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Registry => "registry",
        }
    }
}

/// One entry of the merged desired state. Disabled entries are kept (so the
/// fleet route can report them) but not run.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub spec: AppSpec,
    pub source: Source,
}

/// The registry model's state shape (apps/registry). Tolerant: defaults for
/// everything optional, so a newer registry model never breaks an older
/// host.
#[derive(serde::Deserialize)]
struct RegistryState {
    #[serde(default)]
    apps: Vec<RegistryEntry>,
}

#[derive(serde::Deserialize)]
struct RegistryEntry {
    name: String,
    component: PathBuf,
    ui: PathBuf,
    #[serde(default)]
    data_dir: Option<PathBuf>,
    #[serde(default)]
    allow_hosts: Vec<String>,
    #[serde(default)]
    env: Vec<EnvVar>,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(serde::Deserialize)]
struct EnvVar {
    key: String,
    value: String,
}

fn default_true() -> bool {
    true
}

/// Parse a registry app's `state-json` into validated app specs. Invalid
/// entries (bad name, empty component/ui) are skipped with a warning — one
/// bad install must not take down the rest of the fleet.
pub fn parse_state(registry: &str, state: &serde_json::Value) -> Vec<(String, AppSpec)> {
    let parsed: RegistryState = match serde_json::from_value(state.clone()) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!("{registry}: state is not a registry spec list, ignoring: {e}");
            return Vec::new();
        }
    };
    parsed
        .apps
        .into_iter()
        .filter_map(|entry| {
            if let Err(e) = validate_name(&entry.name) {
                tracing::warn!("{registry}: skipping registry entry: {e}");
                return None;
            }
            if entry.component.as_os_str().is_empty() || entry.ui.as_os_str().is_empty() {
                tracing::warn!(
                    "{registry}: skipping entry {:?}: component and ui must be non-empty",
                    entry.name
                );
                return None;
            }
            let spec = AppSpec {
                component: entry.component,
                ui: entry.ui,
                data_dir: entry.data_dir,
                allow_hosts: entry.allow_hosts,
                env: entry.env.into_iter().map(|e| (e.key, e.value)).collect(),
                remote: None,
                registry: false,
                require_auth: false,
                enabled: entry.enabled,
            };
            Some((entry.name, spec))
        })
        .collect()
}

/// Merge the file config with registry entries into the full desired state.
///
/// - `apps.toml` is the base (bootstrap + the registry's own entry);
/// - registry entries win on name collision — including `enabled = false`,
///   which parks an app the file would otherwise run;
/// - EXCEPT a name that the file flags `registry = true`: a registry cannot
///   redefine (or disable) a registry app through its own replicated
///   document — that stays under the operator's file-level control.
pub fn merge(
    file: &BTreeMap<String, AppSpec>,
    registry_entries: Vec<(String, AppSpec)>,
) -> BTreeMap<String, Desired> {
    let mut desired: BTreeMap<String, Desired> = file
        .iter()
        .map(|(name, spec)| {
            (
                name.clone(),
                Desired {
                    spec: spec.clone(),
                    source: Source::File,
                },
            )
        })
        .collect();
    for (name, spec) in registry_entries {
        if file.get(&name).is_some_and(|f| f.registry) {
            tracing::warn!(
                "registry entry {name:?} collides with a registry app in apps.toml — \
                 ignored (registry apps are file-controlled)"
            );
            continue;
        }
        desired.insert(
            name,
            Desired {
                spec,
                source: Source::Registry,
            },
        );
    }
    desired
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file_spec(registry: bool) -> AppSpec {
        AppSpec {
            component: "file.wasm".into(),
            ui: "file-ui".into(),
            data_dir: None,
            allow_hosts: Vec::new(),
            env: BTreeMap::new(),
            remote: None,
            registry,
            require_auth: false,
            enabled: true,
        }
    }

    fn registry_state(entries: serde_json::Value) -> serde_json::Value {
        json!({ "apps": entries })
    }

    #[test]
    fn parses_entries_and_skips_invalid_ones() {
        let state = registry_state(json!([
            {
                "name": "nutrition",
                "component": "nutrition.wasm",
                "ui": "nutrition-ui",
                "allow_hosts": ["api.calorieninjas.com"],
                "env": [{"key": "K", "value": "${V}"}],
                "enabled": true
            },
            { "name": "bad name", "component": "x.wasm", "ui": "u" },
            { "name": "noview", "component": "", "ui": "u" }
        ]));
        let entries = parse_state("registry", &state);
        assert_eq!(entries.len(), 1);
        let (name, spec) = &entries[0];
        assert_eq!(name, "nutrition");
        assert_eq!(spec.allow_hosts, vec!["api.calorieninjas.com".to_string()]);
        assert_eq!(spec.env.get("K").map(String::as_str), Some("${V}"));
        assert!(spec.enabled);
        assert!(!spec.registry, "registry entries never define registries");
        assert!(!spec.require_auth);
    }

    #[test]
    fn parse_tolerates_non_registry_state() {
        assert!(parse_state("registry", &json!({"notes": []})).is_empty());
        assert!(parse_state("registry", &json!({"apps": "nope"})).is_empty());
    }

    #[test]
    fn merge_unions_file_and_registry() {
        let mut file = BTreeMap::new();
        file.insert("registry".to_string(), file_spec(true));
        file.insert("notes".to_string(), file_spec(false));
        let mut installed = file_spec(false);
        installed.component = "nutrition.wasm".into();
        let desired = merge(&file, vec![("nutrition".to_string(), installed)]);
        assert_eq!(desired.len(), 3);
        assert_eq!(desired["registry"].source, Source::File);
        assert_eq!(desired["notes"].source, Source::File);
        assert_eq!(desired["nutrition"].source, Source::Registry);
    }

    #[test]
    fn registry_wins_name_collisions_with_file_apps() {
        let mut file = BTreeMap::new();
        file.insert("notes".to_string(), file_spec(false));
        let mut overriding = file_spec(false);
        overriding.component = "other-notes.wasm".into();
        let desired = merge(&file, vec![("notes".to_string(), overriding)]);
        assert_eq!(desired["notes"].source, Source::Registry);
        assert_eq!(
            desired["notes"].spec.component,
            PathBuf::from("other-notes.wasm")
        );
    }

    #[test]
    fn disabled_registry_entry_parks_a_file_app() {
        let mut file = BTreeMap::new();
        file.insert("notes".to_string(), file_spec(false));
        let mut disabled = file_spec(false);
        disabled.enabled = false;
        let desired = merge(&file, vec![("notes".to_string(), disabled)]);
        // present (so the fleet reports it) but not enabled (so it won't run)
        assert!(!desired["notes"].spec.enabled);
        assert_eq!(desired["notes"].source, Source::Registry);
    }

    #[test]
    fn registry_cannot_redefine_a_registry_app() {
        let mut file = BTreeMap::new();
        file.insert("registry".to_string(), file_spec(true));
        let mut hostile = file_spec(false);
        hostile.enabled = false;
        let desired = merge(&file, vec![("registry".to_string(), hostile)]);
        assert_eq!(desired["registry"].source, Source::File);
        assert!(desired["registry"].spec.enabled);
        assert!(desired["registry"].spec.registry);
    }
}
