//! Build-3 / GC2 end-to-end: the grocery cart-fill request→runner dispatch loop
//! driving the REAL `tangram_automation::wholefoods::run_fill` flow OFFLINE.
//!
//! Drives the REAL `tangram-host` binary with the `grocery-cart` app + an
//! `[automation]` operator policy. With `TANGRAM_CARTFILL_OFFLINE_FIXTURE=1` the
//! dispatcher's `OfflineFixtureRunner` runs the WHOLE real `run_fill` flow with a
//! mock browser driver + a fixture LLM matcher + a session-reuse preflight — NO
//! browser / 1Password / LLM / network call:
//!
//! - **Happy path:** `fill_cart` records a PENDING request and returns a handle;
//!   the dispatch loop authorizes it against the policy, runs `run_fill` (session
//!   reuse skips login, the LLM matcher picks a product per item honoring
//!   preferences, the StopGate halts before checkout, the cart-VIEW URL is
//!   captured), and writes a `done` `CartFillResult` back — `cart_fill_status`
//!   surfaces it, with the off-ceiling domain trimmed away (never-widen) and the
//!   items LLM-matched into `added`.
//! - **Default-deny:** with the `wholefoods-cart` template NOT approved in
//!   `[automation]`, the same request is denied by policy and recorded `failed`
//!   (the never-checkout rail fails closed — an unapproved template never runs).
//!
//! The MCP tool surface (`fill_cart` + `cart_fill_status`) is confirmed via the
//! app's action list (actions auto-become MCP tools).
//!
//! Requires the `grocery_cart` wasm component (built by CI before `cargo test`):
//!   cargo build -p tangram-grocery-cart --lib --target wasm32-wasip2 --release
//! SKIPS (with a notice) when it is missing.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

mod support;
use support::{HostProc, component, free_port, status_of, wait_for};

fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let child = Command::new(env!("CARGO_BIN_EXE_tangram-host"))
        .arg(apps_toml)
        // cwd = scratch HOME so the repo's .env is NOT loaded by dotenvy.
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        // GC2: drive the WHOLE real `run_fill` flow OFFLINE (a mock browser
        // driver + a fixture LLM matcher + a session-reuse preflight) — no
        // browser/1Password/LLM/network. The production default (unset) is the
        // live runner, which is offline-`NeedsSignIn` until the GC3 live run.
        .env("TANGRAM_CARTFILL_OFFLINE_FIXTURE", "1")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn tangram-host");
    HostProc(child)
}

/// Write an `apps.toml` with the grocery-cart app and an `[automation]` policy.
/// `approve` toggles whether the `wholefoods-cart` template is approved.
fn write_apps_toml(path: &Path, approve_template: bool) {
    let approved = if approve_template {
        "approved_templates = [\"wholefoods-cart\"]"
    } else {
        "# wholefoods-cart NOT approved → default-deny"
    };
    std::fs::write(
        path,
        format!(
            r#"
[automation]
enabled = true
browser_domains_ceiling = ["www.amazon.com", "www.wholefoodsmarket.com"]
{approved}
denied_paths = ["/gp/buy/"]

[automation.credential_grants]
"grocery-cart" = ["op://Private/PLACEHOLDER-amazon-login/password"]

[apps.grocery-cart]
component = "{component}"
ui = "{ui}"
"#,
            component = component("grocery_cart").display(),
            // The app declares no UI; point at the crate dir (served but unused).
            ui = component("grocery_cart").parent().unwrap().display(),
        ),
    )
    .expect("write apps.toml");
}

async fn post_action(
    client: &reqwest::Client,
    base: &str,
    action: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let res = client
        .post(format!("{base}/grocery-cart/api/actions/{action}"))
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("POST {action}: {e}"));
    assert!(
        res.status().is_success(),
        "POST {action} failed: {}",
        res.status()
    );
    // The action route wraps the return as `{ "result": <return> }`; hand back
    // the inner value so callers see the action's own result shape.
    let body: serde_json::Value = res.json().await.expect("action result JSON");
    body["result"].clone()
}

