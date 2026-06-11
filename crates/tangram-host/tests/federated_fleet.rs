//! End-to-end Phase 9 test: federated fleet state — install/remove on ANY
//! tangram-host propagates to all of them (FULL-PROPAGATION, owner-approved).
//!
//! The mechanism, dogfooded: the registry app is already a replicated CRDT
//! whose document IS the fleet's desired state, and the host already
//! converges its running set from that document. So "install on one → runs
//! on all" is just making the registry DOCUMENT sync between hosts (a
//! registry app spec carries `remote`, the host dials it out, and a synced
//! registry-doc change re-triggers converge) plus Phase 8's url+hash, which
//! makes a synced entry portable (a fetch-and-verify-anywhere artifact, not
//! a host-local path).
//!
//! Two hosts on 19xxx: A hosts a registry, B's registry dials A's
//! `/registry/sync`. The walk:
//!   1. install_app with url+hash on A → within seconds B fetches, verifies,
//!      and runs it (/<app>/healthz 200 on BOTH); propagation is latency-
//!      bounded and reported.
//!   2. remove on B → gone on A (full bidirectional propagation).
//!   3. both restart → fleet restored from the persisted, synced registry
//!      docs (no apps.toml entry for the installed app on either host).
//!   4. a path-only entry on A → A runs it; B reports a clear PORTABILITY
//!      fleet error for it but keeps its other apps healthy, and never
//!      mutates the shared doc (anti-flap: the doc is desired state; runtime
//!      failures live only in /api/fleet).
//!   5. a `${SECRET}` referenced but unset on B → B runs that app DEGRADED
//!      (nutrition → offline) without leaking the secret and without crashing
//!      converge; A (with the secret) runs it fully.
//!
//! Requires the wasm components (built by CI before `cargo test`):
//!   cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
//!     --lib --target wasm32-wasip2 --release
//! The test SKIPS (with a notice) when they are missing, so a plain
//! `cargo test` without the wasm target still passes.

use std::collections::HashMap;
use std::future::IntoFuture as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

mod support;
use support::{HostProc, component, free_port, status_of, wait_for, workspace_root};

/// One shared owner token across the whole fleet — both hosts gate their
/// registries with it, so the owner can install/remove on either.
const TOKEN: &str = "test-federated-token";

fn sha256_of(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(std::fs::read(path).expect("artifact"))
    )
}

/// Spawn a host against a scratch HOME. `extra_env` injects per-host
/// environment (e.g. a secret present on A but not on B).
fn spawn_host(
    home: &Path,
    apps_toml: &Path,
    bind: &str,
    log: &Path,
    extra_env: &[(&str, &str)],
) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tangram-host"));
    cmd.arg(apps_toml)
        // cwd = scratch HOME so the repo's .env is NOT loaded by dotenvy
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("TANGRAM_AUTH_TOKEN", TOKEN)
        .env("RUST_LOG", "info")
        .env_remove("TANGRAM_DATA_DIR")
        // Make sure no ambient CALORIENINJAS_API_KEY leaks in from the runner.
        .env_remove("CALORIENINJAS_API_KEY")
        .env_remove("NUTRITION_STRATEGY")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    HostProc(cmd.spawn().expect("spawn tangram-host"))
}

async fn healthz(client: &reqwest::Client, base: &str, app: &str) -> Option<reqwest::StatusCode> {
    status_of(client, &format!("{base}/{app}/healthz")).await
}

/// An app's `/api/state` as raw text (None if the app isn't serving yet).
async fn state_text(client: &reqwest::Client, base: &str, app: &str) -> Option<String> {
    let res = client
        .get(format!("{base}/{app}/api/state"))
        .send()
        .await
        .ok()?;
    res.error_for_status().ok()?.text().await.ok()
}

