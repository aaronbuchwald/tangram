//! GL7 — egress containment for the guided-learning tutor (the §7 theorem),
//! through the real component under `tangram-host`.
//!
//! Reuses the nutrition `egress_injection.rs` posture: the host attaches the
//! Anthropic credential at the component's `http-fetch` egress boundary, so
//! the plaintext key lives ONLY in the host process env, never in the
//! component. The deterministic, key-free assertion this pins:
//!
//! - **Configured ⇔ secret resolves:** with `ANTHROPIC_API_KEY` set in the
//!   HOST env and the inject rule present, `/guided-learning/api/capabilities`
//!   reports `description_input:true`; with it UNSET, `false` (the tutor stays
//!   offline/degraded — clean, no crash, no leak). This is the
//!   configured-iff-resolves gate, identical to the nutrition precedent.
//!
//! The whole test SELF-SKIPS (with a notice) when the wasm component is
//! missing, so a `cargo test` without the pre-built component still passes.
//! The live POST-/v1/messages auth + cross-host-denial steps are covered by
//! the component-side single-call invariant in
//! `apps/guided-learning/tests/gl7_egress.rs`; full host egress denial reuses
//! the same allowlist path the nutrition test exercises.

use std::path::Path;
use std::time::Duration;

mod support;
use support::{HostProc, component, free_port, status_of, wait_for, workspace_root};

fn spawn_host(
    home: &Path,
    apps_toml: &Path,
    bind: &str,
    log: &Path,
    api_key: Option<&str>,
) -> HostProc {
    use std::process::{Command, Stdio};
    let log_file = std::fs::File::create(log).expect("log file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tangram-host"));
    command
        .arg(apps_toml)
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file));
    if let Some(key) = api_key {
        command.env("ANTHROPIC_API_KEY", key);
    }
    HostProc(command.spawn().expect("spawn tangram-host"))
}

fn apps_toml(root: &Path, home: &Path) -> String {
    format!(
        r#"
[apps.guided-learning]
component = "{gl}"
ui = "{root}/apps/guided-learning/ui"
data_dir = "{home}/gl-data"
allow_hosts = ["api.anthropic.com"]

[apps.guided-learning.inject]
"api.anthropic.com" = {{ header = "x-api-key", secret = "env://ANTHROPIC_API_KEY" }}
"#,
        gl = component("guided_learning").display(),
        root = root.display(),
        home = home.display(),
    )
}

#[tokio::test]
async fn tutor_configured_iff_inject_secret_resolves() {
    if !component("guided_learning").exists() {
        eprintln!(
            "SKIPPING guided_learning_egress: {} missing — build the wasm component first \
             (cargo build -p tangram-guided-learning --lib --target wasm32-wasip2 --release)",
            component("guided_learning").display()
        );
        return;
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let toml_path = home.join("apps.toml");
    std::fs::write(&toml_path, apps_toml(&root, home)).expect("write apps.toml");
    let client = reqwest::Client::new();

    // ── 1. Key UNSET → inject does not resolve → description_input:false. ──────
    {
        let port = free_port();
        let base = format!("http://127.0.0.1:{port}");
        let log = home.join("host-offline.log");
        let _host = spawn_host(home, &toml_path, &format!("127.0.0.1:{port}"), &log, None);
        wait_for(
            "guided-learning healthz (offline)",
            Duration::from_secs(120),
            || async {
                status_of(&client, &format!("{base}/guided-learning/healthz")).await
                    == Some(reqwest::StatusCode::OK)
            },
        )
        .await;
        let caps: serde_json::Value = client
            .get(format!("{base}/guided-learning/api/capabilities"))
            .send()
            .await
            .expect("capabilities")
            .json()
            .await
            .expect("caps json");
        assert_eq!(
            caps["description_input"],
            serde_json::Value::Bool(false),
            "unresolvable inject secret ⇒ tutor offline/degraded: {caps}"
        );
    }

    // ── 2. Key SET in the HOST env → inject resolves → description_input:true. ─
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host-online.log");
    let _host = spawn_host(
        home,
        &toml_path,
        &format!("127.0.0.1:{port}"),
        &log,
        Some("placeholder-key"),
    );
    wait_for(
        "guided-learning healthz (online)",
        Duration::from_secs(120),
        || async {
            status_of(&client, &format!("{base}/guided-learning/healthz")).await
                == Some(reqwest::StatusCode::OK)
        },
    )
    .await;
    let caps: serde_json::Value = client
        .get(format!("{base}/guided-learning/api/capabilities"))
        .send()
        .await
        .expect("capabilities")
        .json()
        .await
        .expect("caps json");
    assert_eq!(
        caps["description_input"],
        serde_json::Value::Bool(true),
        "a resolvable inject secret ⇒ description_input:true: {caps}"
    );
}