#[tokio::test]
async fn fill_cart_round_trips_through_the_dispatch_loop() {
    if !component("grocery_cart").exists() {
        eprintln!(
            "SKIPPING cartfill_dispatch: {} missing — build the wasm component first \
             (cargo build -p tangram-grocery-cart --lib --target wasm32-wasip2 --release)",
            component("grocery_cart").display()
        );
        return;
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let apps_toml = home.join("apps.toml");
    write_apps_toml(&apps_toml, true);

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    wait_for("grocery-cart healthz", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/grocery-cart/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;

    // The MCP tool surface: fill_cart + cart_fill_status appear (actions become
    // MCP tools). The action list is the same registry the MCP bridge derives.
    let actions: serde_json::Value = client
        .get(format!("{base}/grocery-cart/api/actions"))
        .send()
        .await
        .expect("GET actions")
        .json()
        .await
        .expect("actions JSON");
    let names: Vec<&str> = actions["actions"]
        .as_array()
        .expect("actions array")
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    for tool in ["fill_cart", "cart_fill_status", "record_cart_result"] {
        assert!(
            names.contains(&tool),
            "MCP tool {tool} missing from {names:?}"
        );
    }

    // 1) fill_cart → a request_id handle (the request is PENDING in the doc).
    let request_id = post_action(
        &client,
        &base,
        "fill_cart",
        serde_json::json!({
            "grocery_list": [
                { "item": "milk", "quantity": 1, "preferences": "organic" },
                { "item": "eggs", "quantity": 2 }
            ]
        }),
    )
    .await;
    let request_id = request_id.as_str().expect("request_id string").to_string();
    assert!(!request_id.is_empty());

    // 2) The dispatch loop authorizes + runs the fixture + writes back. Poll the
    //    status tool until it reports `done` (within a few loop ticks).
    wait_for("cart fill done", Duration::from_secs(30), || {
        let client = &client;
        let base = &base;
        let request_id = request_id.clone();
        async move {
            let status = post_action(
                client,
                base,
                "cart_fill_status",
                serde_json::json!({ "request_id": request_id }),
            )
            .await;
            status["status"] == "done"
        }
    })
    .await;

    // 3) Re-read the final result and assert the fixture round-trip.
    let final_status = post_action(
        &client,
        &base,
        "cart_fill_status",
        serde_json::json!({ "request_id": request_id }),
    )
    .await;
    let result = &final_status["result"];
    let added = result["added"].as_array().expect("added array");
    assert_eq!(added.len(), 2, "both items added by the real run_fill flow");
    assert_eq!(added[0]["item"], "milk");
    // The LLM matcher honored the "organic" preference (the fixture driver
    // surfaced "365 milk" + "Organic Whole milk"; the matcher picked the latter).
    assert_eq!(added[0]["product"], "Organic Whole milk");
    assert_eq!(added[0]["qty"], 1);
    assert_eq!(added[1]["item"], "eggs");
    // No preference → the first item-matching candidate ("365 eggs").
    assert_eq!(added[1]["product"], "365 eggs");
    assert_eq!(added[1]["qty"], 2);
    // The never-widen rail: the cart URL anchors on a ceiling-allowed domain.
    let cart_url = result["cart_url"].as_str().expect("cart_url");
    assert!(
        cart_url.starts_with("https://www.amazon.com/")
            || cart_url.starts_with("https://www.wholefoodsmarket.com/"),
        "cart_url must be on a ceiling domain: {cart_url}"
    );
}

#[tokio::test]
async fn unapproved_template_fails_closed() {
    if !component("grocery_cart").exists() {
        eprintln!("SKIPPING cartfill_dispatch default-deny: grocery_cart component missing");
        return;
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let apps_toml = home.join("apps.toml");
    // Template NOT approved → the dispatch loop must deny + record `failed`.
    write_apps_toml(&apps_toml, false);

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host-deny.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    wait_for("grocery-cart healthz", Duration::from_secs(120), || async {
        status_of(&client, &format!("{base}/grocery-cart/healthz")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;

    let request_id = post_action(
        &client,
        &base,
        "fill_cart",
        serde_json::json!({
            "grocery_list": [{ "item": "milk", "quantity": 1 }]
        }),
    )
    .await;
    let request_id = request_id.as_str().expect("request_id").to_string();

    // The loop authorizes against the policy, the template is unapproved, so it
    // fails closed → `failed`, never run. Poll until `failed`.
    wait_for("cart fill failed", Duration::from_secs(30), || {
        let client = &client;
        let base = &base;
        let request_id = request_id.clone();
        async move {
            let status = post_action(
                client,
                base,
                "cart_fill_status",
                serde_json::json!({ "request_id": request_id }),
            )
            .await;
            status["status"] == "failed"
        }
    })
    .await;

    let status = post_action(
        &client,
        &base,
        "cart_fill_status",
        serde_json::json!({ "request_id": request_id }),
    )
    .await;
    assert_eq!(status["status"], "failed");
    // The failure reason names the policy denial (the never-checkout rail).
    let reason = status["result"]["not_added"][0]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains("operator policy"),
        "failure must cite the policy denial: {reason}"
    );
}
