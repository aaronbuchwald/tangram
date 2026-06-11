//! Registry — the fleet's source of truth (RUNTIME_PLAN Phase 3).
//!
//! The registry is itself a Tangram app: a replicated list of app specs
//! mirroring the `apps.toml` schema. `tangram-host` runs it like any other
//! component, subscribes to its document, and treats the spec list as
//! additional desired state merged over `apps.toml` (registry entries win on
//! name collision). Because the list lives in the replicated document,
//! registry-installed apps persist across host restarts and converge on
//! every replica.
//!
//! Deliberately NOT in this model: live status (running/healthy/error).
//! Status is an observation of one particular host, not shared desired
//! state — the host serves it separately at `GET /api/fleet`.

use tangram::prelude::*;

#[model]
#[derive(Default)]
pub struct Registry {
    apps: Vec<AppSpec>,
}

/// One installed app's spec — the same shape as an `[apps.<name>]` entry in
/// `apps.toml`.
#[model]
pub struct AppSpec {
    /// Unique app name; becomes the path prefix (`/<name>/...`).
    name: String,
    /// Path to the compiled `wasm32-wasip2` component, resolved by the host.
    /// Empty when the app is installed by URL (`component_url`).
    component: String,
    /// Install-from-URL alternative to `component` (RUNTIME_PLAN Phase 8):
    /// the HOST downloads the artifact, verifies `component_sha256` before
    /// instantiation, and caches it immutably by hash. Additive fields:
    /// older documents hydrate them as `None`.
    #[autosurgeon(missing = "Option::default")]
    component_url: Option<String>,
    /// Hex sha-256 the downloaded artifact must hash to — REQUIRED with
    /// `component_url`. A mismatch is a converge error on the host and the
    /// app does not run.
    #[autosurgeon(missing = "Option::default")]
    component_sha256: Option<String>,
    /// Directory of static UI files served at `/<name>/`.
    ui: String,
    /// Where the app's document lives. `None` = the host default,
    /// `$HOME/.<name>`.
    #[autosurgeon(missing = "Option::default")]
    data_dir: Option<String>,
    /// The component's ENTIRE outbound-network grant (exact host names).
    /// Empty = no outbound network at all.
    allow_hosts: Vec<String>,
    /// Environment variables handed to the component. A value of the exact
    /// form `${VAR}` is expanded from the HOST's environment at converge
    /// time, so secrets stay in the host's `.env`, not in this replicated
    /// document.
    env: Vec<EnvVar>,
    /// Disabled apps stay on record but are not run by the host.
    enabled: bool,
}

/// One environment variable granted to an app (`Vec`, not a map — model
/// `Default`s must be deterministic).
#[model]
pub struct EnvVar {
    key: String,
    value: String,
}

/// Same rule as the host's config validation: the name becomes a path
/// prefix, so keep it URL-trivial.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "app name {name:?} must be non-empty alphanumeric/dash/underscore \
             (it becomes a path prefix)"
        ));
    }
    Ok(())
}

fn validate_env(env: &[EnvVar]) -> Result<(), String> {
    match env.iter().find(|e| e.key.trim().is_empty()) {
        Some(_) => Err("env entries must have a non-empty key".into()),
        None => Ok(()),
    }
}

/// Same rule as the host's loader: exactly one component source — a local
/// path, or a URL pinned to a well-formed sha-256 digest.
fn validate_component_source(
    component: &Option<String>,
    url: &Option<String>,
    sha256: &Option<String>,
) -> Result<(), String> {
    let component = component
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    match (component, url.as_deref()) {
        (Some(_), None) => match sha256 {
            None => Ok(()),
            Some(_) => Err("component_sha256 is only valid with component_url \
                            (local component paths are not hash-verified)"
                .into()),
        },
        (None, Some(url)) => {
            if !url.starts_with("https://") && !url.starts_with("http://") {
                return Err(format!("component_url must be http(s), got {url:?}"));
            }
            let digest = sha256.as_deref().unwrap_or("").trim();
            if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(
                    "component_url requires component_sha256: 64 hex characters \
                     (the host verifies the artifact before instantiation)"
                        .into(),
                );
            }
            Ok(())
        }
        (Some(_), Some(_)) => {
            Err("set exactly one of component (local path) and component_url, not both".into())
        }
        (None, None) => Err("set exactly one of component (local path) and component_url".into()),
    }
}

