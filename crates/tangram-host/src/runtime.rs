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

use crate::config::EnforcementMode;
use crate::config::InjectKind;
use crate::egress::{CallSpec, CanonicalRequest};
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
    /// The coarse outer host fence (the cheap first gate): an outbound request
    /// whose canonical host is not here is denied before any call match
    /// (ADR-0005's invariant — the call match is the inner authoritative gate,
    /// never a bypass).
    allow_hosts: Vec<String>,
    /// Call-level egress capabilities (fine-grained-egress §4): the
    /// authoritative inner gate. The host picks the FIRST matching declared
    /// call and injects ONLY that call's credential — resolved host-side at
    /// request time so the component never holds the plaintext (ADR-0005).
    /// For an app with no `[[calls]]` this is the compat shim (one broad
    /// implicit call per allowlisted host), so behavior is byte-identical.
    calls: Vec<CallSpec>,
    /// The enforcement posture (observe / warn / enforce). Decides the
    /// disposition of an UNDECLARED call: log a candidate (observe), allow +
    /// warn (warn), or deny with a precise error (enforce).
    enforcement: EnforcementMode,
    /// The OPT-IN egress policy engine (§9.2; ADR-0009 — the deliberately-marked
    /// escape hatch, `None` for the overwhelming majority of apps). When
    /// present, it runs as an ADDITIONAL gate AFTER the declarative call match
    /// and can only NARROW (deny a request the declarative engine allowed),
    /// never widen and never change which credential is injected. A policy that
    /// blows its bounded latency budget at evaluation FAILS CLOSED (deny).
    policy: Option<crate::policy::Policy>,
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

        // The component's raw method + header names, needed for canonicalization.
        let method_raw = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET");
        let header_names: Vec<String> = request
            .get("headers")
            .and_then(serde_json::Value::as_object)
            .map(|h| h.keys().cloned().collect())
            .unwrap_or_default();
        // The request body, decoded ONCE up front: needed for the body matcher
        // (EC4) and the `max_body_bytes` checks. An empty body is `&[]`.
        let body_bytes: Vec<u8> = match request.get("body-b64").and_then(serde_json::Value::as_str)
        {
            Some(b64) if !b64.is_empty() => B64
                .decode(b64)
                .map_err(|e| format!("body-b64 is not base64: {e}"))?,
            _ => Vec::new(),
        };

        // ── Step 1: canonicalize ONCE, before any matching (the single seam;
        //    the §2 SOCKS5 parser-differential lesson). Both the host fence and
        //    the call match run against this same value. ─────────────────────
        let canon = CanonicalRequest::from_request(
            method_raw,
            &parsed,
            header_names.iter().map(String::as_str),
        )?;

        // ── Step 2: the host fence (unchanged, the cheap first gate). A host
        //    not in allow_hosts is denied before any call match — ADR-0005's
        //    invariant; the call match is the inner gate, never a bypass. ─────
        if !self
            .allow_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&canon.host))
        {
            tracing::warn!(app = %self.app, host = %canon.host, "denied outbound request (not in allow_hosts)");
            return Err(format!(
                "outbound request to {:?} denied: it is not in app {:?}'s allow_hosts \
                 (granted: {:?}); add it to the app's allow_hosts in apps.toml to grant access",
                canon.host, self.app, self.allow_hosts
            ));
        }

        // ── Step 3: the call match — the first declared call whose
        //    method ∧ host ∧ path ∧ query ∧ headers ∧ body all match. ─────────
        let matched = self
            .calls
            .iter()
            .find(|call| call.matches(&canon, &body_bytes));

        // ── Disposition of an UNDECLARED call, by enforcement mode. A matched
        //    call (incl. the compat-shim broad call) always falls through to
        //    step 4. ───────────────────────────────────────────────────────
        let matched = match matched {
            Some(call) => call,
            None => match self.enforcement {
                EnforcementMode::Enforce => {
                    // Deny with a precise error naming the declared calls for
                    // this host (the operator-facing diagnosis).
                    let declared = self.declared_calls_for_host(&canon.host);
                    tracing::warn!(
                        app = %self.app, method = %canon.method, host = %canon.host,
                        path = %canon.path, "denied outbound request (no declared call matches)"
                    );
                    return Err(format!(
                        "outbound {} {}{} denied: no declared call matches it (app {:?}, \
                         enforcement=enforce). Declared calls for this host: {}",
                        canon.method, canon.host, canon.path, self.app, declared
                    ));
                }
                EnforcementMode::Warn => {
                    // Allow, but loudly warn and name the candidate to declare.
                    tracing::warn!(
                        app = %self.app,
                        "would DENY in enforce mode: {} {}{} matches no declared call — \
                         add a [[calls]] entry: {}",
                        canon.method, canon.host, canon.path,
                        Self::candidate_call_toml(&canon)
                    );
                    // The OPT-IN policy gate still applies to an undeclared call
                    // allowed by warn mode (it can only NARROW): a policy Deny
                    // here blocks even what warn would let through.
                    if let Some(denial) = self.policy_denial(&canon, &body_bytes) {
                        return Err(denial);
                    }
                    // Send un-credentialed (no matched call ⇒ no inject).
                    return send_and_strip(
                        &self.client,
                        &canon,
                        &parsed,
                        &header_names,
                        &request,
                        &body_bytes,
                        None,
                    )
                    .await;
                }
                EnforcementMode::Observe => {
                    // Never deny; log the candidate declared call.
                    tracing::info!(
                        app = %self.app,
                        "observe: candidate declared call for {} {}{} — {}",
                        canon.method, canon.host, canon.path,
                        Self::candidate_call_toml(&canon)
                    );
                    // Observe mode never denies — but if a policy is attached,
                    // log what it WOULD have done so the operator sees the
                    // policy's effect before flipping to enforce (the §5.4
                    // observe contract extended to the policy gate).
                    self.log_policy_observation(&canon, &body_bytes);
                    return send_and_strip(
                        &self.client,
                        &canon,
                        &parsed,
                        &header_names,
                        &request,
                        &body_bytes,
                        None,
                    )
                    .await;
                }
            },
        };

        // ── Step 3b (OPT-IN, §9.2 / ADR-0009): the policy gate on the MATCHED
        //    declarative call. The declarative engine has allowed this request
        //    and bound the credential to `matched`; the policy is the ADDITIONAL
        //    narrowing gate. In observe mode it only LOGS (the observe contract);
        //    in warn/enforce a policy Deny (or fail-closed budget) blocks the
        //    request BEFORE the secret is resolved or injected. A policy can
        //    never widen — it only turns this allow into a deny. ──────────────
        if self.enforcement == EnforcementMode::Observe {
            self.log_policy_observation(&canon, &body_bytes);
        } else if let Some(denial) = self.policy_denial(&canon, &body_bytes) {
            return Err(denial);
        }

        // ── Step 4: inject on the matched call ONLY (ADR-0005): resolve its
        //    secret host-side and attach it. A matched call with no inject goes
        //    out un-credentialed (a declared public call). The `SecretString`
        //    lives only for this call and is never logged. ───────────────────
        let injected = match (&matched.inject_kind, &matched.inject) {
            (Some(kind), Some(rule)) => rule
                .resolve_secret(
                    &self.secrets,
                    &format!("{}: call inject {}", self.app, canon.host),
                )
                .await
                .map(|secret| (kind.clone(), secret)),
            _ => None,
        };

        // Clone what `send_and_strip` needs before the store-free network send.
        send_and_strip(
            &self.client,
            &canon,
            &parsed,
            &header_names,
            &request,
            &body_bytes,
            injected,
        )
        .await
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

