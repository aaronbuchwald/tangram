//! End-to-end MCP-gateway test (RUNTIME_PLAN D3): the host runs agentgateway
//! as a supervised child and routes MCP through it, while staying the single
//! public entry point.
//!
//! Walks: per-app `/<app>/mcp` handshake + tools/call THROUGH the gateway
//! (proved by the gateway's composite session id), the aggregate `/mcp`
//! endpoint with namespaced tools, bearer auth surviving the gateway hop
//! (401 unauthed / success authed on a mutating registry tool), converge
//! regeneration (registry install → tools appear on the aggregate; remove →
//! gone), and crash resilience (kill -9 the child → the host restarts it and
//! MCP recovers). A second test pins the fallback: gateway enabled but
//! binary missing → direct serving exactly as today.
//!
//! Requires the wasm components (see registry_lifecycle.rs) AND an
//! `agentgateway` binary on $PATH; SKIPS with a notice when either is
//! missing.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;

const TOKEN: &str = "test-gateway-token";

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

fn agentgateway_on_path() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| dir.join("agentgateway").is_file())
    })
}

/// The spawned host, killed on drop so a failing test never leaks a server
/// (the host's agentgateway child is `kill_on_drop` in the host itself, but
/// SIGKILL on the host orphans it — so also kill the recorded child pid).
struct HostProc {
    child: Child,
    gateway_pid: Option<u32>,
}

impl Drop for HostProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(pid) = self.gateway_pid {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
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
    HostProc {
        child,
        gateway_pid: None,
    }
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

async fn fleet_gateway(client: &reqwest::Client, base: &str) -> serde_json::Value {
    match client.get(format!("{base}/api/fleet")).send().await {
        Ok(res) => res
            .json::<serde_json::Value>()
            .await
            .map(|fleet| fleet["gateway"].clone())
            .unwrap_or(serde_json::Value::Null),
        Err(_) => serde_json::Value::Null,
    }
}

/// An MCP client session against one endpoint: initialize → initialized,
/// then tools/list and tools/call reusing the negotiated `Mcp-Session-Id`.
struct McpSession {
    client: reqwest::Client,
    url: String,
    session: String,
}

impl McpSession {
    /// Try the initialize → initialized handshake. Errs on any failure —
    /// converge wait-loops use this, because the gateway's aggregate
    /// endpoint fans initialize out to every target and transiently fails
    /// while a config reload and the app table are catching up with each
    /// other (install/remove in flight).
    async fn try_connect(client: &reqwest::Client, url: &str) -> Result<Self, String> {
        let res = client
            .post(url)
            .header("accept", "application/json, text/event-stream")
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "gateway-test", "version": "0" }
                }
            }))
            .send()
            .await
            .map_err(|e| format!("initialize send: {e}"))?;
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            return Err(format!("initialize against {url}: {status} {body}"));
        }
        let session = res
            .headers()
            .get("mcp-session-id")
            .ok_or("missing mcp-session-id header")?
            .to_str()
            .map_err(|e| e.to_string())?
            .to_string();
        let me = Self {
            client: client.clone(),
            url: url.to_string(),
            session,
        };
        let res = me
            .post(
                &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                None,
            )
            .await;
        if !res.status().is_success() {
            return Err(format!("initialized: {}", res.status()));
        }
        Ok(me)
    }

    async fn connect(client: &reqwest::Client, url: &str) -> Self {
        Self::try_connect(client, url)
            .await
            .unwrap_or_else(|e| panic!("mcp handshake failed: {e}"))
    }

    async fn post(&self, body: &serde_json::Value, auth: Option<&str>) -> reqwest::Response {
        let mut req = self
            .client
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &self.session)
            .json(body);
        if let Some(token) = auth {
            req = req.bearer_auth(token);
        }
        req.send().await.expect("mcp post")
    }

    async fn try_tool_names(&self) -> Result<Vec<String>, String> {
        let res = self
            .post(
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
                }),
                None,
            )
            .await;
        if !res.status().is_success() {
            return Err(format!("tools/list: {}", res.status()));
        }
        let body = res.text().await.map_err(|e| e.to_string())?;
        let result = sse_result(&body);
        Ok(result["result"]["tools"]
            .as_array()
            .ok_or_else(|| format!("no tools array: {result}"))?
            .iter()
            .filter_map(|t| t["name"].as_str().map(String::from))
            .collect())
    }

    async fn tool_names(&self) -> Vec<String> {
        self.try_tool_names()
            .await
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// tools/call returning (http status, response text).
    async fn call(
        &self,
        name: &str,
        args: serde_json::Value,
        auth: Option<&str>,
    ) -> (reqwest::StatusCode, String) {
        let res = self
            .post(
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": { "name": name, "arguments": args }
                }),
                auth,
            )
            .await;
        let status = res.status();
        let body = res.text().await.expect("tools/call body");
        (status, body)
    }
}

