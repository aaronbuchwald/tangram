//! The MCP plane through agentgateway (RUNTIME_PLAN D3: "agentgateway scope:
//! MCP plane only").
//!
//! With `[gateway] enabled = true` in `apps.toml` and an `agentgateway`
//! binary available, the host runs agentgateway as a SUPERVISED CHILD
//! process on an internal port and routes all public MCP traffic through it:
//!
//! ```text
//!   client ──:8080──▶ tangram-host (the ONE public listener)
//!     /<app>/mcp ───▶ proxy ──▶ agentgateway :<gw> ──▶ host internal :<int>/<app>/mcp
//!     /mcp (aggregate, every app's tools namespaced <app>_<tool>) ──▶ same
//! ```
//!
//! The host's per-app rmcp services keep serving — on an internal
//! loopback-only listener that agentgateway targets — so everything the
//! direct path enforces (the bearer gate on mutating registry tools, the
//! SDK's error envelope) still holds: agentgateway forwards the client's
//! `Authorization` header to the target, and rewrites namespaced aggregate
//! tool names (`registry_install_app`) back to the app's real tool name
//! before the internal endpoint sees them.
//!
//! agentgateway's config is GENERATED from the merged desired state on every
//! converge (never hand-edited) and written atomically; agentgateway watches
//! the file and hot-reloads, so registry installs show up on the aggregate
//! endpoint without restarting anything. agentgateway v1.2 binds its data
//! plane on the wildcard address, so every generated route carries a
//! loopback-only `source.address` authorization policy — the gateway is
//! unreachable from off the box even though the socket is wildcard.
//!
//! If the binary is missing the host logs a clear warning and falls back to
//! today's direct per-app `/mcp` serving — the gateway is an enhancement,
//! never a hard dependency.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::watch;

use crate::tenant::AppKey;

/// `[gateway]` in `apps.toml`. Read once at startup (changing it needs a
/// host restart — unlike app specs, which converge live).
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewaySettings {
    /// Route MCP through a host-managed agentgateway child process.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the agentgateway binary. Default: `agentgateway` on $PATH.
    #[serde(default)]
    pub binary: Option<PathBuf>,
    /// Internal loopback port for the gateway's MCP listener. Default: a
    /// free port chosen at startup.
    #[serde(default)]
    pub port: Option<u16>,
    /// `[[gateway.llm]]` — LLM providers the host proxies through agentgateway's
    /// native `ai` backends (ADR-0012). Each becomes a `/llm/<name>` route with
    /// the provider API key injected host-side (ADR-0005: the key never leaves
    /// the host). Empty (the default) → no LLM proxy, MCP-only behavior.
    #[serde(default)]
    pub llm: Vec<LlmProvider>,
}

/// One `[[gateway.llm]]` provider (ADR-0012). The host renders it into an
/// agentgateway `ai` route at `/llm/<name>` and injects the provider key
/// host-side. Clients pick the provider by URL and POST an OpenAI-style chat
/// request; agentgateway translates it to the provider's native API.
///
/// ```toml
/// [[gateway.llm]]
/// name     = "claude"                      # path segment + backend name (unique, path-safe)
/// provider = "anthropic"                   # one of agentgateway's AI providers
/// model    = "claude-3-5-haiku-20241022"   # OPTIONAL; omit ⇒ passthrough (client's body `model`)
/// key      = "env://ANTHROPIC_API_KEY"     # env-ref; lowered to agentgateway's "$VAR"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmProvider {
    /// Path segment + agentgateway backend name. Unique across providers and
    /// path-safe (alphanumeric / dash / underscore — validated at load).
    pub name: String,
    /// The AI provider, one of agentgateway's supported set (see
    /// [`KNOWN_PROVIDERS`]). Validated at load.
    pub provider: String,
    /// OPTIONAL model pin. Omitted ⇒ passthrough: the client's request-body
    /// `model` is honored by agentgateway.
    #[serde(default)]
    pub model: Option<String>,
    /// The provider API key as an `env://NAME` reference (mirrors the inject
    /// `env://` rule so the secret never lands in a replicated doc). Lowered to
    /// agentgateway's `"$NAME"` substitution; the gateway child inherits the
    /// host env (dotenvy `.env`) so the key resolves host-side at the boundary.
    pub key: String,
}

/// agentgateway's supported AI providers (the `provider` tag set on an `ai`
/// backend) PLUS the OpenAI-compatible providers Tangram renders onto
/// agentgateway's `openAI` provider with a host override (see
/// [`openai_compatible_host`]). Declared providers are validated against this at
/// load so a typo is a clear config error, not a runtime route the gateway
/// silently rejects.
///
/// The native providers (`openai`/`anthropic`/`gemini`/`vertex`/`bedrock`/
/// `groq`) render to their own `{ "<provider>": { model } }` block. The
/// OpenAI-compatible ones (`deepseek`, …) have NO native agentgateway provider —
/// they expose an OpenAI-shaped `/v1/chat/completions` API, so they render to
/// the `openAI` provider with `hostOverride`/`pathOverride`/`backendTLS` pointing
/// at the vendor host (ADR-0012, OpenAI-compatible mechanism).
pub const KNOWN_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "vertex",
    "bedrock",
    "groq",
    "deepseek",
];

