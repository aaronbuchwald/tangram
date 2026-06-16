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

mod agents;

#[model]
pub struct Vault {
    files: Vec<MdFile>,
    /// Last-run bookkeeping for cron-triggered agent INVOCATIONS, keyed by the
    /// invocation's stable id (R1: the trigger — and therefore the run cadence —
    /// belongs to the ```agent``` block, not the definition). A replicated `Vec`
    /// (not a `HashMap`; the model `Default` must stay deterministic) so a
    /// scheduled run survives a host restart and a device's view of "when did
    /// this invocation last run" replicates like any other state. Absent on
    /// documents written by older binaries (the `missing` attribute hydrates the
    /// empty default).
    #[autosurgeon(missing = "Option::default")]
    agent_runs: Option<Vec<AgentRun>>,
}

/// One invocation's last-run timestamp (the cron due-check reads this;
/// `tick_agents` updates it in the same commit that appends the run's output).
#[model]
pub struct AgentRun {
    /// The invocation's stable id (`agents::Invocation::invocation_id`).
    invocation_id: String,
    /// Wall-clock ms of the last run that completed.
    last_run_ms: i64,
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
            agent_runs: Some(Vec::new()),
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

    /// The host scheduler's per-tick entry point (host-side cron): scan every
    /// note body for ```agent``` invocation blocks with a `trigger: cron …`
    /// whose schedule says they are DUE, resolve each block's `use:` to a
    /// definition, run it (the def's instructions = system, the block's prompt =
    /// user), and append each completion right after its block. Returns the
    /// `use:` names that ran this tick. Resolves the LLM call OUTSIDE the lock
    /// and commits each result via `Ctx::mutate` (CLAUDE.md: the store lock is
    /// never held across an await).
    ///
    /// A no-op when nothing is due — the host dispatches this on a ~60s
    /// interval, so the common case is cheap (a snapshot scan, no egress).
    pub async fn tick_agents(ctx: Ctx<Self>) -> Result<Vec<String>, String> {
        let state = ctx.state().map_err(|e| e.to_string())?;
        let now = now_ms();

        // Index the definitions once (by lowercased name) so each due block can
        // resolve its `use:`. Definitions are pure capabilities (no trigger).
        let defs: Vec<agents::AgentDef> = state
            .files
            .iter()
            .filter_map(|f| agents::parse_agent(&f.body))
            .collect();
        let resolve = |use_name: &str| -> Option<agents::AgentDef> {
            let needle = use_name.trim().to_ascii_lowercase();
            defs.iter()
                .find(|d| d.name.to_ascii_lowercase() == needle)
                .cloned()
        };

        // Decide DUE (invocation, definition) pairs from a single snapshot.
        let due: Vec<(agents::Invocation, agents::AgentDef)> = state
            .files
            .iter()
            .flat_map(|f| agents::parse_invocations(&f.id, &f.body))
            .filter(|inv| inv.is_cron())
            .filter(|inv| agents::is_due(inv, state.last_run_ms(&inv.invocation_id), now))
            .filter_map(|inv| resolve(&inv.use_name).map(|def| (inv, def)))
            .collect();

        let mut ran = Vec::new();
        for (inv, def) in due {
            // Resolve the model response OUTSIDE the lock, then commit.
            match run_definition(&def, &inv.prompt).await {
                Ok(output) => {
                    ctx.mutate("tick_agents", |m| {
                        m.append_invocation_output(&inv, &def, &output, now_ms());
                    })
                    .map_err(|e| e.to_string())?;
                    ran.push(inv.use_name.clone());
                }
                // A failing invocation must not abort the whole tick — record the
                // error after its block (so the operator sees it) and continue.
                Err(e) => {
                    let msg = format!("error: {e}");
                    let _ = ctx.mutate("tick_agents", |m| {
                        m.append_invocation_output(&inv, &def, &msg, now_ms());
                    });
                }
            }
        }
        Ok(ran)
    }

