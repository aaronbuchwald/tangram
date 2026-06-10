//! The embedded Wasmtime side: one live component instance per app
//! (instantiate once, call repeatedly — the doc-actor lifecycle a
//! per-request `wasmtime serve` model can't give us), with the host
//! implementing the `tangram:app/host` capabilities. What the host
//! implements IS the grant: `http-fetch` enforces the app's outbound host
//! allowlist, and there is no filesystem/socket/inbound-http capability for
//! a component to even name. The wasip2 std plumbing (env, clocks, random,
//! stdio) is linked with an empty WASI context — no preopens, no sockets.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

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
        if let Some(b64) = request.get("body-b64").and_then(serde_json::Value::as_str)
            && !b64.is_empty()
        {
            let body = B64
                .decode(b64)
                .map_err(|e| format!("body-b64 is not base64: {e}"))?;
            builder = builder.body(body);
        }

        tracing::debug!(app = %self.app, %url, "outbound http-fetch");
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
    pub async fn instantiate(
        engine: &Engine,
        path: &std::path::Path,
        app: &str,
        allow_hosts: &[String],
        env: &[(String, String)],
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
pub fn engine() -> anyhow::Result<Engine> {
    Ok(Engine::new(&wasmtime::Config::new())?)
}