/// OpenAI-compatible providers that have NO native agentgateway provider: they
/// expose an OpenAI-shaped chat-completions API at a vendor host, so the host
/// renders them onto agentgateway's `openAI` provider with a host override
/// (ADR-0012). Returns `Some((host_port, path))` for an OpenAI-compatible
/// provider — `host_port` is the `"host:port"` string agentgateway's
/// `hostOverride` wants and `path` the upstream chat-completions path
/// (`pathOverride`) — or `None` for a NATIVE provider (which keeps the existing
/// `{ "<provider>": { model } }` rendering, no override).
///
/// The empirically-confirmed standalone-config shape (agentgateway v1.2.1): the
/// `ai` backend takes `provider: { openAI: { model } }`, a `hostOverride`
/// `"host:port"` STRING (port required), a `pathOverride` string, and the route
/// needs `policies.backendTLS = {}` so the upstream hop is TLS (the native
/// providers get TLS automatically; an overridden host does not). Adding a new
/// compatible vendor (mistral/together/fireworks/…) is one line here.
pub fn openai_compatible_host(provider: &str) -> Option<(&'static str, &'static str)> {
    match provider {
        "deepseek" => Some(("api.deepseek.com:443", "/v1/chat/completions")),
        _ => None,
    }
}

impl LlmProvider {
    /// Validate this provider entry: a non-empty, path-safe, unique `name`
    /// (checked against `seen`); a `provider` in [`KNOWN_PROVIDERS`]; and a
    /// `key` that is an `env://NAME` reference (the secret never lands inline).
    pub fn validate(&self, seen: &mut std::collections::BTreeSet<String>) -> anyhow::Result<()> {
        crate::config::validate_name(&self.name)
            .with_context(|| format!("[[gateway.llm]] name {:?}", self.name))?;
        anyhow::ensure!(
            seen.insert(self.name.clone()),
            "[[gateway.llm]] name {:?} is declared more than once (names map to /llm/<name> \
             routes and must be unique)",
            self.name
        );
        anyhow::ensure!(
            KNOWN_PROVIDERS.contains(&self.provider.as_str()),
            "[[gateway.llm]] {:?}: provider {:?} is not one of agentgateway's AI providers ({})",
            self.name,
            self.provider,
            KNOWN_PROVIDERS.join(", ")
        );
        self.env_var()?;
        Ok(())
    }

    /// The env var name behind the `env://NAME` key reference. Errors when the
    /// key is not an `env://` ref (an inline key would land the secret in the
    /// spec — refused, mirroring the inject rule).
    pub fn env_var(&self) -> anyhow::Result<&str> {
        let name = self.key.strip_prefix("env://").ok_or_else(|| {
            anyhow::anyhow!(
                "[[gateway.llm]] {:?}: key {:?} must be an env reference of the form \
                 \"env://VAR_NAME\" (the plaintext key stays host-side, never in the spec)",
                self.name,
                self.key
            )
        })?;
        anyhow::ensure!(
            !name.trim().is_empty(),
            "[[gateway.llm]] {:?}: key \"env://\" is missing the variable name",
            self.name
        );
        Ok(name)
    }
}

