//! Multi-tenancy (RUNTIME_PLAN Phase 5): one host process, one public port,
//! N tenants — each an isolated app set under `/t/<tenant>/` with its own
//! data tree, outbound ceiling, app cap, and per-tenant registry. This
//! module holds the tenant-scoped identity and policy pieces:
//!
//! - [`AppKey`] — the live-table key: a (tenant, app) pair, where
//!   `tenant = None` is today's single-tenant top level (unchanged);
//! - [`validate_tenant_data_dir`] / [`effective_spec`] — confinement: a
//!   tenant app's data lives under `<data_root>/<tenant>/…` no matter what
//!   its spec says, and its outbound allowlist is intersected with the
//!   tenant's ceiling;
//! - [`enforce_max_apps`] — the per-tenant app cap.
//!
//! Authentication for the namespace (the [`crate::auth::Principal`] seam)
//! lives in `auth.rs`.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use crate::config::AppSpec;
use crate::registry::{Desired, Source};

/// Identity of one app in the live table and fleet: top-level
/// (`tenant = None`, served at `/<app>/`) or tenant-scoped
/// (served at `/t/<tenant>/<app>/`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppKey {
    pub tenant: Option<String>,
    pub app: String,
}

impl AppKey {
    pub fn top(app: impl Into<String>) -> Self {
        Self {
            tenant: None,
            app: app.into(),
        }
    }

    pub fn tenant(tenant: impl Into<String>, app: impl Into<String>) -> Self {
        Self {
            tenant: Some(tenant.into()),
            app: app.into(),
        }
    }

    /// The app's route prefix: `/<app>` or `/t/<tenant>/<app>`.
    pub fn route_prefix(&self) -> String {
        match &self.tenant {
            Some(tenant) => format!("/t/{tenant}/{}", self.app),
            None => format!("/{}", self.app),
        }
    }
}

impl std::fmt::Display for AppKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.tenant {
            Some(tenant) => write!(f, "t/{tenant}/{}", self.app),
            None => write!(f, "{}", self.app),
        }
    }
}

/// A tenant app's `data_dir` must stay inside the tenant's data tree: only
/// plain relative paths (no absolute paths, no `..`) are accepted — they are
/// then joined under `<data_root>/<tenant>/`.
pub fn validate_tenant_data_dir(dir: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        dir.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
            && !dir.as_os_str().is_empty(),
        "tenant app data_dir must be a relative path inside the tenant's data root \
         (no absolute paths, no '..')"
    );
    Ok(())
}

/// Compute the EFFECTIVE spec the host runs for a tenant app — the tenant's
/// policy applied on top of whatever the spec (bootstrap file or the
/// tenant's registry) asked for:
///
/// - data confinement: the doc dir is forced under `<data_root>/<tenant>/`
///   (`<tenant_root>/<app>` by default; a relative `data_dir` is joined
///   under the tenant root; anything else is an error);
/// - outbound ceiling: `allow_hosts` is intersected with the tenant's
///   ceiling — hosts outside it are silently (well, loudly-logged) dropped,
///   so a wider grant degrades instead of failing;
/// - registry-sourced entries lose `${VAR}` env expansion: the host's
///   environment holds other tenants' tokens and platform secrets, and a
///   tenant's replicated doc must not be able to name them.
pub fn effective_spec(
    tenant: &str,
    app: &str,
    spec: &AppSpec,
    source: Source,
    tenant_root: &Path,
    ceiling: Option<&[String]>,
) -> Result<AppSpec, String> {
    let mut effective = spec.clone();

    effective.data_dir = Some(match &spec.data_dir {
        None => tenant_root.join(app),
        Some(dir) => {
            validate_tenant_data_dir(dir).map_err(|e| e.to_string())?;
            tenant_root.join(dir)
        }
    });

    if let Some(ceiling) = ceiling {
        let (kept, dropped): (Vec<String>, Vec<String>) = effective
            .allow_hosts
            .drain(..)
            .partition(|host| ceiling.contains(host));
        if !dropped.is_empty() {
            tracing::warn!(
                "t/{tenant}/{app}: allow_hosts {dropped:?} outside the tenant ceiling — \
                 dropped (effective grant is the intersection)"
            );
        }
        effective.allow_hosts = kept;
    }

    if source == Source::Registry {
        for (key, value) in effective.env.iter_mut() {
            if value.starts_with("${") && value.ends_with('}') {
                tracing::warn!(
                    "t/{tenant}/{app}: env {key} requests host-env expansion ({value}) — \
                     blanked (tenant-installed apps cannot read the host environment)"
                );
                value.clear();
            }
        }
        // Registry entries never carry remote/remote_token today; clear them
        // defensively so a future entry field can't dial out with host creds.
        effective.remote = None;
        effective.remote_token = None;
    }

    Ok(effective)
}

