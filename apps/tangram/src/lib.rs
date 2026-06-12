//! tangram — the Obsidian-style shell app and the host's default view.
//!
//! This app is special in two ways, both blessed by ADR-0007:
//!
//! 1. Its **frontend is a build (`ui/dist/`), not a single file** — the only
//!    app granted that exception (it bundles a markdown renderer + the
//!    sidebar/tab chrome). Every other app keeps the strict single-file UI
//!    contract.
//! 2. Its **backend is a perfectly ordinary wasm component** under the
//!    unchanged capability contract — a markdown vault (folders + `.md`
//!    files) living entirely in the app's replicated Automerge document. No
//!    host filesystem, no network, just state transitions.
//!
//! Folder structure is DERIVED from `/`-separated `path`s (the same trick the
//! notes app uses for titles), so the model stays a flat, deterministic
//! `Vec<MdFile>` — no `HashMap` (model `Default` must be deterministic to be
//! the shared genesis commit).
//!
//! Phase S1 (this slice) ships the model + actions and the shell UI (sidebar
//! folder tree + live apps list + tabbed main window with an Obsidian-style
//! `CodeMirror` live-preview markdown editor and app-iframe embedding). See
//! `ui/README.md` for the deferred follow-up phases (marketplace upload,
//! default-`/` route, `postMessage` coordination, etc.).

use tangram::prelude::*;

#[model]
pub struct Vault {
    files: Vec<MdFile>,
}

/// `Default` is the shared genesis commit, so it must be DETERMINISTIC and
/// byte-identical across native and wasm builds (no random ids, no clock).
/// We seed exactly one welcome note with a fixed id and zero timestamps —
/// proving the live-preview editor end to end on a fresh vault.
impl Default for Vault {
    fn default() -> Self {
        Self {
            files: vec![MdFile {
                id: "welcome".to_string(),
                path: "welcome.md".to_string(),
                body: welcome_body(),
                created_at_ms: 0,
                updated_at_ms: None,
            }],
        }
    }
}

/// One markdown file in the vault. Its `path` is a `/`-separated virtual
/// path (e.g. `projects/roadmap.md`); folders are derived from the path
/// segments, never stored explicitly — an empty folder is simply one with no
/// files under it, and is represented by a zero-body placeholder file named
/// `<folder>/.keep` so the tree can show it (see `create_folder`).
#[model]
pub struct MdFile {
    /// Stable id, independent of the (renameable) path.
    id: String,
    /// `/`-separated virtual path including the filename, e.g.
    /// `projects/roadmap.md`. Unique within the vault.
    path: String,
    /// Raw markdown text. Rendered inline by the client's Obsidian-style
    /// `CodeMirror` live-preview editor (the editable view *is* the render).
    body: String,
    created_at_ms: i64,
    /// When the body was last edited. `None` on documents written by older
    /// binaries (the `missing` attribute hydrates the absent key); treat
    /// `created_at_ms` as the edit time in that case.
    #[autosurgeon(missing = "Option::default")]
    updated_at_ms: Option<i64>,
}

/// The sentinel filename that materializes an otherwise-empty folder in the
/// tree. A folder `foo/bar` with no real files is kept alive by a file at
/// `foo/bar/.keep`; the UI hides `.keep` entries and shows the folder.
const KEEP: &str = ".keep";

#[actions]
impl Vault {
    /// List every file in the vault, sorted by path (the UI derives the
    /// folder tree from the `/`-separated paths). Includes `.keep` sentinels;
    /// the UI is responsible for hiding them and surfacing their folders.
    pub fn list_files(&self) -> Vec<MdFile> {
        let mut files = self.files.clone();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }

    /// Read one file's raw markdown body by id. Errors if absent.
    pub fn read_file(&self, id: String) -> Result<String, String> {
        self.files
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.body.clone())
            .ok_or_else(|| format!("no file with id {id}"))
    }

    /// Create a new `.md` file at `path` with optional initial `body`.
    /// Normalizes the path and rejects a collision. Returns the new id.
    pub fn create_file(&mut self, path: String, body: String) -> Result<String, String> {
        let path = normalize_path(&path)?;
        if self.files.iter().any(|f| f.path == path) {
            return Err(format!("a file already exists at {path}"));
        }
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        self.files.push(MdFile {
            id: id.clone(),
            path,
            body,
            created_at_ms: now,
            updated_at_ms: Some(now),
        });
        Ok(id)
    }

    /// Replace a file's entire body (last-writer-wins) and stamp the edit
    /// time. Errors if no file has the given id.
    pub fn write_file(&mut self, id: String, body: String) -> Result<(), String> {
        let file = self
            .files
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| format!("no file with id {id}"))?;
        file.body = body;
        file.updated_at_ms = Some(now_ms());
        Ok(())
    }

    /// Rename / move a file to a new `path`. Rejects a collision with an
    /// existing file. Errors if no file has the given id.
    pub fn rename_file(&mut self, id: String, new_path: String) -> Result<(), String> {
        let new_path = normalize_path(&new_path)?;
        if self.files.iter().any(|f| f.id != id && f.path == new_path) {
            return Err(format!("a file already exists at {new_path}"));
        }
        let file = self
            .files
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| format!("no file with id {id}"))?;
        file.path = new_path;
        file.updated_at_ms = Some(now_ms());
        Ok(())
    }

    /// Delete a file by id. Errors if no file has the given id.
    pub fn delete_file(&mut self, id: String) -> Result<(), String> {
        let before = self.files.len();
        self.files.retain(|f| f.id != id);
        if self.files.len() == before {
            return Err(format!("no file with id {id}"));
        }
        Ok(())
    }

    /// Materialize an (empty) folder at `path` by creating a hidden `.keep`
    /// sentinel under it, so the tree can show the folder before it has any
    /// real files. No-op (Ok) if the folder already has any file under it.
    pub fn create_folder(&mut self, path: String) -> Result<(), String> {
        let folder = normalize_folder(&path)?;
        let prefix = format!("{folder}/");
        if self.files.iter().any(|f| f.path.starts_with(&prefix)) {
            return Ok(());
        }
        let now = now_ms();
        self.files.push(MdFile {
            id: uuid::Uuid::new_v4().to_string(),
            path: format!("{prefix}{KEEP}"),
            body: String::new(),
            created_at_ms: now,
            updated_at_ms: Some(now),
        });
        Ok(())
    }

    /// Rename a folder: rewrite the prefix of every file path under it.
    /// Errors if the destination would collide with an existing file path.
    pub fn rename_folder(&mut self, path: String, new_path: String) -> Result<(), String> {
        let from = normalize_folder(&path)?;
        let to = normalize_folder(&new_path)?;
        let from_prefix = format!("{from}/");
        let to_prefix = format!("{to}/");
        // Collision check: any existing path (not itself under `from`) that
        // equals a destination path we'd produce.
        let moving: Vec<String> = self
            .files
            .iter()
            .filter(|f| f.path.starts_with(&from_prefix))
            .map(|f| f.path[from_prefix.len()..].to_string())
            .collect();
        if moving.is_empty() {
            return Err(format!("no folder at {from}"));
        }
        for tail in &moving {
            let dest = format!("{to_prefix}{tail}");
            if self.files.iter().any(|f| f.path == dest) {
                return Err(format!("a file already exists at {dest}"));
            }
        }
        let now = now_ms();
        for file in self.files.iter_mut() {
            if let Some(tail) = file.path.strip_prefix(&from_prefix) {
                file.path = format!("{to_prefix}{tail}");
                file.updated_at_ms = Some(now);
            }
        }
        Ok(())
    }

    /// Delete a folder and every file under it. Errors if the folder is
    /// empty/absent.
    pub fn delete_folder(&mut self, path: String) -> Result<(), String> {
        let folder = normalize_folder(&path)?;
        let prefix = format!("{folder}/");
        let before = self.files.len();
        self.files.retain(|f| !f.path.starts_with(&prefix));
        if self.files.len() == before {
            return Err(format!("no folder at {folder}"));
        }
        Ok(())
    }
}

/// Normalize a file path: trim, collapse leading/trailing slashes and empty
/// segments, reject `.`/`..` segments and an empty result. Folder paths and
/// file paths share this; a file path additionally is expected to end in a
/// filename (not enforced — a trailing `.md` is a UI convention, not a model
/// invariant).
fn normalize_path(path: &str) -> Result<String, String> {
    let segments: Vec<&str> = path
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return Err("empty path".to_string());
    }
    if segments.iter().any(|s| *s == "." || *s == "..") {
        return Err("path may not contain '.' or '..' segments".to_string());
    }
    Ok(segments.join("/"))
}

