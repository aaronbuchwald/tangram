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
            mcp_grants: Some(Vec::new()),
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
            .filter(|inv| inv.is_scheduled())
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

    fn empty() -> Vault {
        Vault {
            files: Vec::new(),
            agent_runs: Some(Vec::new()),
            mcp_grants: Some(Vec::new()),
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
            mcp_servers: Vec::new(),
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
