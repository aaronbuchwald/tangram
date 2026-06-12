//! End-to-end test: the OPT-IN egress policy engine (§9.2; ADR-0009 — the
//! deliberately-marked escape hatch, NOT the default).
//!
//! The policy engine runs at the host egress boundary as an ADDITIONAL gate,
//! AFTER the declarative call match, and can only NARROW (turn an allow into a
//! deny). It never widens and never changes which credential is injected. This
//! walks the security-relevant behaviors through the real nutrition component
//! (which issues `GET api.calorieninjas.com/v1/nutrition`):
//!
//! - **Policy NARROWS a declared, allowed call (deterministic, no network):**
//!   `nutrition-policy-deny` declares the real `GET /v1/nutrition` call (so the
//!   declarative engine ALLOWS it) but attaches a policy whose default is deny
//!   and whose single allow rule names a DIFFERENT path. The policy therefore
//!   denies the real call before it leaves the host, so `log_meal` fails
//!   WITHOUT any outbound request — and fails even with a resolvable key,
//!   proving the deny is the policy gate, not a missing credential.
//! - **A permissive policy leaves a declared call working (capabilities
//!   deterministic):** `nutrition-policy-allow` declares the same real call and
//!   attaches a policy that allows it; the call-level inject is still
//!   configured, so the capabilities probe reports `description_input:true`.
//!
//! The whole test skips (with a notice) when the wasm components are missing.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

mod support;
use support::{HostProc, component, free_port, status_of, wait_for, workspace_root};

fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path, api_key: &str) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tangram-host"));
    command
        .arg(apps_toml)
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        .env_remove("NUTRITION_STRATEGY")
        .env_remove("TANGRAM_DATA_DIR")
        .env("CALORIENINJAS_API_KEY", api_key)
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file));
    HostProc(command.spawn().expect("spawn tangram-host"))
}

/// Two nutrition apps that BOTH declare the real `GET /v1/nutrition` call (so
/// the declarative engine allows it) but attach DIFFERENT opt-in policies:
///   - `nutrition-policy-deny` attaches a default-deny policy whose only allow
///     rule names a DIFFERENT path, so the policy DENIES the real call (the
///     policy narrows what the declarative engine allowed).
///   - `nutrition-policy-allow` attaches a policy that allows the real call, so
///     it proceeds and the call-level inject stays configured.
fn apps_toml(root: &Path) -> String {
    format!(
        r#"
[apps.nutrition-policy-deny]
component = "{nutrition}"
ui = "{root}/apps/nutrition/ui"
data_dir = "{{home}}/deny-data"
allow_hosts = ["api.calorieninjas.com"]
enforcement = "enforce"

[apps.nutrition-policy-deny.env]
NUTRITION_STRATEGY = "calorieninjas"

[[apps.nutrition-policy-deny.calls]]
method = "GET"
host   = "api.calorieninjas.com"
path   = "/v1/nutrition"
query  = {{ required = ["query"] }}
inject = {{ header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }}

# The OPT-IN policy: default deny, and the only allow rule names a path the
# component never calls — so the real GET /v1/nutrition is policy-denied even
# though the declarative call above allowed it. The policy NARROWS.
[apps.nutrition-policy-deny.policy]
default = "deny"
rules = [
  {{ effect = "allow", method = ["GET"], host = "api.calorieninjas.com", path_prefix = ["v1", "something-else"] }},
]

[apps.nutrition-policy-allow]
component = "{nutrition}"
ui = "{root}/apps/nutrition/ui"
data_dir = "{{home}}/allow-data"
allow_hosts = ["api.calorieninjas.com"]
enforcement = "enforce"

[apps.nutrition-policy-allow.env]
NUTRITION_STRATEGY = "calorieninjas"

[[apps.nutrition-policy-allow.calls]]
method = "GET"
host   = "api.calorieninjas.com"
path   = "/v1/nutrition"
query  = {{ required = ["query"] }}
inject = {{ header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }}

# A policy that ALLOWS the real call (and denies anything else): the declared
# call proceeds, the call-level inject stays configured.
[apps.nutrition-policy-allow.policy]
default = "deny"
rules = [
  {{ effect = "allow", method = ["GET"], host = "api.calorieninjas.com", path_prefix = ["v1", "nutrition"], query_present = ["query"] }},
]
"#,
        nutrition = component("nutrition").display(),
        root = root.display(),
    )
}

#[tokio::test]
async fn opt_in_policy_narrows_a_declared_call_and_permits_when_allowed() {
    if !component("nutrition").exists() {
        eprintln!(
            "SKIPPING egress_policy: {} missing — build the wasm components first \
             (cargo build -p tangram-nutrition --lib --target wasm32-wasip2 --release)",
            component("nutrition").display()
        );
        return;
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let toml = apps_toml(&root).replace("{home}", home.to_str().unwrap());
    let toml_path = home.join("apps.toml");
    std::fs::write(&toml_path, &toml).expect("write apps.toml");
    let client = reqwest::Client::new();

    // A resolvable key so the capabilities gate flips to configured; the deny
    // step does not depend on the key (the policy denies before the network).
    let key = std::env::var("CALORIENINJAS_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .unwrap_or_else(|| "placeholder-key".into());

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &toml_path, &format!("127.0.0.1:{port}"), &log, &key);
    for app in ["nutrition-policy-deny", "nutrition-policy-allow"] {
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

    // ── 1. The policy NARROWS a declared, allowed call (deterministic, no
    //    network). The declarative engine allows GET /v1/nutrition, but the
    //    attached default-deny policy (whose allow rule names a different path)
    //    denies it BEFORE any outbound request. log_meal therefore fails — and
    //    fails even with a resolvable key, proving the deny is the policy gate,
    //    not a missing credential. ──────────────────────────────────────────
    let denied = client
        .post(format!("{base}/nutrition-policy-deny/api/actions/log_meal"))
        .json(&serde_json::json!({ "description": "100g grilled chicken breast" }))
        .send()
        .await
        .expect("log_meal (policy-deny)");
    assert_ne!(
        denied.status(),
        reqwest::StatusCode::OK,
        "the OPT-IN policy must DENY the declared call (its allow rule names a different \
         path; default deny) — success would mean the policy gate did not run"
    );

    // ── 2. A permissive policy leaves the declared call's credential
    //    configured (deterministic). The policy allows GET /v1/nutrition, so the
    //    call-level inject stays configured and the capabilities probe reports
    //    description_input:true. ──────────────────────────────────────────────
    let caps: serde_json::Value = client
        .get(format!("{base}/nutrition-policy-allow/api/capabilities"))
        .send()
        .await
        .expect("capabilities")
        .json()
        .await
        .expect("caps json");
    assert_eq!(
        caps["description_input"],
        serde_json::Value::Bool(true),
        "a permissive policy leaves the declared call's inject configured \
         ⇒ description_input:true: {caps}"
    );

    // The host log names the custom policy (never silent — the §9.2 surfacing).
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log_text.contains("CUSTOM POLICY"),
        "the host must loudly surface that an app uses a custom egress policy; log:\n{log_text}"
    );
}
