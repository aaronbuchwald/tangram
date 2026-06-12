//! Desired state, part 2: the registry app (RUNTIME_PLAN Phase 3). An app
//! flagged `registry = true` in `apps.toml` carries a replicated list of app
//! specs (`apps/registry`); the host parses that list out of the app's
//! `state-json` and merges it OVER the file config — `apps.toml` stays the
//! bootstrap (and the registry's own entry), registry entries win on name
//! collision. Because the list lives in the registry's automerge document,
//! registry-installed apps persist across host restarts and replicate.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::{AppSpec, InjectRule, validate_name};

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
    /// This entry came from a FEDERATED registry — one whose document syncs
    /// with a peer (`remote` set). Used to enforce portability (Phase 9): a
    /// federated registry's entries are seen by every peer, so a
    /// local-`component` PATH (meaningful only on the host that wrote it) is
    /// non-portable. A peer that lacks the path reports a
    /// portability-flavored fleet error (see `Host::ensure_app`) rather than
    /// a bare "file not found", and NEVER mutates the shared document — the
    /// doc is desired state; runtime failures live only in `/api/fleet`.
    pub federated: bool,
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
    /// Local component path. The registry model keeps this a plain string
    /// for compatibility — empty means "use component_url instead".
    #[serde(default)]
    component: PathBuf,
    #[serde(default)]
    component_url: Option<String>,
    #[serde(default)]
    component_sha256: Option<String>,
    ui: PathBuf,
    #[serde(default)]
    data_dir: Option<PathBuf>,
    #[serde(default)]
    allow_hosts: Vec<String>,
    #[serde(default)]
    env: Vec<EnvVar>,
    /// Egress credential injection rules (ADR-0005, Phase 10b), one per
    /// outbound host — the host applies the credential at the `http-fetch`
    /// boundary so the component never holds the plaintext secret. The
    /// `secret` is a `scheme://locator` reference resolved from the host's
    /// environment, exactly like `env` values — secrets never enter the
    /// replicated document.
    #[serde(default)]
    inject: Vec<InjectEntry>,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(serde::Deserialize)]
struct EnvVar {
    key: String,
    value: String,
}

/// One injection rule in the registry document (model `Vec`, like `EnvVar`),
/// mapped to the host's host-keyed [`InjectRule`].
#[derive(serde::Deserialize)]
struct InjectEntry {
    /// Outbound host this rule applies to (must also be in `allow_hosts`).
    host: String,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    bearer: bool,
    #[serde(default)]
    query: Option<String>,
    /// `scheme://locator` secret reference, resolved host-side at egress.
    secret: String,
}

fn default_true() -> bool {
    true
}

/// A federated registry's sync coordinates (Phase 9), derived from the
/// registry app's own `remote` (`<base>/registry/sync` → `base`). When a
/// registry federates, every app it lists not only converges fleet-wide but
/// also replicates its OWN document with the same peer: the host derives each
/// installed app's sync remote as `<base>/<app>/sync` (carrying the
/// registry's `remote_token`), so one `remote` setting syncs both the desired
/// state AND the app data across the fleet. Apps installed BY url+hash become
/// portable; their documents converge through these derived remotes.
#[derive(Debug, Clone)]
pub struct Federation {
    /// The peer's sync base (no trailing slash, `/registry/sync` stripped).
    pub base: String,
    /// The bearer presented on every derived per-app sync remote (the
    /// registry's resolved `remote_token`), for private peers.
    pub token: Option<String>,
}

impl Federation {
    /// Derive `<base>/<app>/sync` for an installed app — the remote its
    /// document replicates with, matching the registry's own peer.
    fn app_remote(&self, app: &str) -> String {
        format!("{}/{app}/sync", self.base)
    }
}

/// Strip a registry app's `remote` (`…/registry/sync`) to the peer's sync
/// base, so per-app remotes can be derived as `<base>/<app>/sync`. A remote
/// that doesn't end in `/registry/sync` is used as-is (best effort).
pub fn sync_base(registry_remote: &str) -> String {
    let trimmed = registry_remote.trim_end_matches('/');
    trimmed
        .strip_suffix("/registry/sync")
        .or_else(|| trimmed.strip_suffix("/registry"))
        .unwrap_or(trimmed)
        .to_string()
}