impl GatewaySettings {
    /// The binary to run: the configured path (if it exists), else
    /// `agentgateway` found on $PATH. `None` triggers the direct-serving
    /// fallback.
    pub fn resolve_binary(&self) -> Option<PathBuf> {
        match &self.binary {
            Some(path) => is_executable(path).then(|| path.clone()),
            None => find_on_path("agentgateway"),
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file() && std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

// ── config generation ────────────────────────────────────────────────────────

/// agentgateway 1.2 always binds its data plane on the wildcard address, so
/// every generated route enforces loopback-only sources — the host (and
/// local processes, which could reach the internal endpoints anyway) stay,
/// the network is shut out, and the public surface remains the host's one
/// port.
const LOOPBACK_RULE: &str =
    r#"string(source.address).startsWith("127.") || string(source.address) == "::1""#;

/// agentgateway target names may not contain underscores (they become the
/// `<target>_<tool>` namespace separator on the aggregate endpoint).
fn target_name(app: &str) -> String {
    app.replace('_', "-")
}

fn loopback_policy() -> Value {
    json!({ "authorization": { "rules": [LOOPBACK_RULE] } })
}

fn mcp_target(prefix: &str, app: &str, internal_port: u16) -> Value {
    json!({
        "name": target_name(app),
        "mcp": { "host": format!("http://127.0.0.1:{internal_port}{prefix}/mcp") }
    })
}

/// One per-app MCP route: `<prefix>/mcp` → the same path on the host's
/// internal listener (tool names unchanged).
fn app_route(route_name: String, prefix: &str, app: &str, internal_port: u16) -> Value {
    json!({
        "name": route_name,
        "policies": loopback_policy(),
        "matches": [{ "path": { "pathPrefix": format!("{prefix}/mcp") } }],
        "backends": [{ "mcp": { "targets": [mcp_target(prefix, app, internal_port)] } }]
    })
}

/// One aggregate route multiplexing `apps` (already scoped: the global
/// top-level set, or one tenant's set) as namespaced targets.
fn aggregate_route(route_name: &str, path: &str, apps: &[&AppKey], internal_port: u16) -> Value {
    let mut seen = std::collections::BTreeSet::new();
    let targets: Vec<Value> = apps
        .iter()
        .filter(|key| {
            // `a_b` and `a-b` both map to target `a-b`; keep the first and
            // warn rather than emit a config agentgateway rejects.
            let kept = seen.insert(target_name(&key.app));
            if !kept {
                tracing::warn!(
                    "{key}: target name {:?} collides on the aggregate {path} endpoint — \
                     skipped (rename the app)",
                    target_name(&key.app)
                );
            }
            kept
        })
        .map(|key| mcp_target(&key.route_prefix(), &key.app, internal_port))
        .collect();
    json!({
        "name": route_name,
        "policies": loopback_policy(),
        "matches": [{ "path": { "pathPrefix": path } }],
        "backends": [{ "mcp": { "targets": targets } }]
    })
}

/// One LLM proxy route (ADR-0012): `/llm/<name>` → agentgateway's native `ai`
/// backend for the provider, with the provider API key injected host-side via
/// `backendAuth` (`key="env://VAR"` lowered to `"$VAR"`; the gateway child
/// inherits the host env). Carries the SAME loopback-only authorization rule
/// every MCP route does — the gateway binds wildcard, so the LLM spend surface
/// is unreachable from off the box (ADR-0012 §security; v1 is loopback-trusted,
/// non-loopback exposure MUST first gate per-principal). A `model` is emitted
/// only when pinned; omitted ⇒ passthrough (the client's body `model`).
///
/// An OpenAI-COMPATIBLE provider (one with NO native agentgateway provider —
/// see [`openai_compatible_host`], e.g. `deepseek`) renders onto agentgateway's
/// `openAI` provider with a `hostOverride`/`pathOverride` pointing at the vendor
/// host plus `backendTLS` (the overridden hop needs TLS). A NATIVE provider
/// keeps the existing `{ "<provider>": { model } }` rendering with no override.
fn llm_route(provider: &LlmProvider) -> Value {
    let env_var = provider
        .env_var()
        .expect("validated at load: key is an env:// ref");

    // Loopback-only authorization + the host-injected key (the `$VAR` env ref —
    // the plaintext key never lands in the rendered config).
    let mut policies = serde_json::Map::new();
    policies.insert(
        "authorization".to_string(),
        json!({ "rules": [LOOPBACK_RULE] }),
    );
    policies.insert(
        "backendAuth".to_string(),
        json!({ "key": format!("${env_var}") }),
    );

    // The model goes under the `openAI` block (compatible) or the native
    // provider block; either way `{ "model": … }` only when pinned.
    let mut model_block = serde_json::Map::new();
    if let Some(model) = &provider.model {
        model_block.insert("model".to_string(), json!(model));
    }

    let mut ai = serde_json::Map::new();
    ai.insert("name".to_string(), json!(provider.name));

    match openai_compatible_host(&provider.provider) {
        // OpenAI-compatible: agentgateway's `openAI` provider + a host/path
        // override at the vendor host, and `backendTLS` so the overridden hop is
        // TLS (the native providers get TLS automatically; an override does not).
        Some((host_port, path)) => {
            ai.insert(
                "provider".to_string(),
                json!({ "openAI": Value::Object(model_block) }),
            );
            ai.insert("hostOverride".to_string(), json!(host_port));
            ai.insert("pathOverride".to_string(), json!(path));
            policies.insert("backendTLS".to_string(), json!({}));
        }
        // Native provider: `{ "<provider>": { "model": … } }` — the tag is
        // dynamic, so build the one-key map directly (json! keys must be
        // literals). No host override, no explicit backendTLS.
        None => {
            let mut provider_map = serde_json::Map::new();
            provider_map.insert(provider.provider.clone(), Value::Object(model_block));
            ai.insert("provider".to_string(), Value::Object(provider_map));
        }
    }

    json!({
        "name": format!("llm-{}", provider.name),
        "policies": Value::Object(policies),
        "matches": [{ "path": { "pathPrefix": format!("/llm/{}", provider.name) } }],
        "backends": [{ "ai": Value::Object(ai) }]
    })
}

/// Render the full agentgateway config for the given RUNNING apps: one
/// per-app route (`/<app>/mcp` or `/t/<tenant>/<app>/mcp` → the same path on
/// the host's internal listener, tool names unchanged), the aggregate `/mcp`
/// route multiplexing the TOP-LEVEL apps only, and one aggregate
/// `/t/<tenant>/mcp` per tenant multiplexing exactly that tenant's apps —
/// tenant tools never appear on the global aggregate, and the host's
/// internal endpoints still enforce the tenant bearer (the gateway forwards
/// Authorization). Deterministic for a given input (apps are sorted), so
/// converge can diff bytes to decide whether to rewrite.
///
/// Appends one LLM proxy route per `llm` provider (ADR-0012): a `/llm/<name>`
/// `ai` backend with the host-injected key. These are startup config (the
/// provider list is fixed; only the MCP routes change as apps converge), but
/// they ride along in every render so the file stays the single source.
pub fn render_config(
    apps: &[AppKey],
    llm: &[LlmProvider],
    gateway_port: u16,
    internal_port: u16,
) -> Value {
    let mut apps: Vec<&AppKey> = apps.iter().collect();
    apps.sort();
    apps.dedup();
    let top: Vec<&AppKey> = apps
        .iter()
        .copied()
        .filter(|key| key.tenant.is_none())
        .collect();
    let mut tenants: std::collections::BTreeMap<&str, Vec<&AppKey>> =
        std::collections::BTreeMap::new();
    for key in &apps {
        if let Some(tenant) = key.tenant.as_deref() {
            tenants.entry(tenant).or_default().push(key);
        }
    }

    let mut routes = Vec::with_capacity(apps.len() + tenants.len() + 1);
    for key in &top {
        routes.push(app_route(
            format!("{}-mcp", key.app),
            &key.route_prefix(),
            &key.app,
            internal_port,
        ));
    }
    for (tenant, keys) in &tenants {
        for key in keys {
            routes.push(app_route(
                format!("t-{tenant}-{}-mcp", key.app),
                &key.route_prefix(),
                &key.app,
                internal_port,
            ));
        }
        // The per-tenant aggregate: that tenant's tools and no one else's.
        routes.push(aggregate_route(
            &format!("t-{tenant}-mcp-aggregate"),
            &format!("/t/{tenant}/mcp"),
            keys,
            internal_port,
        ));
    }
    // The global aggregate last (order is irrelevant for matching — the
    // prefixes are disjoint — but humans read this file). Top-level apps
    // only: tenant apps must never leak onto the public aggregate.
    routes.push(aggregate_route(
        "mcp-aggregate",
        "/mcp",
        &top,
        internal_port,
    ));

    // The LLM proxy routes (ADR-0012), after the MCP routes: one `/llm/<name>`
    // `ai` backend per declared provider, each loopback-gated with the key
    // injected host-side. Disjoint prefixes from `/mcp`, so order is cosmetic.
    for provider in llm {
        routes.push(llm_route(provider));
    }

    json!({
        // The admin/stats/readiness listeners are pinned to ephemeral
        // loopback ports — never a public surface, never a port conflict.
        "config": {
            "adminAddr": "127.0.0.1:0",
            "statsAddr": "127.0.0.1:0",
            "readinessAddr": "127.0.0.1:0"
        },
        "binds": [{
            "port": gateway_port,
            "listeners": [{ "routes": routes }]
        }]
    })
}

// ── restart backoff (pure state machine, unit-tested) ────────────────────────

/// Restart backoff for the supervised child: doubles on quick crashes
/// (500 ms → 10 s cap), resets once a run survives 30 s.
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    const MIN: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(10);
    /// A run at least this long counts as healthy and resets the backoff.
    const STABLE: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        Self { next: Self::MIN }
    }

    /// The delay before the next restart, given how long the child ran.
    pub fn after_exit(&mut self, uptime: Duration) -> Duration {
        if uptime >= Self::STABLE {
            self.next = Self::MIN;
        }
        let delay = self.next;
        self.next = (delay * 2).min(Self::MAX);
        delay
    }
}

// ── the running gateway ──────────────────────────────────────────────────────

/// A host-managed agentgateway instance: generated config + supervised child
/// process + the reverse proxy the public listener uses for MCP paths.
pub struct Gateway {
    binary: PathBuf,
    /// The gateway's internal listen port (loopback use only; see
    /// [`LOOPBACK_RULE`]).
    pub port: u16,
    /// The host's internal listener that agentgateway targets per app.
    pub internal_port: u16,
    /// `[[gateway.llm]]` providers rendered into `/llm/<name>` `ai` routes
    /// (ADR-0012). Fixed at startup (startup config, like the ports).
    llm: Vec<LlmProvider>,
    config_path: PathBuf,
    client: reqwest::Client,
    running: AtomicBool,
    child_pid: AtomicU32,
    shutdown: watch::Sender<bool>,
}

impl Gateway {
    pub fn new(
        binary: PathBuf,
        port: u16,
        internal_port: u16,
        llm: Vec<LlmProvider>,
        config_path: PathBuf,
    ) -> Self {
        Self {
            binary,
            port,
            internal_port,
            llm,
            config_path,
            client: reqwest::Client::new(),
            running: AtomicBool::new(false),
            child_pid: AtomicU32::new(0),
            shutdown: watch::Sender::new(false),
        }
    }