async fn fleet_json(client: &reqwest::Client, base: &str) -> Option<serde_json::Value> {
    client
        .get(format!("{base}/api/fleet"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}

/// The fleet error string for `app`, if any (`null`/absent → None).
async fn fleet_error(client: &reqwest::Client, base: &str, app: &str) -> Option<String> {
    let fleet = fleet_json(client, base).await?;
    fleet["apps"].as_array()?.iter().find_map(|a| {
        (a["name"] == app)
            .then(|| a["error"].as_str().map(str::to_string))
            .flatten()
    })
}

/// A scratch artifact server: serves the built components at
/// `/artifacts/<name>.wasm`, counting hits so url+hash installs are real
/// fetch-and-verify (not shared-fs reads), and proving the cache is keyed by
/// digest across the fleet.
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

/// `install_app` on a host's registry (authed). Returns the HTTP status.
async fn install(
    client: &reqwest::Client,
    base: &str,
    args: &serde_json::Value,
) -> reqwest::StatusCode {
    client
        .post(format!("{base}/registry/api/actions/install_app"))
        .bearer_auth(TOKEN)
        .json(args)
        .send()
        .await
        .expect("install_app")
        .status()
}

#[tokio::test]
async fn federated_fleet_propagates_installs_removes_and_persists() {
    for name in ["registry", "notes", "nutrition"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING federated_fleet: {} missing — build the wasm components first \
                 (cargo build -p tangram-notes -p tangram-nutrition -p tangram-registry \
                 --lib --target wasm32-wasip2 --release)",
                component(name).display()
            );
            return;
        }
    }

    let root = workspace_root();
    let scratch = tempfile::tempdir().expect("tempdir");
    let home_a = scratch.path().join("a");
    let home_b = scratch.path().join("b");
    std::fs::create_dir_all(&home_a).unwrap();
    std::fs::create_dir_all(&home_b).unwrap();

    let artifacts = ArtifactServer::serve(&["notes", "nutrition"]).await;
    let port_a = free_port();
    let port_b = free_port();
    let base_a = format!("http://127.0.0.1:{port_a}");
    let base_b = format!("http://127.0.0.1:{port_b}");

    // Host A: the registry is the source of truth (no remote — it is dialed).
    let toml_a = home_a.join("apps.toml");
    std::fs::write(
        &toml_a,
        format!(
            "[apps.registry]\ncomponent = \"{reg}\"\nui = \"{root}/apps/registry/ui\"\n\
             registry = true\n",
            reg = component("registry").display(),
            root = root.display(),
        ),
    )
    .unwrap();

    // Host B: same registry, but its document FEDERATES — it dials A's
    // /registry/sync, so A's desired state flows into B and back.
    let toml_b = home_b.join("apps.toml");
    std::fs::write(
        &toml_b,
        format!(
            "[apps.registry]\ncomponent = \"{reg}\"\nui = \"{root}/apps/registry/ui\"\n\
             registry = true\nremote = \"{base_a}/registry/sync\"\n",
            reg = component("registry").display(),
            root = root.display(),
        ),
    )
    .unwrap();

    // A's secret is present; B's is deliberately absent (Phase 9 §3).
    let host_a = spawn_host(
        &home_a,
        &toml_a,
        &format!("127.0.0.1:{port_a}"),
        &home_a.join("host.log"),
        &[("CALORIENINJAS_API_KEY", "secret-key-only-on-A")],
    );
    let host_b = spawn_host(
        &home_b,
        &toml_b,
        &format!("127.0.0.1:{port_b}"),
        &home_b.join("host.log"),
        &[],
    );
    let client = reqwest::Client::new();

    // Both registries come up from their apps.toml.
    for base in [&base_a, &base_b] {
        wait_for(
            &format!("registry up at {base}"),
            Duration::from_secs(120),
            || async { healthz(&client, base, "registry").await == Some(reqwest::StatusCode::OK) },
        )
        .await;
    }

    // ── 1. install url+hash on A → propagates to B (fetch + verify + run) ───
    let notes_sha = sha256_of(&component("notes"));
    let notes_args = serde_json::json!({
        "name": "notes",
        "component_url": artifacts.url("notes"),
        "component_sha256": notes_sha,
        "ui": root.join("apps/notes/ui").display().to_string(),
    });
    let installed_at = Instant::now();
    assert_eq!(
        install(&client, &base_a, &notes_args).await,
        reqwest::StatusCode::OK
    );

    // A runs it (its own converge).
    wait_for("notes healthy on A", Duration::from_secs(120), || async {
        healthz(&client, &base_a, "notes").await == Some(reqwest::StatusCode::OK)
    })
    .await;
    // …and B runs it too — purely from the synced registry document.
    wait_for("notes healthy on B", Duration::from_secs(120), || async {
        healthz(&client, &base_b, "notes").await == Some(reqwest::StatusCode::OK)
    })
    .await;
    let propagation = installed_at.elapsed();
    println!("PROPAGATION: install-on-A → /notes/ healthy on BOTH in {propagation:?}");

    // The artifact was fetched and verified on each host independently (the
    // url+hash is what made the synced entry portable). Hit count proves it
    // was a real network fetch, not a shared-fs path read.
    assert_eq!(
        artifacts.hits("notes"),
        2,
        "A and B each fetched+verified once"
    );

    // B reports it as a registry-sourced, healthy app with no error.
    let err_b = fleet_error(&client, &base_b, "notes").await;
    assert_eq!(err_b, None, "B has no error for the propagated app");

    // DATA propagation, not just desired state: a federated registry derives
    // each installed app's own sync remote (<base>/<app>/sync), so the app's
    // DOCUMENT replicates with the same peer. A note written on A appears on
    // B (the derived remote, from one `remote` setting on B's registry).
    let res = client
        .post(format!("{base_a}/notes/api/actions/add_note"))
        .json(&serde_json::json!({ "text": "written on A" }))
        .send()
        .await
        .expect("add_note on A");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    wait_for(
        "note 'written on A' shows up on B",
        Duration::from_secs(120),
        || async {
            state_text(&client, &base_b, "notes")
                .await
                .is_some_and(|t| t.contains("written on A"))
        },
    )
    .await;
    println!("DATA: a note written on A replicated to B via the derived per-app remote");

    // The installed app also works on B (dispatch through the component); this
    // write rides back to A through the same derived remote.
    let res = client
        .post(format!("{base_b}/notes/api/actions/add_note"))
        .json(&serde_json::json!({ "text": "written on B" }))
        .send()
        .await
        .expect("add_note on B");
    assert_eq!(res.status(), reqwest::StatusCode::OK);

    // ── 2. remove on B → gone on A (bidirectional propagation) ──────────────
    let res = client
        .post(format!("{base_b}/registry/api/actions/remove_app"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "name": "notes" }))
        .send()
        .await
        .expect("remove on B");
    assert_eq!(res.status(), reqwest::StatusCode::OK);
    wait_for("notes gone on A", Duration::from_secs(120), || async {
        healthz(&client, &base_a, "notes").await == Some(reqwest::StatusCode::NOT_FOUND)
    })
    .await;
    wait_for("notes gone on B", Duration::from_secs(120), || async {
        healthz(&client, &base_b, "notes").await == Some(reqwest::StatusCode::NOT_FOUND)
    })
    .await;
    println!("PROPAGATION: remove-on-B → /notes/ gone on A");

    // Reinstall notes (it is the fleet member we persistence-check below).
    assert_eq!(
        install(&client, &base_a, &notes_args).await,
        reqwest::StatusCode::OK
    );
    wait_for("notes back on B", Duration::from_secs(120), || async {
        healthz(&client, &base_b, "notes").await == Some(reqwest::StatusCode::OK)
    })
    .await;

    // ── 5. a ${SECRET} unset on B → degraded, no leak, no converge crash ────
    // Install nutrition with CALORIENINJAS_API_KEY = ${CALORIENINJAS_API_KEY}
    // and NO explicit NUTRITION_STRATEGY: the host expands the host var, so A
    // (secret present) auto-enables CalorieNinjas; B (secret absent) gets an
    // empty value → offline, healthy but degraded.
    let nutr_sha = sha256_of(&component("nutrition"));
    let nutr_args = serde_json::json!({
        "name": "nutrition",
        "component_url": artifacts.url("nutrition"),
        "component_sha256": nutr_sha,
        "ui": root.join("apps/nutrition/ui").display().to_string(),
        "allow_hosts": ["api.calorieninjas.com"],
        "env": [{ "key": "CALORIENINJAS_API_KEY", "value": "${CALORIENINJAS_API_KEY}" }],
    });
    assert_eq!(
        install(&client, &base_a, &nutr_args).await,
        reqwest::StatusCode::OK
    );
    for base in [&base_a, &base_b] {
        wait_for(
            &format!("nutrition healthy at {base}"),
            Duration::from_secs(120),
            || async { healthz(&client, base, "nutrition").await == Some(reqwest::StatusCode::OK) },
        )
        .await;
    }

    // A resolved the secret → it can resolve descriptions; B did not →
    // offline (degraded), but both are healthy. Convergence never crashed on
    // B (its registry stays up and nutrition runs).
    let caps_a: serde_json::Value = client
        .get(format!("{base_a}/nutrition/api/capabilities"))
        .send()
        .await
        .expect("caps A")
        .json()
        .await
        .expect("caps A json");
    let caps_b: serde_json::Value = client
        .get(format!("{base_b}/nutrition/api/capabilities"))
        .send()
        .await
        .expect("caps B")
        .json()
        .await
        .expect("caps B json");
    assert_eq!(caps_a["description_input"], true, "A has the key → online");
    assert_eq!(
        caps_b["description_input"], false,
        "B lacks the secret → degraded to offline, NOT crashed"
    );
    assert_eq!(
        fleet_error(&client, &base_b, "nutrition").await,
        None,
        "B's nutrition is healthy/degraded, not errored"
    );

    // The secret value NEVER leaks onto B: it is nowhere in B's registry doc
    // (only the ${VAR} reference replicates) nor in B's fleet/state surfaces.
    let reg_state_b = client
        .get(format!("{base_b}/registry/api/state"))
        .send()
        .await
        .expect("registry state B")
        .text()
        .await
        .expect("registry state B text");
    assert!(
        reg_state_b.contains("${CALORIENINJAS_API_KEY}"),
        "the ${{VAR}} reference (not the value) is what replicates"
    );
    for surface in ["registry/api/state", "nutrition/api/state", "api/fleet"] {
        let body = client
            .get(format!("{base_b}/{surface}"))
            .send()
            .await
            .expect("surface")
            .text()
            .await
            .expect("surface text");
        assert!(
            !body.contains("secret-key-only-on-A"),
            "the secret value must never appear on B ({surface})"
        );
    }
    println!("SECRETS: ${{VAR}} unset on B → nutrition degraded (offline), no leak, no crash");

    // ── 4. path-only entry on A → A runs it; B fleet-errors (portability),
    //    keeps its other apps healthy, and never mutates the shared doc ──────
    // A path-only entry in a FEDERATED registry is host-local: the path is
    // meaningful only on the host that wrote it. On a single test box, the
    // faithful cross-host reality (B's filesystem lacks A's path) is a path
    // that does not exist on the fleet at all. We install a "ghost" entry at
    // such a path: the federated peer that lacks it reports the PORTABILITY
    // error (not a bare file-not-found), keeps converging everything else,
    // and never writes the runtime failure back to the shared doc.
    let missing_args = serde_json::json!({
        "name": "ghost",
        "component": home_a.join("nope/does-not-exist.wasm").display().to_string(),
        "ui": root.join("apps/notes/ui").display().to_string(),
    });
    assert_eq!(
        install(&client, &base_a, &missing_args).await,
        reqwest::StatusCode::OK
    );

    // B reports a PORTABILITY fleet error for the missing path-only entry…
    wait_for(
        "B portability error for ghost",
        Duration::from_secs(120),
        || async {
            fleet_error(&client, &base_b, "ghost")
                .await
                .is_some_and(|e| e.contains("not portable") || e.contains("does not exist"))
        },
    )
    .await;
    let ghost_err = fleet_error(&client, &base_b, "ghost").await.unwrap();
    assert!(
        ghost_err.contains("portable"),
        "B's ghost error names portability: {ghost_err}"
    );
    assert_eq!(
        healthz(&client, &base_b, "ghost").await,
        Some(reqwest::StatusCode::NOT_FOUND),
        "the non-portable entry does not run on B"
    );
    println!("PORTABILITY: path-only entry → B fleet error: {ghost_err}");

    // …but B keeps everything else healthy (one bad entry never thrashes).
    assert_eq!(
        healthz(&client, &base_b, "registry").await,
        Some(reqwest::StatusCode::OK)
    );
    assert_eq!(
        healthz(&client, &base_b, "notes").await,
        Some(reqwest::StatusCode::OK)
    );
    assert_eq!(
        healthz(&client, &base_b, "nutrition").await,
        Some(reqwest::StatusCode::OK)
    );

    // Anti-flap: B never wrote a runtime failure back into the shared desired
    // state. A's registry doc still lists ghost (desired), and B's view of it
    // (synced) matches — the doc is unchanged by B's local failure. We assert
    // both registries still LIST ghost (desired state intact) and that B's
    // ghost entry is byte-identical to A's, proving no write-back oscillation.
    let list = |base: &str| {
        let client = client.clone();
        let base = base.to_string();
        async move {
            client
                .get(format!("{base}/registry/api/state"))
                .send()
                .await
                .expect("list")
                .json::<serde_json::Value>()
                .await
                .expect("list json")
        }
    };
    let ghost_in = |state: &serde_json::Value| {
        state["apps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "ghost")
    };
    assert!(
        ghost_in(&list(&base_a).await),
        "A's desired state still lists ghost"
    );
    assert!(
        ghost_in(&list(&base_b).await),
        "B's synced desired state still lists ghost — runtime failure was NOT written back"
    );
    // The shared document converges identically on both hosts: B's local
    // runtime failure produced no write-back, so the two views are equal
    // (give the CRDT a moment to settle — neither side is mutating it).
    wait_for(
        "registry docs identical on A and B",
        Duration::from_secs(15),
        || async { list(&base_a).await["apps"] == list(&base_b).await["apps"] },
    )
    .await;

    // ── 3. both restart → fleet restored from the persisted, synced docs ────
    // Neither apps.toml mentions notes/nutrition/ghost — persistence is the
    // replicated registry document on disk.
    for toml in [&toml_a, &toml_b] {
        let text = std::fs::read_to_string(toml).unwrap();
        for app in ["notes", "nutrition", "ghost"] {
            assert!(
                !text.contains(app),
                "{app} must NOT be in {} — persistence is the registry doc",
                toml.display()
            );
        }
    }
    drop(host_a);
    drop(host_b);
    let host_a = spawn_host(
        &home_a,
        &toml_a,
        &format!("127.0.0.1:{port_a}"),
        &home_a.join("host-2.log"),
        &[("CALORIENINJAS_API_KEY", "secret-key-only-on-A")],
    );
    let host_b = spawn_host(
        &home_b,
        &toml_b,
        &format!("127.0.0.1:{port_b}"),
        &home_b.join("host-2.log"),
        &[],
    );

    // Both hosts bring the installed apps back from their docs.
    for base in [&base_a, &base_b] {
        for app in ["notes", "nutrition"] {
            wait_for(
                &format!("{app} healthy at {base} after restart"),
                Duration::from_secs(120),
                || async { healthz(&client, base, app).await == Some(reqwest::StatusCode::OK) },
            )
            .await;
        }
    }
    // The note written on B before the restart survived in the document.
    let notes_state: serde_json::Value = client
        .get(format!("{base_b}/notes/api/state"))
        .send()
        .await
        .expect("notes state B")
        .json()
        .await
        .expect("notes state json");
    assert!(
        notes_state["notes"]
            .as_array()
            .is_some_and(|n| n.iter().any(|x| x["text"] == "written on B")),
        "the note written on B survived the restart via the synced doc"
    );
    println!("PERSISTENCE: both hosts restarted → fleet restored from synced registry docs");

    // Keep the guards alive until the end.
    drop(host_a);
    drop(host_b);
}