/// Parse a registry app's `state-json` into validated app specs. Invalid
/// entries (bad name, empty component/ui) are skipped with a warning — one
/// bad install must not take down the rest of the fleet. `federation` is
/// `Some` when the registry federates (its `remote` is set): on a federated
/// registry a local-`component` PATH entry is flagged non-portable (Phase 9)
/// — it works only on the host that wrote it; peers report a clear fleet
/// error — AND each installed app gets a derived `remote` (`<base>/<app>/
/// sync`) so its document replicates with the same peer. The entry is still
/// returned (so the writing host runs it); the warning and the portability
/// error path live downstream.
pub fn parse_state(
    registry: &str,
    state: &serde_json::Value,
    federation: Option<&Federation>,
) -> Vec<RegistryDesired> {
    let federated = federation.is_some();
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
            if entry.ui.as_os_str().is_empty() {
                tracing::warn!(
                    "{registry}: skipping entry {:?}: ui must be non-empty",
                    entry.name
                );
                return None;
            }
            let component = (!entry.component.as_os_str().is_empty()).then_some(entry.component);
            // Portability advisory (Phase 9): a federated registry's document
            // is seen by every peer, so a local path means nothing on a peer.
            // We warn but do not skip — the writing host runs it fine; a peer
            // that lacks the path surfaces a clear fleet error in ensure_app.
            if federated && component.is_some() {
                tracing::warn!(
                    "{registry}: entry {:?} uses a local component PATH in a FEDERATED \
                     registry — this is host-local and will fleet-error on peers that lack \
                     the path; use component_url + component_sha256 for a portable install",
                    entry.name
                );
            }
            // A federated registry's apps replicate their OWN documents with
            // the same peer: derive `<base>/<app>/sync` (with the registry's
            // bearer). This is what turns "the fleet runs everywhere" into "a
            // replica that also has the data" from one `remote` setting.
            let (remote, remote_token) = match federation {
                Some(fed) => (Some(fed.app_remote(&entry.name)), fed.token.clone()),
                None => (None, None),
            };
            let inject: BTreeMap<String, InjectRule> = entry
                .inject
                .into_iter()
                .map(|i| {
                    (
                        i.host,
                        InjectRule {
                            header: i.header,
                            bearer: i.bearer,
                            query: i.query,
                            secret: i.secret,
                        },
                    )
                })
                .collect();
            let spec = AppSpec {
                // Empty path in the replicated doc = "installed by URL".
                component,
                component_url: entry.component_url,
                component_sha256: entry.component_sha256,
                ui: entry.ui,
                data_dir: entry.data_dir,
                allow_hosts: entry.allow_hosts,
                env: entry.env.into_iter().map(|e| (e.key, e.value)).collect(),
                inject,
                // The registry document does not yet carry call-level grants;
                // a registry-installed app desugars to the host-keyed compat
                // shim (its host-keyed inject becomes the broad implicit call),
                // and defaults to the migration enforcement mode (EC7).
                calls: Vec::new(),
                enforcement: None,
                // The OPT-IN policy engine (§9.2) is NOT carried by registry-
                // installed specs in this variant: a replicated policy would be
                // custom egress glue granted from the document plane, which the
                // §9.2/§6 posture keeps operator-authoritative. A policy stays in
                // the operator's apps.toml.
                policy: None,
                remote,
                remote_token,
                registry: false,
                require_auth: false,
                enabled: entry.enabled,
            };
            // Same gate as the file loader: exactly one component source,
            // well-formed sha-256, valid injection rules (each names one kind
            // and targets an allowlisted host), valid calls — one bad install
            // must not take down the rest of the fleet, so invalid entries are
            // skipped.
            if let Err(e) = spec.component_source() {
                tracing::warn!("{registry}: skipping entry {:?}: {e:#}", entry.name);
                return None;
            }
            if let Err(e) = spec.validate_inject() {
                tracing::warn!("{registry}: skipping entry {:?}: {e:#}", entry.name);
                return None;
            }
            Some(RegistryDesired {
                name: entry.name,
                spec,
                federated,
            })
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
    registry_entries: Vec<RegistryDesired>,
) -> BTreeMap<String, Desired> {
    let mut desired: BTreeMap<String, Desired> = file
        .iter()
        .map(|(name, spec)| {
            (
                name.clone(),
                Desired {
                    spec: spec.clone(),
                    source: Source::File,
                    // The file layer is local-authoritative, never "federated
                    // desired state from a peer".
                    federated: false,
                },
            )
        })
        .collect();
    for entry in registry_entries {
        if file.get(&entry.name).is_some_and(|f| f.registry) {
            tracing::warn!(
                "registry entry {:?} collides with a registry app in apps.toml — \
                 ignored (registry apps are file-controlled)",
                entry.name
            );
            continue;
        }
        desired.insert(
            entry.name,
            Desired {
                spec: entry.spec,
                source: Source::Registry,
                federated: entry.federated,
            },
        );
    }
    desired
}

