//! The replicated document store: an Automerge CRDT document holding the
//! model, with typed access via autosurgeon, disk persistence, and a change
//! signal that drives SSE streams and sync peers.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use automerge::sync::SyncDoc;
use automerge::transaction::CommitOptions;
use automerge::{ActorId, AutoCommit, Automerge};
use tokio::sync::watch;

use crate::Model;
use crate::action::{ActionDef, ActionError, ActionHandler, Actions};

/// A cloneable context handle onto a running app's store, for async code
/// (async actions, background resolvers, custom routes) that lives OUTSIDE
/// the lock-held synchronous layer. The CRDT document is still only ever
/// mutated under the lock via attributed commits: a `Ctx` can read hydrated
/// snapshots and run synchronous mutation closures, but the lock is acquired
/// and released inside each call — it is never held across an await.
pub struct Ctx<M> {
    store: Arc<Store<M>>,
}

impl<M> Clone for Ctx<M> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
        }
    }
}

impl<M: Model + Actions> Ctx<M> {
    pub(crate) fn new(store: Arc<Store<M>>) -> Self {
        Self { store }
    }

    /// A hydrated snapshot of the current state (the lock is released before
    /// this returns).
    pub fn state(&self) -> Result<M, ActionError> {
        self.store.hydrate()
    }

    /// The current replicated state as JSON.
    pub fn state_json(&self) -> serde_json::Value {
        self.store.state_json()
    }

    /// Run a synchronous mutation under the store lock as one attributed
    /// commit: hydrate → `f` → reconcile → commit with `name` as the message
    /// → persist → wake subscribers — the same pipeline a sync action runs.
    /// `f` is synchronous by construction, so the lock cannot be held across
    /// an await.
    pub fn mutate<R>(&self, name: &str, f: impl FnOnce(&mut M) -> R) -> Result<R, ActionError> {
        self.store.mutate(name, f)
    }

    /// Run any registered action by name (sync or async) — the same dispatch
    /// path the HTTP and MCP surfaces use.
    pub async fn apply(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        self.store.dispatch(name, args).await
    }
}

/// All Tangram instances build their initial document with this fixed actor
/// and a zero timestamp, so any two fresh instances of the same app produce a
/// byte-identical genesis change. That shared root is what lets their
/// histories merge into ONE document (rather than two rival container
/// objects) the first time they sync.
const GENESIS_ACTOR: [u8; 16] = *b"tangram-genesis!";

pub(crate) struct Store<M> {
    doc: Mutex<AutoCommit>,
    version: watch::Sender<u64>,
    path: PathBuf,
    actions: HashMap<&'static str, ActionDef<M>>,
    _marker: PhantomData<fn() -> M>,
}

