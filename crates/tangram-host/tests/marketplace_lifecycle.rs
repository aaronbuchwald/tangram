//! End-to-end Phase 8 test: marketplace → registry → host install-from-URL
//! with sha-256 verification.
//!
//! Spawns the real `tangram-host` binary against a scratch HOME (bootstrap
//! `apps.toml`: registry + marketplace) plus an in-test artifact server with
//! per-path hit counters, then walks the flow: the marketplace serves its
//! seeded catalog + UI; `add_listing` is bearer-gated; the listing's pinned
//! url+sha256 installs notes through the registry exactly as the UI does
//! (verified download → healthy app); a WRONG sha-256 is rejected — clear
//! fleet error, app not running, nothing cached; and the immutable
//! content-addressed cache means converge ticks AND a full host restart
//! never refetch (the hit counter stays at one).
//!
//! Requires the wasm components (built by CI before `cargo test`):
//!   cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
//!     -p tangram-marketplace --lib --target wasm32-wasip2 --release
//! The test SKIPS (with a notice) when they are missing, so a plain
//! `cargo test` without the wasm target still passes.

use std::collections::HashMap;
use std::future::IntoFuture as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

const TOKEN: &str = "test-market-token";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn component(name: &str) -> PathBuf {
    workspace_root().join(format!("target/wasm32-wasip2/release/{name}.wasm"))
}

fn sha256_of(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(std::fs::read(path).expect("artifact"))
    )
}

/// The spawned host, killed on drop so a failing test never leaks a server.
struct HostProc(Child);

impl Drop for HostProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let child = Command::new(env!("CARGO_BIN_EXE_tangram-host"))
        .arg(apps_toml)
        // cwd = scratch HOME so the repo's .env is NOT loaded by dotenvy
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("TANGRAM_AUTH_TOKEN", TOKEN)
        .env("RUST_LOG", "info")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn tangram-host");
    HostProc(child)
}