/// One spec parsed out of a registry document, tagged with whether the
/// registry it came from is federated (its `remote` is set). Federation is a
/// per-registry property, so every entry from one document shares it.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryDesired {
    pub name: String,
    pub spec: AppSpec,
    pub federated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file_spec(registry: bool) -> AppSpec {
        AppSpec {
            component: Some("file.wasm".into()),
            component_url: None,
            component_sha256: None,
            ui: "file-ui".into(),
            data_dir: None,
            allow_hosts: Vec::new(),
            env: BTreeMap::new(),
            inject: BTreeMap::new(),
            calls: Vec::new(),
            enforcement: None,
            policy: None,
            remote: None,
            remote_token: None,
            registry,
            require_auth: false,
            enabled: true,
        }
    }

    fn registry_state(entries: serde_json::Value) -> serde_json::Value {
        json!({ "apps": entries })
    }

    /// A registry entry as it arrives at `merge` (non-federated by default).
    fn entry(name: &str, spec: AppSpec) -> RegistryDesired {
        RegistryDesired {
            name: name.to_string(),
            spec,
            federated: false,
        }
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
        let entries = parse_state("registry", &state, None);
        assert_eq!(entries.len(), 1);
        let RegistryDesired { name, spec, .. } = &entries[0];
        assert_eq!(name, "nutrition");
        assert_eq!(spec.allow_hosts, vec!["api.calorieninjas.com".to_string()]);
        assert_eq!(spec.env.get("K").map(String::as_str), Some("${V}"));
        assert!(spec.enabled);
        assert!(!spec.registry, "registry entries never define registries");
        assert!(!spec.require_auth);
    }

    #[test]
    fn parses_inject_rules_and_skips_bad_ones() {
        let state = registry_state(json!([
            {
                "name": "nutrition",
                "component": "nutrition.wasm",
                "ui": "nutrition-ui",
                "allow_hosts": ["api.calorieninjas.com"],
                "inject": [
                    { "host": "api.calorieninjas.com", "header": "X-Api-Key",
                      "secret": "env://CALORIENINJAS_API_KEY" }
                ]
            },
            // Inject targets a host NOT in allow_hosts → whole entry skipped.
            {
                "name": "bad-inject",
                "component": "x.wasm",
                "ui": "u",
                "allow_hosts": ["api.example.com"],
                "inject": [
                    { "host": "evil.example.com", "header": "X", "secret": "env://K" }
                ]
            }
        ]));
        let entries = parse_state("registry", &state, None);
        assert_eq!(entries.len(), 1, "the bad-inject entry is skipped");
        let spec = &entries[0].spec;
        assert_eq!(spec.inject.len(), 1);
        let rule = &spec.inject["api.calorieninjas.com"];
        assert_eq!(rule.header.as_deref(), Some("X-Api-Key"));
        assert_eq!(rule.secret, "env://CALORIENINJAS_API_KEY");
        assert_eq!(
            rule.kind().unwrap(),
            crate::config::InjectKind::Header("X-Api-Key".into())
        );
    }

    #[test]
    fn parses_url_entries_and_skips_invalid_sources() {
        let sha = "a".repeat(64);
        let state = registry_state(json!([
            {
                "name": "by-url",
                "component_url": "https://example.test/by-url.wasm",
                "component_sha256": sha,
                "ui": "by-url-ui"
            },
            // url without sha, sha with a path, both sources: all skipped
            { "name": "no-sha", "component_url": "https://example.test/x.wasm", "ui": "u" },
            { "name": "sha-with-path", "component": "x.wasm", "component_sha256": sha, "ui": "u" },
            { "name": "both", "component": "x.wasm",
              "component_url": "https://example.test/x.wasm", "component_sha256": sha, "ui": "u" },
            { "name": "bad-sha", "component_url": "https://example.test/x.wasm",
              "component_sha256": "nothex", "ui": "u" },
            { "name": "neither", "ui": "u" }
        ]));
        let entries = parse_state("registry", &state, None);
        assert_eq!(entries.len(), 1);
        let RegistryDesired { name, spec, .. } = &entries[0];
        assert_eq!(name, "by-url");
        assert_eq!(spec.component, None);
        assert_eq!(
            spec.component_source().unwrap(),
            crate::config::ComponentSource::Url {
                url: "https://example.test/by-url.wasm".into(),
                sha256: sha,
            }
        );
    }

    #[test]
    fn parse_tolerates_non_registry_state() {
        assert!(parse_state("registry", &json!({"notes": []}), None).is_empty());
        assert!(parse_state("registry", &json!({"apps": "nope"}), None).is_empty());
    }

    #[test]
    fn federated_path_entry_is_tagged_for_portability() {
        let state = registry_state(json!([
            { "name": "path-app", "component": "local.wasm", "ui": "u" },
            {
                "name": "url-app",
                "component_url": "https://example.test/u.wasm",
                "component_sha256": "a".repeat(64),
                "ui": "u"
            }
        ]));
        // Federated: every entry is tagged federated; the path entry is the
        // non-portable one (the warning fires; the entry is still returned),
        // and each app gets a derived `<base>/<app>/sync` remote so its
        // document replicates with the registry's peer.
        let fed = Federation {
            base: "https://peer.test".into(),
            token: Some("${TOK}".into()),
        };
        let entries = parse_state("registry", &state, Some(&fed));
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.federated));
        let path_app = entries.iter().find(|e| e.name == "path-app").unwrap();
        assert!(path_app.spec.component.is_some(), "path entry kept");
        let url_app = entries.iter().find(|e| e.name == "url-app").unwrap();
        assert_eq!(
            url_app.spec.remote.as_deref(),
            Some("https://peer.test/url-app/sync"),
            "the app's own document replicates with the registry's peer"
        );
        assert_eq!(
            url_app.spec.remote_token.as_deref(),
            Some("${TOK}"),
            "the registry's bearer reference (not a value) rides along"
        );
        // Non-federated: same parse, but nothing is flagged federated and no
        // per-app remote is derived.
        let entries = parse_state("registry", &state, None);
        assert!(entries.iter().all(|e| !e.federated));
        assert!(entries.iter().all(|e| e.spec.remote.is_none()));
    }

    #[test]
    fn sync_base_strips_the_registry_sync_suffix() {
        assert_eq!(
            sync_base("https://host:8080/registry/sync"),
            "https://host:8080"
        );
        assert_eq!(
            sync_base("https://host/t/alice/registry/sync/"),
            "https://host/t/alice"
        );
        // A base that isn't the conventional registry endpoint is used as-is.
        assert_eq!(sync_base("https://host/weird"), "https://host/weird");
    }

    #[test]
    fn merge_unions_file_and_registry() {
        let mut file = BTreeMap::new();
        file.insert("registry".to_string(), file_spec(true));
        file.insert("notes".to_string(), file_spec(false));
        let mut installed = file_spec(false);
        installed.component = Some("nutrition.wasm".into());
        let desired = merge(&file, vec![entry("nutrition", installed)]);
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
        overriding.component = Some("other-notes.wasm".into());
        let desired = merge(&file, vec![entry("notes", overriding)]);
        assert_eq!(desired["notes"].source, Source::Registry);
        assert_eq!(
            desired["notes"].spec.component,
            Some(PathBuf::from("other-notes.wasm"))
        );
    }

    #[test]
    fn disabled_registry_entry_parks_a_file_app() {
        let mut file = BTreeMap::new();
        file.insert("notes".to_string(), file_spec(false));
        let mut disabled = file_spec(false);
        disabled.enabled = false;
        let desired = merge(&file, vec![entry("notes", disabled)]);
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
        let desired = merge(&file, vec![entry("registry", hostile)]);
        assert_eq!(desired["registry"].source, Source::File);
        assert!(desired["registry"].spec.enabled);
        assert!(desired["registry"].spec.registry);
    }
}
