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
mod mcp_client;

#[model]
pub struct Vault {
    files: Vec<MdFile>,
    /// LEGACY: last-run bookkeeping for the retired ```agent```-block scheduling
    /// path, keyed by the old content-hash invocation id. The redesign moved
    /// last-run onto the `invocations` index entry (`Invocation.last_run_ms`);
    /// this field is retained only so documents written by older binaries still
    /// hydrate (the `missing` attribute supplies the empty default), and is no
    /// longer written. A deterministic `Vec` (not a `HashMap`).
    #[autosurgeon(missing = "Option::default")]
    agent_runs: Option<Vec<AgentRun>>,
    /// The replicated index of SCHEDULED agent invocations (the redesign): the
    /// source of truth for a scheduled run's trigger/prompt/last-run/status,
    /// keyed by a stable UUID that is also embedded in the note text as an inline
    /// `[⚡ <agent>](agent://<id>)` link (the handle). The markdown carries only
    /// `{id, agent}`; everything else lives here. A deterministic `Vec` (not a
    /// `HashMap`; the model `Default` must stay deterministic). Absent on
    /// documents written by older binaries (the `missing` attribute hydrates the
    /// empty default).
    #[autosurgeon(missing = "Option::default")]
    invocations: Option<Vec<Invocation>>,
    /// Tools/MCP T1: the replicated record of the user's decision on each
    /// `kind: agent` definition's `mcp_servers:` access REQUEST. The request is
    /// the declaration (the def's `mcp_servers`); this records the user's grant
    /// against a HASH of the requested set, so if the def later changes its
    /// requested servers the grant goes stale → pending re-approval (the
    /// auto-todo plan-hash-bound-approval precedent). A deterministic `Vec`
    /// (not a `HashMap`), keyed by the def's `name`. Absent on documents written
    /// by older binaries (the `missing` attribute hydrates the empty default).
    #[autosurgeon(missing = "Option::default")]
    mcp_grants: Option<Vec<McpGrant>>,
    /// The replicated, append-only **executions log** (embedded-runs R3): one
    /// [`Execution`] record per Run execution, out of the note body. Backs the
    /// Run editor's History (Executions) tab and gives each run a reproducible
    /// snapshot (`config_hash` of the resolved effective config + the
    /// `output_block_id` the callout carries). A deterministic `Vec` (not a
    /// `HashMap`). Absent on documents written by older binaries (the `missing`
    /// attribute hydrates the empty default).
    #[autosurgeon(missing = "Option::default")]
    executions: Option<Vec<Execution>>,
}

/// One execution of a Run (embedded-runs R3) — the append-only executions log
/// entry. Records WHAT ran, WHEN, the OUTCOME, and a reproducibility snapshot:
/// `config_hash` is the sha256 of the resolved effective config (the Agent's
/// definition ⊕ the Run's overrides) at run time, and `output_block_id` is the
/// callout's block id so a row can deep-link to its card. Keyed by
/// `execution_id`; `run_id` ties it to the `invocations` index entry.
#[model]
pub struct Execution {
    /// A stable per-execution id (a UUID minted at append time).
    execution_id: String,
    /// The Run (`Invocation.id`) this execution belongs to.
    run_id: String,
    /// The agent/skill definition name that ran.
    agent: String,
    /// Wall-clock ms of the execution.
    ts: i64,
    /// `"ran"` | `"error"` (mirrors the Run status the execution produced).
    status: String,
    /// The model the execution called.
    model: String,
    /// The callout's block id (`agents::callout_block_id`) for deep-linking.
    output_block_id: String,
    /// sha256 (hex) of the resolved effective config that produced this
    /// execution (`agents::config_hash`) — the reproducibility seam a later
    /// versioning pass builds on (embedded-runs §4).
    config_hash: String,
}

/// LEGACY (retired block-scheduling path): one invocation's last-run timestamp,
/// keyed by the old content-hash id. Kept as a `#[model]` only so older
/// documents with an `agent_runs` array still hydrate; the redesign records
/// last-run on the `invocations` index entry instead.
#[model]
pub struct AgentRun {
    /// The retired content-hash invocation id.
    invocation_id: String,
    /// Wall-clock ms of the last run that completed.
    last_run_ms: i64,
}

/// One SCHEDULED agent invocation in the replicated index (the redesign). The
/// inline `[⚡ <agent>](agent://<id>)` link in a note is just the handle
/// (`{id, agent}`); this record owns the trigger, prompt, host-file pointer,
/// last-run bookkeeping, and status. Keyed by the stable `id` (a UUID the UI
/// mints when it inserts the link). A deterministic `Vec` (not a `HashMap`).
#[model]
pub struct Invocation {
    /// The stable UUID embedded in the note's `agent://<id>` link.
    id: String,
    /// The agent/skill definition this invocation runs (a definition `name`,
    /// matched case-insensitively against the vault's defs).
    agent: String,
    /// The raw schedule trigger, e.g. `2h`, `daily at 09:00 America/New_York`,
    /// `weekly on mon,wed,fri at 14:00 America/New_York` (the v2 grammar in
    /// `agents::parse_schedule`). Scheduled invocations only — one-time stays a
    /// run-now flow with no index entry.
    trigger: String,
    /// The user prompt sent on each run (the def's instructions are the system
    /// message; this is the user message).
    prompt: String,
    /// The note this invocation's link lives in (the file id where output is
    /// appended). The reconcile pass prunes entries whose link has vanished.
    host_file_id: String,
    /// Wall-clock ms of the last completed run, or `None` if it has never run
    /// (the due-check reads this). `None` on documents written by older binaries
    /// (the `missing` attribute hydrates the absent key).
    #[autosurgeon(missing = "Option::default")]
    last_run_ms: Option<i64>,
    /// The precomputed **next fire time** (epoch-ms): the scheduler selects an
    /// invocation when `next_fire_ms <= now`, instead of re-deriving due-ness for
    /// the whole index every tick (the next-fire model). Computed from the
    /// trigger's schedule grammar (`agents::next_fire_ms`, DST-aware) whenever the
    /// invocation is created/updated and after each run, and backfilled on a tick
    /// for any entry left `None` (older documents / a just-migrated index). `None`
    /// also for a `one-time`/unknown trigger (never scheduled). The `missing`
    /// attribute hydrates the absent key on documents written by older binaries.
    #[autosurgeon(missing = "Option::default")]
    next_fire_ms: Option<i64>,
    /// A short status string for the UI (`"scheduled"` | `"ran"` | `"error"`).
    /// Free-form/forward-compatible; the scheduler writes `"ran"`/`"error"`.
    status: String,
}

/// The user's decision on one `kind: agent` definition's MCP-access request
/// (Tools/MCP T1). The REQUEST is the definition's `mcp_servers:` declaration;
/// this records the user's grant against a HASH of that requested set
/// (`requested_hash`), so a later edit to the def's `mcp_servers` changes the
/// hash and renders the grant STALE → pending re-approval. T1 is the
/// access-control layer only — NO tool-calling loop and NO agentgateway curated
/// route read this yet (that is T2; enforcement lands with the tool loop).
#[model]
pub struct McpGrant {
    /// The definition's `name` this grant is for (case as stored; matched
    /// case-insensitively against the live defs).
    agent: String,
    /// The canonical requested-server set at the time of the decision (the
    /// def's `mcp_servers`, canonicalized).
    requested: Vec<String>,
    /// The hash of the requested set this decision binds to
    /// (`agents::mcp_request_hash`). The staleness check compares this against
    /// the live def's request hash.
    requested_hash: String,
    /// The servers actually approved (== `requested` on approval; empty on a
    /// pending/denied/revoked grant).
    approved: Vec<String>,
    /// `"pending"` | `"approved"` | `"denied"`. A `"revoked"` decision is
    /// modeled as `"denied"` with an empty `approved` set (so the UI offers
    /// re-approval), keeping the status set small and deterministic.
    status: String,
    /// Wall-clock ms of the last decision.
    updated_at_ms: i64,
}

/// Grant status values (string-typed in the model for forward-compatibility and
/// trivial JSON shape; these are the only values written).
const STATUS_PENDING: &str = "pending";
const STATUS_APPROVED: &str = "approved";
const STATUS_DENIED: &str = "denied";

/// Scheduled-invocation status values (string-typed for forward-compatibility):
/// freshly created / never run, currently executing, last run succeeded, last
/// run errored. `running` is written in a commit *before* the (lock-free) LLM
/// call and cleared by the append commit, so the replicated chip can reflect an
/// in-flight execution between the two commits (embedded-runs R1; the chip's
/// "Running" state in `apps/tangram/ui/src/agentLink.ts`).
const STATUS_SCHEDULED: &str = "scheduled";
const STATUS_RUNNING: &str = "running";
const STATUS_RAN: &str = "ran";
const STATUS_ERROR: &str = "error";