#[actions]
impl Registry {
    /// Install an app on the host fleet, or replace its spec if the name is
    /// already installed. Give the component as EITHER `component` (path to
    /// a compiled wasm32-wasip2 component on the HOST's filesystem) OR
    /// `component_url` + `component_sha256` (the host downloads the
    /// artifact, verifies the sha-256 BEFORE running it, and caches it by
    /// hash — e.g. a marketplace listing). `ui` is a directory of static
    /// files on the host. `allow_hosts` is the app's entire
    /// outbound-network grant (exact host names; omit for no network).
    /// `env` entries with a value of the exact form `${VAR}` are expanded
    /// from the host's environment, so secrets can stay in the host's .env.
    /// The app starts disabled=false only via `set_enabled`; new installs
    /// are enabled and serving at /<name>/ within seconds.
    #[allow(clippy::too_many_arguments)]
    pub fn install_app(
        &mut self,
        name: String,
        component: Option<String>,
        component_url: Option<String>,
        component_sha256: Option<String>,
        ui: String,
        data_dir: Option<String>,
        allow_hosts: Option<Vec<String>>,
        env: Option<Vec<EnvVar>>,
    ) -> Result<(), String> {
        validate_name(&name)?;
        validate_component_source(&component, &component_url, &component_sha256)?;
        if ui.trim().is_empty() {
            return Err("ui must be a non-empty directory path".into());
        }
        let env = env.unwrap_or_default();
        validate_env(&env)?;
        let spec = AppSpec {
            name: name.clone(),
            component: component.unwrap_or_default(),
            component_url,
            component_sha256,
            ui,
            data_dir,
            allow_hosts: allow_hosts.unwrap_or_default(),
            env,
            enabled: true,
        };
        match self.apps.iter_mut().find(|a| a.name == name) {
            Some(existing) => *existing = spec,
            None => self.apps.push(spec),
        }
        Ok(())
    }

    /// Enable or disable an installed app. Disabled apps keep their spec
    /// (and their data on the host) but are stopped and their routes
    /// removed; re-enabling brings them back.
    pub fn set_enabled(&mut self, name: String, enabled: bool) -> Result<(), String> {
        self.find_mut(&name)?.enabled = enabled;
        Ok(())
    }

    /// Remove an app from the registry entirely. The host stops it and its
    /// routes disappear; its data directory is left untouched on the host,
    /// so a later reinstall under the same name picks the document back up.
    pub fn remove_app(&mut self, name: String) -> Result<(), String> {
        let before = self.apps.len();
        self.apps.retain(|a| a.name != name);
        if self.apps.len() == before {
            return Err(format!("no app named {name:?}"));
        }
        Ok(())
    }

    /// Point an installed app at a different compiled component path (e.g.
    /// after rebuilding to a new location). The host reloads the app live.
    /// Clears any component_url/component_sha256 source — the local path
    /// becomes the app's single component source.
    pub fn set_component(&mut self, name: String, component: String) -> Result<(), String> {
        if component.trim().is_empty() {
            return Err("component must be a non-empty path to a .wasm component".into());
        }
        let app = self.find_mut(&name)?;
        app.component = component;
        app.component_url = None;
        app.component_sha256 = None;
        Ok(())
    }

    /// Replace an installed app's outbound-network grant. The list is the
    /// app's ENTIRE outbound HTTP allowlist (exact host names); an empty
    /// list revokes all network access.
    pub fn set_allow_hosts(
        &mut self,
        name: String,
        allow_hosts: Vec<String>,
    ) -> Result<(), String> {
        self.find_mut(&name)?.allow_hosts = allow_hosts;
        Ok(())
    }