/// Both rmcp and agentgateway answer POSTs as an SSE stream (or plain
/// JSON); extract the JSON-RPC message — rmcp opens its stream with an
/// EMPTY data event, so take the first non-empty payload.
fn sse_result(body: &str) -> serde_json::Value {
    let payload = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find(|payload| !payload.trim().is_empty())
        .unwrap_or(body);
    serde_json::from_str(payload).unwrap_or_else(|e| panic!("bad rpc payload ({e}): {body}"))
}

fn write_apps_toml(home: &Path, gateway_section: &str) -> PathBuf {
    let root = workspace_root();
    let apps_toml = home.join("apps.toml");
    std::fs::write(
        &apps_toml,
        format!(
            r#"
{gateway_section}

[apps.registry]
component = "{registry}"
ui = "{root}/apps/registry/ui"
registry = true

[apps.notes]
component = "{notes}"
ui = "{root}/apps/notes/ui"
"#,
            registry = component("registry").display(),
            notes = component("notes").display(),
            root = root.display(),
        ),
    )
    .expect("write apps.toml");
    apps_toml
}

#[tokio::test]
async fn mcp_through_gateway_with_converge_auth_and_crash_recovery() {
    for name in ["registry", "notes", "nutrition"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING gateway_lifecycle: {} missing",
                component(name).display()
            );
            return;
        }
    }
    if !agentgateway_on_path() {
        eprintln!("SKIPPING gateway_lifecycle: no agentgateway binary on PATH");
        return;
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let root = workspace_root();
    let apps_toml = write_apps_toml(home, "[gateway]\nenabled = true");

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let mut host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    // Host up, apps healthy, gateway child running.
    wait_for("gateway running", Duration::from_secs(30), || async {
        fleet_gateway(&client, &base).await["running"] == serde_json::Value::Bool(true)
    })
    .await;
    let gateway_pid = fleet_gateway(&client, &base).await["pid"]
        .as_u64()
        .expect("gateway pid") as u32;
    host.gateway_pid = Some(gateway_pid);

    // The generated config exists in the host's data root and is the merged
    // desired state, not a hand-written file.
    let config_path = home.join(".tangram-host/agentgateway.json");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).expect("generated config"))
            .expect("config json");
    let routes = config["binds"][0]["listeners"][0]["routes"]
        .as_array()
        .expect("routes");
    assert_eq!(routes.len(), 3, "notes + registry + aggregate");

    // ── 1. per-app MCP through the gateway: full handshake + tools/call.
    //    Hop evidence: agentgateway mints composite sessions — base64 JSON
    //    naming the target — unlike rmcp's plain UUIDs. ──────────────────────
    let notes = McpSession::connect(&client, &format!("{base}/notes/mcp")).await;
    let decoded = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(notes.session.trim_end_matches('='))
        .expect("gateway session is base64");
    let decoded: serde_json::Value =
        serde_json::from_slice(&decoded).expect("gateway session is JSON");
    assert_eq!(
        decoded["t"], "mcp",
        "session minted by agentgateway: {decoded}"
    );
    assert_eq!(decoded["s"][0]["t"], "notes", "session names the target");

    let tools = notes.tool_names().await;
    assert!(
        tools.contains(&"add_note".to_string()),
        "per-app tools keep their plain names through the gateway: {tools:?}"
    );
    let (status, body) = notes
        .call(
            "add_note",
            serde_json::json!({ "text": "via the gateway" }),
            None,
        )
        .await;
    assert!(status.is_success(), "add_note: {status} {body}");
    assert!(
        !body.contains("\"isError\":true"),
        "add_note failed: {body}"
    );

    // ── 2. the aggregate /mcp endpoint: every app's tools, namespaced ──────
    let aggregate = McpSession::connect(&client, &format!("{base}/mcp")).await;
    let tools = aggregate.tool_names().await;
    for expected in [
        "notes_add_note",
        "registry_list_apps",
        "registry_install_app",
    ] {
        assert!(
            tools.contains(&expected.to_string()),
            "missing {expected}: {tools:?}"
        );
    }
    let (status, body) = aggregate
        .call("notes_list_notes", serde_json::json!({}), None)
        .await;
    assert!(status.is_success(), "notes_list_notes: {status}");
    assert!(
        body.contains("via the gateway"),
        "the note added per-app is visible through the aggregate: {body}"
    );

    // ── 3. auth THROUGH the gateway: mutating registry tool unauthed → 401
    //    (the host's internal bearer gate, not bypassed by the hop) ──────────
    let install_args = serde_json::json!({
        "name": "nutrition",
        "component": component("nutrition").display().to_string(),
        "ui": root.join("apps/nutrition/ui").display().to_string(),
        "env": [{ "key": "NUTRITION_STRATEGY", "value": "offline" }],
    });
    let (status, _) = aggregate
        .call("registry_install_app", install_args.clone(), None)
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "unauthenticated mutating tools/call must 401 through the gateway"
    );
    // Non-mutating tools stay open, same as direct serving.
    let (status, _) = aggregate
        .call("registry_list_apps", serde_json::json!({}), None)
        .await;
    assert!(status.is_success(), "reads stay open: {status}");

    // ── 4. converge regeneration: authed install → config regenerated →
    //    tools appear on the aggregate WITHOUT restarting host or gateway ────
    let (status, body) = aggregate
        .call("registry_install_app", install_args, Some(TOKEN))
        .await;
    assert!(status.is_success(), "authed install: {status} {body}");
    assert!(!body.contains("\"isError\":true"), "install failed: {body}");
    wait_for(
        "nutrition in gateway config",
        Duration::from_secs(30),
        || async {
            std::fs::read_to_string(&config_path)
                .is_ok_and(|config| config.contains("/nutrition/mcp"))
        },
    )
    .await;
    // agentgateway hot-reloads the regenerated file; a NEW aggregate session
    // sees the installed app's namespaced tools. (try_connect: the aggregate
    // fans out to every target, so it transiently errors mid-reload.)
    wait_for(
        "nutrition tools on /mcp",
        Duration::from_secs(30),
        || async {
            match McpSession::try_connect(&client, &format!("{base}/mcp")).await {
                Ok(session) => session
                    .try_tool_names()
                    .await
                    .is_ok_and(|tools| tools.contains(&"nutrition_log_meal".to_string())),
                Err(_) => false,
            }
        },
    )
    .await;
    let session = McpSession::connect(&client, &format!("{base}/mcp")).await;
    let (status, body) = session
        .call(
            "nutrition_log_meal",
            serde_json::json!({
                "name": "gateway meal",
                "components": [{ "component": "egg", "qty_g": 100.0 }]
            }),
            None,
        )
        .await;
    assert!(status.is_success(), "nutrition_log_meal: {status}");
    assert!(
        !body.contains("\"isError\":true"),
        "log_meal failed: {body}"
    );

    // …and remove → gone from the aggregate (converge shrinks the config).
    let res = client
        .post(format!("{base}/registry/api/actions/remove_app"))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "name": "nutrition" }))
        .send()
        .await
        .expect("remove_app");
    assert!(res.status().is_success());
    wait_for(
        "nutrition gone from /mcp",
        Duration::from_secs(30),
        || async {
            match McpSession::try_connect(&client, &format!("{base}/mcp")).await {
                Ok(session) => session
                    .try_tool_names()
                    .await
                    .is_ok_and(|tools| !tools.iter().any(|t| t.starts_with("nutrition_"))),
                Err(_) => false,
            }
        },
    )
    .await;

    // ── 5. crash resilience: SIGKILL the child → supervisor restarts it →
    //    MCP through the gateway recovers ───────────────────────────────────
    assert!(
        Command::new("kill")
            .args(["-9", &gateway_pid.to_string()])
            .status()
            .expect("kill")
            .success(),
        "killing the agentgateway child"
    );
    wait_for("gateway restarted", Duration::from_secs(30), || async {
        let gateway = fleet_gateway(&client, &base).await;
        gateway["running"] == serde_json::Value::Bool(true)
            && gateway["pid"].as_u64() != Some(gateway_pid as u64)
    })
    .await;
    host.gateway_pid = fleet_gateway(&client, &base).await["pid"]
        .as_u64()
        .map(|pid| pid as u32);
    wait_for("mcp recovered", Duration::from_secs(30), || async {
        // A fresh handshake works end to end again.
        let res = client
            .post(format!("{base}/notes/mcp"))
            .header("accept", "application/json, text/event-stream")
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "recovery-probe", "version": "0" }
                }
            }))
            .send()
            .await;
        res.is_ok_and(|res| res.status().is_success())
    })
    .await;
}

