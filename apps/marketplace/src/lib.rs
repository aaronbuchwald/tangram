//! Marketplace — a catalog of installable Tangram apps (RUNTIME_PLAN
//! Phase 8).
//!
//! Each listing carries everything a host needs to install the app safely:
//! a `component_url` + pinned `component_sha256` (the host downloads,
//! verifies the hash BEFORE instantiation, and caches the artifact
//! immutably by hash), and — REQUIRED — a human-readable capability
//! manifest ("this app can reach: api.calorieninjas.com; env:
//! CALORIENINJAS_API_KEY") displayed alongside the mechanical import audit
//! (the `wasm-tools component wit` world dump proving the component's
//! closed world). Installing is the registry's job: the marketplace UI
//! posts the listing's url + sha + manifest grants to the local registry's
//! `install_app`.
//!
//! The marketplace is itself an ordinary Tangram app (replicated document,
//! actions, MCP, sync). Run it under tangram-host with
//! `require_auth = true` so that adding/removing listings needs the bearer
//! token while browsing stays open.
//!
//! TODO (explicitly NOT built — recorded for a later phase): third-party
//! submissions. A submission pipeline must gate listing approval on
//! (1) automated capability verification — the declared manifest is a
//! subset of the audited imports, (2) a sandboxed smoke-run of the
//! component, and (3) an LLM behavioral sanity check. Until then, listings
//! are curated by the operator through `add_listing`.

use tangram::prelude::*;

#[model]
pub struct Marketplace {
    listings: Vec<Listing>,
}

/// One installable app.
#[model]
pub struct Listing {
    /// Suggested install name; becomes the path prefix (`/<name>/...`).
    name: String,
    /// What the app does, for humans browsing the catalog.
    description: String,
    /// Version of the published artifact (the sha-256 pins the exact bytes;
    /// the version is for humans).
    version: String,
    /// Where the host downloads the `wasm32-wasip2` component from.
    component_url: String,
    /// Hex sha-256 the artifact must hash to — the host verifies BEFORE
    /// instantiation and refuses a mismatch.
    component_sha256: String,
    /// UI directory on the host (first-party apps ship their `ui/` with the
    /// repo checkout), or a note describing where the UI is bundled.
    ui: String,
    /// Who published the listing.
    publisher: String,
    /// REQUIRED declared capability manifest — what the app asks the host
    /// to grant. The UI renders this prominently next to Install.
    capabilities: CapabilityManifest,
    /// The mechanical import audit: the `world root` block of
    /// `wasm-tools component wit <artifact>` — the component's ENTIRE
    /// import surface, proving the closed world (no sockets, no
    /// filesystem, no wasi:http; outbound reach only via tangram:app/host
    /// behind the host's allow_hosts gate).
    import_audit: String,
}

/// The declared capability manifest of a listing. This is what the install
/// will GRANT (the registry passes it through to the host spec), so the
/// marketplace UI shows it before the user clicks Install.
#[model]
pub struct CapabilityManifest {
    /// Outbound hosts the app needs (exact host names). Empty = the app
    /// runs with no outbound network at all.
    allow_hosts: Vec<String>,
    /// Environment variable KEYS the app reads (e.g. API keys). The values
    /// stay on the host: installs pass `${KEY}` so the host expands them
    /// from its own .env — secrets never enter a replicated document.
    env_keys: Vec<String>,
    /// Where the app's data lives and what it contains, for humans.
    data_note: String,
}

fn validate_sha256(digest: &str) -> Result<(), String> {
    let digest = digest.trim();
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "component_sha256 must be 64 hex characters (a sha-256 digest), got {digest:?}"
        ));
    }
    Ok(())
}

/// Same rule as the host/registry: the name becomes a path prefix.
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

#[actions]
impl Marketplace {
    /// List every app in the catalog: name, version, publisher, artifact
    /// URL + pinned sha-256, the declared capability manifest (outbound
    /// hosts, env keys, data note), and the mechanical import audit.
    /// Install any of them through the local registry's `install_app` with
    /// the listing's component_url + component_sha256 and the manifest's
    /// grants.
    pub fn list_listings(&self) -> Vec<Listing> {
        self.listings.clone()
    }