/// Response headers stripped before the body is handed back to the component
/// (fine-grained-egress §4.2 step 5): auth-bearing headers the upstream might
/// echo, so a component can never read a credential out of a response now that
/// the host owns it per-call. Cookies are credentials too. Names are matched
/// case-insensitively.
const STRIPPED_RESPONSE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "www-authenticate",
    "proxy-authenticate",
    "set-cookie",
];

impl HostState {
    /// The declared calls for one host, rendered for the enforce-mode denial
    /// message (the operator-facing diagnosis). `<none>` when the host has no
    /// declared call (only the host fence let it reach here).
    fn declared_calls_for_host(&self, host: &str) -> String {
        let mut shown: Vec<String> = self
            .calls
            .iter()
            .filter(|c| c.host == host)
            .map(|c| format!("{} {}", c.method_str(), c.path_str()))
            .collect();
        if shown.is_empty() {
            "<none>".to_string()
        } else {
            shown.sort();
            shown.dedup();
            shown.join(", ")
        }
    }

    /// The OPT-IN policy gate (§9.2 / ADR-0009), evaluated against the SHARED
    /// canonical request + body. Returns `Some(error)` when the policy DENIES
    /// (or fails closed on its bounded budget) — the request is blocked before
    /// it leaves the host; `None` when there is no policy, or the policy allows.
    /// The policy can only NARROW: this is consulted only on requests the
    /// declarative engine already allowed, and a `Some` here turns that allow
    /// into a deny. It never resolves or names a secret.
    fn policy_denial(&self, canon: &CanonicalRequest, body: &[u8]) -> Option<String> {
        use crate::policy::PolicyVerdict;
        let policy = self.policy.as_ref()?;
        match policy.evaluate(canon, body) {
            PolicyVerdict::Allow => None,
            PolicyVerdict::Deny(reason) => {
                tracing::warn!(
                    app = %self.app, method = %canon.method, host = %canon.host,
                    path = %canon.path, "denied outbound request (egress policy): {reason}"
                );
                Some(format!(
                    "outbound {} {}{} denied by app {:?}'s egress policy: {reason} \
                     (this app uses a custom egress policy — §9.2)",
                    canon.method, canon.host, canon.path, self.app
                ))
            }
            PolicyVerdict::FailClosed(reason) => {
                tracing::warn!(
                    app = %self.app, method = %canon.method, host = %canon.host,
                    path = %canon.path, "denied outbound request (egress policy fail-closed): {reason}"
                );
                Some(format!(
                    "outbound {} {}{} denied by app {:?}'s egress policy: {reason}",
                    canon.method, canon.host, canon.path, self.app
                ))
            }
        }
    }