    /// Is the child process currently alive?
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// The live child's PID (0 = not running).
    pub fn child_pid(&self) -> u32 {
        self.child_pid.load(Ordering::Relaxed)
    }

    /// Regenerate the config for the given running apps; written atomically
    /// (tmp + rename) and only when the bytes changed — agentgateway watches
    /// the file and hot-reloads. Called by every converge pass.
    pub fn sync_apps(&self, apps: &[AppKey]) {
        let rendered = render_config(apps, &self.llm, self.port, self.internal_port);
        let bytes = serde_json::to_vec_pretty(&rendered).expect("static json renders");
        match write_if_changed(&self.config_path, &bytes) {
            Ok(true) => tracing::info!(
                "gateway: config regenerated ({} apps) — agentgateway hot-reloads {}",
                apps.len(),
                self.config_path.display()
            ),
            Ok(false) => {}
            Err(e) => tracing::error!("gateway: writing config failed: {e:#}"),
        }
    }

    /// Supervise the child: spawn, forward its output to tracing, restart on
    /// crash with [`Backoff`], kill on host shutdown.
    pub fn spawn_supervisor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let gw = self.clone();
        tokio::spawn(async move {
            let mut shutdown = gw.shutdown.subscribe();
            let mut backoff = Backoff::new();
            loop {
                if *shutdown.borrow() {
                    return;
                }
                let mut child = match tokio::process::Command::new(&gw.binary)
                    .arg("-f")
                    .arg(&gw.config_path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(child) => child,
                    Err(e) => {
                        tracing::error!("gateway: failed to spawn {}: {e}", gw.binary.display());
                        tokio::select! {
                            _ = tokio::time::sleep(backoff.after_exit(Duration::ZERO)) => continue,
                            _ = shutdown.changed() => return,
                        }
                    }
                };
                let pid = child.id().unwrap_or(0);
                gw.child_pid.store(pid, Ordering::Relaxed);
                gw.running.store(true, Ordering::Relaxed);
                tracing::info!(
                    "gateway: agentgateway running (pid {pid}, mcp on 127.0.0.1:{})",
                    gw.port
                );
                forward_output(&mut child);

                let started = std::time::Instant::now();
                tokio::select! {
                    status = child.wait() => {
                        gw.running.store(false, Ordering::Relaxed);
                        gw.child_pid.store(0, Ordering::Relaxed);
                        let delay = backoff.after_exit(started.elapsed());
                        tracing::warn!(
                            "gateway: agentgateway exited ({status:?}) — restarting in {delay:?}"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = shutdown.changed() => return,
                        }
                    }
                    _ = shutdown.changed() => {
                        gw.running.store(false, Ordering::Relaxed);
                        gw.child_pid.store(0, Ordering::Relaxed);
                        let _ = child.kill().await;
                        tracing::info!("gateway: agentgateway stopped");
                        return;
                    }
                }
            }
        })
    }