/// The effective MCP status of one agent, computed from the live def's request
/// and the recorded grant (the [`Vault::mcp_status`] read returns these). This
/// is what the Agents-view approval UI renders.
#[model]
pub struct McpStatus {
    /// The agent definition's name.
    agent: String,
    /// The canonical servers this agent currently REQUESTS (the live def's
    /// `mcp_servers`).
    requested: Vec<String>,
    /// The hash of the current request (what `approve_mcp` must be called with).
    requested_hash: String,
    /// The servers currently APPROVED (empty unless `status == "approved"`).
    approved: Vec<String>,
    /// One of `"pending"` | `"approved"` | `"denied"` | `"stale"`. `"stale"`
    /// means a grant exists but the def's request changed since the decision —
    /// treated by the UI exactly like `"pending"` (re-approval required), but
    /// surfaced distinctly so the user sees the request moved.
    status: String,
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
            invocations: Some(Vec::new()),
            mcp_grants: Some(Vec::new()),
            executions: Some(Vec::new()),
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

    // ── scheduled invocations (the redesign): the replicated index ────────────
    //
    // A scheduled invocation is an inline `[⚡ <agent>](agent://<id>)` link in a
    // note (the handle) backed by an entry in the `invocations` index (the
    // trigger/prompt/last-run). The UI mints the UUID, inserts the link, and
    // calls `create_invocation`. Editing the trigger/prompt goes through
    // `update_invocation`; removing the link prunes the entry on the next tick.

    /// Create (or replace) a scheduled invocation in the index, keyed by `id`
    /// (the UUID the UI embedded in the note's `agent://<id>` link). Idempotent
    /// on `id` (a re-create overwrites trigger/prompt/agent and resets the
    /// last-run). The link insertion into the note is the UI's job; this records
    /// the source of truth.
    pub fn create_invocation(
        &mut self,
        id: String,
        agent: String,
        trigger: String,
        prompt: String,
        host_file_id: String,
    ) -> Result<(), String> {
        let id = id.trim().to_string();
        if id.is_empty() {
            return Err("invocation id must not be empty".to_string());
        }
        // Compute the next fire from the (fresh, never-run) trigger so the tick
        // can select by stored timestamp. `None` ⇒ a one-time/unknown trigger.
        let now = now_ms();
        let next_fire_ms = agents::next_fire_ms(&trigger, None, now);
        let invs = self.invocations.get_or_insert_with(Vec::new);
        if let Some(existing) = invs.iter_mut().find(|i| i.id == id) {
            existing.agent = agent;
            existing.trigger = trigger;
            existing.prompt = prompt;
            existing.host_file_id = host_file_id;
            existing.last_run_ms = None;
            existing.next_fire_ms = next_fire_ms;
            existing.status = STATUS_SCHEDULED.to_string();
        } else {
            invs.push(Invocation {
                id,
                agent,
                trigger,
                prompt,
                host_file_id,
                last_run_ms: None,
                next_fire_ms,
                status: STATUS_SCHEDULED.to_string(),
            });
        }
        Ok(())
    }

