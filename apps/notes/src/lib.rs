//! Notes — the smallest possible Tangram app: a replicated list of notes.

use tangram::prelude::*;

#[model]
#[derive(Default)]
pub struct Notes {
    notes: Vec<Note>,
}

#[model]
pub struct Note {
    id: String,
    text: String,
    created_at_ms: i64,
    /// When the note body was last edited. `None` on documents written by
    /// older binaries (the `missing` attribute hydrates the absent key);
    /// treat `created_at_ms` as the edit time in that case.
    #[autosurgeon(missing = "Option::default")]
    updated_at_ms: Option<i64>,
}

impl Note {
    /// Effective last-edited time: `updated_at_ms`, falling back to creation.
    fn edited_at_ms(&self) -> i64 {
        self.updated_at_ms.unwrap_or(self.created_at_ms)
    }
}

#[actions]
impl Notes {
    /// Add a note with the given body text. Returns the new note's id.
    /// The note's title is its first line; there is no separate title field.
    pub fn add_note(&mut self, text: String) -> String {
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        self.notes.push(Note {
            id: id.clone(),
            text,
            created_at_ms: now,
            updated_at_ms: Some(now),
        });
        id
    }

    /// Create a new empty note and return its id. Use `update_note` to fill
    /// in the body afterwards. This is how the editor UI starts a note.
    pub fn create_note(&mut self) -> String {
        self.add_note(String::new())
    }

    /// Replace the entire body text of an existing note (last-writer-wins;
    /// there is no partial/merge editing) and stamp its last-edited time.
    /// The first line of the text serves as the note's title. Errors if no
    /// note has the given id.
    pub fn update_note(&mut self, id: String, text: String) -> Result<(), String> {
        let note = self
            .notes
            .iter_mut()
            .find(|n| n.id == id)
            .ok_or_else(|| format!("no note with id {id}"))?;
        note.text = text;
        note.updated_at_ms = Some(now_ms());
        Ok(())
    }

    /// Delete a note by id. Errors if no note has the given id.
    pub fn delete_note(&mut self, id: String) -> Result<(), String> {
        let before = self.notes.len();
        self.notes.retain(|n| n.id != id);
        if self.notes.len() == before {
            return Err(format!("no note with id {id}"));
        }
        Ok(())
    }

    /// List all notes, most recently edited first (notes never edited sort
    /// by creation time).
    pub fn list_notes(&self) -> Vec<Note> {
        let mut notes = self.notes.clone();
        notes.sort_by_key(|n| std::cmp::Reverse(n.edited_at_ms()));
        notes
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The notes app, fully configured. Call `.serve()` to run it standalone or
/// `.build()` to mount it in a multi-app host.
pub fn app() -> App<Notes> {
    App::<Notes>::new("notes")
        .instructions(
            "A shared, replicated notes list. Notes you add are visible to humans \
             in the web UI and on every synced device.",
        )
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
}
