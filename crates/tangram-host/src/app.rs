//! One running app: a live component instance + the host-side document +
//! the parsed `describe()` manifest, assembled from an [`AppSpec`].

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Context;
use axum::http::StatusCode;
use serde_json::Value;
use tangram::sync::DocHandle as _;

use crate::config::AppSpec;
use crate::doc::AppDoc;
use crate::runtime::ComponentHandle;
use crate::secrets::SecretRegistry;

/// The component's `describe()` manifest, parsed.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Describe {
    pub name: String,
    #[serde(default)]
    pub instructions: Option<String>,
    pub actions: Vec<ActionInfo>,
    /// Optional capabilities object, computed by the app at instantiation
    /// (e.g. nutrition's active-strategy probe); served verbatim at
    /// `GET /<app>/api/capabilities`. `None` = the app publishes no
    /// capabilities and the route 404s, like a native app without the probe.
    #[serde(default)]
    pub capabilities: Option<Value>,
    /// Optional call-level egress DECLARATION (fine-grained-egress §6, EC5):
    /// the calls the component says it makes, carried out via `describe()`.
    /// ADDITIVE (`#[serde(default)]`) — an older component omits it. This is a
    /// REQUEST, never authority: the host INTERSECTS it with the operator
    /// spec's calls (a component declaring more than its spec is narrowed to
    /// the spec). Declared calls name reach only — never a credential (the
    /// inject always comes from the operator spec).
    #[serde(default)]
    pub calls: Vec<DescribeCall>,
}

/// One component-declared call in `describe()` (EC5). The same small grammar as
/// a `[[calls]]` entry MINUS `inject` (a component cannot grant itself a
/// credential — fine-grained-egress §6) and minus `enforcement`. Unknown fields
/// are tolerated (a newer component may add fields an older host ignores).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DescribeCall {
    #[serde(default = "describe_method_any")]
    pub method: String,
    pub host: String,
    #[serde(default = "describe_path_subtree")]
    pub path: String,
    #[serde(default)]
    pub query: crate::config::NameConstraintToml,
    #[serde(default)]
    pub headers: crate::config::NameConstraintToml,
    #[serde(default)]
    pub max_body_bytes: Option<usize>,
    #[serde(default)]
    pub body: Option<crate::config::BodyMatchToml>,
}

fn describe_method_any() -> String {
    "*".to_string()
}

fn describe_path_subtree() -> String {
    "/**".to_string()
}

impl DescribeCall {
    /// Lower a component-declared call into an `egress::CallSpec` (no inject).
    /// A malformed declaration is dropped (it can only narrow, so a bad entry
    /// fails safe by simply not covering anything).
    fn resolve(&self) -> Option<crate::egress::CallSpec> {
        let toml = crate::config::CallSpecToml {
            method: self.method.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            query: self.query.clone(),
            headers: self.headers.clone(),
            max_body_bytes: self.max_body_bytes,
            body: self.body.clone(),
            inject: None,
        };
        match toml.resolve() {
            Ok(spec) => Some(spec),
            Err(e) => {
                tracing::warn!(
                    "ignoring describe()-declared call for host {:?}: {e:#}",
                    self.host
                );
                None
            }
        }
    }
}

/// Intersect the operator's effective calls with the component's `describe()`-
/// declared calls (EC5). A REQUEST that can only narrow: the result is the
/// operator calls covered by some declared call; an empty declaration leaves
/// the operator calls unchanged. Shared by `build` and its tests.
pub fn intersect_describe_calls(
    operator: Vec<crate::egress::CallSpec>,
    declared: &[DescribeCall],
) -> Vec<crate::egress::CallSpec> {
    let declared: Vec<crate::egress::CallSpec> =
        declared.iter().filter_map(DescribeCall::resolve).collect();
    crate::egress::intersect_with_declared(operator, &declared)
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ActionInfo {
    pub name: String,
    pub description: String,
    pub mutates: bool,
    /// The JSON Schema for this action's arguments, shared behind an `Arc` so
    /// the MCP bridge can hand the same allocation to `rmcp::Tool` without a
    /// deep clone per bridge construction.
    #[serde(deserialize_with = "deserialize_schema")]
    pub input_schema: Arc<serde_json::Map<String, Value>>,
}

/// Deserialize an action's `input_schema` field: extract the object map if the
/// value is a JSON object, fall back to an empty map otherwise (mirrors the
/// defensive match in the old `McpBridge::new`).
fn deserialize_schema<'de, D>(de: D) -> Result<Arc<serde_json::Map<String, Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let v = Value::deserialize(de)?;
    Ok(Arc::new(match v {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }))
}