    /// Edit a scheduled invocation's trigger + prompt in place (the Trigger
    /// popup's Save). Resets the last-run/status so the new trigger fires on its
    /// own cadence. Errors if no invocation has the given id.
    pub fn update_invocation(
        &mut self,
        id: String,
        trigger: String,
        prompt: String,
    ) -> Result<(), String> {
        let inv = self
            .invocations
            .get_or_insert_with(Vec::new)
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| format!("no invocation with id {id}"))?;
        inv.next_fire_ms = agents::next_fire_ms(&trigger, None, now_ms());
        inv.trigger = trigger;
        inv.prompt = prompt;
        inv.last_run_ms = None;
        inv.status = STATUS_SCHEDULED.to_string();
        Ok(())
    }

    /// Delete a scheduled invocation from the index by id (the popup's explicit
    /// delete; removing the inline link also prunes it on the next tick). Errors
    /// if no invocation has the given id.
    pub fn delete_invocation(&mut self, id: String) -> Result<(), String> {
        let invs = self.invocations.get_or_insert_with(Vec::new);
        let before = invs.len();
        invs.retain(|i| i.id != id);
        if invs.len() == before {
            return Err(format!("no invocation with id {id}"));
        }
        Ok(())
    }

    /// List the scheduled invocations in the index (the UI reads these off the
    /// state frame, but this is also a queryable action for parity/tests).
    pub fn list_invocations(&self) -> Vec<Invocation> {
        self.invocations.clone().unwrap_or_default()
    }

    /// Prune index entries whose backing `agent://<id>` link no longer exists in
    /// any note body (stray-ref reconcile — no orphans, like the wikilink index).
    /// Returns the number pruned. Also runs implicitly on every tick.
    pub fn reconcile_invocations(&mut self) -> i64 {
        let pruned = self.prune_orphan_invocations();
        i64::try_from(pruned).unwrap_or(i64::MAX)
    }

    /// The host scheduler's per-tick entry point (host-side cron): scan the
    /// replicated `invocations` index for DUE recurring invocations (reusing the
    /// v2 schedule grammar on each `Invocation.trigger`), resolve each
    /// invocation's `agent` to a definition, run it (the def's instructions =
    /// system, the invocation's `prompt` = user), append each completion near the
    /// `](agent://<id>)` link in the host note, and record `last_run_ms` by id.
    /// Returns the `agent` names that ran this tick. Resolves the LLM call
    /// OUTSIDE the lock and commits each result via `Ctx::mutate` (CLAUDE.md: the
    /// store lock is never held across an await). A stray-ref reconcile (prune
    /// index entries with no backing link) runs first.
    ///
    /// A no-op when nothing is due — the host dispatches this on a ~60s
    /// interval, so the common case is cheap (a snapshot scan, no egress).
    pub async fn tick_agents(ctx: Ctx<Self>) -> Result<Vec<String>, String> {
        // Stray-ref reconcile up front (self-cleaning, like the link index): an
        // invocation whose inline link was deleted should not keep firing.
        ctx.mutate("tick_agents", Self::prune_orphan_invocations)
            .map_err(|e| e.to_string())?;

        let now = now_ms();
        // Backfill the stored next-fire for any entry left `None` (older
        // documents / a just-migrated index), so the timestamp selection below is
        // complete. A no-op once every entry has a next-fire.
        ctx.mutate("tick_agents", |m| m.backfill_next_fire(now))
            .map_err(|e| e.to_string())?;

        let state = ctx.state().map_err(|e| e.to_string())?;

        // Index the definitions once (by lowercased name) so each due invocation
        // can resolve its `agent`. Definitions are pure capabilities (no trigger).
        let defs: Vec<agents::AgentDef> = state
            .files
            .iter()
            .filter_map(|f| agents::parse_agent(&f.body))
            .collect();
        let resolve = |agent: &str| -> Option<agents::AgentDef> {
            let needle = agent.trim().to_ascii_lowercase();
            defs.iter()
                .find(|d| d.name.to_ascii_lowercase() == needle)
                .cloned()
        };

        // Select the "due now" set by the STORED next-fire timestamp instead of
        // re-deriving due-ness for every entry (the next-fire model). After the
        // backfill above, every scheduled entry carries a `next_fire_ms`; a
        // `None` here is a one-time/unknown trigger (never scheduled) and is
        // skipped. The next-fire is the inverse of the old due check, so the same
        // invocations fire at the same times (see `agents::next_fire_ms`).
        let due: Vec<(Invocation, agents::AgentDef)> = state
            .invocations
            .iter()
            .flatten()
            .filter(|inv| inv.next_fire_ms.is_some_and(|nf| nf <= now))
            .filter_map(|inv| resolve(&inv.agent).map(|def| (inv.clone(), def)))
            .collect();

        let mut ran = Vec::new();
        for (inv, def) in due {
            // The per-agent approved MCP subset (T2), from the same snapshot.
            let servers = state.approved_servers_for(&def);
            // Mark this Run "running" BEFORE the lock-free LLM call so the
            // replicated chip can show an in-flight execution (embedded-runs R1).
            // The append commit below clears it to `ran`/`error`.
            let _ = ctx.mutate("tick_agents", |m| m.mark_invocation_running(&inv.id));
            // Resolve the model response OUTSIDE the lock, then commit.
            match run_definition(&def, &inv.prompt, &servers).await {
                Ok(output) => {
                    ctx.mutate("tick_agents", |m| {
                        m.append_invocation_output(&inv, &def, &output, now_ms(), STATUS_RAN);
                    })
                    .map_err(|e| e.to_string())?;
                    ran.push(inv.agent.clone());
                }
                // A failing invocation must not abort the whole tick — record the
                // error near its link (so the operator sees it) and continue.
                Err(e) => {
                    let msg = format!("error: {e}");
                    let _ = ctx.mutate("tick_agents", |m| {
                        m.append_invocation_output(&inv, &def, &msg, now_ms(), STATUS_ERROR);
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

        // The per-agent approved MCP subset (T2): only servers the user granted
        // (and the grant is fresh) get a tool plane. Resolved from a snapshot
        // before the (lock-free) run.
        let servers = state.approved_servers_for(&def);
        let output = run_definition(&def, "Run now.", &servers).await?;
        ctx.mutate("run_agent", |m| {
            m.append_manual_output(&def, &output, now_ms());
        })
        .map_err(|e| e.to_string())?;
        Ok(output)
    }

    /// List the executions log (embedded-runs R3), newest first. The UI reads
    /// the log off the vault state frame; this is also a queryable action for
    /// parity/tests.
    pub fn list_executions(&self) -> Vec<Execution> {
        let mut out = self.executions.clone().unwrap_or_default();
        out.sort_by(|a, b| b.ts.cmp(&a.ts));
        out
    }

    // ── Tools/MCP T1: per-agent MCP access requests + user approval ───────────
    //
    // The REQUEST is the `kind: agent` definition's `mcp_servers:` declaration;
    // these actions record the USER's decision against a hash of the requested
    // set. They are user-approval gates (mirroring how auto-todo gates its
    // risk-bearing transitions); `require_auth` in apps.toml decides whether
    // they need a bearer like the other mutating actions.

    /// The effective MCP status of every `kind: agent` definition in the vault
    /// that requests servers, derived from the live defs + the recorded grants:
    /// `pending` (requested, no decision), `approved [servers]`, `denied`, or
    /// `stale` (a decision exists but the def's request changed since — the UI
    /// treats `stale` like `pending`). The UI renders the approval affordance
    /// from this. Agents that request no servers are omitted.
    pub fn mcp_status(&self) -> Vec<McpStatus> {
        let mut out: Vec<McpStatus> = Vec::new();
        // Index defs by name (first wins, matching the UI index); only agents
        // that actually request servers are surfaced.
        let mut seen: Vec<String> = Vec::new();
        for file in &self.files {
            let Some(def) = agents::parse_agent(&file.body) else {
                continue;
            };
            if def.kind != "agent" || def.mcp_servers.is_empty() {
                continue;
            }
            let key = def.name.to_ascii_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);

            let requested_hash = agents::mcp_request_hash(&def.mcp_servers);
            let (status, approved) = match self.grant_for(&def.name) {
                None => (STATUS_PENDING.to_string(), Vec::new()),
                Some(g) if g.requested_hash != requested_hash => {
                    // The def's request changed since the decision → stale,
                    // re-approval required (auto-todo plan-hash precedent).
                    ("stale".to_string(), Vec::new())
                }
                Some(g) if g.status == STATUS_APPROVED => {
                    (STATUS_APPROVED.to_string(), g.approved.clone())
                }
                Some(g) if g.status == STATUS_DENIED => (STATUS_DENIED.to_string(), Vec::new()),
                // Any other (pending) recorded grant on the current hash.
                Some(_) => (STATUS_PENDING.to_string(), Vec::new()),
            };
            out.push(McpStatus {
                agent: def.name,
                requested: def.mcp_servers,
                requested_hash,
                approved,
                status,
            });
        }
        out.sort_by(|a, b| {
            a.agent
                .to_ascii_lowercase()
                .cmp(&b.agent.to_ascii_lowercase())
        });
        out
    }

    /// Approve an agent's current MCP-server request (the user grant). Binds to
    /// `requested_hash`: only succeeds when it matches the agent's CURRENT
    /// request (guards approving a STALE set the user did not see). Sets the
    /// grant to `approved` with `approved == requested`. Errors if the agent
    /// has no MCP request, or the hash is stale.
    pub fn approve_mcp(&mut self, agent: String, requested_hash: String) -> Result<(), String> {
        let requested = self.live_request(&agent)?;
        let current = agents::mcp_request_hash(&requested);
        if current != requested_hash {
            return Err(format!(
                "stale request: {agent}'s requested MCP servers changed since you saw them \
                 (approving {requested_hash}, current is {current}); re-review and approve again"
            ));
        }
        self.upsert_grant(
            &agent,
            &requested,
            &current,
            STATUS_APPROVED,
            requested.clone(),
        );
        Ok(())
    }

    /// Deny an agent's MCP-server request. Records a `denied` decision bound to
    /// the current request hash (a later edit to the request re-opens it as
    /// stale/pending). Errors if the agent has no MCP request.
    pub fn deny_mcp(&mut self, agent: String) -> Result<(), String> {
        let requested = self.live_request(&agent)?;
        let hash = agents::mcp_request_hash(&requested);
        self.upsert_grant(&agent, &requested, &hash, STATUS_DENIED, Vec::new());
        Ok(())
    }

    /// Revoke a previously-approved (or any) MCP grant for an agent: modeled as
    /// a `denied` decision with an empty approved set, so the UI offers
    /// re-approval. Errors if the agent has no MCP request.
    pub fn revoke_mcp(&mut self, agent: String) -> Result<(), String> {
        self.deny_mcp(agent)
    }
}

impl Vault {
    /// The recorded grant for `agent` (case-insensitive on the name), if any.
    fn grant_for(&self, agent: &str) -> Option<&McpGrant> {
        let needle = agent.trim().to_ascii_lowercase();
        self.mcp_grants
            .as_ref()?
            .iter()
            .find(|g| g.agent.to_ascii_lowercase() == needle)
    }

    /// The MCP servers an agent definition may CURRENTLY use in a run: the
    /// servers it requests, intersected with a FRESH `approved` grant (Tools/MCP
    /// T2 — the grant is the thing that opens the tool plane). Returns empty
    /// when the agent requests nothing, has no grant, the grant is denied, or
    /// the grant is STALE (the def's request changed since the decision — the
    /// same staleness rule [`Vault::mcp_status`] surfaces). Canonicalized order
    /// (the grant stores canonical names). This is the per-agent subset the
    /// tool-loop offers; an un-granted server never appears here, so the loop
    /// never constructs a client for it.
    fn approved_servers_for(&self, def: &agents::AgentDef) -> Vec<String> {
        if def.kind != "agent" || def.mcp_servers.is_empty() {
            return Vec::new();
        }
        let live_hash = agents::mcp_request_hash(&def.mcp_servers);
        match self.grant_for(&def.name) {
            Some(g) if g.status == STATUS_APPROVED && g.requested_hash == live_hash => {
                // Intersect with the live request so a server the def no longer
                // asks for can never sneak through a stale-but-same-hash grant.
                g.approved
                    .iter()
                    .filter(|s| def.mcp_servers.contains(s))
                    .cloned()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// The live (canonical) MCP-server request for the `kind: agent` definition
    /// named `agent`. Errors if there is no such agent OR it requests nothing
    /// (the decision actions are meaningless without a request).
    fn live_request(&self, agent: &str) -> Result<Vec<String>, String> {
        let needle = agent.trim().to_ascii_lowercase();
        let def = self
            .files
            .iter()
            .filter_map(|f| agents::parse_agent(&f.body))
            .find(|d| d.kind == "agent" && d.name.to_ascii_lowercase() == needle)
            .ok_or_else(|| format!("no agent named {agent:?} in the vault"))?;
        if def.mcp_servers.is_empty() {
            return Err(format!("agent {agent:?} does not request any MCP servers"));
        }
        Ok(def.mcp_servers)
    }

    /// Upsert the grant for `agent` (keyed by name, case-insensitively),
    /// recording the decision against `hash`. The replicated `mcp_grants` is a
    /// deterministic `Vec` (not a `HashMap`).
    fn upsert_grant(
        &mut self,
        agent: &str,
        requested: &[String],
        hash: &str,
        status: &str,
        approved: Vec<String>,
    ) {
        let grants = self.mcp_grants.get_or_insert_with(Vec::new);
        let needle = agent.trim().to_ascii_lowercase();
        let now = now_ms();
        if let Some(g) = grants
            .iter_mut()
            .find(|g| g.agent.to_ascii_lowercase() == needle)
        {
            g.requested = requested.to_vec();
            g.requested_hash = hash.to_string();
            g.status = status.to_string();
            g.approved = approved;
            g.updated_at_ms = now;
        } else {
            grants.push(McpGrant {
                agent: agent.to_string(),
                requested: requested.to_vec(),
                requested_hash: hash.to_string(),
                approved,
                status: status.to_string(),
                updated_at_ms: now,
            });
        }
    }

    /// The recorded last-run wall-clock for the invocation `id` in the index, if
    /// any (read directly off the index entry in the tick; this convenience is
    /// used by the tests).
    #[cfg(test)]
    fn last_run_ms(&self, id: &str) -> Option<i64> {
        self.invocations
            .as_ref()?
            .iter()
            .find(|i| i.id == id)
            .and_then(|i| i.last_run_ms)
    }

    /// The set of live `agent://<id>` link ids across every note body. The
    /// reconcile pass keeps only index entries that still have a backing link.
    fn live_invocation_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for file in &self.files {
            for link in agents::parse_agent_links(&file.body) {
                if !ids.contains(&link.id) {
                    ids.push(link.id);
                }
            }
        }
        ids
    }

    /// Stray-ref reconcile: drop every index entry whose `agent://<id>` link no
    /// longer appears in any note body. Returns the number pruned (so the
    /// `reconcile_invocations` action can report it). Self-cleaning, like the
    /// wikilink index.
    fn prune_orphan_invocations(&mut self) -> usize {
        let live = self.live_invocation_ids();
        let invs = self.invocations.get_or_insert_with(Vec::new);
        let before = invs.len();
        invs.retain(|i| live.contains(&i.id));
        before - invs.len()
    }

    /// Backfill the stored `next_fire_ms` for any index entry that still has it
    /// `None` — older documents written before the field existed, or a freshly
    /// hydrated index. Computed from each entry's trigger + recorded last run
    /// (`agents::next_fire_ms`, DST-aware), so a backfilled entry fires exactly
    /// when the old derive-every-tick path would have. A no-op once every entry
    /// carries a next-fire (the steady-state). Entries whose trigger is
    /// one-time/unknown stay `None` (never scheduled).
    fn backfill_next_fire(&mut self, now: i64) {
        for inv in self.invocations.get_or_insert_with(Vec::new) {
            if inv.next_fire_ms.is_none() {
                inv.next_fire_ms = agents::next_fire_ms(&inv.trigger, inv.last_run_ms, now);
            }
        }
    }

    /// Append a scheduled invocation's output near its inline `agent://<id>` link
    /// and record the run (`last_run_ms` + `status`) on the index entry — both in
    /// the SAME commit (so the due-check and the visible output never disagree).
    /// The link is located by re-scanning the host note for the matching id (so a
    /// concurrent edit that moved the link is handled safely: if the id no longer
    /// has a link we skip the append but still record the run, so a vanished
    /// invocation does not re-fire). Falls back to scanning all notes when the
    /// recorded `host_file_id` no longer matches (the link was moved to another
    /// note).
    /// Mark a Run as currently executing (`STATUS_RUNNING`) by id, in its own
    /// commit before the lock-free LLM call. A no-op if the id is gone (the link
    /// was deleted between selection and now). Leaves `last_run_ms`/`next_fire_ms`
    /// untouched — only the visible status flips, so the replicated chip shows an
    /// in-flight execution; the append commit clears it to `ran`/`error`
    /// (embedded-runs R1).
    fn mark_invocation_running(&mut self, id: &str) {
        if let Some(entry) = self
            .invocations
            .get_or_insert_with(Vec::new)
            .iter_mut()
            .find(|i| i.id == id)
        {
            entry.status = STATUS_RUNNING.to_string();
        }
    }

    /// Record a Run execution (embedded-runs R3): render the output as a
    /// **callout card** below the chip's host paragraph (stamping the host's
    /// `^run-<id>` block id and the callout's `^runout-<id>` so the chip ⇄
    /// callout backlinks both resolve), refresh that card in place on a re-run
    /// (one card per Run, always the latest), update the index entry's
    /// last-run/next-fire/status, and append one [`Execution`] to the
    /// append-only log. This is the SINGLE display path for both one-time
    /// (`once`) and recurring Runs (embedded-runs R3 unification).
    fn append_invocation_output(
        &mut self,
        inv: &Invocation,
        def: &agents::AgentDef,
        output: &str,
        at_ms: i64,
        status: &str,
    ) {
        let is_error = status == STATUS_ERROR;
        let when = run_when_label(&inv.trigger);
        let callout =
            agents::build_run_callout(&inv.id, &def.name, &def.model, &when, output, is_error);

        // Locate the host note carrying the chip (recorded id first, then any
        // note carrying the link — the user may have moved it) and insert/refresh
        // the callout below the chip's paragraph, stamping the host block id.
        let mut placed = self.place_callout(inv.host_file_id.as_str(), &inv.id, &callout, at_ms);
        if !placed {
            // Fall back to any note carrying the link (host_file_id stale).
            let ids: Vec<String> = self.files.iter().map(|f| f.id.clone()).collect();
            for fid in ids {
                if self.place_callout(&fid, &inv.id, &callout, at_ms) {
                    placed = true;
                    break;
                }
            }
        }
        let _ = placed; // a vanished link records the run but appends no card

        // Record the run on the index entry by id (the source of truth).
        if let Some(entry) = self
            .invocations
            .get_or_insert_with(Vec::new)
            .iter_mut()
            .find(|i| i.id == inv.id)
        {
            entry.last_run_ms = Some(at_ms);
            // Advance the stored next fire from this run so the next tick selects
            // it only when the next occurrence/interval is reached (DST-aware).
            // For a `once` Run this is `None` — it never re-fires.
            entry.next_fire_ms = agents::next_fire_ms(&entry.trigger, Some(at_ms), at_ms);
            entry.status = status.to_string();
        }

        // Append the Execution record (embedded-runs R3 executions log).
        self.append_execution(inv, def, at_ms, status);
    }

    /// Insert (or refresh in place) the run-output callout for `run_id` in the
    /// note `file_id`, just below the chip's host paragraph, stamping the host
    /// block id `^run-<id>` on that paragraph. Returns true when the note
    /// carried the chip's link (so the card was placed). A re-run REPLACES the
    /// existing callout for the Run rather than appending a second card.
    fn place_callout(&mut self, file_id: &str, run_id: &str, callout: &str, at_ms: i64) -> bool {
        let Some(file) = self.files.iter_mut().find(|f| f.id == file_id) else {
            return false;
        };
        let Some(link) = agents::parse_agent_links(&file.body)
            .into_iter()
            .find(|l| l.id == run_id)
        else {
            return false;
        };

        // Stamp the host paragraph's block id once (idempotent), at the end of
        // the line the chip sits on, so the callout header's `[↑]` resolves.
        let host_anchor = format!("^{}", agents::host_block_id(run_id));
        if !file.body.contains(&host_anchor) {
            let para_end = agents::line_end(&file.body, link.link_end);
            file.body.insert_str(para_end, &format!(" {host_anchor}"));
        }

        // Refresh in place if a card for this Run already exists; else insert a
        // fresh card after the host paragraph (a blank line separates them).
        if let Some((start, end)) = agents::find_run_callout(&file.body, run_id) {
            file.body.replace_range(start..end, callout);
        } else {
            let para_end = agents::line_end(&file.body, link.link_end);
            file.body.insert_str(para_end, &format!("\n\n{callout}"));
        }
        file.updated_at_ms = Some(at_ms);
        true
    }

    /// Append one [`Execution`] to the replicated, append-only executions log
    /// (embedded-runs R3), snapshotting the resolved-effective-config hash and
    /// the callout's block id for the History tab + reproducibility.
    fn append_execution(
        &mut self,
        inv: &Invocation,
        def: &agents::AgentDef,
        at_ms: i64,
        status: &str,
    ) {
        let config_hash = agents::config_hash(def, &inv.prompt, &inv.trigger);
        let execution_id = uuid::Uuid::new_v4().to_string();
        self.executions
            .get_or_insert_with(Vec::new)
            .push(Execution {
                execution_id,
                run_id: inv.id.clone(),
                agent: def.name.clone(),
                ts: at_ms,
                status: status.to_string(),
                model: def.model.clone(),
                output_block_id: agents::callout_block_id(&inv.id),
                config_hash,
            });
    }

    /// Append a MANUAL run's output (the `run_agent` Re-run-now path) to the
    /// agent definition's own note as a callout card — the same visual language
    /// as a Run's output, but unbound (no chip, so no host-paragraph block id /
    /// backlink). Records no invocation last-run and no Execution (it is not a
    /// scheduled/once Run).
    fn append_manual_output(&mut self, def: &agents::AgentDef, output: &str, at_ms: i64) {
        // A synthetic, def-scoped block id keeps the card self-consistent without
        // colliding with a real Run's `^run-*`/`^runout-*` ids.
        let manual_id = format!("manual-{}", def.name.to_ascii_lowercase());
        let callout =
            agents::build_run_callout(&manual_id, &def.name, &def.model, "manual", output, false);
        for file in self.files.iter_mut() {
            if agents::parse_agent(&file.body).is_some_and(|d| d.name == def.name) {
                file.body.push_str(&format!("\n\n{callout}"));
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

/// A short human label for a Run's trigger, shown in the callout header (e.g.
/// `one-time` for a `once` Run, otherwise the raw trigger text trimmed). Kept
/// minimal — the Run editor renders the full schedule summary.
fn run_when_label(trigger: &str) -> String {
    match trigger.trim() {
        "once" => "one-time".to_string(),
        "" => "manual".to_string(),
        other => other.to_string(),
    }
}

/// The DeepSeek chat-completions endpoint (the default LLM target).
const DEEPSEEK_URL: &str = "https://api.deepseek.com/v1/chat/completions";

/// The chat-completions URL the agent run POSTs to. Default: DeepSeek. A
/// CI/test run points the call at a local fixture server via
/// `TANGRAM_AGENT_LLM_AUTHORITY` — a scheme-free `host:port` (the loop builds
/// `http://<authority>/v1/chat/completions`). It is an authority, not a full
/// URL, for the same reason as `TANGRAM_MCP_AUTHORITY`: an app-`env` value
/// containing `://` is read by the host as a `scheme://locator` secret
/// reference and an unknown scheme blanks it. When the fixture is loopback, the
/// declared `[[apps.tangram.calls]]` must permit `POST 127.0.0.1
/// /v1/chat/completions` (the live config grants only the real DeepSeek host).
fn agent_llm_url() -> String {
    match std::env::var("TANGRAM_AGENT_LLM_AUTHORITY")
        .ok()
        .filter(|a| !a.trim().is_empty())
    {
        Some(authority) => format!("http://{authority}/v1/chat/completions"),
        None => DEEPSEEK_URL.to_string(),
    }
}

/// The AUTHORITY (`host:port`) the agent tool-loop addresses an app's MCP
/// endpoint under — the loop builds `http://<authority>/<server>/mcp`. The
/// host's public listener is the same origin the shell is served from; the
/// component reaches it over loopback (always plain HTTP). Overridable via
/// `TANGRAM_MCP_AUTHORITY` (set in `[apps.tangram.env]`) so the operator pins
/// the host's bind address, and a test can point it at a fixture. Default
/// `127.0.0.1:8080` (the host's default bind).
///
/// It is an AUTHORITY, not a full URL, on purpose: a spec/env value containing
/// `://` is interpreted by the host secret-resolver as a `scheme://locator`
/// reference (an unknown scheme resolves to empty), so a `http://…` literal
/// would be silently blanked. The loop supplies the `http://` scheme.
///
/// IMPORTANT (host enforcement): the egress to `<authority>/<server>/mcp` is
/// itself gated by the tangram app's `allow_hosts` + call-level `[[apps.tangram.calls]]`
/// grant — an MCP endpoint the operator did NOT declare is denied at the
/// `http-fetch` boundary (`enforcement = "enforce"`). So the universe of
/// reachable servers is the operator's declared call list; the PER-AGENT
/// approved subset (the `servers` arg below) narrows within it. See
/// `mcp_client.rs` and the apps.toml `[[apps.tangram.calls]]` block.
#[cfg(not(test))]
const DEFAULT_MCP_AUTHORITY: &str = "127.0.0.1:8080";

#[cfg(not(test))]
fn agent_mcp_base() -> String {
    let authority = std::env::var("TANGRAM_MCP_AUTHORITY")
        .ok()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MCP_AUTHORITY.to_string());
    format!("http://{authority}")
}

/// The maximum number of model round-trips in one agent run (the tool-loop
/// cap). Each iteration is one DeepSeek call; tool calls in between do not
/// count against this — they advance toward a final answer. A run that never
/// settles is stopped with a clear note rather than looping unbounded.
#[cfg(not(test))]
const MAX_TOOL_ITERATIONS: usize = 6;

/// Run one agent definition with a `prompt` (Tools/MCP T2). The system message
/// is the definition's instructions; the user message is `prompt`. `servers` is
/// the PER-AGENT approved MCP subset (from [`Vault::approved_servers_for`]) —
/// only these servers' tools are offered, and the loop never constructs a
/// client for a server outside this list, so an un-granted server is never
/// reached. When `servers` is empty this is exactly the prior bare call.
///
/// The request carries NO API key — the HOST injects the DeepSeek credential at
/// the component's http-fetch egress boundary (ADR-0005), so the key never
/// enters the component's address space.
async fn run_definition(
    def: &agents::AgentDef,
    prompt: &str,
    servers: &[String],
) -> Result<String, String> {
    let messages = vec![
        serde_json::json!({ "role": "system", "content": def.instructions }),
        serde_json::json!({ "role": "user", "content": prompt }),
    ];
    if servers.is_empty() {
        // No approved tools → the prior bare chat-completions call (one round).
        let message = post_chat(&def.model, &messages, &[]).await?;
        return message_text(&message);
    }
    run_tool_loop(def, messages, servers).await
}

/// POST one OpenAI-style chat-completions request and return `choices[0].message`
/// (the assistant turn, which may carry `tool_calls`). `tools` is the
/// function-tool schema list (empty ⇒ no `tools`/`tool_choice` keys, identical
/// to the prior bare call).
async fn post_chat(
    model: &str,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
) -> Result<serde_json::Value, String> {
    use tangram::http;

    let mut body = serde_json::json!({ "model": model, "messages": messages });
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools.to_vec());
        body["tool_choice"] = serde_json::json!("auto");
    }

    let req = http::Request::post(agent_llm_url()).json(&body);
    let resp = http::fetch(req).await.map_err(|e| e.to_string())?;
    let payload: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    if !resp.is_success() {
        return Err(format!(
            "DeepSeek request failed ({}): {payload}",
            resp.status
        ));
    }
    payload
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|choices| choices.first())
        .and_then(|c| c.get("message"))
        .cloned()
        .ok_or_else(|| format!("DeepSeek response had no message: {payload}"))
}

/// The assistant text from a chat-completions `message` (the `content` field),
/// or an error if it is absent.
fn message_text(message: &serde_json::Value) -> Result<String, String> {
    message
        .get("content")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("DeepSeek response had no message content: {message}"))
}

/// The tool-calling loop over the agent's APPROVED MCP servers (T2). Lists each
/// approved server's tools, offers them to DeepSeek, and on `tool_calls`
/// executes each via MCP `tools/call` (routing the namespaced tool name back to
/// its server), feeds the results back, and re-asks — capped at
/// [`MAX_TOOL_ITERATIONS`] model round-trips. Returns the final answer with a
/// compact tool-call trace appended. A failure listing one server's tools is
/// non-fatal (that server is dropped, the run continues with the rest).
#[cfg(not(test))]
async fn run_tool_loop(
    def: &agents::AgentDef,
    mut messages: Vec<serde_json::Value>,
    servers: &[String],
) -> Result<String, String> {
    use std::collections::BTreeMap;

    let base = agent_mcp_base();
    let namespaced = servers.len() > 1;

    // One client per approved server; list its tools. A server that fails to
    // list is skipped (logged), not fatal.
    let mut clients: BTreeMap<String, mcp_client::McpClient> = BTreeMap::new();
    let mut openai_tools: Vec<serde_json::Value> = Vec::new();
    let mut trace: Vec<String> = Vec::new();
    for server in servers {
        let mut client = mcp_client::McpClient::new(&base, server);
        match client.list_tools().await {
            Ok(tools) => {
                openai_tools.extend(mcp_client::tools_to_openai(server, &tools, namespaced));
                clients.insert(server.clone(), client);
            }
            // A server that fails to list (denied egress, unreachable, …) is
            // dropped from this run rather than aborting it — the trace records
            // it so the operator sees which server was unavailable.
            Err(e) => trace.push(format!("{server} (unavailable): {}", truncate(&e, 160))),
        }
    }

    let default_server = servers.first().cloned().unwrap_or_default();

    for _ in 0..MAX_TOOL_ITERATIONS {
        let message = post_chat(&def.model, &messages, &openai_tools).await?;
        let tool_calls = message
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        // No tool calls → a final answer.
        if tool_calls.is_empty() {
            let reply = message_text(&message)?;
            return Ok(with_trace(reply, &trace));
        }

        // Record the assistant turn verbatim (content may be null), then execute
        // each requested call and append a `tool` result message.
        messages.push(message.clone());
        for call in &tool_calls {
            let call_id = call.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let function = call.get("function");
            let raw_name = function
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args: serde_json::Value = function
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .filter(|s| !s.trim().is_empty())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| serde_json::json!({}));

            let (server, tool) = mcp_client::split_tool_name(raw_name, &default_server);
            let result = match clients.get_mut(server) {
                Some(client) => client
                    .call_tool(tool, args.clone())
                    .await
                    .unwrap_or_else(|e| mcp_client::McpCallResult {
                        text: format!("[tool error] {e}"),
                        is_error: true,
                    }),
                // The model named a server outside the approved set (it cannot,
                // since we only offered approved tools — but defend anyway): the
                // un-granted server is NOT reached.
                None => mcp_client::McpCallResult {
                    text: format!("[tool error] tool {raw_name:?} is not in the approved set"),
                    is_error: true,
                },
            };
            trace.push(format!(
                "{}{tool}{} → {}",
                if server.is_empty() {
                    String::new()
                } else {
                    format!("{server}.")
                },
                if result.is_error { " [error]" } else { "" },
                truncate(&result.text, 200)
            ));
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call_id,
                "name": raw_name,
                "content": result.text,
            }));
        }
    }

    // Hit the iteration cap without a final answer.
    Ok(with_trace(
        "(stopped after the maximum number of tool steps without a final answer)".to_string(),
        &trace,
    ))
}