    /// Stop the supervisor and kill the child (used on Ctrl-C; the child is
    /// also `kill_on_drop` as a backstop).
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Reverse-proxy one MCP request to the gateway, streaming both ways —
    /// SSE responses (and the `Mcp-Session-Id` header) pass through intact.
    pub async fn proxy(&self, req: Request) -> Response {
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| req.uri().path().to_string());
        let url = format!("http://127.0.0.1:{}{path_and_query}", self.port);
        let (parts, body) = req.into_parts();

        let mut upstream = self.client.request(parts.method, &url);
        for (name, value) in &parts.headers {
            if !skip_request_header(name) {
                upstream = upstream.header(name, value);
            }
        }
        let upstream = upstream.body(reqwest::Body::wrap_stream(body.into_data_stream()));

        match upstream.send().await {
            Ok(resp) => {
                let mut out = Response::builder().status(resp.status().as_u16());
                if let Some(headers) = out.headers_mut() {
                    for (name, value) in resp.headers() {
                        if !skip_response_header(name) {
                            headers.insert(name.clone(), value.clone());
                        }
                    }
                }
                out.body(Body::from_stream(resp.bytes_stream()))
                    .unwrap_or_else(|e| {
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("proxy: {e}")).into_response()
                    })
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                format!("mcp gateway unreachable (restarting?): {e}"),
            )
                .into_response(),
        }
    }
}

/// Forward the child's stdout/stderr lines into tracing under the
/// `agentgateway` target (its access log is the routing-hop evidence).
fn forward_output(child: &mut tokio::process::Child) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "agentgateway", "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "agentgateway", "{line}");
            }
        });
    }
}

/// Hop-by-hop request headers (plus Host, which reqwest derives from the
/// URL) that must not be forwarded.
fn skip_request_header(name: &HeaderName) -> bool {
    name == header::HOST
        || name == header::CONNECTION
        || name == header::TE
        || name == header::TRAILER
        || name == header::TRANSFER_ENCODING
        || name == header::UPGRADE
}

fn skip_response_header(name: &HeaderName) -> bool {
    name == header::CONNECTION || name == header::TRANSFER_ENCODING
}

