//! The embedded Wasmtime side: one live component instance per app
//! (instantiate once, call repeatedly — the doc-actor lifecycle a
//! per-request `wasmtime serve` model can't give us), with the host
//! implementing the `tangram:app/host` capabilities. What the host
//! implements IS the grant: `http-fetch` enforces the app's outbound host
//! allowlist, and there is no filesystem/socket/inbound-http capability for
//! a component to even name. The wasip2 std plumbing (env, clocks, random,
//! stdio) is linked with an empty WASI context — no preopens, no sockets.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use secrecy::ExposeSecret as _;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::config::{InjectKind, InjectRule};
use crate::secrets::SecretRegistry;

// In their own module so the generated `tangram::app::...` paths can't
// collide with the `tangram` SDK crate.
mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "app",
        // Host functions are async (http-fetch awaits reqwest); guest calls
        // are awaited by the host but synchronous inside the component.
        imports: { default: async },
        exports: { default: async },
    });
}

use bindings::App;
use bindings::tangram::app::host;

pub use bindings::exports::tangram::app::guest::DispatchResult;

/// Per-instance host state: the WASI plumbing plus the app's capability
/// grants.
pub struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    app: String,
    allow_hosts: Vec<String>,
    /// Egress credential-injection rules (ADR-0005, Phase 10b), keyed by
    /// lowercased outbound host. For a matching `http-fetch` the host
    /// resolves the rule's secret through `secrets` and attaches it just
    /// before the real request — the component never holds the plaintext.
    inject: Vec<(String, InjectKind, InjectRule)>,
    /// The secret-resolution seam, used host-side at request time to turn an
    /// inject rule's `scheme://locator` reference into a `SecretString`.
    secrets: Arc<SecretRegistry>,
    client: reqwest::Client,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl host::Host for HostState {
    /// One outbound HTTP request on the component's behalf, allowlist
    /// enforced. Async on the host (reqwest), synchronous from the guest's
    /// point of view.
    async fn http_fetch(&mut self, request_json: String) -> Result<String, String> {
        let request: serde_json::Value =
            serde_json::from_str(&request_json).map_err(|e| format!("malformed request: {e}"))?;
        let url = request
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or("request is missing url")?;
        let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url {url:?}: {e}"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| format!("url {url:?} has no host"))?;

        // The capability check: the app spec grants exact outbound hosts.
        if !self
            .allow_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
        {
            tracing::warn!(app = %self.app, %host, "denied outbound request (not in allow_hosts)");
            return Err(format!(
                "outbound request to {host:?} denied: it is not in app {:?}'s allow_hosts \
                 (granted: {:?}); add it to the app's allow_hosts in apps.toml to grant access",
                self.app, self.allow_hosts
            ));
        }

        // Egress credential injection (ADR-0005): if an injection rule
        // matches this (allowlisted) host, resolve its secret host-side and
        // attach the credential just before the real request — the component
        // issued a BARE request and never held the plaintext. A rule whose
        // secret does not resolve is skipped (the request goes out
        // unauthenticated → degraded, never a crash). The `SecretString`
        // lives only for this call and is never logged.
        let host_lc = host.to_ascii_lowercase();
        let injected = match self.inject.iter().find(|(h, _, _)| *h == host_lc) {
            Some((_, kind, rule)) => rule
                .resolve_secret(&self.secrets, &format!("{}: inject {host_lc}", self.app))
                .await
                .map(|secret| (kind.clone(), secret)),
            None => None,
        };

        let mut parsed = parsed;
        if let Some((InjectKind::Query(name), secret)) = &injected {
            // Query injection must mutate the URL before it is consumed by
            // the request builder.
            parsed
                .query_pairs_mut()
                .append_pair(name, secret.expose_secret());
        }

        let method: reqwest::Method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET")
            .parse()
            .map_err(|e| format!("invalid method: {e}"))?;
        let mut builder = self
            .client
            .request(method, parsed)
            .timeout(std::time::Duration::from_secs(30));
        if let Some(headers) = request
            .get("headers")
            .and_then(serde_json::Value::as_object)
        {
            for (name, value) in headers {
                builder = builder.header(name, value.as_str().unwrap_or_default());
            }
        }
        // Apply header/bearer injection AFTER the component's own headers, so
        // the host-attached credential is authoritative (a component cannot
        // pre-set the injected header to a value of its choosing).
        match &injected {
            Some((InjectKind::Header(name), secret)) => {
                builder = builder.header(name, secret.expose_secret());
            }
            Some((InjectKind::Bearer, secret)) => {
                builder = builder.bearer_auth(secret.expose_secret());
            }
            _ => {}
        }
        if let Some(b64) = request.get("body-b64").and_then(serde_json::Value::as_str)
            && !b64.is_empty()
        {
            let body = B64
                .decode(b64)
                .map_err(|e| format!("body-b64 is not base64: {e}"))?;
            builder = builder.body(body);
        }

        tracing::debug!(app = %self.app, %url, injected = injected.is_some(), "outbound http-fetch");
        let response = builder
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = response.status().as_u16();
        let headers: serde_json::Map<String, serde_json::Value> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    serde_json::Value::String(String::from_utf8_lossy(v.as_bytes()).into()),
                )
            })
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("reading response body: {e}"))?;
        Ok(serde_json::json!({
            "status": status,
            "headers": headers,
            "body-b64": B64.encode(&body),
        })
        .to_string())
    }

    async fn log(&mut self, level: String, message: String) {
        let app = &self.app;
        match level.as_str() {
            "error" => tracing::error!(%app, "{message}"),
            "warn" => tracing::warn!(%app, "{message}"),
            "debug" => tracing::debug!(%app, "{message}"),
            "trace" => tracing::trace!(%app, "{message}"),
            _ => tracing::info!(%app, "{message}"),
        }
    }

    async fn now_ms(&mut self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A live component instance plus its store, behind a mutex: guest calls are
/// synchronous and quick, so the host simply serializes them per app.
pub struct ComponentHandle {
    inner: tokio::sync::Mutex<Inner>,
}

struct Inner {
    store: Store<HostState>,
    bindings: App,
}

impl ComponentHandle {
    /// Compile + instantiate the component at `path` with this app's grants.
    /// `inject` carries the egress credential-injection rules (ADR-0005) and
    /// `secrets` the resolver seam used to turn their references into values
    /// host-side at request time — neither is ever exposed to the component.
    #[allow(clippy::too_many_arguments)]
    pub async fn instantiate(
        engine: &Engine,
        path: &std::path::Path,
        app: &str,
        allow_hosts: &[String],
        env: &[(String, String)],
        inject: Vec<(String, InjectKind, InjectRule)>,
        secrets: Arc<SecretRegistry>,
    ) -> anyhow::Result<Self> {
        let component = Component::from_file(engine, path)?;
        let mut linker = Linker::<HostState>::new(engine);
        // wasip2 std plumbing with an EMPTY context: no preopens, no
        // sockets — only env/clocks/random/stdio, which carry data, not
        // reach. Guest stderr is inherited for crash diagnostics.
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        host::add_to_linker::<_, HasSelf<HostState>>(&mut linker, |state| state)?;

        let mut wasi = WasiCtx::builder();
        wasi.inherit_stderr();
        for (key, value) in env {
            wasi.env(key, value);
        }
        let state = HostState {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            app: app.to_string(),
            allow_hosts: allow_hosts.to_vec(),
            inject,
            secrets,
            client: reqwest::Client::new(),
        };
        let mut store = Store::new(engine, state);
        let bindings = App::instantiate_async(&mut store, &component, &linker).await?;
        Ok(Self {
            inner: tokio::sync::Mutex::new(Inner { store, bindings }),
        })
    }

    pub async fn describe(&self) -> anyhow::Result<String> {
        let mut inner = self.inner.lock().await;
        let Inner { store, bindings } = &mut *inner;
        Ok(bindings.tangram_app_guest().call_describe(store).await?)
    }

    pub async fn genesis(&self) -> anyhow::Result<Vec<u8>> {
        let mut inner = self.inner.lock().await;
        let Inner { store, bindings } = &mut *inner;
        Ok(bindings.tangram_app_guest().call_genesis(store).await?)
    }

    /// One action dispatch. Outer error = trap/engine failure; inner error =
    /// the app's own action error (rendered like the SDK's `ActionError`).
    pub async fn dispatch(
        &self,
        action: &str,
        args_json: &str,
        doc: &[u8],
    ) -> anyhow::Result<Result<DispatchResult, String>> {
        let mut inner = self.inner.lock().await;
        let Inner { store, bindings } = &mut *inner;
        Ok(bindings
            .tangram_app_guest()
            .call_dispatch(store, action, args_json, doc)
            .await?)
    }

    pub async fn state_json(&self, doc: &[u8]) -> anyhow::Result<String> {
        let mut inner = self.inner.lock().await;
        let Inner { store, bindings } = &mut *inner;
        Ok(bindings
            .tangram_app_guest()
            .call_state_json(store, doc)
            .await?)
    }
}

/// The shared engine (async support is the default in this wasmtime).
///
/// The on-disk compilation cache (keyed by component content-hash + compiler
/// settings) is enabled so each unique component is cranelift-compiled once
/// and cheaply loaded thereafter — this speeds production cold starts (host
/// restart/reload) and removes the concurrent-compile contention that flaked
/// the 2-core CI integration tests (which spin up several hosts each
/// instantiating multiple components at once).
pub fn engine() -> anyhow::Result<Engine> {
    let cache_dir = match std::env::var("HOME") {
        Ok(home) => std::path::PathBuf::from(home).join(".tangram-host/wasmtime-cache"),
        Err(_) => std::path::PathBuf::from("data/tangram-host/wasmtime-cache"),
    };
    let mut config = wasmtime::Config::new();
    let mut cache = wasmtime::CacheConfig::new();
    cache.with_directory(&cache_dir);
    match wasmtime::Cache::new(cache) {
        Ok(cache) => {
            config.cache(Some(cache));
        }
        // A bad cache dir must never stop the host from running — just warn
        // and fall back to compiling every time.
        Err(e) => tracing::warn!("wasmtime compilation cache disabled ({e}); cold starts slower"),
    }
    Ok(Engine::new(&config)?)
}