/// In tests the loop is exercised only at the pure-helper level (no live MCP
/// server / no `http-fetch`); the live loop is `#[cfg(not(test))]` so the test
/// build stays I/O-free. `run_definition` with an empty `servers` (the common
/// path) does not call this.
#[cfg(test)]
async fn run_tool_loop(
    _def: &agents::AgentDef,
    _messages: Vec<serde_json::Value>,
    _servers: &[String],
) -> Result<String, String> {
    Err("tool-loop is not exercised in unit tests (needs a live MCP server)".to_string())
}

/// Append a compact tool-call trace to the answer (one line per call), or the
/// answer unchanged when no tools were called.
#[cfg(not(test))]
fn with_trace(reply: String, trace: &[String]) -> String {
    if trace.is_empty() {
        return reply;
    }
    let lines = trace
        .iter()
        .map(|t| format!("- {t}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{reply}\n\n_Tools used:_\n{lines}")
}

/// Truncate a string to `max` chars with an ellipsis (for the compact trace).
#[cfg(not(test))]
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}…")
}

/// The seeded welcome note — a deterministic genesis so a fresh vault is not
/// empty (and proves rendering end to end). `Default` stays deterministic.
fn welcome_body() -> String {
    "# Welcome to Tangram\n\nThis is the **tangram** shell — an Obsidian-style \
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

    /// epoch-ms for a wall-clock instant in a named zone (next-fire test helper,
    /// mirrors the one in the `agents` test module).
    fn ms_at(tz_name: &str, y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        use chrono::TimeZone;
        let tz: chrono_tz::Tz = tz_name.parse().unwrap();
        tz.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn empty() -> Vault {
        Vault {
            files: Vec::new(),
            agent_runs: Some(Vec::new()),
            invocations: Some(Vec::new()),
            mcp_grants: Some(Vec::new()),
            executions: Some(Vec::new()),
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

    // ── scheduled invocations (the redesign): the replicated index ────────────

    /// A def named `name` plus a note carrying its inline `agent://<id>` link,
    /// and the matching index entry created via `create_invocation`. Returns the
    /// host file id.
    fn vault_with_invocation(name: &str, id: &str, trigger: &str) -> Vault {
        let mut v = empty();
        v.create_file(
            format!("agents/{name}.md"),
            format!("---\nkind: skill\nname: {name}\n---\nWrite a status."),
        )
        .unwrap();
        let host = v
            .create_file(
                "daily.md".into(),
                format!("# Daily\n\nRun: [⚡ {name}](agent://{id}) every day.\n"),
            )
            .unwrap();
        v.create_invocation(
            id.into(),
            name.into(),
            trigger.into(),
            "Summarize today.".into(),
            host.clone(),
        )
        .unwrap();
        v
    }

    /// A def for the append path tests.
    fn standup_def() -> agents::AgentDef {
        agents::AgentDef {
            kind: "skill".into(),
            name: "standup".into(),
            model: "deepseek-chat".into(),
            instructions: "Write a status.".into(),
            mcp_servers: Vec::new(),
        }
    }

    #[test]
    fn create_invocation_indexes_by_id_and_lists() {
        let v = vault_with_invocation("standup", "uuid-1", "daily at 09:00 UTC");
        let invs = v.list_invocations();
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].id, "uuid-1");
        assert_eq!(invs[0].agent, "standup");
        assert_eq!(invs[0].trigger, "daily at 09:00 UTC");
        assert_eq!(invs[0].status, STATUS_SCHEDULED);
        assert_eq!(invs[0].last_run_ms, None);
    }

    #[test]
    fn create_invocation_rejects_empty_id() {
        let mut v = empty();
        assert!(
            v.create_invocation("".into(), "a".into(), "2h".into(), "p".into(), "f".into())
                .is_err()
        );
    }

    #[test]
    fn update_invocation_edits_trigger_and_prompt() {
        let mut v = vault_with_invocation("standup", "uuid-1", "2h");
        v.update_invocation(
            "uuid-1".into(),
            "daily at 08:00 UTC".into(),
            "New prompt.".into(),
        )
        .unwrap();
        let inv = &v.list_invocations()[0];
        assert_eq!(inv.trigger, "daily at 08:00 UTC");
        assert_eq!(inv.prompt, "New prompt.");
        assert_eq!(inv.status, STATUS_SCHEDULED);
        // Updating an absent id errors.
        assert!(
            v.update_invocation("nope".into(), "2h".into(), "p".into())
                .is_err()
        );
    }

    #[test]
    fn delete_invocation_removes_entry() {
        let mut v = vault_with_invocation("standup", "uuid-1", "2h");
        v.delete_invocation("uuid-1".into()).unwrap();
        assert!(v.list_invocations().is_empty());
        assert!(v.delete_invocation("uuid-1".into()).is_err());
    }

    #[test]
    fn due_scan_reads_from_index_keyed_by_id() {
        // An interval invocation never run is due; recording a run by id stops it
        // until the interval elapses (the index `last_run_ms` is the due source).
        let v = vault_with_invocation("standup", "uuid-1", "1m");
        let inv = &v.list_invocations()[0];
        assert!(agents::trigger_is_due(
            &inv.trigger,
            inv.last_run_ms,
            1_000_000
        ));
        // After a run, last_run_ms on the index entry suppresses an immediate re-fire.
        let mut v2 = v;
        let inv = v2.list_invocations()[0].clone();
        v2.append_invocation_output(&inv, &standup_def(), "ok", 1_000_000, STATUS_RAN);
        assert_eq!(v2.last_run_ms("uuid-1"), Some(1_000_000));
        let after = &v2.list_invocations()[0];
        assert!(!agents::trigger_is_due(
            &after.trigger,
            after.last_run_ms,
            1_000_000
        ));
        assert!(agents::trigger_is_due(
            &after.trigger,
            after.last_run_ms,
            1_000_000 + 60_000
        ));
    }

    /// The stored next-fire for the invocation `id`, if any (mirrors the
    /// `last_run_ms` test helper).
    fn next_fire_ms_of(v: &Vault, id: &str) -> Option<i64> {
        v.list_invocations()
            .into_iter()
            .find(|i| i.id == id)
            .and_then(|i| i.next_fire_ms)
    }

    /// The "due now" set the tick selects: entries whose stored next-fire is
    /// `<= now` (the exact filter in `tick_agents`). A pure stand-in so the
    /// next-fire selection is unit-testable without the async egress path.
    fn due_now_ids(v: &Vault, now: i64) -> Vec<String> {
        v.list_invocations()
            .into_iter()
            .filter(|inv| inv.next_fire_ms.is_some_and(|nf| nf <= now))
            .map(|inv| inv.id)
            .collect()
    }

    #[test]
    fn next_fire_is_computed_on_create() {
        // A scheduled interval invocation gets a next-fire on create. Never run
        // ⇒ it is `<= now` (immediately due), matching the old never-run rule.
        let v = vault_with_invocation("standup", "uuid-1", "1m");
        let nf = next_fire_ms_of(&v, "uuid-1").expect("scheduled ⇒ Some");
        assert!(nf <= now_ms() + 5_000, "fresh interval fires immediately");
        // A one-time invocation is never scheduled ⇒ no next-fire stored.
        let v2 = vault_with_invocation("standup2", "uuid-2", "one-time");
        assert_eq!(next_fire_ms_of(&v2, "uuid-2"), None);
    }

    #[test]
    fn next_fire_is_recomputed_on_update() {
        // Updating the trigger recomputes the stored next-fire from the new
        // schedule (and resets the run).
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        v.update_invocation("uuid-1".into(), "daily at 09:00 UTC".into(), "p".into())
            .unwrap();
        let nf = next_fire_ms_of(&v, "uuid-1").expect("daily ⇒ Some");
        // Must equal what the grammar computes for a never-run daily schedule.
        let expected = agents::next_fire_ms("daily at 09:00 UTC", None, now_ms());
        assert_eq!(Some(nf), expected);
        // Updating to a one-time trigger clears the next-fire.
        v.update_invocation("uuid-1".into(), "one-time".into(), "p".into())
            .unwrap();
        assert_eq!(next_fire_ms_of(&v, "uuid-1"), None);
    }

    #[test]
    fn tick_selects_only_due_entries_by_next_fire() {
        // Two interval invocations: one already run (next-fire in the future),
        // one never run (next-fire now). Only the never-run one is "due now".
        let mut v = vault_with_invocation("a", "uuid-a", "1h");
        v.create_file(
            "agents/b.md".into(),
            "---\nkind: skill\nname: b\n---\nDo b.".into(),
        )
        .unwrap();
        let hb = v
            .create_file("b.md".into(), "[⚡ b](agent://uuid-b) hi".into())
            .unwrap();
        v.create_invocation("uuid-b".into(), "b".into(), "1h".into(), "p".into(), hb)
            .unwrap();
        // Record a run for `a` at T so its next-fire is T + 1h (future).
        let t = 2_000_000_000_000;
        let inv_a = v
            .list_invocations()
            .into_iter()
            .find(|i| i.id == "uuid-a")
            .unwrap();
        v.append_invocation_output(&inv_a, &standup_def(), "ok", t, STATUS_RAN);
        // At T, only `b` (never run, next-fire ≤ T) is due; `a` fires at T+1h.
        let due = due_now_ids(&v, t);
        assert_eq!(due, vec!["uuid-b".to_string()]);
        // One hour later both are due.
        let later = t + 60 * 60 * 1000;
        let mut both = due_now_ids(&v, later);
        both.sort();
        assert_eq!(both, vec!["uuid-a".to_string(), "uuid-b".to_string()]);
    }

    #[test]
    fn next_fire_advances_after_a_run() {
        // After a run the stored next-fire moves to last_run + interval, so the
        // invocation is no longer due until the interval elapses.
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        let inv = v.list_invocations()[0].clone();
        let t = 1_000_000;
        v.append_invocation_output(&inv, &standup_def(), "ok", t, STATUS_RAN);
        assert_eq!(next_fire_ms_of(&v, "uuid-1"), Some(t + 60_000));
        // Not due right after the run; due once the interval elapses.
        assert!(due_now_ids(&v, t).is_empty());
        assert_eq!(due_now_ids(&v, t + 60_000), vec!["uuid-1".to_string()]);
    }

    #[test]
    fn backfill_sets_next_fire_for_none_entries() {
        // Simulate an older document: an index entry hydrated with no next-fire.
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        // Force the field back to None (as a pre-field document would hydrate).
        v.invocations.as_mut().unwrap()[0].next_fire_ms = None;
        assert_eq!(next_fire_ms_of(&v, "uuid-1"), None);
        // The tick's backfill fills it from the trigger + last run.
        let now = 1_000_000;
        v.backfill_next_fire(now);
        // Never run ⇒ next-fire is `now` (immediately due), so the entry selects.
        assert_eq!(next_fire_ms_of(&v, "uuid-1"), Some(now));
        assert_eq!(due_now_ids(&v, now), vec!["uuid-1".to_string()]);
        // A one-time entry stays None through backfill (never scheduled).
        v.invocations.as_mut().unwrap()[0].trigger = "one-time".into();
        v.invocations.as_mut().unwrap()[0].next_fire_ms = None;
        v.backfill_next_fire(now);
        assert_eq!(next_fire_ms_of(&v, "uuid-1"), None);
    }

    #[test]
    fn next_fire_matches_due_for_daily_and_weekly_invocations() {
        // The stored next-fire must reproduce trigger_is_due for calendar
        // schedules at create time (never run): selecting by next-fire ≤ now
        // equals the old due check.
        // Use fixed clock instants (not now_ms()) so `selected` and `due` are
        // evaluated against the exact same `now` — the equivalence is per-`now`.
        let cases: &[(&str, i64)] = &[
            // Daily 09:00 ET, now = 10:00 ET (past today's slot), never run ⇒ due.
            (
                "daily at 09:00 America/New_York",
                ms_at("America/New_York", 2026, 3, 10, 10, 0),
            ),
            // Daily 09:00 ET, now = 08:00 ET (before today's slot), never run ⇒
            // the most-recent occurrence is yesterday's, ≤ now ⇒ due.
            (
                "daily at 09:00 America/New_York",
                ms_at("America/New_York", 2026, 3, 10, 8, 0),
            ),
            // Weekly Mon/Wed/Fri 14:00 ET, now = Mon 15:00 ⇒ due (slot passed).
            (
                "weekly on mon,wed,fri at 14:00 America/New_York",
                ms_at("America/New_York", 2026, 6, 15, 15, 0),
            ),
        ];
        for (trigger, now) in cases {
            // Reproduce the create-time computation at this exact `now`.
            let nf = agents::next_fire_ms(trigger, None, *now);
            let selected = nf.is_some_and(|t| t <= *now);
            let due = agents::trigger_is_due(trigger, None, *now);
            assert_eq!(
                selected, due,
                "next-fire selection must match due for {trigger:?}"
            );
        }
    }

    #[test]
    fn output_renders_a_callout_card_with_block_ids_and_records_last_run_by_id() {
        // embedded-runs R3: a run renders a `> [!run]+` CALLOUT below the host
        // paragraph (not the legacy indented blockquote), stamps the host
        // paragraph's `^run-<id>` block id, and the callout carries `^runout-<id>`.
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        let inv = v.list_invocations()[0].clone();
        v.append_invocation_output(&inv, &standup_def(), "all good", 1_234, STATUS_RAN);
        let host = v
            .files
            .iter()
            .find(|f| f.path == "daily.md")
            .unwrap()
            .clone();
        // The callout card is present with the agent + output.
        assert!(host.body.contains("> [!run]+ ✓ /standup"));
        assert!(host.body.contains("> all good"));
        // The bidirectional block ids: host paragraph anchor + callout block id.
        assert!(host.body.contains("^run-uuid-1"), "host block id stamped");
        assert!(host.body.contains("> ^runout-uuid-1"), "callout block id");
        // The header backlinks to the chip's host block.
        assert!(host.body.contains("[↑](#^run-uuid-1)"));
        // The tail text after the link is preserved.
        assert!(host.body.contains("every day."));
        // Run recorded by id + status updated.
        assert_eq!(v.last_run_ms("uuid-1"), Some(1_234));
        assert_eq!(v.list_invocations()[0].status, STATUS_RAN);
    }

    #[test]
    fn rerun_refreshes_the_callout_in_place_not_a_second_card() {
        // A re-run REPLACES the Run's existing callout (one card per Run, always
        // the latest output) rather than appending a second.
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        let inv = v.list_invocations()[0].clone();
        v.append_invocation_output(&inv, &standup_def(), "first", 1_000, STATUS_RAN);
        v.append_invocation_output(&inv, &standup_def(), "second", 2_000, STATUS_RAN);
        let host = v.files.iter().find(|f| f.path == "daily.md").unwrap();
        // Exactly one callout header and one callout block id remain.
        assert_eq!(host.body.matches("> [!run]+").count(), 1);
        assert_eq!(host.body.matches("^runout-uuid-1").count(), 1);
        // The card shows the latest output, not the first.
        assert!(host.body.contains("> second"));
        assert!(!host.body.contains("> first"));
        // The host block id is stamped exactly once (idempotent).
        assert_eq!(host.body.matches("^run-uuid-1").count(), 2); // anchor + the `[↑]` ref
    }

    #[test]
    fn each_run_appends_an_execution_with_config_hash_and_block_id() {
        // embedded-runs R3 executions log: one Execution per run, carrying the
        // resolved-config hash + the callout's output block id.
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        let inv = v.list_invocations()[0].clone();
        assert!(v.list_executions().is_empty());
        v.append_invocation_output(&inv, &standup_def(), "ok", 5_000, STATUS_RAN);
        let log = v.list_executions();
        assert_eq!(log.len(), 1);
        let e = &log[0];
        assert_eq!(e.run_id, "uuid-1");
        assert_eq!(e.agent, "standup");
        assert_eq!(e.ts, 5_000);
        assert_eq!(e.status, STATUS_RAN);
        assert_eq!(e.output_block_id, "runout-uuid-1");
        // The config hash is a 64-hex sha256 and stable for the same config.
        assert_eq!(e.config_hash.len(), 64);
        assert_eq!(
            e.config_hash,
            agents::config_hash(&standup_def(), &inv.prompt, &inv.trigger)
        );
        // A second run appends a second Execution (append-only log).
        v.append_invocation_output(&inv, &standup_def(), "ok2", 6_000, STATUS_RAN);
        assert_eq!(v.list_executions().len(), 2);
    }

    #[test]
    fn once_run_fires_once_then_never_again() {
        // embedded-runs R3 one-time unification: a `once` Run is an index entry
        // (chip + record) that fires exactly once. Never run ⇒ due now; once run
        // ⇒ next_fire is None (the scheduler never re-selects it).
        let v = vault_with_invocation("standup", "uuid-1", "once");
        // Created with the `once` trigger and immediately due.
        assert_eq!(v.list_invocations()[0].trigger, "once");
        let nf = next_fire_ms_of(&v, "uuid-1").expect("once, never run ⇒ due now");
        assert!(nf <= now_ms() + 5_000);
        // After the (single) run, it never fires again.
        let mut v2 = v;
        let inv = v2.list_invocations()[0].clone();
        v2.append_invocation_output(&inv, &standup_def(), "done", 9_000, STATUS_RAN);
        assert_eq!(next_fire_ms_of(&v2, "uuid-1"), None, "once never re-fires");
        assert!(due_now_ids(&v2, 9_000 + 1_000_000).is_empty());
    }

    #[test]
    fn mark_running_flips_status_then_append_clears_it() {
        // The embedded-runs R1 running transition: `mark_invocation_running`
        // flips the Run's visible status to `running` (so the replicated chip
        // shows an in-flight execution) WITHOUT touching last-run/next-fire; the
        // append commit then clears it to `ran`.
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        let inv = v.list_invocations()[0].clone();
        assert_eq!(inv.status, STATUS_SCHEDULED);

        v.mark_invocation_running("uuid-1");
        let running = v.list_invocations()[0].clone();
        assert_eq!(running.status, STATUS_RUNNING);
        // Only the status changed — last-run is still unset.
        assert_eq!(running.last_run_ms, None);

        v.append_invocation_output(&inv, &standup_def(), "done", 9_000, STATUS_RAN);
        let done = v.list_invocations()[0].clone();
        assert_eq!(done.status, STATUS_RAN);
        assert_eq!(done.last_run_ms, Some(9_000));

        // Marking a missing id is a harmless no-op (the link was deleted).
        v.mark_invocation_running("does-not-exist");
        assert_eq!(v.list_invocations().len(), 1);
    }

    #[test]
    fn reconcile_prunes_orphan_invocations() {
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        assert_eq!(v.list_invocations().len(), 1);
        // Remove the inline link from the note → the index entry is an orphan.
        let host = v
            .files
            .iter()
            .find(|f| f.path == "daily.md")
            .unwrap()
            .clone();
        v.write_file(host.id, "# Daily\n\nno link anymore.\n".into())
            .unwrap();
        assert_eq!(v.reconcile_invocations(), 1);
        assert!(v.list_invocations().is_empty());
        // A second reconcile prunes nothing.
        assert_eq!(v.reconcile_invocations(), 0);
    }

    #[test]
    fn reconcile_keeps_invocations_with_a_live_link() {
        let mut v = vault_with_invocation("standup", "uuid-1", "1m");
        assert_eq!(v.reconcile_invocations(), 0);
        assert_eq!(v.list_invocations().len(), 1);
    }

    // ── Tools/MCP T1: the grant model lifecycle ──────────────────────────────

    /// Seed a vault with one `kind: agent` def requesting `servers`.
    fn vault_with_agent(name: &str, servers: &[&str]) -> Vault {
        let mut v = empty();
        let list = servers.join(", ");
        let body = format!("---\nkind: agent\nname: {name}\nmcp_servers: [{list}]\n---\nDo it.");
        v.create_file(format!("agents/{name}.md"), body).unwrap();
        v
    }

    #[test]
    fn request_starts_pending_then_approves_binding_to_hash() {
        let mut v = vault_with_agent("planner", &["nutrition", "notes"]);
        // A declared-but-undecided request is pending.
        let st = v.mcp_status();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].agent, "planner");
        assert_eq!(st[0].status, "pending");
        assert_eq!(st[0].requested, vec!["notes", "nutrition"]); // canonicalized
        assert!(st[0].approved.is_empty());

        // Approve with the current hash → approved, approved == requested.
        let hash = st[0].requested_hash.clone();
        v.approve_mcp("planner".into(), hash).unwrap();
        let st = v.mcp_status();
        assert_eq!(st[0].status, "approved");
        assert_eq!(st[0].approved, vec!["notes", "nutrition"]);
    }

    #[test]
    fn approve_rejects_a_stale_hash() {
        let mut v = vault_with_agent("planner", &["nutrition"]);
        // A hash the user never saw (some other request) must be refused.
        let wrong = agents::mcp_request_hash(&["something-else".into()]);
        let err = v.approve_mcp("planner".into(), wrong).unwrap_err();
        assert!(err.contains("stale request"), "got: {err}");
        // Nothing was recorded — still pending.
        assert_eq!(v.mcp_status()[0].status, "pending");
    }

    #[test]
    fn editing_the_request_invalidates_an_approval_to_stale() {
        let mut v = vault_with_agent("planner", &["nutrition"]);
        let hash = v.mcp_status()[0].requested_hash.clone();
        v.approve_mcp("planner".into(), hash).unwrap();
        assert_eq!(v.mcp_status()[0].status, "approved");

        // Edit the def to request MORE servers → the recorded grant's hash no
        // longer matches the live request → stale (re-approval required).
        let file = v
            .files
            .iter()
            .find(|f| f.path == "agents/planner.md")
            .unwrap()
            .clone();
        let edited = file.body.replace(
            "mcp_servers: [nutrition]",
            "mcp_servers: [nutrition, notes]",
        );
        v.write_file(file.id, edited).unwrap();
        let st = v.mcp_status();
        assert_eq!(st[0].status, "stale");
        assert!(
            st[0].approved.is_empty(),
            "stale clears the effective grant"
        );

        // Approving the NEW hash re-grants.
        let new_hash = st[0].requested_hash.clone();
        v.approve_mcp("planner".into(), new_hash).unwrap();
        let st = v.mcp_status();
        assert_eq!(st[0].status, "approved");
        assert_eq!(st[0].approved, vec!["notes", "nutrition"]);
    }

    #[test]
    fn deny_then_reapprove_and_revoke() {
        let mut v = vault_with_agent("planner", &["nutrition"]);
        v.deny_mcp("planner".into()).unwrap();
        assert_eq!(v.mcp_status()[0].status, "denied");

        // Reconsider: approve the (unchanged) request.
        let hash = v.mcp_status()[0].requested_hash.clone();
        v.approve_mcp("planner".into(), hash).unwrap();
        assert_eq!(v.mcp_status()[0].status, "approved");

        // Revoke → back to denied with no approved servers.
        v.revoke_mcp("planner".into()).unwrap();
        let st = v.mcp_status();
        assert_eq!(st[0].status, "denied");
        assert!(st[0].approved.is_empty());
    }

    // ── Tools/MCP T2: the per-agent approved subset offered to the tool-loop ──

    /// Resolve the def for the named agent in a vault (test helper).
    fn def_of(v: &Vault, name: &str) -> agents::AgentDef {
        v.files
            .iter()
            .filter_map(|f| agents::parse_agent(&f.body))
            .find(|d| d.name == name)
            .expect("agent def present")
    }

    #[test]
    fn approved_servers_offered_only_after_a_fresh_grant() {
        let mut v = vault_with_agent("planner", &["nutrition", "notes"]);
        let def = def_of(&v, "planner");
        // Pending (no decision) → NOTHING is offered.
        assert!(
            v.approved_servers_for(&def).is_empty(),
            "no grant ⇒ no servers offered"
        );

        // Approve → exactly the approved (canonical) set is offered.
        let hash = v.mcp_status()[0].requested_hash.clone();
        v.approve_mcp("planner".into(), hash).unwrap();
        assert_eq!(
            v.approved_servers_for(&def),
            vec!["notes", "nutrition"],
            "approved ⇒ the granted set is the tool plane"
        );
    }

    #[test]
    fn denied_grant_offers_no_servers() {
        let mut v = vault_with_agent("planner", &["nutrition"]);
        v.deny_mcp("planner".into()).unwrap();
        let def = def_of(&v, "planner");
        assert!(
            v.approved_servers_for(&def).is_empty(),
            "denied ⇒ un-granted ⇒ unreachable (no server offered)"
        );
    }

    #[test]
    fn an_ungranted_server_is_never_offered_even_after_an_edit() {
        // Approve {nutrition}; then edit the def to ALSO request {notes}. The
        // grant is now STALE (hash changed) → the whole tool plane closes until
        // re-approval. Critically, `notes` (never approved) is never offered.
        let mut v = vault_with_agent("planner", &["nutrition"]);
        let hash = v.mcp_status()[0].requested_hash.clone();
        v.approve_mcp("planner".into(), hash).unwrap();

        let file = v
            .files
            .iter()
            .find(|f| f.path == "agents/planner.md")
            .unwrap()
            .clone();
        let edited = file.body.replace(
            "mcp_servers: [nutrition]",
            "mcp_servers: [nutrition, notes]",
        );
        v.write_file(file.id, edited).unwrap();

        let def = def_of(&v, "planner");
        assert!(
            v.approved_servers_for(&def).is_empty(),
            "a stale grant closes the tool plane — notes was never granted, nutrition is now stale"
        );

        // Re-approve the NEW request → both are offered.
        let new_hash = v.mcp_status()[0].requested_hash.clone();
        v.approve_mcp("planner".into(), new_hash).unwrap();
        assert_eq!(
            v.approved_servers_for(&def_of(&v, "planner")),
            vec!["notes", "nutrition"]
        );
    }

    #[test]
    fn approved_subset_is_what_the_loop_offers_to_the_model() {
        // The loop offers ONLY the approved servers' tools. Simulate the
        // conversion the loop does and assert an un-granted server's tools never
        // appear in the function-tool list handed to the model.
        let mut v = vault_with_agent("planner", &["nutrition"]);
        let hash = v.mcp_status()[0].requested_hash.clone();
        v.approve_mcp("planner".into(), hash).unwrap();
        let servers = v.approved_servers_for(&def_of(&v, "planner"));
        assert_eq!(servers, vec!["nutrition"]);

        // Pretend each approved server listed one tool; build the offer.
        let mut offered: Vec<serde_json::Value> = Vec::new();
        let namespaced = servers.len() > 1;
        for server in &servers {
            let tools = vec![mcp_client::McpTool {
                name: format!("{server}_tool"),
                description: None,
                input_schema: None,
            }];
            offered.extend(mcp_client::tools_to_openai(server, &tools, namespaced));
        }
        let names: Vec<&str> = offered
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert_eq!(names, vec!["nutrition_tool"]);
        // The un-granted `notes` server contributes NOTHING.
        assert!(!names.iter().any(|n| n.contains("notes")));
    }

    #[test]
    fn skill_requests_are_not_surfaced_and_actions_refuse() {
        let mut v = empty();
        v.create_file(
            "agents/sum.md".into(),
            "---\nkind: skill\nname: sum\nmcp_servers: [nutrition]\n---\nb".into(),
        )
        .unwrap();
        // A skill never appears in the MCP status (parse-and-ignore).
        assert!(v.mcp_status().is_empty());
        // And the decision actions refuse (no such *agent* requesting servers).
        assert!(v.approve_mcp("sum".into(), "x".into()).is_err());
        assert!(v.deny_mcp("sum".into()).is_err());
    }

    #[test]
    fn agent_requesting_nothing_is_omitted_and_actions_refuse() {
        let mut v = empty();
        v.create_file(
            "agents/plain.md".into(),
            "---\nkind: agent\nname: plain\n---\nbody".into(),
        )
        .unwrap();
        assert!(v.mcp_status().is_empty());
        assert!(v.approve_mcp("plain".into(), "x".into()).is_err());
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