/// Atomic write (tmp + rename), skipped when the content is unchanged so
/// agentgateway only reloads on real config changes. Returns whether the
/// file was (re)written.
fn write_if_changed(path: &Path, bytes: &[u8]) -> anyhow::Result<bool> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(false);
    }
    let dir = path.parent().context("config path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Find a free loopback port (bind-and-drop; the tiny race is acceptable for
/// a port the host hands to its own child immediately).
pub fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// Where the generated config lives: the host's own data root,
/// `$HOME/.tangram-host` (`./data/tangram-host` without a HOME) — never
/// hand-edited, regenerated on every converge.
pub fn default_config_file() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".tangram-host/agentgateway.json"),
        Err(_) => PathBuf::from("data/tangram-host/agentgateway.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_and_default_off() {
        let parsed: GatewaySettings = toml::from_str("enabled = true").unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.binary, None);
        assert_eq!(parsed.port, None);
        let parsed: GatewaySettings =
            toml::from_str("enabled = true\nbinary = \"/x/agentgateway\"\nport = 19200").unwrap();
        assert_eq!(parsed.binary, Some(PathBuf::from("/x/agentgateway")));
        assert_eq!(parsed.port, Some(19200));
        assert!(!GatewaySettings::default().enabled, "default is off");
    }

    #[test]
    fn renders_llm_routes_after_mcp_routes() {
        // Two providers: a model-pinned one and a passthrough (no model).
        let llm = vec![
            LlmProvider {
                name: "claude".into(),
                provider: "anthropic".into(),
                model: Some("claude-3-5-haiku-20241022".into()),
                key: "env://ANTHROPIC_API_KEY".into(),
            },
            LlmProvider {
                name: "gpt".into(),
                provider: "openai".into(),
                model: None,
                key: "env://OPENAI_API_KEY".into(),
            },
        ];
        let config = render_config(&[AppKey::top("notes")], &llm, 19200, 19300);
        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .expect("routes");
        // 1 per-app MCP + 1 aggregate MCP + 2 LLM = 4, LLM routes LAST.
        assert_eq!(routes.len(), 4);

        // The model-pinned provider.
        let claude = &routes[2];
        assert_eq!(claude["name"], "llm-claude");
        assert_eq!(claude["matches"][0]["path"]["pathPrefix"], "/llm/claude");
        let ai = &claude["backends"][0]["ai"];
        assert_eq!(ai["name"], "claude");
        assert_eq!(
            ai["provider"]["anthropic"]["model"],
            "claude-3-5-haiku-20241022"
        );
        // The key is lowered to agentgateway's "$VAR" substitution (the
        // plaintext key never appears in the rendered config).
        assert_eq!(
            claude["policies"]["backendAuth"]["key"],
            "$ANTHROPIC_API_KEY"
        );
        // Loopback-only authorization rule, same hardening as every MCP route.
        let rule = claude["policies"]["authorization"]["rules"][0]
            .as_str()
            .unwrap();
        assert!(rule.contains("source.address"));

        // The passthrough provider: NO `model` key (client's body model wins).
        let gpt = &routes[3];
        assert_eq!(gpt["name"], "llm-gpt");
        assert_eq!(gpt["matches"][0]["path"]["pathPrefix"], "/llm/gpt");
        let openai = &gpt["backends"][0]["ai"]["provider"]["openai"];
        assert!(
            openai.get("model").is_none(),
            "omitted model ⇒ no model field (passthrough), got {openai:?}"
        );
        assert_eq!(gpt["policies"]["backendAuth"]["key"], "$OPENAI_API_KEY");
    }

    #[test]
    fn renders_openai_compatible_deepseek_with_host_override() {
        // DeepSeek has no native agentgateway provider: it renders onto the
        // `openAI` provider with a host/path override at api.deepseek.com plus
        // backendTLS. The exact shape was confirmed empirically against
        // agentgateway v1.2.1 (a bogus key returns a DeepSeek-side 401).
        let llm = vec![LlmProvider {
            name: "deepseek".into(),
            provider: "deepseek".into(),
            model: Some("deepseek-chat".into()),
            key: "env://DEEPSEEK_API_KEY".into(),
        }];
        let config = render_config(&[AppKey::top("notes")], &llm, 19200, 19300);
        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .expect("routes");
        // 1 per-app MCP + 1 aggregate MCP + 1 LLM.
        assert_eq!(routes.len(), 3);
        let ds = &routes[2];
        assert_eq!(ds["name"], "llm-deepseek");
        assert_eq!(ds["matches"][0]["path"]["pathPrefix"], "/llm/deepseek");

        let ai = &ds["backends"][0]["ai"];
        assert_eq!(ai["name"], "deepseek");
        // Rendered onto the `openAI` provider (NOT a `deepseek` provider tag —
        // agentgateway has none), with the model under it.
        assert_eq!(ai["provider"]["openAI"]["model"], "deepseek-chat");
        assert!(
            ai["provider"].get("deepseek").is_none(),
            "must not emit a native `deepseek` provider tag: {ai}"
        );
        // The host/path override aim the OpenAI provider at the DeepSeek host.
        assert_eq!(ai["hostOverride"], "api.deepseek.com:443");
        assert_eq!(ai["pathOverride"], "/v1/chat/completions");

        // backendTLS is present (the overridden upstream hop must be TLS).
        assert!(
            ds["policies"]["backendTLS"].is_object(),
            "openai-compatible route needs backendTLS: {ds}"
        );
        // The key is still the host-injected env ref, loopback rule still there.
        assert_eq!(ds["policies"]["backendAuth"]["key"], "$DEEPSEEK_API_KEY");
        assert!(
            ds["policies"]["authorization"]["rules"][0]
                .as_str()
                .unwrap()
                .contains("source.address")
        );

        // A NATIVE provider gets NO host override and NO explicit backendTLS.
        let native = vec![LlmProvider {
            name: "gpt".into(),
            provider: "openai".into(),
            model: None,
            key: "env://OPENAI_API_KEY".into(),
        }];
        let config = render_config(&[], &native, 1, 2);
        let route = &config["binds"][0]["listeners"][0]["routes"][1];
        let ai = &route["backends"][0]["ai"];
        assert!(ai.get("hostOverride").is_none(), "native: no host override");
        assert!(ai.get("pathOverride").is_none(), "native: no path override");
        assert!(
            route["policies"].get("backendTLS").is_none(),
            "native: TLS is automatic, no explicit backendTLS"
        );
        // Native still uses the `{ "<provider>": {…} }` tag.
        assert!(ai["provider"]["openai"].is_object());
    }

    #[test]
    fn llm_provider_validation() {
        let mut seen = std::collections::BTreeSet::new();
        // Valid entry parses.
        let ok = LlmProvider {
            name: "claude".into(),
            provider: "anthropic".into(),
            model: None,
            key: "env://ANTHROPIC_API_KEY".into(),
        };
        assert!(ok.validate(&mut seen).is_ok());
        // Duplicate name rejected (same `seen` set).
        let dup = LlmProvider {
            name: "claude".into(),
            provider: "openai".into(),
            model: None,
            key: "env://OPENAI_API_KEY".into(),
        };
        assert!(dup.validate(&mut seen).is_err(), "dup name");
        // Unknown provider rejected.
        let mut s2 = std::collections::BTreeSet::new();
        let bad_provider = LlmProvider {
            name: "x".into(),
            provider: "notaprovider".into(),
            model: None,
            key: "env://K".into(),
        };
        assert!(bad_provider.validate(&mut s2).is_err(), "bad provider");
        // Non-env key rejected (the secret must stay host-side).
        let mut s3 = std::collections::BTreeSet::new();
        let inline_key = LlmProvider {
            name: "y".into(),
            provider: "openai".into(),
            model: None,
            key: "sk-literal-secret".into(),
        };
        assert!(inline_key.validate(&mut s3).is_err(), "inline key");
        // Path-unsafe name rejected.
        let mut s4 = std::collections::BTreeSet::new();
        let bad_name = LlmProvider {
            name: "bad/name".into(),
            provider: "openai".into(),
            model: None,
            key: "env://K".into(),
        };
        assert!(bad_name.validate(&mut s4).is_err(), "path-unsafe name");
    }

    #[test]
    fn renders_per_app_and_aggregate_routes() {
        let apps = vec![
            AppKey::top("nutrition"),
            AppKey::top("notes"),
            AppKey::top("registry"),
        ];
        let config = render_config(&apps, &[], 19200, 19300);

        // One public-ish surface knob: the bind port; admin planes pinned to
        // ephemeral loopback.
        assert_eq!(config["binds"][0]["port"], 19200);
        assert_eq!(config["config"]["adminAddr"], "127.0.0.1:0");
        assert_eq!(config["config"]["statsAddr"], "127.0.0.1:0");
        assert_eq!(config["config"]["readinessAddr"], "127.0.0.1:0");

        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .expect("routes");
        assert_eq!(routes.len(), 4, "3 per-app + 1 aggregate");

        // Sorted per-app routes, each a single target at the internal port.
        for (i, app) in ["notes", "nutrition", "registry"].iter().enumerate() {
            let route = &routes[i];
            assert_eq!(route["name"], format!("{app}-mcp"));
            assert_eq!(
                route["matches"][0]["path"]["pathPrefix"],
                format!("/{app}/mcp")
            );
            let targets = route["backends"][0]["mcp"]["targets"]
                .as_array()
                .expect("targets");
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0]["name"], *app);
            assert_eq!(
                targets[0]["mcp"]["host"],
                format!("http://127.0.0.1:19300/{app}/mcp")
            );
        }

        // Aggregate multiplexes every app.
        let aggregate = &routes[3];
        assert_eq!(aggregate["name"], "mcp-aggregate");
        assert_eq!(aggregate["matches"][0]["path"]["pathPrefix"], "/mcp");
        let targets = aggregate["backends"][0]["mcp"]["targets"]
            .as_array()
            .expect("aggregate targets");
        let names: Vec<&str> = targets.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["notes", "nutrition", "registry"]);

        // Every route is loopback-gated (the bind is a wildcard socket).
        for route in routes {
            let rules = route["policies"]["authorization"]["rules"]
                .as_array()
                .expect("authz rules");
            assert_eq!(rules.len(), 1);
            assert!(rules[0].as_str().unwrap().contains("source.address"));
        }
    }

    #[test]
    fn render_scopes_tenant_apps_off_the_global_aggregate() {
        let apps = vec![
            AppKey::top("notes"),
            AppKey::tenant("alice", "notes"),
            AppKey::tenant("alice", "todo"),
            AppKey::tenant("bob", "notes"),
        ];
        let config = render_config(&apps, &[], 19200, 19300);
        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .expect("routes");
        // 1 top per-app + 3 tenant per-app + 2 tenant aggregates + 1 global.
        assert_eq!(routes.len(), 7);

        let by_name = |name: &str| -> &Value {
            routes
                .iter()
                .find(|r| r["name"] == name)
                .unwrap_or_else(|| panic!("route {name}"))
        };

        // Tenant per-app routes match and target the namespaced path.
        let alice_notes = by_name("t-alice-notes-mcp");
        assert_eq!(
            alice_notes["matches"][0]["path"]["pathPrefix"],
            "/t/alice/notes/mcp"
        );
        assert_eq!(
            alice_notes["backends"][0]["mcp"]["targets"][0]["mcp"]["host"],
            "http://127.0.0.1:19300/t/alice/notes/mcp"
        );

        // The per-tenant aggregate lists exactly that tenant's apps…
        let alice_aggregate = by_name("t-alice-mcp-aggregate");
        assert_eq!(
            alice_aggregate["matches"][0]["path"]["pathPrefix"],
            "/t/alice/mcp"
        );
        let names: Vec<&str> = alice_aggregate["backends"][0]["mcp"]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names, ["notes", "todo"]);
        let bob_aggregate = by_name("t-bob-mcp-aggregate");
        let names: Vec<&str> = bob_aggregate["backends"][0]["mcp"]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names, ["notes"]);

        // …and the GLOBAL aggregate excludes tenant apps entirely.
        let global = by_name("mcp-aggregate");
        let targets = global["backends"][0]["mcp"]["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0]["mcp"]["host"],
            "http://127.0.0.1:19300/notes/mcp"
        );

        // Every route (tenant ones included) keeps the loopback source rule.
        for route in routes {
            assert!(
                route["policies"]["authorization"]["rules"][0]
                    .as_str()
                    .unwrap()
                    .contains("source.address")
            );
        }
    }

    #[test]
    fn render_sanitizes_underscores_and_dedupes_collisions() {
        let apps = vec![AppKey::top("my_app"), AppKey::top("my-app")];
        let config = render_config(&apps, &[], 1, 2);
        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .unwrap();
        // Both per-app routes exist (paths keep the real app name)…
        assert_eq!(routes[0]["matches"][0]["path"]["pathPrefix"], "/my-app/mcp");
        assert_eq!(routes[1]["matches"][0]["path"]["pathPrefix"], "/my_app/mcp");
        // …and the per-app target for my_app is sanitized (no underscores —
        // agentgateway rejects them in target names).
        assert_eq!(
            routes[1]["backends"][0]["mcp"]["targets"][0]["name"],
            "my-app"
        );
        // The aggregate keeps only the first of the colliding pair.
        let aggregate = routes.last().unwrap();
        let targets = aggregate["backends"][0]["mcp"]["targets"]
            .as_array()
            .unwrap();
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn render_with_no_apps_keeps_the_aggregate_bind() {
        let config = render_config(&[], &[], 19200, 19300);
        let routes = config["binds"][0]["listeners"][0]["routes"]
            .as_array()
            .unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["name"], "mcp-aggregate");
        assert_eq!(
            routes[0]["backends"][0]["mcp"]["targets"],
            serde_json::json!([])
        );
    }

    #[test]
    fn render_is_deterministic_for_unsorted_input() {
        let a = render_config(&[AppKey::top("b"), AppKey::top("a")], &[], 1, 2);
        let b = render_config(
            &[AppKey::top("a"), AppKey::top("b"), AppKey::top("a")],
            &[],
            1,
            2,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn backoff_doubles_on_crash_loop_and_resets_after_stable_run() {
        let mut backoff = Backoff::new();
        let crash = Duration::from_millis(10);
        assert_eq!(backoff.after_exit(crash), Duration::from_millis(500));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(1));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(2));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(4));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(8));
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(10), "capped");
        assert_eq!(
            backoff.after_exit(crash),
            Duration::from_secs(10),
            "stays capped"
        );
        // A stable run resets the ladder.
        assert_eq!(
            backoff.after_exit(Duration::from_secs(31)),
            Duration::from_millis(500)
        );
        assert_eq!(backoff.after_exit(crash), Duration::from_secs(1));
    }

    #[test]
    fn write_if_changed_diffs_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.json");
        assert!(write_if_changed(&path, b"v1").unwrap(), "first write");
        assert!(
            !write_if_changed(&path, b"v1").unwrap(),
            "unchanged → no-op"
        );
        assert!(
            write_if_changed(&path, b"v2").unwrap(),
            "changed → rewritten"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"v2");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "tmp file renamed away"
        );
    }
}
