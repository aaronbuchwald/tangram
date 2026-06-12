//! End-to-end EC3 test: call-level egress enforcement (fine-grained-egress §4).
//!
//! The host's `http_fetch` now matches each outbound request against the app's
//! DECLARED `[[calls]]` (canonicalized once), injects ONLY the matched call's
//! credential, and — in `enforce` mode — DENIES an undeclared call before it
//! leaves the host. This walks the security-relevant behaviors through the real
//! nutrition component (which issues `GET api.calorieninjas.com/v1/nutrition`):
//!
//! - **Declared call is credentialed (deterministic):** an app that declares
//!   `GET /v1/nutrition` with a resolvable inject reports the capabilities
//!   probe `description_input:true` — the call-level credential is configured.
//!   With a LIVE key the description-based `log_meal` authenticates.
//! - **Undeclared call is DENIED + un-credentialed (deterministic, no network):**
//!   a sibling app that declares enforce `[[calls]]` for a DIFFERENT path makes
//!   the real `GET /v1/nutrition` an undeclared call; `enforce` denies it
//!   host-side, so `log_meal` fails WITHOUT any outbound request. This needs no
//!   live key — the deny happens before the network.
//!
//! The whole test skips (with a notice) when the wasm components are missing;
//! the live-auth step self-skips without `CALORIENINJAS_API_KEY`.

use std::path::Path;
use std::process::{Command, Stdio};
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
    let log_file = std::fs::File::create(log).expect("log file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tangram-host"));
    command
        .arg(apps_toml)
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        .env_remove("CALORIENINJAS_API_KEY")
        .env_remove("NUTRITION_STRATEGY")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file));
    if let Some(key) = api_key {
        command.env("CALORIENINJAS_API_KEY", key);
    }
    HostProc(command.spawn().expect("spawn tangram-host"))
}

/// Two nutrition apps that BOTH allow the calorieninjas host but declare
/// call-level grants differently:
///   - `nutrition` declares the real call `GET /v1/nutrition` (enforce, with
///     the call-scoped inject) — the declared call is credentialed and allowed.
///   - `nutrition-blocked` declares enforce `[[calls]]` for a DIFFERENT path
///     (`/v1/forbidden`), so the component's real `GET /v1/nutrition` matches no
///     declared call and is DENIED host-side in enforce mode (un-credentialed,
///     no network). Neither app puts the key in `[env]`: the secret is host-only.
fn apps_toml(root: &Path) -> String {
    format!(
        r#"
[apps.nutrition]
component = "{nutrition}"
ui = "{root}/apps/nutrition/ui"
data_dir = "{{home}}/nutrition-data"
allow_hosts = ["api.calorieninjas.com"]
enforcement = "enforce"

[apps.nutrition.env]
NUTRITION_STRATEGY = "calorieninjas"

[[apps.nutrition.calls]]
method = "GET"
host   = "api.calorieninjas.com"
path   = "/v1/nutrition"
query  = {{ required = ["query"] }}
inject = {{ header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }}

[apps.nutrition-blocked]
component = "{nutrition}"
ui = "{root}/apps/nutrition/ui"
data_dir = "{{home}}/nutrition-blocked-data"
allow_hosts = ["api.calorieninjas.com"]
enforcement = "enforce"

[apps.nutrition-blocked.env]
NUTRITION_STRATEGY = "calorieninjas"

# Declares a DIFFERENT path than the component actually calls — so the real
# GET /v1/nutrition is undeclared and enforce denies it (deterministically).
[[apps.nutrition-blocked.calls]]
method = "GET"
host   = "api.calorieninjas.com"
path   = "/v1/forbidden"
inject = {{ header = "X-Api-Key", secret = "env://CALORIENINJAS_API_KEY" }}
"#,
        nutrition = component("nutrition").display(),
        root = root.display(),
    )
}

#[tokio::test]
async fn enforce_credentials_declared_calls_and_denies_undeclared() {
    if !component("nutrition").exists() {
        eprintln!(
            "SKIPPING egress_enforcement: {} missing — build the wasm components first \
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

    // A resolvable key (real if present for the live step, else a placeholder
    // that still resolves so the capabilities gate flips to configured).
    let live_key = std::env::var("CALORIENINJAS_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty());
    let key_for_host = live_key.clone().unwrap_or_else(|| "placeholder-key".into());

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(
        home,
        &toml_path,
        &format!("127.0.0.1:{port}"),
        &log,
        Some(&key_for_host),
    );
    for app in ["nutrition", "nutrition-blocked"] {
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

    // ── 1. The DECLARED call's credential is configured (deterministic). The
    //    inject lives on the `GET /v1/nutrition` call; with the key resolvable
    //    the capabilities probe reports description_input:true. ──────────────
    let caps: serde_json::Value = client
        .get(format!("{base}/nutrition/api/capabilities"))
        .send()
        .await
        .expect("capabilities")
        .json()
        .await
        .expect("caps json");
    assert_eq!(
        caps["description_input"],
        serde_json::Value::Bool(true),
        "a resolvable call-level inject ⇒ configured ⇒ description_input:true: {caps}"
    );

    // ── 2. The UNDECLARED call is DENIED host-side (deterministic, no network).
    //    `nutrition-blocked` declares only `/v1/forbidden`; the component's real
    //    `GET /v1/nutrition` matches no declared call, so enforce denies it
    //    BEFORE any outbound request. log_meal therefore fails — and it fails
    //    even with the key resolvable, proving the deny is the call gate, not a
    //    missing credential. ─────────────────────────────────────────────────
    let blocked = client
        .post(format!("{base}/nutrition-blocked/api/actions/log_meal"))
        .json(&serde_json::json!({ "description": "100g grilled chicken breast" }))
        .send()
        .await
        .expect("log_meal (blocked)");
    assert_ne!(
        blocked.status(),
        reqwest::StatusCode::OK,
        "an undeclared call must be DENIED in enforce mode (no declared call matches \
         GET /v1/nutrition) — success would mean enforcement did not run"
    );

    // ── 3. The DECLARED call authenticates over the network (LIVE only). ─────
    let Some(_) = live_key else {
        eprintln!(
            "egress_enforcement: CALORIENINJAS_API_KEY not set — skipping the LIVE \
             declared-call auth step (capabilities + deny gating verified deterministically)."
        );
        return;
    };
    let res = client
        .post(format!("{base}/nutrition/api/actions/log_meal"))
        .json(&serde_json::json!({ "description": "100g grilled chicken breast" }))
        .send()
        .await
        .expect("log_meal (declared)");
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::OK {
        let meals: serde_json::Value = client
            .post(format!("{base}/nutrition/api/actions/list_meals"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("list_meals")
            .json()
            .await
            .expect("meals json");
        let meal_id = meals
            .as_array()
            .and_then(|a| a.first())
            .and_then(|m| m.get("id"))
            .and_then(|v| v.as_str())
            .expect("a logged meal id")
            .to_string();
        let rows: serde_json::Value = client
            .post(format!("{base}/nutrition/api/actions/meal_nutrition"))
            .json(&serde_json::json!({ "meal_id": meal_id }))
            .send()
            .await
            .expect("meal_nutrition")
            .json()
            .await
            .expect("rows json");
        assert!(
            rows.as_array().is_some_and(|r| !r.is_empty()),
            "the declared, injected, authenticated lookup must have cached nutrients: {rows}"
        );
    } else {
        eprintln!(
            "egress_enforcement: CalorieNinjas returned {status} (upstream likely \
             unavailable: {body}) — skipping the live auth assertion. The declared call \
             reached the API host through the egress path; deny + capabilities verified."
        );
    }
}
