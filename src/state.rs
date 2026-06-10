//! Shared application state.
//!
//! This is the heart of the template's design: one backend state, exposed
//! through two frontends — MCP tools (for AI) and a JSON/web API (for humans).
//! Replace the notes scratchpad with your app's real domain logic.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Note {
    pub id: u64,
    pub text: String,
    pub created_at_unix: u64,
}

#[derive(Clone, Default)]
pub struct AppState {
    notes: Arc<RwLock<Vec<Note>>>,
    next_id: Arc<AtomicU64>,
}

impl AppState {
    pub async fn add_note(&self, text: String) -> Note {
        let note = Note {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            text,
            created_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        self.notes.write().await.push(note.clone());
        note
    }

    pub async fn list_notes(&self) -> Vec<Note> {
        self.notes.read().await.clone()
    }

    pub async fn clear_notes(&self) -> usize {
        let mut notes = self.notes.write().await;
        let count = notes.len();
        notes.clear();
        count
    }
}
