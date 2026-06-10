//! Notes — the smallest possible Tangram app: a replicated list of notes.

use tangram::prelude::*;

#[model]
#[derive(Default)]
struct Notes {
    notes: Vec<Note>,
}

#[model]
struct Note {
    id: String,
    text: String,
    created_at_ms: i64,
}

#[actions]
impl Notes {
    /// Add a note. Returns the new note's id.
    pub fn add_note(&mut self, text: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.notes.push(Note {
            id: id.clone(),
            text,
            created_at_ms: now_ms(),
        });
        id
    }

    /// Delete a note by id.
    pub fn delete_note(&mut self, id: String) -> Result<(), String> {
        let before = self.notes.len();
        self.notes.retain(|n| n.id != id);
        if self.notes.len() == before {
            return Err(format!("no note with id {id}"));
        }
        Ok(())
    }

    /// List all notes, newest first.
    pub fn list_notes(&self) -> Vec<Note> {
        let mut notes = self.notes.clone();
        notes.sort_by_key(|n| std::cmp::Reverse(n.created_at_ms));
        notes
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    App::<Notes>::new("notes")
        .instructions(
            "A shared, replicated notes list. Notes you add are visible to humans \
             in the web UI and on every synced device.",
        )
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
        .serve()
        .await
}
