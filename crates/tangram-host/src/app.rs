//! One running app: a live component instance + the host-side document +
//! the parsed `describe()` manifest, assembled from an [`AppSpec`].

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Context;
use serde_json::Value;
use tangram::sync::DocHandle as _;

use crate::config::AppSpec;
use crate::doc::AppDoc;
use crate::runtime::ComponentHandle;

/// The component's `describe()` manifest, parsed.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Describe {
    pub name: String,
    #[serde(default)]
    pub instructions: Option<String>,
    pub actions: Vec<ActionInfo>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ActionInfo {
    pub name: String,
    pub description: String,
    pub mutates: bool,
    pub input_schema: Value,
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

impl DispatchError {
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
    pub async fn build(
        engine: &wasmtime::Engine,
        name: &str,
        spec: &AppSpec,
    ) -> anyhow::Result<Self> {
        let component_mtime = component_mtime(&spec.component);
        let env = spec.resolved_env(name);
        let component =
            ComponentHandle::instantiate(engine, &spec.component, name, &spec.allow_hosts, &env)
                .await
                .with_context(|| format!("instantiating component {}", spec.component.display()))?;

        let describe: Describe = serde_json::from_str(&component.describe().await?)
            .context("parsing the component's describe() manifest")?;

        // A fresh install's document starts from the component's
        // deterministic genesis — byte-identical to a native instance's, so
        // host-managed and native documents share one root and merge.
        let doc_path = spec
            .resolved_data_dir(name)
            .join(format!("{name}.automerge"));
        let genesis = component.genesis().await.context("component genesis()")?;
        let doc = Arc::new(AppDoc::open(doc_path, &genesis)?);

        let remote_task = spec.remote.clone().map(|remote| {
            tracing::info!("{name}: replicating with {remote}");
            tokio::spawn(tangram::sync::run_remote(remote, doc.clone()))
        });

        Ok(Self {
            name: name.to_string(),
            spec: spec.clone(),
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

    /// The current state as JSON, rendered by the component.
    pub async fn state_json(&self) -> Value {
        let doc_bytes = self.doc.save();
        match self.component.state_json(&doc_bytes).await {
            Ok(json) => serde_json::from_str(&json)
                .unwrap_or_else(|e| serde_json::json!({ "error": format!("bad state JSON: {e}") })),
            Err(e) => serde_json::json!({ "error": format!("state-json failed: {e:#}") }),
        }
    }
}