/// Normalize a folder path (same rules as a file path; folders are just the
/// directory portion of file paths).
fn normalize_folder(path: &str) -> Result<String, String> {
    normalize_path(path)
}

fn now_ms() -> i64 {
    tangram::time::now_ms()
}

/// The seeded welcome note — a deterministic genesis so a fresh vault is not
/// empty (and proves rendering end to end). `Default` stays deterministic.
fn welcome_body() -> String {
    "# Welcome to tangram\n\nThis is the **tangram** shell — an Obsidian-style \
home for your device's apps (the *tangrams*).\n\n- The left sidebar lists your \
markdown vault and the live apps on this host.\n- Click a `.md` file to open it \
in a tab; click an app to embed it in a tab as an iframe.\n- Open as many tabs \
as you like.\n\nEdit this note in the editor pane, or create new notes and \
folders from the sidebar.\n"
        .to_string()
}

/// MCP / app instructions, shared between the native builder and the WASM
/// component's `describe()` export.
const INSTRUCTIONS: &str = "The tangram shell: a markdown vault (folders and .md files) that \
     also embeds the other apps on this host. Files are replicated and visible on every synced \
     device.";

/// The tangram app, fully configured. Call `.serve()` to run it standalone or
/// `.build()` to mount it in a multi-app host.
#[cfg(not(target_family = "wasm"))]
pub fn app() -> App<Vault> {
    App::<Vault>::new("tangram")
        .instructions(INSTRUCTIONS)
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/dist"))
}

// Compiled for wasm32-wasip2, the same model + actions become a Tangram
// component (`tangram-host` owns the platform around it). The genesis there
// is the derived `Default` (empty vault); the host serves the built UI dir.
#[cfg(target_family = "wasm")]
tangram::export_component!(Vault {
    name: "tangram",
    instructions: INSTRUCTIONS,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rejects_dot_segments() {
        assert!(normalize_path("a/../b").is_err());
        assert!(normalize_path("./a").is_err());
        assert!(normalize_path("").is_err());
        assert!(normalize_path("   ").is_err());
        assert_eq!(normalize_path("/a//b/").unwrap(), "a/b");
        assert_eq!(
            normalize_path(" projects / roadmap.md ").unwrap(),
            "projects/roadmap.md"
        );
    }

    fn empty() -> Vault {
        Vault { files: Vec::new() }
    }

    #[test]
    fn default_seeds_welcome_note() {
        let v = Vault::default();
        assert_eq!(v.list_files().len(), 1);
        assert_eq!(v.list_files()[0].path, "welcome.md");
    }

    #[test]
    fn create_read_write_delete_roundtrip() {
        let mut v = empty();
        let id = v.create_file("notes/hi.md".into(), "# hi".into()).unwrap();
        assert_eq!(v.read_file(id.clone()).unwrap(), "# hi");
        v.write_file(id.clone(), "# bye".into()).unwrap();
        assert_eq!(v.read_file(id.clone()).unwrap(), "# bye");
        v.delete_file(id.clone()).unwrap();
        assert!(v.read_file(id).is_err());
    }

    #[test]
    fn create_file_rejects_collision() {
        let mut v = empty();
        v.create_file("a.md".into(), String::new()).unwrap();
        assert!(v.create_file("a.md".into(), String::new()).is_err());
    }

    #[test]
    fn rename_file_moves_and_guards_collision() {
        let mut v = empty();
        let a = v.create_file("a.md".into(), String::new()).unwrap();
        let b = v.create_file("b.md".into(), String::new()).unwrap();
        v.rename_file(a.clone(), "sub/a.md".into()).unwrap();
        assert!(v.list_files().iter().any(|f| f.path == "sub/a.md"));
        // moving b onto a's new path collides
        assert!(v.rename_file(b, "sub/a.md".into()).is_err());
    }

    #[test]
    fn folder_create_rename_delete() {
        let mut v = empty();
        v.create_folder("projects".into()).unwrap();
        assert!(v.list_files().iter().any(|f| f.path == "projects/.keep"));
        let f = v
            .create_file("projects/x.md".into(), String::new())
            .unwrap();
        v.rename_folder("projects".into(), "work".into()).unwrap();
        assert!(v.list_files().iter().any(|f| f.path == "work/x.md"));
        assert!(v.read_file(f).is_ok());
        v.delete_folder("work".into()).unwrap();
        assert!(v.list_files().is_empty());
    }
}