#[tokio::test]
async fn missing_binary_falls_back_to_direct_serving() {
    for name in ["registry", "notes"] {
        if !component(name).exists() {
            eprintln!(
                "SKIPPING gateway fallback test: {} missing",
                component(name).display()
            );
            return;
        }
    }

    let scratch = tempfile::tempdir().expect("tempdir");
    let home = scratch.path();
    let apps_toml = write_apps_toml(
        home,
        "[gateway]\nenabled = true\nbinary = \"/nonexistent/agentgateway\"",
    );

    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let log = home.join("host.log");
    let _host = spawn_host(home, &apps_toml, &format!("127.0.0.1:{port}"), &log);
    let client = reqwest::Client::new();

    wait_for("notes healthy", Duration::from_secs(30), || async {
        client
            .get(format!("{base}/notes/healthz"))
            .send()
            .await
            .is_ok_and(|res| res.status() == reqwest::StatusCode::OK)
    })
    .await;

    // Fleet reports no gateway; the fallback warning is logged.
    assert_eq!(fleet_gateway(&client, &base).await, serde_json::Value::Null);
    let logged = std::fs::read_to_string(&log).expect("host log");
    assert!(
        logged.contains("falling back"),
        "expected a clear fallback warning in the startup log"
    );

    // Direct per-app MCP works exactly as today — rmcp's plain-UUID session,
    // unprefixed tools, tools/call lands in the document.
    let notes = McpSession::connect(&client, &format!("{base}/notes/mcp")).await;
    assert!(
        !notes.session.starts_with("eyJ"),
        "direct serving must not mint gateway sessions: {}",
        notes.session
    );
    let tools = notes.tool_names().await;
    assert!(tools.contains(&"add_note".to_string()), "{tools:?}");
    let (status, body) = notes
        .call("add_note", serde_json::json!({ "text": "direct" }), None)
        .await;
    assert!(status.is_success(), "add_note: {status} {body}");

    // No gateway → no aggregate endpoint.
    let res = client
        .post(format!("{base}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(
            &serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
        )
        .send()
        .await
        .expect("aggregate probe");
    assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);
}
