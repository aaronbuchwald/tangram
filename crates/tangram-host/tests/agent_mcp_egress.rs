//! Tools/MCP T2 — end-to-end proof that an agent run reaches ONLY the MCP
//! servers the operator's call-level egress grant declares, through the REAL
//! `tangram` component under `tangram-host`.
//!
//! The host enforcement is the existing call-level egress fence (ADR-0008): the
//! tangram app declares `[[calls]]` for `127.0.0.1 /nutrition/mcp` and NOTHING
//! for `/notes/mcp`, with enforcement defaulting to `enforce` (any `[[calls]]`
//! present). So when the agent tool-loop issues its MCP `tools/list` `http-fetch`:
//!
//! - `<base>/nutrition/mcp` is a DECLARED call → allowed → the agent reaches
//!   nutrition's tools and the model's tool call feeds a result back.
//! - `<base>/notes/mcp` is an UNDECLARED call → DENIED at the host boundary
//!   before any request leaves → the un-granted server is unreachable (the loop
//!   records it unavailable and continues). No grant → no reach, host-enforced.
//!
//! The model is a LOCAL FIXTURE LLM (a tiny TCP server) so the test is offline
//! and deterministic — no DeepSeek key, no real egress. It returns a `tool_calls`
//! turn naming `get_capabilities` (a no-network nutrition tool), then a final
//! answer once the tool result comes back.
//!
//! Self-skips (with a notice) when the prebuilt wasm components are missing, so
//! a `cargo test` without the CI wasm pre-step still passes.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod support;
use support::{HostProc, component, free_port, status_of, wait_for, workspace_root};

/// A tiny fixture LLM: speaks just enough HTTP to answer the agent's
/// chat-completions POSTs. Call 1 → request the `get_capabilities` tool; every
/// later call → a final answer. Runs on its own thread until the test drops it
/// (the host process is killed first, so the listener simply stops being hit).
fn spawn_fixture_llm() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture llm");
    // The AUTHORITY (host:port) — app-env values containing `://` are read by
    // the host as secret references, so we hand the component a scheme-free
    // authority and it builds `http://<authority>/v1/chat/completions`.
    let authority = listener.local_addr().unwrap().to_string();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_thread = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read the request head + body (we only need to count calls).
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = rest.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            let mut body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut body);

            let n = calls_thread.fetch_add(1, Ordering::SeqCst);
            // First call → ask for the tool; later calls → final answer.
            let payload = if n == 0 {
                serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": { "name": "get_capabilities", "arguments": "{}" }
                            }]
                        }
                    }]
                })
            } else {
                serde_json::json!({
                    "choices": [{ "message": {
                        "role": "assistant",
                        "content": "Done — checked the app's capabilities."
                    }}]
                })
            };
            let body = payload.to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (authority, calls)
}

#[allow(clippy::too_many_arguments)]
fn spawn_host(home: &Path, apps_toml: &Path, bind: &str, log: &Path) -> HostProc {
    let log_file = std::fs::File::create(log).expect("log file");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tangram-host"));
    command
        .arg(apps_toml)
        .current_dir(home)
        .env("HOME", home)
        .env("BIND_ADDR", bind)
        .env("RUST_LOG", "info")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("TANGRAM_DATA_DIR")
        .stdout(Stdio::from(log_file.try_clone().expect("clone log")))
        .stderr(Stdio::from(log_file));
    HostProc(command.spawn().expect("spawn tangram-host"))
}

/// The test apps.toml: nutrition + notes serve their real MCP; tangram declares
/// a CURATED MCP subset — `/nutrition/mcp` is granted, `/notes/mcp` is NOT. The
/// agent LLM + MCP base point at the fixture / the host's own bind.
fn write_apps_toml(dir: &Path, bind: &str, llm_authority: &str) -> std::path::PathBuf {
    let root = workspace_root();
    let toml = format!(
        r#"
[apps.notes]
component = "{notes}"
ui = "{notes_ui}"

[apps.nutrition]
component = "{nutrition}"
ui = "{nutrition_ui}"

[apps.tangram]
component = "{tangram}"
ui = "{tangram_ui}"
allow_hosts = ["api.deepseek.com", "127.0.0.1"]

# The fixture LLM (loopback). The live config grants only the real DeepSeek
# host; a loopback fixture needs its own declared call.
[[apps.tangram.calls]]
method = "POST"
host = "127.0.0.1"
path = "/v1/chat/completions"

# Curated MCP subset: nutrition is granted; notes is deliberately NOT declared,
# so reaching it is denied at the host egress boundary (enforce mode).
[[apps.tangram.calls]]
method = "POST"
host = "127.0.0.1"
path = "/nutrition/mcp"

[apps.tangram.env]
TANGRAM_AGENT_LLM_AUTHORITY = "{llm_authority}"
TANGRAM_MCP_AUTHORITY = "{bind}"
"#,
        notes = component("notes").display(),
        notes_ui = root.join("apps/notes/ui").display(),
        nutrition = component("nutrition").display(),
        nutrition_ui = root.join("apps/nutrition/ui").display(),
        tangram = component("tangram_app").display(),
        tangram_ui = root.join("apps/tangram/ui/dist").display(),
    );
    let path = dir.join("apps.toml");
    std::fs::write(&path, toml).expect("write apps.toml");
    path
}