    /// Force a single agent to run NOW, ignoring any schedule (a manual run from
    /// the UI or the host, and the seam the tests drive). Errors if no
    /// agent/skill named `name` exists in the vault. Appends the output to the
    /// agent's own note. Uses a minimal standing prompt (`Run now.`) since a
    /// manual run is not bound to a specific invocation block.
    pub async fn run_agent(ctx: Ctx<Self>, name: String) -> Result<String, String> {
        let state = ctx.state().map_err(|e| e.to_string())?;
        let needle = name.trim().to_ascii_lowercase();
        let def = state
            .files
            .iter()
            .filter_map(|f| agents::parse_agent(&f.body))
            .find(|d| d.name.to_ascii_lowercase() == needle)
            .ok_or_else(|| format!("no agent or skill named {name:?} in the vault"))?;

        let output = run_definition(&def, "Run now.").await?;
        ctx.mutate("run_agent", |m| {
            m.append_manual_output(&def, &output, now_ms());
        })
        .map_err(|e| e.to_string())?;
        Ok(output)
    }
}

impl Vault {
    /// The recorded last-run wall-clock for the invocation `id`, if any (the
    /// cron due-check reads this).
    fn last_run_ms(&self, id: &str) -> Option<i64> {
        self.agent_runs
            .as_ref()?
            .iter()
            .find(|r| r.invocation_id == id)
            .map(|r| r.last_run_ms)
    }

    /// Record an invocation's last run, upserting into the replicated
    /// `agent_runs` map (deterministic `Vec`, not a `HashMap`), keyed by the
    /// invocation's stable id.
    fn record_run(&mut self, id: &str, at_ms: i64) {
        let runs = self.agent_runs.get_or_insert_with(Vec::new);
        if let Some(run) = runs.iter_mut().find(|r| r.invocation_id == id) {
            run.last_run_ms = at_ms;
        } else {
            runs.push(AgentRun {
                invocation_id: id.to_string(),
                last_run_ms: at_ms,
            });
        }
    }

    /// Append a cron invocation's output right after its ```agent``` block and
    /// record the run time — both in the SAME commit (so the due-check and the
    /// visible output never disagree). The block is located by re-parsing the
    /// invocation's host note and matching the stable `invocation_id` (so an
    /// edit to the block — which changes the id — never appends to a stale spot).
    fn append_invocation_output(
        &mut self,
        inv: &agents::Invocation,
        def: &agents::AgentDef,
        output: &str,
        at_ms: i64,
    ) {
        let block = format!(
            "\n\n> Agent: /{name} · model: {model} · {trigger}\n> Output: {output}\n",
            name = def.name,
            model = def.model,
            trigger = inv.trigger.trim(),
        );
        // Find the host note (by id) and the live block end (re-parsed, so a
        // concurrent edit that moved/changed the block is handled safely: if the
        // id no longer matches any block we skip the append but still record the
        // run, so a vanished invocation does not re-fire).
        for file in self.files.iter_mut() {
            if let Some(live) = agents::parse_invocations(&file.id, &file.body)
                .into_iter()
                .find(|i| i.invocation_id == inv.invocation_id)
            {
                file.body.insert_str(live.block_end, &block);
                file.updated_at_ms = Some(at_ms);
                break;
            }
        }
        self.record_run(&inv.invocation_id, at_ms);
    }