/// A dispatch failure, classified so the HTTP/MCP surfaces can keep the
/// SDK's error envelope. App-level errors arrive as strings rendered by the
/// SDK's `ActionError` inside the guest; the prefixes below are that
/// rendering, so classification round-trips exactly.
#[derive(Debug)]
pub enum DispatchError {
    /// No such action in the registry (HTTP 404).
    Unknown(String),
    /// Bad argument object (HTTP 400).
    BadArgs(String),
    /// The action itself failed — a domain error (HTTP 422).
    Failed(String),
    /// Guest internal error, trap, or engine failure (HTTP 500).
    Internal(String),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(m) | Self::BadArgs(m) | Self::Failed(m) | Self::Internal(m) => {
                write!(f, "{m}")
            }
        }
    }
}

/// How a [`DispatchError`] maps onto the MCP protocol's two failure modes:
/// tool-level errors (returned inside a `CallToolResult` so the agent can read
/// them) vs. JSON-RPC errors (unknown tool / internal fault).
pub enum McpErrorKind {
    /// Domain / bad-args failure → `CallToolResult::error`. Agent can recover.
    ToolError,
    /// No such tool → `ErrorData::invalid_params`.
    InvalidParams,
    /// Internal fault → `ErrorData::internal_error`.
    InternalError,
}