/// Enforce the tenant's app cap over its merged desired state: bootstrap
/// (file-sourced) apps are kept first, then registry-installed ones in
/// INSTALL order (`registry_order` is the name order of the tenant
/// registry's replicated list, which appends on install) — so a new install
/// can never evict the tenant's own registry or an earlier, already-running
/// install; the excess install itself errors. Returns the names (and the
/// error message) of apps beyond the cap; the caller reports them in the
/// tenant's fleet and does not run them.
pub fn enforce_max_apps(
    desired: &BTreeMap<String, Desired>,
    max_apps: usize,
    registry_order: &[String],
) -> Vec<(String, String)> {
    let rank = |name: &str| {
        registry_order
            .iter()
            .position(|n| n == name)
            .unwrap_or(usize::MAX)
    };
    let mut enabled: Vec<(&String, &Desired)> =
        desired.iter().filter(|(_, d)| d.spec.enabled).collect();
    enabled.sort_by_key(|(name, d)| {
        (
            d.source == Source::Registry,
            rank(name.as_str()),
            name.as_str(),
        )
    });
    enabled
        .into_iter()
        .skip(max_apps)
        .map(|(name, _)| {
            (
                name.clone(),
                format!("tenant app cap reached (max_apps = {max_apps}) — not running"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> AppSpec {
        AppSpec {
            component: "app.wasm".into(),
            ui: "ui".into(),
            data_dir: None,
            allow_hosts: vec!["api.calorieninjas.com".into(), "evil.example.com".into()],
            env: [
                ("LITERAL".to_string(), "x".to_string()),
                ("SECRET".to_string(), "${HOST_SECRET}".to_string()),
            ]
            .into_iter()
            .collect(),
            remote: None,
            remote_token: None,
            registry: false,
            require_auth: false,
            enabled: true,
        }
    }

    #[test]
    fn data_dir_is_confined_to_the_tenant_root() {
        let root = Path::new("/data/tenants/alice");

        // Default: <root>/<app>.
        let eff = effective_spec("alice", "notes", &spec(), Source::File, root, None).unwrap();
        assert_eq!(eff.data_dir, Some(root.join("notes")));

        // Relative dirs are joined under the root.
        let mut relative = spec();
        relative.data_dir = Some("custom/dir".into());
        let eff = effective_spec("alice", "notes", &relative, Source::File, root, None).unwrap();
        assert_eq!(eff.data_dir, Some(root.join("custom/dir")));

        // Absolute paths and traversal are rejected.
        for escape in ["/etc", "../bob", "ok/../../bob"] {
            let mut bad = spec();
            bad.data_dir = Some(escape.into());
            let err = effective_spec("alice", "notes", &bad, Source::Registry, root, None)
                .expect_err(escape);
            assert!(err.contains("relative path"), "{escape}: {err}");
        }
    }

    #[test]
    fn allow_hosts_intersects_with_the_ceiling() {
        let root = Path::new("/r");
        let ceiling = vec!["api.calorieninjas.com".to_string()];
        let eff =
            effective_spec("alice", "a", &spec(), Source::File, root, Some(&ceiling)).unwrap();
        assert_eq!(eff.allow_hosts, vec!["api.calorieninjas.com".to_string()]);

        // No ceiling → grant unchanged.
        let eff = effective_spec("alice", "a", &spec(), Source::File, root, None).unwrap();
        assert_eq!(eff.allow_hosts.len(), 2);

        // Empty ceiling → no outbound at all.
        let eff = effective_spec("alice", "a", &spec(), Source::File, root, Some(&[])).unwrap();
        assert!(eff.allow_hosts.is_empty());
    }

    #[test]
    fn registry_sourced_specs_cannot_read_the_host_env() {
        let root = Path::new("/r");
        let eff = effective_spec("alice", "a", &spec(), Source::Registry, root, None).unwrap();
        assert_eq!(eff.env.get("LITERAL").map(String::as_str), Some("x"));
        assert_eq!(eff.env.get("SECRET").map(String::as_str), Some(""));
        // Bootstrap (file) specs keep expansion — the operator wrote them.
        let eff = effective_spec("alice", "a", &spec(), Source::File, root, None).unwrap();
        assert_eq!(
            eff.env.get("SECRET").map(String::as_str),
            Some("${HOST_SECRET}")
        );
    }

    #[test]
    fn max_apps_keeps_bootstrap_apps_and_evicts_newest_installs() {
        let entry = |source| Desired {
            spec: spec(),
            source,
        };
        let mut desired = BTreeMap::new();
        desired.insert("registry".to_string(), entry(Source::File));
        desired.insert("notes".to_string(), entry(Source::File));
        desired.insert("zzz-first-install".to_string(), entry(Source::Registry));
        desired.insert("aaa-second-install".to_string(), entry(Source::Registry));
        // Install order from the registry doc: zzz was installed first, so
        // the LATER aaa install is the one over the cap (not alphabetical —
        // a new install must never evict an already-running one).
        let order = vec![
            "zzz-first-install".to_string(),
            "aaa-second-install".to_string(),
        ];

        let over = enforce_max_apps(&desired, 3, &order);
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].0, "aaa-second-install", "the newest install errs");
        assert!(over[0].1.contains("max_apps = 3"));

        assert!(enforce_max_apps(&desired, 4, &order).is_empty());

        // The cap can never evict bootstrap (file) apps.
        let over = enforce_max_apps(&desired, 2, &order);
        assert_eq!(
            over.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["zzz-first-install", "aaa-second-install"]
        );

        // Disabled apps don't count toward the cap.
        desired.get_mut("aaa-second-install").unwrap().spec.enabled = false;
        assert!(enforce_max_apps(&desired, 3, &order).is_empty());
    }

    #[test]
    fn app_key_prefixes_and_display() {
        assert_eq!(AppKey::top("notes").route_prefix(), "/notes");
        assert_eq!(
            AppKey::tenant("alice", "notes").route_prefix(),
            "/t/alice/notes"
        );
        assert_eq!(
            AppKey::tenant("alice", "notes").to_string(),
            "t/alice/notes"
        );
        assert_eq!(AppKey::top("notes").to_string(), "notes");
    }
}