    /// Observe-mode logging for the policy gate: log what the policy WOULD have
    /// decided without denying (the §5.4 observe contract extended to the
    /// policy engine), so an operator can see the policy's effect before
    /// flipping to enforce. A no-op when there is no policy.
    fn log_policy_observation(&self, canon: &CanonicalRequest, body: &[u8]) {
        use crate::policy::PolicyVerdict;
        let Some(policy) = self.policy.as_ref() else {
            return;
        };
        match policy.evaluate(canon, body) {
            PolicyVerdict::Allow => {}
            PolicyVerdict::Deny(reason) | PolicyVerdict::FailClosed(reason) => {
                tracing::info!(
                    app = %self.app,
                    "observe: egress policy WOULD deny {} {}{} — {reason}",
                    canon.method, canon.host, canon.path
                );
            }
        }
    }

    /// A paste-ready `[[calls]]` candidate for an observed/undeclared request
    /// (the warn/observe diagnostic and the EC6 generator's per-line form):
    /// canonical method + host + path. Numeric/uuid path segments are
    /// parameterized to `{id}` so the candidate generalizes (EC6).
    fn candidate_call_toml(canon: &CanonicalRequest) -> String {
        let path = crate::egress::templatize_path(&canon.segments);
        format!(
            "[[calls]] method = {:?}, host = {:?}, path = {:?}",
            canon.method, canon.host, path
        )
    }
}

