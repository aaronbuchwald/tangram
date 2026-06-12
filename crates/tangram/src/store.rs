//! The native host's view of the portable document store: wraps
//! [`tangram_core::Store`] and turns its change callback into a tokio watch
//! channel, which is what drives the SSE state streams, sync pokes, and
//! dial-out sync rounds. Everything document-shaped lives in `tangram-core`;
//! only the runtime plumbing (the watch channel) is native.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use crate::action::{ActionDef, ActionError, Actions};
use crate::{Ctx, Model};

pub(super) struct Store<M> {
    core: Arc<tangram_core::Store<M>>,
    version: watch::Sender<u64>,
}

impl<M: Model + Actions> Store<M> {
    /// Load the document from `path`, or create it from `M::default()`.
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let version = watch::Sender::new(0);
        let signal = version.clone();
        let core = Arc::new(tangram_core::Store::open(path, move || {
            signal.send_modify(|v| *v += 1);
        })?);
        Ok(Self { core, version })
    }

    /// A [`Ctx`] onto this store for async actions and custom routes.
    pub fn ctx(&self) -> Ctx<M> {
        Ctx::new(self.core.clone())
    }

    pub fn action_defs(&self) -> impl Iterator<Item = &ActionDef<M>> {
        self.core.action_defs()
    }

    pub fn state_json(&self) -> serde_json::Value {
        self.core.state_json()
    }

    /// Run an action by name — the single dispatch path shared by the HTTP
    /// action route and the MCP tool bridge (see [`tangram_core::Store::dispatch`]).
    pub async fn dispatch(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        self.core.dispatch(name, args).await
    }

    /// Subscribe to the change signal (fires on local actions and on changes
    /// received from sync peers).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    pub fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    // ── sync protocol access ──────────────────────────────────────────────

    pub fn generate_sync(&self, state: &mut automerge::sync::State) -> Option<Vec<u8>> {
        self.core.generate_sync(state)
    }

    pub fn receive_sync(
        &self,
        state: &mut automerge::sync::State,
        bytes: &[u8],
    ) -> anyhow::Result<bool> {
        self.core.receive_sync(state, bytes)
    }
}