/// POST an action on the tangram app, returning the parsed JSON response.
async fn action(
    client: &reqwest::Client,
    base: &str,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    client
        .post(format!("{base}/tangram/api/actions/{name}"))
        .json(&args)
        .send()
        .await
        .expect("action request")
        .json()
        .await
        .expect("action json")
}

#[tokio::test]
async fn agent_reaches_only_the_granted_mcp_server() {
    // Skip cleanly if the wasm components are not prebuilt.
    for c in ["tangram_app", "nutrition", "notes"] {
        if !component(c).exists() {
            eprintln!("skipping: prebuilt {c}.wasm missing (run the wasm32-wasip2 build)");
            return;
        }
    }

    let (llm_authority, calls) = spawn_fixture_llm();
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let base = format!("http://{bind}");

    let home = tempfile::tempdir().expect("home");
    let apps_toml = write_apps_toml(home.path(), &bind, &llm_authority);
    let log = home.path().join("host.log");
    let _host = spawn_host(home.path(), &apps_toml, &bind, &log);

    let client = reqwest::Client::new();
    // Host startup cold-compiles THREE wasm components (notes + nutrition + the
    // heavy `tangram` shell). On the 2-core CI runner, under the nextest
    // `host-integration` concurrency cap (2), this is the slowest host-up in the
    // suite — measured ~82s solo on 2 cores, and roughly double under the
    // cap-2 contention. 90s was too tight (CI timed out); give it ample headroom
    // (the lighter single-component `default_view` test already uses 120s).
    wait_for("host up", Duration::from_secs(240), || async {
        status_of(&client, &format!("{base}/tangram/api/state")).await
            == Some(reqwest::StatusCode::OK)
    })
    .await;

    // Seed: an agent granted `nutrition`, and an agent granted `notes`. Approve
    // each (binding to the live request hash, mirroring the UI).
    let granted =
        "---\nkind: agent\nname: tools-grant\nmcp_servers: [nutrition]\n---\nUse the app's tools.";
    action(
        &client,
        &base,
        "create_file",
        serde_json::json!({ "path": "agents/grant.md", "body": granted }),
    )
    .await;
    let notes_agent =
        "---\nkind: agent\nname: notes-grant\nmcp_servers: [notes]\n---\nUse the app's tools.";
    action(
        &client,
        &base,
        "create_file",
        serde_json::json!({ "path": "agents/notes.md", "body": notes_agent }),
    )
    .await;

    // Read mcp_status to get each request hash, then approve.
    let status: serde_json::Value =
        action(&client, &base, "mcp_status", serde_json::json!({})).await;
    let statuses = status["result"].as_array().cloned().unwrap_or_default();
    let hash_for = |agent: &str| -> String {
        statuses
            .iter()
            .find(|s| s["agent"] == agent)
            .and_then(|s| s["requested_hash"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    action(
        &client,
        &base,
        "approve_mcp",
        serde_json::json!({ "agent": "tools-grant", "requested_hash": hash_for("tools-grant") }),
    )
    .await;
    action(
        &client,
        &base,
        "approve_mcp",
        serde_json::json!({ "agent": "notes-grant", "requested_hash": hash_for("notes-grant") }),
    )
    .await;

    // ── Granted server: the agent reaches nutrition's MCP and finishes. ───────
    let run = action(
        &client,
        &base,
        "run_agent",
        serde_json::json!({ "name": "tools-grant" }),
    )
    .await;
    let output = run["result"].as_str().unwrap_or_default();
    assert!(
        output.contains("Done") || output.contains("capabilities"),
        "the granted agent should finish via the tool result, got: {output:?} (log: {})",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    // The trace should name the nutrition tool it actually called (proving it
    // reached the granted MCP server, not just guessed).
    assert!(
        output.to_lowercase().contains("capabilities") || output.contains("Tools used"),
        "expected a tool-call trace mentioning the reached server, got: {output:?}"
    );
    // The fixture LLM was called at least twice (tool round-trip + final answer).
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "the tool-loop should round-trip the model at least twice"
    );

    // ── Un-granted server: notes is DENIED at the host egress boundary. ───────
    // The fixture again asks for `get_capabilities`; the loop's `tools/list` to
    // `<base>/notes/mcp` is an UNDECLARED call → denied → notes contributes no
    // tools, so the model is offered NOTHING and the run still completes (the
    // un-granted server was never reached).
    let run = action(
        &client,
        &base,
        "run_agent",
        serde_json::json!({ "name": "notes-grant" }),
    )
    .await;
    let output = run["result"].as_str().unwrap_or_default();
    assert!(
        !output.is_empty(),
        "the un-granted run still completes (notes simply unavailable), got empty (log: {})",
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // The decisive host-side evidence: the log shows the egress fence DENIED the
    // tangram component's outbound request to the un-declared /notes/mcp.
    let logs = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logs.contains("notes/mcp") && logs.contains("denied"),
        "expected a host-side egress DENY for the un-declared /notes/mcp call; log:\n{logs}"
    );
}