impl DispatchError {
    /// The HTTP status code for this error, shared by the action route
    /// (`POST /api/actions/:name`) and any future HTTP surface.
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::Unknown(_) => StatusCode::NOT_FOUND,
            Self::BadArgs(_) => StatusCode::BAD_REQUEST,
            Self::Failed(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// How this error should surface on the MCP transport, shared between the
    /// WASM host's `McpBridge` and any future MCP surface.
    pub fn mcp_kind(&self) -> McpErrorKind {
        match self {
            Self::BadArgs(_) | Self::Failed(_) => McpErrorKind::ToolError,
            Self::Unknown(_) => McpErrorKind::InvalidParams,
            Self::Internal(_) => McpErrorKind::InternalError,
        }
    }

    /// Classify a guest-rendered `ActionError` string by its stable prefix.
    fn from_guest(message: String) -> Self {
        if message.starts_with("unknown action:") {
            Self::Unknown(message)
        } else if message.starts_with("invalid arguments:") {
            Self::BadArgs(message)
        } else if message.starts_with("internal error:") {
            Self::Internal(message)
        } else {
            Self::Failed(message)
        }
    }
}

/// A converged, serving app.
pub struct AppRuntime {
    pub name: String,
    pub spec: AppSpec,
    /// The local component file actually instantiated: the spec's path, or
    /// the verified cache slot of a `component_url` spec.
    pub component_path: std::path::PathBuf,
    /// Component file mtime at instantiation — converge reloads on change.
    pub component_mtime: Option<SystemTime>,
    pub component: ComponentHandle,
    pub doc: Arc<AppDoc>,
    pub describe: Describe,
    pub sessions: tangram::sync::Sessions,
    /// The dial-out sync client, if a remote is configured; aborted when the
    /// runtime is dropped (app removed or reloaded).
    remote_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for AppRuntime {
    fn drop(&mut self) {
        if let Some(task) = &self.remote_task {
            task.abort();
        }
    }
}

pub fn component_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl AppRuntime {
    /// Instantiate the component, open (or genesis) the document, parse the
    /// manifest, and start the optional dial-out sync client.
    /// `component_path` is the resolved LOCAL file: the spec's `component`
    /// path, or — for `component_url` specs — the hash-verified cache slot
    /// the converge loop downloaded into (`crate::fetch`).
    pub async fn build(
        engine: &wasmtime::Engine,
        secrets: &Arc<SecretRegistry>,
        name: &str,
        spec: &AppSpec,
        component_path: &Path,
    ) -> anyhow::Result<Self> {
        let component_mtime = component_mtime(component_path);
        let env = spec.resolved_env(secrets, name).await;
        // The effective call-level egress capabilities (fine-grained-egress):
        // either the app's declared `[[calls]]` or the host-keyed compat shim
        // (byte-identical for legacy apps). The enforcement posture decides the
        // disposition of an undeclared call.
        let operator_calls = spec.resolved_calls();
        let enforcement = spec.effective_enforcement();
        let component = ComponentHandle::instantiate(
            engine,
            component_path,
            name,
            &spec.allow_hosts,
            &env,
            operator_calls.clone(),
            enforcement,
            secrets.clone(),
        )
        .await
        .with_context(|| format!("instantiating component {}", component_path.display()))?;

        let mut describe: Describe = serde_json::from_str(&component.describe().await?)
            .context("parsing the component's describe() manifest")?;

        // EC5: the component's describe()-DECLARED calls are a REQUEST, not a
        // grant (fine-grained-egress §6). Intersect them with the operator
        // spec's calls — the result can only NARROW (a component declaring more
        // than its spec is narrowed to the spec; an empty declaration leaves
        // the spec unchanged). The narrowed set replaces the enforced list on
        // the live instance, once, before any dispatch. The credential always
        // stays on the operator call (a declared call names reach only).
        if !describe.calls.is_empty() {
            let narrowed = intersect_describe_calls(operator_calls, &describe.calls);
            let dropped = narrowed.len();
            tracing::debug!(
                app = %name,
                "describe()-declared {} call(s); enforced set narrowed to {} operator call(s)",
                describe.calls.len(),
                dropped
            );
            component.set_calls(narrowed).await;
        }

        // ADR-0005: when the app declares egress injection, the capabilities
        // probe's "configured" signal is derived HOST-side from whether an
        // injection secret resolves — NOT from the component seeing a secret
        // env var (it no longer does). AND the component's `description_input`
        // with that, so an app whose credential is missing or unresolvable
        // reports `description_input: false` and stays offline/degraded
        // cleanly. Apps with no injection rules are left exactly as the
        // component reported (env-injected/native parity preserved).
        if spec.has_any_inject() {
            let configured = spec.any_inject_resolves(secrets, name).await;
            if let Some(caps) = describe.capabilities.as_mut()
                && let Some(di) = caps.get_mut("description_input")
                && di.as_bool() == Some(true)
                && !configured
            {
                *di = serde_json::Value::Bool(false);
            }
        }

        // A fresh install's document starts from the component's
        // deterministic genesis — byte-identical to a native instance's, so
        // host-managed and native documents share one root and merge.
        let doc_path = spec
            .resolved_data_dir(name)
            .join(format!("{name}.automerge"));
        let genesis = component.genesis().await.context("component genesis()")?;
        let doc = Arc::new(AppDoc::open(doc_path, &genesis)?);

        let remote_task = match spec.remote.clone() {
            Some(remote) => {
                tracing::info!("{name}: replicating with {remote}");
                let token = spec.resolved_remote_token(secrets, name).await;
                Some(tokio::spawn(tangram::sync::run_remote(
                    remote,
                    token,
                    doc.clone(),
                )))
            }
            None => None,
        };

        Ok(Self {
            name: name.to_string(),
            spec: spec.clone(),
            component_path: component_path.to_path_buf(),
            component_mtime,
            component,
            doc,
            describe,
            sessions: tangram::sync::Sessions::default(),
            remote_task,
        })
    }

    /// Run one action — the single dispatch path shared by the HTTP action
    /// route and the MCP tool bridge, mirroring the SDK's store dispatch.
    /// Doc-in/doc-out: the guest gets the current save, and a mutated save
    /// is merged back, persisted, and announced to every subscriber.
    pub async fn dispatch(&self, action: &str, args: Value) -> Result<Value, DispatchError> {
        if !self.describe.actions.iter().any(|a| a.name == action) {
            return Err(DispatchError::Unknown(format!("unknown action: {action}")));
        }
        let doc_bytes = self.doc.save();
        let outcome = self
            .component
            .dispatch(action, &args.to_string(), &doc_bytes)
            .await
            .map_err(|e| DispatchError::Internal(format!("internal error: {e:#}")))?
            .map_err(DispatchError::from_guest)?;

        if let Some(bytes) = outcome.doc {
            let changed = self
                .doc
                .merge_saved(&bytes)
                .map_err(|e| DispatchError::Internal(format!("internal error: {e:#}")))?;
            if changed {
                self.doc.bump();
            }
        }
        serde_json::from_str(&outcome.result_json)
            .map_err(|e| DispatchError::Internal(format!("internal error: bad result JSON: {e}")))
    }

    /// Liveness probe for the fleet status: the instance is healthy if it
    /// still renders state for the current document.
    pub async fn healthy(&self) -> bool {
        let doc_bytes = self.doc.save();
        self.component.state_json(&doc_bytes).await.is_ok()
    }

    /// The current state as JSON text, exactly as the component rendered it.
    /// Served verbatim (after a syntax-only `RawValue` validation) rather than
    /// parsed into a `Value` and re-serialized: even with serde_json's
    /// `float_roundtrip` feature the round trip is wasted work, and without it
    /// the reparse was lossy by 1 ULP (printed 30.599999999999998 as 30.6),
    /// making replica convergence checks report false mismatches.
    pub async fn state_json(&self) -> String {
        let doc_bytes = self.doc.save();
        match self.component.state_json(&doc_bytes).await {
            Ok(json) => match serde_json::from_str::<&serde_json::value::RawValue>(&json) {
                Ok(_) => json,
                Err(e) => {
                    serde_json::json!({ "error": format!("bad state JSON: {e}") }).to_string()
                }
            },
            Err(e) => {
                serde_json::json!({ "error": format!("state-json failed: {e:#}") }).to_string()
            }
        }
    }
}

// The test module name carries `describe_calls` so the build plan's filter
// `cargo test -p tangram-host describe_calls` selects exactly the EC5 suite.
#[cfg(test)]
mod describe_calls {
    use super::*;
    use crate::config::CallSpecToml;
    use crate::egress::{MethodMatch, PathPattern};