    /// Append a MANUAL run's output block to the agent definition's own note
    /// (the `run_agent` path — not bound to an invocation block, so it appends
    /// to the def note like the one-time inline flow does, and records no
    /// invocation last-run).
    fn append_manual_output(&mut self, def: &agents::AgentDef, output: &str, at_ms: i64) {
        let block = format!(
            "\n\n> Agent: /{name} · model: {model} · (manual)\n> Output: {output}\n",
            name = def.name,
            model = def.model,
        );
        for file in self.files.iter_mut() {
            if agents::parse_agent(&file.body).is_some_and(|d| d.name == def.name) {
                file.body.push_str(&block);
                file.updated_at_ms = Some(at_ms);
                break;
            }
        }
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

/// The DeepSeek chat-completions endpoint. Overridable via `TANGRAM_AGENT_LLM_URL`
/// so a test/CI run can point the call at a local recorded-fixture server (no
/// live key, no real egress).
const DEEPSEEK_URL: &str = "https://api.deepseek.com/v1/chat/completions";

fn agent_llm_url() -> String {
    std::env::var("TANGRAM_AGENT_LLM_URL").unwrap_or_else(|_| DEEPSEEK_URL.to_string())
}

/// Run one agent definition with a `prompt`: issue a BARE chat-completions call
/// to DeepSeek (system = the definition's instructions, user = the invocation's
/// `prompt`) and return the assistant's text. The request carries NO API key —
/// the HOST injects the DeepSeek credential at the component's http-fetch egress
/// boundary (ADR-0005), so the key never enters the component's address space.
async fn run_definition(def: &agents::AgentDef, prompt: &str) -> Result<String, String> {
    use tangram::http;

    let body = serde_json::json!({
        "model": def.model,
        "messages": [
            { "role": "system", "content": def.instructions },
            { "role": "user", "content": prompt },
        ],
    });

    let req = http::Request::post(agent_llm_url()).json(&body);
    let resp = http::fetch(req).await.map_err(|e| e.to_string())?;
    let payload: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    if !resp.is_success() {
        return Err(format!(
            "DeepSeek request failed ({}): {payload}",
            resp.status
        ));
    }
    // OpenAI-shaped response: choices[0].message.content.
    payload
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|choices| choices.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("DeepSeek response had no message content: {payload}"))
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
        Vault {
            files: Vec::new(),
            agent_runs: Some(Vec::new()),
        }
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

    /// The #14 happy path the shell UI now drives: create a folder, create a
    /// file *inside* it (the UI joins the clicked folder's path to the typed
    /// name), edit the body, and confirm the edit persists and the file lives
    /// under the folder. Guards against a regression where a note created "in"
    /// a folder silently landed at the vault root.
    #[test]
    fn create_file_inside_folder_then_edit_persists() {
        let mut v = empty();
        v.create_folder("projects".into()).unwrap();
        // UI joins folder + filename → "projects/roadmap.md".
        let id = v
            .create_file("projects/roadmap.md".into(), "# Roadmap\n\n".into())
            .unwrap();
        assert!(
            v.list_files()
                .iter()
                .any(|f| f.path == "projects/roadmap.md"),
            "the new file should live under the folder, not at the root"
        );
        v.write_file(id.clone(), "# Roadmap\n\n- ship it\n".into())
            .unwrap();
        assert_eq!(v.read_file(id).unwrap(), "# Roadmap\n\n- ship it\n");
    }

    /// The append path the cron tick drives: an invocation block's output is
    /// inserted right after the block, and the run is recorded by the block's
    /// stable id (not the definition name) so a second, distinct invocation of
    /// the same def is tracked independently.
    #[test]
    fn invocation_output_appends_after_block_and_records_by_id() {
        let mut v = empty();
        let body = "# Daily\n\n```agent\nuse: standup\ntrigger: cron @hourly\n\
                    prompt: Summarize today.\n```\n\ntail";
        let id = v.create_file("daily.md".into(), body.into()).unwrap();
        let file = v.files.iter().find(|f| f.id == id).unwrap();
        let inv = agents::parse_invocations(&file.id, &file.body)
            .into_iter()
            .next()
            .unwrap();
        let def = agents::AgentDef {
            kind: "skill".into(),
            name: "standup".into(),
            model: "deepseek-chat".into(),
            instructions: "Write a status.".into(),
        };
        assert_eq!(v.last_run_ms(&inv.invocation_id), None);
        v.append_invocation_output(&inv, &def, "all good", 1_234);
        let after = v.read_file(id).unwrap();
        // The output block lands right after the fence, before `tail`.
        let fence_end = after.find("```\n").unwrap() + "```\n".len();
        assert!(after[fence_end..].starts_with("\n\n> Agent: /standup"));
        assert!(after.contains("> Output: all good"));
        assert!(after.trim_end().ends_with("tail"));
        // Run recorded by invocation id.
        assert_eq!(v.last_run_ms(&inv.invocation_id), Some(1_234));
    }

    /// Editing the block changes its id, so the prior run no longer suppresses
    /// it (stray-ref-safe / self-cleaning bookkeeping).
    #[test]
    fn editing_a_block_yields_a_fresh_invocation_id() {
        let a = agents::invocation_id("f", "x", "cron @hourly", "p");
        let b = agents::invocation_id("f", "x", "cron @daily", "p");
        assert_ne!(a, b);
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