    /// Add a listing to the catalog, or replace the listing with the same
    /// name. The capability manifest is REQUIRED: declare every outbound
    /// host and env key the app needs — installs grant exactly that, and
    /// users see it before installing. `import_audit` should be the
    /// `world root` block of `wasm-tools component wit <artifact>`.
    /// (Third-party submissions with automated manifest⊆imports
    /// verification are a recorded TODO; today this action is the curated
    /// path, gated by the host's bearer token via require_auth.)
    #[allow(clippy::too_many_arguments)]
    pub fn add_listing(
        &mut self,
        name: String,
        description: String,
        version: String,
        component_url: String,
        component_sha256: String,
        ui: String,
        publisher: String,
        capabilities: CapabilityManifest,
        import_audit: String,
    ) -> Result<(), String> {
        validate_name(&name)?;
        if !component_url.starts_with("https://") && !component_url.starts_with("http://") {
            return Err(format!(
                "component_url must be http(s), got {component_url:?}"
            ));
        }
        validate_sha256(&component_sha256)?;
        for (field, value) in [
            ("version", &version),
            ("ui", &ui),
            ("publisher", &publisher),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{field} must be non-empty"));
            }
        }
        if capabilities.env_keys.iter().any(|k| k.trim().is_empty())
            || capabilities.allow_hosts.iter().any(|h| h.trim().is_empty())
        {
            return Err("capability manifest entries must be non-empty".into());
        }
        let listing = Listing {
            name: name.clone(),
            description,
            version,
            component_url,
            component_sha256: component_sha256.trim().to_ascii_lowercase(),
            ui,
            publisher,
            capabilities,
            import_audit,
        };
        match self.listings.iter_mut().find(|l| l.name == name) {
            Some(existing) => *existing = listing,
            None => self.listings.push(listing),
        }
        Ok(())
    }

    /// Remove a listing from the catalog by name. Already-installed apps
    /// are not affected — the catalog only describes what CAN be installed.
    pub fn remove_listing(&mut self, name: String) -> Result<(), String> {
        let before = self.listings.len();
        self.listings.retain(|l| l.name != name);
        if self.listings.len() == before {
            return Err(format!("no listing named {name:?}"));
        }
        Ok(())
    }
}

/// Release artifact URL for a seeded first-party app. Seeds document the
/// self-hosting pattern: a GitHub release publishing the exact
/// `target/wasm32-wasip2/release/<app>.wasm` bytes the seed digests pin
/// (any static file server works the same — point `component_url` at it).
fn release_url(app: &str) -> String {
    format!("https://github.com/aaronbuchwald/tangram/releases/download/v0.1.0/{app}.wasm")
}

/// The seed catalog: the three first-party apps, with REAL sha-256 digests
/// of the builds at commit time and their mechanical import audits — both
/// generated by `apps/marketplace/seed/refresh.sh` and refreshed per
/// release (the genesis document must stay deterministic, so the seed is
/// data checked into the repo, not computed at runtime).
impl Default for Marketplace {
    fn default() -> Self {
        let manifest =
            |allow_hosts: &[&str], env_keys: &[&str], data_note: &str| CapabilityManifest {
                allow_hosts: allow_hosts.iter().map(ToString::to_string).collect(),
                env_keys: env_keys.iter().map(ToString::to_string).collect(),
                data_note: data_note.to_string(),
            };
        Self {
            listings: vec![
                Listing {
                    name: "notes".into(),
                    description: "A shared, replicated notes list — the smallest possible \
                                  Tangram app."
                        .into(),
                    version: "0.1.0".into(),
                    component_url: release_url("notes"),
                    component_sha256: include_str!("../seed/notes.sha256").trim().into(),
                    ui: "apps/notes/ui".into(),
                    publisher: "tangram (first-party)".into(),
                    capabilities: manifest(
                        &[],
                        &[],
                        "One automerge document of notes under $HOME/.notes, \
                         touched only by the host.",
                    ),
                    import_audit: include_str!("../seed/notes.wit").trim().into(),
                },
                Listing {
                    name: "nutrition".into(),
                    description: "A replicated nutrition tracker with pluggable meal \
                                  resolution (offline table or CalorieNinjas lookup)."
                        .into(),
                    version: "0.1.0".into(),
                    component_url: release_url("nutrition"),
                    component_sha256: include_str!("../seed/nutrition.sha256").trim().into(),
                    ui: "apps/nutrition/ui".into(),
                    publisher: "tangram (first-party)".into(),
                    capabilities: manifest(
                        &["api.calorieninjas.com"],
                        &["NUTRITION_STRATEGY", "CALORIENINJAS_API_KEY"],
                        "One automerge document of meals under $HOME/.nutrition, \
                         touched only by the host. Meal descriptions are sent to \
                         api.calorieninjas.com when that strategy is enabled.",
                    ),
                    import_audit: include_str!("../seed/nutrition.wit").trim().into(),
                },
                Listing {
                    name: "registry".into(),
                    description: "The fleet's source of truth: a replicated list of app \
                                  specs the host converges on. Usually bootstrapped from \
                                  apps.toml rather than installed."
                        .into(),
                    version: "0.1.0".into(),
                    component_url: release_url("registry"),
                    component_sha256: include_str!("../seed/registry.sha256").trim().into(),
                    ui: "apps/registry/ui".into(),
                    publisher: "tangram (first-party)".into(),
                    capabilities: manifest(
                        &[],
                        &[],
                        "One automerge document of app specs under $HOME/.registry, \
                         touched only by the host. Its mutating actions are gated by \
                         TANGRAM_AUTH_TOKEN.",
                    ),
                    import_audit: include_str!("../seed/registry.wit").trim().into(),
                },
            ],
        }
    }
}