    /// Build an operator CallSpec via the config lowering (the real path).
    fn op(method: &str, host: &str, path: &str) -> crate::egress::CallSpec {
        CallSpecToml {
            method: method.into(),
            host: host.into(),
            path: path.into(),
            query: Default::default(),
            headers: Default::default(),
            max_body_bytes: None,
            body: None,
            // The operator call carries the credential; the describe channel
            // never does. A header inject so we can assert it survives.
            inject: Some(crate::config::InjectRule {
                header: Some("X-Api-Key".into()),
                bearer: false,
                query: None,
                secret: "env://K".into(),
            }),
        }
        .resolve()
        .unwrap()
    }

    fn declared(method: &str, host: &str, path: &str) -> DescribeCall {
        DescribeCall {
            method: method.into(),
            host: host.into(),
            path: path.into(),
            query: Default::default(),
            headers: Default::default(),
            max_body_bytes: None,
            body: None,
        }
    }

    #[test]
    fn empty_declaration_leaves_the_operator_spec_unchanged() {
        // No declaration to intersect with ⇒ the operator spec is authority.
        let operator = vec![op("GET", "api.vendor.com", "/v1/me")];
        let out = intersect_describe_calls(operator.clone(), &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].host, "api.vendor.com");
        // The operator credential is preserved.
        assert!(out[0].inject.is_some());
    }

    #[test]
    fn declaring_a_subset_narrows_the_spec() {
        // Operator grants two calls; component declares only one ⇒ enforced set
        // narrows to the declared one. Declaring FEWER narrows the spec.
        let operator = vec![
            op("GET", "api.vendor.com", "/v1/me"),
            op("POST", "api.vendor.com", "/v1/me"),
        ];
        let out =
            intersect_describe_calls(operator, &[declared("GET", "api.vendor.com", "/v1/me")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, MethodMatch::Exact("GET".into()));
        // The surviving call still carries the operator's credential.
        assert!(out[0].inject.is_some());
    }

    #[test]
    fn declaring_more_than_the_spec_is_narrowed_to_the_spec() {
        // Component declares calls the operator never granted ⇒ they do NOT
        // appear (the declaration is a request, not a grant). The result is
        // bounded above by the operator spec.
        let operator = vec![op("GET", "api.vendor.com", "/v1/me")];
        let out = intersect_describe_calls(
            operator,
            &[
                declared("GET", "api.vendor.com", "/v1/me"),
                declared("POST", "api.vendor.com", "/v1/accounts/{id}/import"),
                declared("GET", "evil.example.com", "/exfil"),
            ],
        );
        assert_eq!(out.len(), 1, "only the granted call survives");
        assert_eq!(out[0].host, "api.vendor.com");
        assert_eq!(out[0].path, PathPattern::Exact("/v1/me".into()));
    }

    #[test]
    fn a_broad_declaration_covers_a_narrow_operator_call() {
        // A subtree declaration covers a specific operator call (the component
        // says "I call this host"; the operator pins the exact path). The
        // operator's narrower grant is kept (covered), credential intact.
        let operator = vec![op("GET", "api.vendor.com", "/v1/me")];
        let out = intersect_describe_calls(operator, &[declared("*", "api.vendor.com", "/**")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, PathPattern::Exact("/v1/me".into()));
    }

    #[test]
    fn a_narrow_declaration_does_not_cover_a_broad_operator_call() {
        // The component declaring a SPECIFIC path does not cover an operator
        // call that is broader (subtree) — covering requires the declaration
        // to permit at least everything the operator call does. The broad
        // operator call is dropped (the component asked for less).
        let operator = vec![{
            let mut c = op("GET", "api.vendor.com", "/v1/me");
            c.path = PathPattern::Subtree;
            c
        }];
        let out =
            intersect_describe_calls(operator, &[declared("GET", "api.vendor.com", "/v1/me")]);
        assert!(
            out.is_empty(),
            "a narrow declaration cannot cover a subtree grant"
        );
    }

    #[test]
    fn host_mismatch_never_covers() {
        let operator = vec![op("GET", "api.vendor.com", "/v1/me")];
        let out = intersect_describe_calls(operator, &[declared("GET", "other.com", "/v1/me")]);
        assert!(out.is_empty());
    }
}