impl<M: Model + Actions> Store<M> {
    /// Load the document from `path`, or create it from `M::default()`.
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let mut doc = if path.exists() {
            let bytes = std::fs::read(&path)?;
            AutoCommit::load(&bytes)?
        } else {
            let mut genesis = Automerge::new().with_actor(ActorId::from(&GENESIS_ACTOR[..]));
            let mut tx = genesis.transaction();
            autosurgeon::reconcile(&mut tx, M::default())?;
            tx.commit_with(
                CommitOptions::default()
                    .with_message("genesis")
                    .with_time(0),
            );
            let bytes = genesis.save();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, &bytes)?;
            AutoCommit::load(&bytes)?
        };
        // Every instance edits as its own random actor; only genesis is fixed.
        doc.set_actor(ActorId::random());

        let actions = M::actions().into_iter().map(|a| (a.name, a)).collect();
        Ok(Self {
            doc: Mutex::new(doc),
            version: watch::Sender::new(0),
            path,
            actions,
            _marker: PhantomData,
        })
    }

    pub fn action_defs(&self) -> impl Iterator<Item = &ActionDef<M>> {
        self.actions.values()
    }

    /// Current state hydrated into the model type.
    pub fn hydrate(&self) -> Result<M, ActionError> {
        let doc = self.doc.lock().expect("store lock");
        autosurgeon::hydrate(&*doc).map_err(ActionError::internal)
    }

    pub fn state_json(&self) -> serde_json::Value {
        match self.hydrate() {
            Ok(m) => serde_json::to_value(m).unwrap_or(serde_json::Value::Null),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        }
    }

    /// Run an action by name — the single dispatch path shared by the HTTP
    /// action route, the MCP tool bridge, and [`Ctx::apply`], so every
    /// surface exposes the same contract by construction. Sync actions run
    /// inline under the store lock; async actions are awaited OUTSIDE the
    /// lock (they mutate through [`Ctx::mutate`], which takes the lock per
    /// commit).
    pub async fn dispatch(
        self: &Arc<Self>,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        let def = self
            .actions
            .get(name)
            .ok_or_else(|| ActionError::Unknown(name.to_string()))?;
        match def.handler {
            ActionHandler::Sync(handler) => self.apply_sync(def, handler, args),
            ActionHandler::Async(handler) => handler(Ctx::new(self.clone()), args).await,
        }
    }

    /// Run a sync action under the lock: hydrate the model, invoke the
    /// handler, and (for mutating actions) reconcile the result back into the
    /// document as one attributed change, persist it, and wake every
    /// subscriber.
    fn apply_sync(
        &self,
        def: &ActionDef<M>,
        handler: fn(&mut M, serde_json::Value) -> Result<serde_json::Value, ActionError>,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        let mut doc = self.doc.lock().expect("store lock");
        let mut model: M = autosurgeon::hydrate(&*doc).map_err(ActionError::internal)?;
        let result = handler(&mut model, args)?;

        if def.mutates {
            autosurgeon::reconcile(&mut *doc, model).map_err(ActionError::internal)?;
            if doc
                .commit_with(CommitOptions::default().with_message(def.name))
                .is_some()
            {
                self.persist(&mut doc);
                drop(doc);
                self.bump();
            }
        }
        Ok(result)
    }

    /// Run a synchronous mutation closure as one attributed commit (the
    /// pipeline behind [`Ctx::mutate`]): hydrate → `f` → reconcile → commit
    /// with `message` → persist → wake subscribers.
    pub fn mutate<R>(&self, message: &str, f: impl FnOnce(&mut M) -> R) -> Result<R, ActionError> {
        let mut doc = self.doc.lock().expect("store lock");
        let mut model: M = autosurgeon::hydrate(&*doc).map_err(ActionError::internal)?;
        let result = f(&mut model);
        autosurgeon::reconcile(&mut *doc, model).map_err(ActionError::internal)?;
        if doc
            .commit_with(CommitOptions::default().with_message(message))
            .is_some()
        {
            self.persist(&mut doc);
            drop(doc);
            self.bump();
        }
        Ok(result)
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

    /// Next pending sync message for the peer represented by `state`.
    pub fn generate_sync(&self, state: &mut automerge::sync::State) -> Option<Vec<u8>> {
        let mut doc = self.doc.lock().expect("store lock");
        doc.sync().generate_sync_message(state).map(|m| m.encode())
    }

    /// Apply a sync message from a peer. Returns true if the document changed
    /// (the caller should then `bump()` to wake UIs and other peers).
    pub fn receive_sync(
        &self,
        state: &mut automerge::sync::State,
        bytes: &[u8],
    ) -> anyhow::Result<bool> {
        let message = automerge::sync::Message::decode(bytes)?;
        let mut doc = self.doc.lock().expect("store lock");
        let before = doc.get_heads();
        doc.sync().receive_sync_message(state, message)?;
        let changed = doc.get_heads() != before;
        if changed {
            self.persist(&mut doc);
        }
        Ok(changed)
    }

    /// Write the full document to disk (atomic via temp file + rename).
    /// Documents here are small app states; incremental saves can come later.
    fn persist(&self, doc: &mut AutoCommit) {
        let bytes = doc.save();
        let tmp = self.path.with_extension("automerge.tmp");
        let result = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = result {
            tracing::error!("failed to persist document to {}: {e}", self.path.display());
        }
    }
}