    /// Replace an installed app's environment variables. Values of the
    /// exact form `${VAR}` are expanded from the HOST's environment at
    /// converge time (keep secrets in the host's .env, not here — this
    /// document replicates).
    pub fn set_env(&mut self, name: String, env: Vec<EnvVar>) -> Result<(), String> {
        validate_env(&env)?;
        self.find_mut(&name)?.env = env;
        Ok(())
    }

    /// List every installed app spec, enabled or not. Live status
    /// (running/healthy/error) is served by the host at GET /api/fleet,
    /// not stored here.
    pub fn list_apps(&self) -> Vec<AppSpec> {
        self.apps.clone()
    }
}

impl Registry {
    fn find_mut(&mut self, name: &str) -> Result<&mut AppSpec, String> {
        self.apps
            .iter_mut()
            .find(|a| a.name == name)
            .ok_or_else(|| format!("no app named {name:?}"))
    }
}

/// MCP instructions, shared between the native app builder and the WASM
/// component's `describe()` export.
const INSTRUCTIONS: &str = "The Tangram fleet registry: the replicated source of truth for which \
     apps the host runs. install_app/remove_app/set_enabled change the \
     desired state; the host converges within seconds. Mutating tools \
     require Authorization: Bearer <TANGRAM_AUTH_TOKEN> when the host has a \
     token configured. Live per-app status is at the host's GET /api/fleet.";

/// The registry app, fully configured. Call `.serve()` to run it standalone
/// or `.build()` to mount it in a multi-app host.
#[cfg(not(target_family = "wasm"))]
pub fn app() -> App<Registry> {
    App::<Registry>::new("registry")
        .instructions(INSTRUCTIONS)
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
}