/// Build the outbound request, apply the (already-resolved) inject on the
/// MATCHED call only, send, and strip auth-bearing response headers before
/// returning the body to the component (fine-grained-egress §4.2 steps 4-5).
///
/// A FREE function (not a `&self` method) on purpose: `HostState` carries the
/// WASI streams and is not `Sync`, so holding `&HostState` across the network
/// await would make the `http_fetch` future non-`Send`. We pass only the
/// `Sync` pieces (`&reqwest::Client`) and owned data, so nothing non-`Send`
/// crosses the await. The store lock is never held here.
#[allow(clippy::too_many_arguments)]
async fn send_and_strip(
    client: &reqwest::Client,
    canon: &CanonicalRequest,
    parsed: &reqwest::Url,
    header_names: &[String],
    request: &serde_json::Value,
    body_bytes: &[u8],
    injected: Option<(InjectKind, secrecy::SecretString)>,
) -> Result<String, String> {
    let mut url = parsed.clone();
    if let Some((InjectKind::Query(name), secret)) = &injected {
        // Query injection must mutate the URL before the builder consumes it.
        url.query_pairs_mut()
            .append_pair(name, secret.expose_secret());
    }

    let method: reqwest::Method = canon
        .method
        .parse()
        .map_err(|e| format!("invalid method: {e}"))?;
    let mut builder = client
        .request(method, url)
        .timeout(std::time::Duration::from_secs(30));
    // The component's own headers (values come from the request object;
    // names from `header_names`, preserving the component's casing).
    if let Some(headers) = request
        .get("headers")
        .and_then(serde_json::Value::as_object)
    {
        for name in header_names {
            if let Some(value) = headers.get(name).and_then(serde_json::Value::as_str) {
                builder = builder.header(name, value);
            }
        }
    }
    // Apply header/bearer injection AFTER the component's own headers, so the
    // host-attached credential is authoritative (a component cannot pre-set
    // the injected header to a value of its choosing).
    match &injected {
        Some((InjectKind::Header(name), secret)) => {
            builder = builder.header(name, secret.expose_secret());
        }
        Some((InjectKind::Bearer, secret)) => {
            builder = builder.bearer_auth(secret.expose_secret());
        }
        _ => {}
    }
    if !body_bytes.is_empty() {
        builder = builder.body(body_bytes.to_vec());
    }

    tracing::debug!(
        method = %canon.method, host = %canon.host, path = %canon.path,
        injected = injected.is_some(), "outbound http-fetch"
    );
    let response = builder
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = response.status().as_u16();
    // Strip auth-bearing response headers before handing the body back to the
    // component (§4.2 step 5): now that the host owns the credential per-call,
    // the component must never read one back out of a response.
    let headers: serde_json::Map<String, serde_json::Value> = response
        .headers()
        .iter()
        .filter(|(k, _)| {
            !STRIPPED_RESPONSE_HEADERS
                .iter()
                .any(|s| k.as_str().eq_ignore_ascii_case(s))
        })
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
    /// `calls` carries the effective call-level egress capabilities
    /// (fine-grained-egress; for a no-`[[calls]]` app this is the compat shim,
    /// so behavior is byte-identical), `enforcement` the posture for undeclared
    /// calls, `policy` the OPT-IN egress policy engine (§9.2 / ADR-0009; `None`
    /// for almost all apps — an additional NARROWING gate that never widens),
    /// and `secrets` the resolver seam used to turn a matched call's inject
    /// reference into a value host-side at request time — none of these is ever
    /// exposed to the component.
    #[allow(clippy::too_many_arguments)]
    pub async fn instantiate(
        engine: &Engine,
        path: &std::path::Path,
        app: &str,
        allow_hosts: &[String],
        env: &[(String, String)],
        calls: Vec<CallSpec>,
        enforcement: EnforcementMode,
        policy: Option<crate::policy::Policy>,
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
            calls,
            enforcement,
            policy,
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

    /// Replace the enforced call list on the live `HostState` (EC5): the
    /// component is instantiated with the OPERATOR spec's calls, then —
    /// after `describe()` is read — the host narrows them by intersecting with
    /// the component's declared calls (a request that can only narrow). Called
    /// exactly once at build, before any dispatch; the component cannot widen
    /// its grant (the intersection only removes calls).
    pub async fn set_calls(&self, calls: Vec<CallSpec>) {
        let mut inner = self.inner.lock().await;
        inner.store.data_mut().calls = calls;
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