async fn wait_for<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn status_of(client: &reqwest::Client, url: &str) -> Option<reqwest::StatusCode> {
    client.get(url).send().await.ok().map(|r| r.status())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A scratch artifact server: serves the built components at
/// `/artifacts/<name>.wasm` and counts hits per artifact, so the test can
/// PROVE the host's content-addressed cache never refetches.
struct ArtifactServer {
    base: String,
    hits: Arc<HashMap<String, AtomicUsize>>,
}

impl ArtifactServer {
    async fn serve(artifacts: &[&str]) -> Self {
        let hits: Arc<HashMap<String, AtomicUsize>> = Arc::new(
            artifacts
                .iter()
                .map(|name| (name.to_string(), AtomicUsize::new(0)))
                .collect(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind artifact server");
        let addr = listener.local_addr().expect("artifact addr");
        let router = axum::Router::new().route(
            "/artifacts/{name}",
            axum::routing::get({
                let hits = hits.clone();
                move |axum::extract::Path(name): axum::extract::Path<String>| {
                    let hits = hits.clone();
                    async move {
                        match hits.get(name.trim_end_matches(".wasm")) {
                            Some(counter) => {
                                counter.fetch_add(1, Ordering::SeqCst);
                                let bytes =
                                    std::fs::read(component(name.trim_end_matches(".wasm")))
                                        .expect("read artifact");
                                Ok(bytes)
                            }
                            None => Err(axum::http::StatusCode::NOT_FOUND),
                        }
                    }
                }
            }),
        );
        tokio::spawn(axum::serve(listener, router).into_future());
        Self {
            base: format!("http://{addr}/artifacts"),
            hits,
        }
    }

    fn url(&self, name: &str) -> String {
        format!("{}/{name}.wasm", self.base)
    }

    fn hits(&self, name: &str) -> usize {
        self.hits[name].load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn marketplace_to_registry_install_by_url_with_hash_verification() {
    for name in ["registry", "marketplace", "notes", "nutrition"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING marketplace_lifecycle: {} missing — build the wasm components \
                 first (cargo build -p tangram-notes -p tangram-nutrition \
                 -p tangram-registry -p tangram-marketplace --lib \
                 --target wasm32-wasip2 --release)",
                component(name).display()
            );
            return;
        }
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
[apps.registry]
component = "{registry}"
ui = "{root}/apps/registry/ui"
registry = true

[apps.marketplace]
component = "{marketplace}"
ui = "{root}/apps/marketplace/ui"
require_auth = true
"#,
            registry = component("registry").display(),
            marketplace = component("marketplace").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");

    let artifacts = ArtifactServer::serve(&["notes", "nutrition"]).await;
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host-1.log");
    let host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    // Bootstrap apps come up from apps.toml.
    for app in ["registry", "marketplace"] {
        wait_for(
            &format!("{app} healthz"),
            Duration::from_secs(120),
            || async {
                status_of(&client, &format!("{base}/{app}/healthz")).await
                    == Some(reqwest::StatusCode::OK)
            },
        )
        .await;
    }

    // ── the marketplace serves its UI and the seeded catalog ───────────────
    let ui = client
        .get(format!("{base}/marketplace/"))
        .send()
        .await
        .expect("marketplace ui");
    assert_eq!(ui.status(), reqwest::StatusCode::OK);
    let ui_html = ui.text().await.expect("ui body");
    assert!(ui_html.contains("Marketplace"), "marketplace UI served");
    assert!(
        ui_html.contains("third-party submissions: not built"),
        "the TODO marker is visible in the UI footer"
    );

    let state: serde_json::Value = client
        .get(format!("{base}/marketplace/api/state"))
        .send()
        .await
        .expect("marketplace state")
        .json()
        .await
        .expect("state json");
    let listings = state["listings"].as_array().expect("listings array");
    let seed_names: Vec<&str> = listings
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    assert_eq!(seed_names, ["notes", "nutrition", "registry"]);
    for listing in listings {
        assert!(
            listing["import_audit"]
                .as_str()
                .unwrap()
                .starts_with("world root {"),
            "seed listings carry the mechanical import audit"
        );
        assert!(listing["capabilities"].is_object(), "manifest is required");
    }
    assert_eq!(
        listings[1]["capabilities"]["allow_hosts"],
        serde_json::json!(["api.calorieninjas.com"]),
        "nutrition's manifest declares its outbound host"
    );

    // ── curation is bearer-gated (require_auth) ─────────────────────────────
    let notes_sha = sha256_of(&component("notes"));
    let listing_args = serde_json::json!({
        "name": "notes",
        "description": "notes, published on the test artifact server",
        "version": "0.1.0-test",
        "component_url": artifacts.url("notes"),
        "component_sha256": notes_sha,
        "ui": root.join("apps/notes/ui").display().to_string(),
        "publisher": "lifecycle test",
        "capabilities": {
            "allow_hosts": [],
            "env_keys": [],
            "data_note": "one automerge document under $HOME/.notes"
        },
        "import_audit": "world root { import tangram:app/host; }",
    });
    let res = client
        .post(format!("{base}/marketplace/api/actions/add_listing"))
        .json(&listing_args)
        .send()
        .await
        .expect("unauthed add_listing");
    assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
    let res = client
        .post(format!("{base}/marketplace/api/actions/add_listing"))
        .bearer_auth(TOKEN)
        .json(&listing_args)
        .send()
        .await
        .expect("authed add_listing");
    assert_eq!(res.status(), reqwest::StatusCode::OK);

    // ── marketplace → registry → host: install notes-by-URL exactly as the
    //    UI does — read the listing back, post its pinned url+sha and the
    //    manifest's grants to the local registry ─────────────────────────────
    let state: serde_json::Value = client
        .get(format!("{base}/marketplace/api/state"))
        .send()
        .await
        .expect("marketplace state")
        .json()
        .await
        .expect("state json");
    let listing = state["listings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["name"] == "notes")
        .expect("notes listing");
    assert_eq!(
        listing["component_url"].as_str().unwrap(),
        artifacts.url("notes")
    );
    assert_eq!(listing["component_sha256"].as_str().unwrap(), notes_sha);

    let installed_at = Instant::now();
    let res = client
        .post(format!("{base}/registry/api/actions/install_app"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({
            "name": listing["name"],
            "component_url": listing["component_url"],
            "component_sha256": listing["component_sha256"],
            "ui": listing["ui"],
            "allow_hosts": listing["capabilities"]["allow_hosts"],
            "env": [],
        }))
        .send()
        .await
        .expect("install by url");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    wait_for("notes healthy", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/notes/healthz")).await == Some(reqwest::StatusCode::OK)
    })
    .await;
    println!(
        "install_app (by URL) → /notes/ healthy in {:?}",
        installed_at.elapsed()
    );
    assert_eq!(artifacts.hits("notes"), 1, "exactly one verified fetch");

    // The installed app actually works, and the fleet reports it healthy.
    let res = client
        .post(format!("{base}/notes/api/actions/add_note"))
        .json(&serde_json::json!({ "text": "installed from the marketplace" }))
        .send()
        .await
        .expect("add_note");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    let fleet: serde_json::Value = client
        .get(format!("{base}/api/fleet"))
        .send()
        .await
        .expect("fleet")
        .json()
        .await
        .expect("fleet json");
    let notes = fleet["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "notes")
        .expect("notes in fleet");
    assert_eq!(notes["source"], "registry");
    assert_eq!(notes["running"], true);
    assert_eq!(notes["healthy"], true);
    assert_eq!(notes["error"], serde_json::Value::Null);

    // The artifact landed in the immutable content-addressed cache.
    let cache_slot = home
        .join(".tangram-host/components")
        .join(format!("{notes_sha}.wasm"));
    assert!(cache_slot.exists(), "cached at {}", cache_slot.display());

    // ── wrong sha-256 → clear fleet error, app NOT running, not cached ─────
    let wrong_sha = "0".repeat(64);
    let res = client
        .post(format!("{base}/registry/api/actions/install_app"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({
            "name": "badnotes",
            "component_url": artifacts.url("nutrition"),
            "component_sha256": wrong_sha,
            "ui": root.join("apps/nutrition/ui").display().to_string(),
        }))
        .send()
        .await
        .expect("install with wrong sha");
    assert_eq!(
        res.status(),
        reqwest::StatusCode::OK,
        "the spec is well-formed"
    );
    wait_for("badnotes fleet error", Duration::from_secs(15), || async {
        let Ok(res) = client.get(format!("{base}/api/fleet")).send().await else {
            return false;
        };
        let Ok(fleet) = res.json::<serde_json::Value>().await else {
            return false;
        };
        fleet["apps"].as_array().is_some_and(|apps| {
            apps.iter().any(|a| {
                a["name"] == "badnotes"
                    && a["error"]
                        .as_str()
                        .is_some_and(|e| e.contains("sha256 mismatch"))
            })
        })
    })
    .await;
    assert_eq!(
        status_of(&client, &format!("{base}/badnotes/healthz")).await,
        Some(reqwest::StatusCode::NOT_FOUND),
        "the unverified app must not run"
    );
    assert!(
        !home
            .join(".tangram-host/components")
            .join(format!("{wrong_sha}.wasm"))
            .exists(),
        "nothing unverified reaches the cache"
    );
    assert_eq!(
        artifacts.hits("nutrition"),
        1,
        "the mismatch was fetched once and is then remembered (backoff)"
    );

    // ── re-converge uses the cache: ride out several converge ticks, then
    //    force a converge via a config touch — still exactly one fetch ──────
    let toml_text = std::fs::read_to_string(&apps_toml).expect("read apps.toml");
    std::fs::write(&apps_toml, format!("{toml_text}\n# touched\n")).expect("touch apps.toml");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        status_of(&client, &format!("{base}/notes/healthz")).await,
        Some(reqwest::StatusCode::OK)
    );
    assert_eq!(artifacts.hits("notes"), 1, "re-converges never refetch");

    // ── restart: the registry doc brings notes back, FROM THE CACHE ────────
    drop(host); // kill + wait
    let log2 = home.join("host-2.log");
    let _host2 = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log2);
    wait_for(
        "notes healthy after restart",
        Duration::from_secs(120),
        || async {
            status_of(&client, &format!("{base}/notes/healthz")).await
                == Some(reqwest::StatusCode::OK)
        },
    )
    .await;
    assert_eq!(
        artifacts.hits("notes"),
        1,
        "the cache survives a host restart — still exactly one fetch ever"
    );
    let state: serde_json::Value = client
        .get(format!("{base}/notes/api/state"))
        .send()
        .await
        .expect("notes state")
        .json()
        .await
        .expect("state json");
    assert!(
        state["notes"].as_array().is_some_and(|notes| {
            notes
                .iter()
                .any(|n| n["text"] == "installed from the marketplace")
        }),
        "the note logged before the restart survived"
    );
}