// Compiled for wasm32-wasip2, the same model + actions become a Tangram
// component (`tangram-host` owns the platform around it).
#[cfg(target_family = "wasm")]
tangram::export_component!(Registry {
    name: "registry",
    instructions: INSTRUCTIONS,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn install(reg: &mut Registry, name: &str) {
        reg.install_app(
            name.into(),
            Some(format!("target/{name}.wasm")),
            None,
            None,
            format!("apps/{name}/ui"),
            None,
            None,
            None,
        )
        .expect("install");
    }

    #[test]
    fn install_lists_enabled_app() {
        let mut reg = Registry::default();
        install(&mut reg, "notes");
        let apps = reg.list_apps();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "notes");
        assert!(apps[0].enabled);
        assert!(apps[0].allow_hosts.is_empty());
    }

    #[test]
    fn install_same_name_replaces_spec() {
        let mut reg = Registry::default();
        install(&mut reg, "notes");
        reg.set_enabled("notes".into(), false).unwrap();
        reg.install_app(
            "notes".into(),
            Some("elsewhere/notes.wasm".into()),
            None,
            None,
            "apps/notes/ui".into(),
            None,
            Some(vec!["api.example.com".into()]),
            None,
        )
        .unwrap();
        let apps = reg.list_apps();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].component, "elsewhere/notes.wasm");
        assert_eq!(apps[0].allow_hosts, vec!["api.example.com".to_string()]);
        // a reinstall is a fresh enabled spec
        assert!(apps[0].enabled);
    }

    #[test]
    fn validation_rejects_bad_specs() {
        let mut reg = Registry::default();
        let install = |reg: &mut Registry, name: &str, component: &str, ui: &str| {
            reg.install_app(
                name.into(),
                Some(component.into()),
                None,
                None,
                ui.into(),
                None,
                None,
                None,
            )
        };
        assert!(install(&mut reg, "bad name", "c.wasm", "ui").is_err());
        assert!(install(&mut reg, "", "c.wasm", "ui").is_err());
        assert!(install(&mut reg, "ok", "  ", "ui").is_err());
        assert!(install(&mut reg, "ok", "c.wasm", "").is_err());
        assert!(
            reg.install_app(
                "ok".into(),
                Some("c.wasm".into()),
                None,
                None,
                "ui".into(),
                None,
                None,
                Some(vec![EnvVar {
                    key: " ".into(),
                    value: "x".into()
                }]),
            )
            .is_err()
        );
        assert!(reg.list_apps().is_empty());
    }

    #[test]
    fn install_by_url_requires_exactly_one_source_and_a_sha() {
        let mut reg = Registry::default();
        let sha = "a".repeat(64);
        let by_url =
            |reg: &mut Registry, component: Option<&str>, url: Option<&str>, sha: Option<&str>| {
                reg.install_app(
                    "marketplace-notes".into(),
                    component.map(Into::into),
                    url.map(Into::into),
                    sha.map(Into::into),
                    "apps/notes/ui".into(),
                    None,
                    None,
                    None,
                )
            };
        // url + well-formed sha-256 → ok; component stays empty in the doc.
        by_url(
            &mut reg,
            None,
            Some("https://example.test/notes.wasm"),
            Some(&sha),
        )
        .expect("url install");
        let app = &reg.list_apps()[0];
        assert_eq!(app.component, "");
        assert_eq!(
            app.component_url.as_deref(),
            Some("https://example.test/notes.wasm")
        );
        assert_eq!(app.component_sha256.as_deref(), Some(sha.as_str()));

        // Bad shapes are rejected: url without sha, malformed sha, both
        // sources, neither source, non-http scheme, sha with a local path.
        assert!(by_url(&mut reg, None, Some("https://x.test/a.wasm"), None).is_err());
        assert!(by_url(&mut reg, None, Some("https://x.test/a.wasm"), Some("beef")).is_err());
        assert!(
            by_url(
                &mut reg,
                Some("c.wasm"),
                Some("https://x.test/a.wasm"),
                Some(&sha)
            )
            .is_err()
        );
        assert!(by_url(&mut reg, None, None, None).is_err());
        assert!(by_url(&mut reg, None, Some("ftp://x.test/a.wasm"), Some(&sha)).is_err());
        assert!(by_url(&mut reg, Some("c.wasm"), None, Some(&sha)).is_err());

        // set_component switches the app to a single local-path source.
        reg.set_component("marketplace-notes".into(), "local/notes.wasm".into())
            .unwrap();
        let app = &reg.list_apps()[0];
        assert_eq!(app.component, "local/notes.wasm");
        assert_eq!(app.component_url, None);
        assert_eq!(app.component_sha256, None);
    }

    #[test]
    fn set_enabled_and_remove() {
        let mut reg = Registry::default();
        install(&mut reg, "notes");
        reg.set_enabled("notes".into(), false).unwrap();
        assert!(!reg.list_apps()[0].enabled);
        assert!(reg.set_enabled("ghost".into(), true).is_err());
        reg.remove_app("notes".into()).unwrap();
        assert!(reg.list_apps().is_empty());
        assert!(reg.remove_app("notes".into()).is_err());
    }

    #[test]
    fn setters_update_fields() {
        let mut reg = Registry::default();
        install(&mut reg, "nutrition");
        reg.set_component("nutrition".into(), "new/nutrition.wasm".into())
            .unwrap();
        reg.set_allow_hosts("nutrition".into(), vec!["api.calorieninjas.com".into()])
            .unwrap();
        reg.set_env(
            "nutrition".into(),
            vec![EnvVar {
                key: "CALORIENINJAS_API_KEY".into(),
                value: "${CALORIENINJAS_API_KEY}".into(),
            }],
        )
        .unwrap();
        let app = &reg.list_apps()[0];
        assert_eq!(app.component, "new/nutrition.wasm");
        assert_eq!(app.allow_hosts, vec!["api.calorieninjas.com".to_string()]);
        assert_eq!(app.env[0].value, "${CALORIENINJAS_API_KEY}");
        assert!(reg.set_component("ghost".into(), "x.wasm".into()).is_err());
        assert!(reg.set_component("nutrition".into(), "".into()).is_err());
    }
}
