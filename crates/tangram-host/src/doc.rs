//! The host-side document: an UNTYPED automerge document per app (the host
//! doesn't know the model — only the component does), with the same
//! persist/notify contract as the SDK's typed store. It implements the
//! SDK's [`tangram::sync::DocHandle`], so the existing sync server core and
//! dial-out client run over it unchanged — host-managed documents replicate
//! with native instances because the component's `genesis()` is
//! byte-identical to the native genesis.

use std::path::PathBuf;
use std::sync::Mutex;

use automerge::sync::SyncDoc;
use automerge::{ActorId, AutoCommit};
use tokio::sync::watch;

pub struct AppDoc {
    doc: Mutex<AutoCommit>,
    version: watch::Sender<u64>,
    path: PathBuf,
}

impl AppDoc {
    /// Load the document at `path`, creating it from `genesis` bytes (the
    /// component's deterministic genesis) when absent.
    pub fn open(path: PathBuf, genesis: &[u8]) -> anyhow::Result<Self> {
        let bytes = if path.exists() {
            std::fs::read(&path)?
        } else {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, genesis)?;
            genesis.to_vec()
        };
        let mut doc = AutoCommit::load(&bytes)?;
        doc.set_actor(ActorId::random());
        Ok(Self {
            doc: Mutex::new(doc),
            version: watch::Sender::new(0),
            path,
        })
    }

    /// Full save of the current document (what `dispatch`/`state-json` calls
    /// hand to the component).
    pub fn save(&self) -> Vec<u8> {
        self.doc.lock().expect("doc lock").save()
    }

    /// Merge a full save returned by the component back in. Merging (rather
    /// than replacing) means a sync message that landed while the guest was
    /// dispatching is never lost. Returns true if the document changed (the
    /// caller then bumps subscribers).
    pub fn merge_saved(&self, bytes: &[u8]) -> anyhow::Result<bool> {
        let mut incoming = AutoCommit::load(bytes)?;
        let mut doc = self.doc.lock().expect("doc lock");
        let before = doc.get_heads();
        doc.merge(&mut incoming)?;
        let changed = doc.get_heads() != before;
        if changed {
            self.persist(&mut doc);
        }
        Ok(changed)
    }

    /// Write the full document to disk (atomic via temp file + rename) —
    /// same contract as the SDK store.
    fn persist(&self, doc: &mut AutoCommit) {
        let bytes = doc.save();
        let tmp = self.path.with_extension("automerge.tmp");
        let result = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = result {
            tracing::error!("failed to persist document to {}: {e}", self.path.display());
        }
    }
}

impl tangram::sync::DocHandle for AppDoc {
    fn generate_sync(&self, state: &mut automerge::sync::State) -> Option<Vec<u8>> {
        let mut doc = self.doc.lock().expect("doc lock");
        doc.sync().generate_sync_message(state).map(|m| m.encode())
    }

    fn receive_sync(
        &self,
        state: &mut automerge::sync::State,
        bytes: &[u8],
    ) -> anyhow::Result<bool> {
        let message = automerge::sync::Message::decode(bytes)?;
        let mut doc = self.doc.lock().expect("doc lock");
        let before = doc.get_heads();
        doc.sync().receive_sync_message(state, message)?;
        let changed = doc.get_heads() != before;
        if changed {
            self.persist(&mut doc);
        }
        Ok(changed)
    }

    fn bump(&self) {
        self.version.send_modify(|v| *v += 1);
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }
}