/// MCP instructions, shared between the native app builder and the WASM
/// component's `describe()` export.
const INSTRUCTIONS: &str = "The Tangram app marketplace: a replicated catalog of installable apps. \
     Every listing pins its artifact by component_url + component_sha256 \
     (the host verifies the hash before running anything) and declares a \
     capability manifest (outbound hosts, env keys, data note) shown next \
     to the mechanical import audit. To install a listing, call the LOCAL \
     registry app's install_app with the listing's component_url, \
     component_sha256, ui, the manifest's allow_hosts, and env entries \
     of the form {key: K, value: \"${K}\"} for each declared env key. \
     add_listing/remove_listing curate the catalog and require the host's \
     bearer token.";

/// The marketplace app, fully configured. Call `.serve()` to run it
/// standalone or `.build()` to mount it in a multi-app host.
#[cfg(not(target_family = "wasm"))]
pub fn app() -> App<Marketplace> {
    App::<Marketplace>::new("marketplace")
        .instructions(INSTRUCTIONS)
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
}

// Compiled for wasm32-wasip2, the same model + actions become a Tangram
// component (`tangram-host` owns the platform around it).
#[cfg(target_family = "wasm")]
tangram::export_component!(Marketplace {
    name: "marketplace",
    instructions: INSTRUCTIONS,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn listing_args() -> (String, CapabilityManifest) {
        (
            "a".repeat(64),
            CapabilityManifest {
                allow_hosts: vec!["api.example.com".into()],
                env_keys: vec!["EXAMPLE_KEY".into()],
                data_note: "one doc".into(),
            },
        )
    }

    #[test]
    fn default_seeds_the_first_party_apps_with_real_digests() {
        let market = Marketplace::default();
        let listings = market.list_listings();
        let names: Vec<&str> = listings.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["notes", "nutrition", "registry"]);
        for listing in &listings {
            assert_eq!(listing.component_sha256.len(), 64, "{}", listing.name);
            assert!(
                listing
                    .component_sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit())
            );
            assert!(listing.component_url.starts_with("https://"));
            // The import audit is the mechanical closed-world proof: the
            // only non-wasi-std import is the tangram host interface.
            assert!(listing.import_audit.starts_with("world root {"));
            assert!(listing.import_audit.contains("import tangram:app/host;"));
            assert!(!listing.import_audit.contains("wasi:sockets"));
            assert!(!listing.import_audit.contains("wasi:http"));
        }
        let nutrition = &listings[1];
        assert_eq!(
            nutrition.capabilities.allow_hosts,
            vec!["api.calorieninjas.com".to_string()]
        );
        assert!(
            nutrition
                .capabilities
                .env_keys
                .contains(&"CALORIENINJAS_API_KEY".to_string())
        );
    }

    #[test]
    fn add_listing_validates_and_replaces_by_name() {
        let mut market = Marketplace::default();
        let (sha, caps) = listing_args();
        market
            .add_listing(
                "extra".into(),
                "desc".into(),
                "1.0.0".into(),
                "https://example.test/extra.wasm".into(),
                sha.to_ascii_uppercase(), // canonicalized to lowercase
                "apps/extra/ui".into(),
                "someone".into(),
                caps.clone(),
                "world root {}".into(),
            )
            .expect("add");
        assert_eq!(market.list_listings().len(), 4);
        let added = market
            .list_listings()
            .into_iter()
            .find(|l| l.name == "extra")
            .unwrap();
        assert_eq!(added.component_sha256, sha);

        // Replace by name.
        market
            .add_listing(
                "extra".into(),
                "desc2".into(),
                "1.0.1".into(),
                "https://example.test/extra2.wasm".into(),
                sha.clone(),
                "apps/extra/ui".into(),
                "someone".into(),
                caps.clone(),
                "world root {}".into(),
            )
            .expect("replace");
        assert_eq!(market.list_listings().len(), 4);

        // Rejections: bad name, bad scheme, bad sha, empty fields, empty
        // manifest entries.
        let add = |market: &mut Marketplace, name: &str, url: &str, sha: &str, version: &str| {
            market.add_listing(
                name.into(),
                "d".into(),
                version.into(),
                url.into(),
                sha.into(),
                "ui".into(),
                "p".into(),
                caps.clone(),
                String::new(),
            )
        };
        assert!(add(&mut market, "bad name", "https://x.test/a.wasm", &sha, "1").is_err());
        assert!(add(&mut market, "x", "ftp://x.test/a.wasm", &sha, "1").is_err());
        assert!(add(&mut market, "x", "https://x.test/a.wasm", "feed", "1").is_err());
        assert!(add(&mut market, "x", "https://x.test/a.wasm", &sha, " ").is_err());
        let mut bad_caps = caps.clone();
        bad_caps.allow_hosts.push("  ".into());
        assert!(
            market
                .add_listing(
                    "x".into(),
                    "d".into(),
                    "1".into(),
                    "https://x.test/a.wasm".into(),
                    sha.clone(),
                    "ui".into(),
                    "p".into(),
                    bad_caps,
                    String::new(),
                )
                .is_err()
        );
    }

    #[test]
    fn remove_listing_by_name() {
        let mut market = Marketplace::default();
        market.remove_listing("notes".into()).expect("remove");
        assert_eq!(market.list_listings().len(), 2);
        assert!(market.remove_listing("notes".into()).is_err());
    }
}
