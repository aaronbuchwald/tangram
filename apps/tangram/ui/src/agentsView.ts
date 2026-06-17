// The Agents view (P2): two sub-tabs — "Agents" and "Runs" — behind a
// segmented control at the top of the view (default: Agents). The Agents tab is
// a GitHub-issues-style sortable, filterable table of every agent/skill indexed
// across the vault (the P1 frontmatter index in agents.ts); the Runs tab is
// the scheduled-Run table (user-facing "Runs" over the replicated `invocations`
// index — the data model/identifiers are unchanged, this is a UI relabel; the
// two-layer Agent/Run/Execution model is docs/design/embedded-runs.md, R1).
// Shell-UI-only — rendered into the main content area as an
// "agents" tab. Reuses the P1 index (no new backend), the run popup
// (agentPopup.ts), and the vault write API to edit a row's frontmatter.
//
// What it does:
//  - columns: Name, Kind, Model, Labels, Version, Path
//  - click a header to sort asc/desc (toggle), active column indicated
//  - a query bar that AND-combines tokens:
//      kind:agent | kind:skill   → row.kind
//      label:<x>                 → row has that label
//      <key>=<value>             → row.meta[key] === value (string-compared)
//      bare word                 → case-insensitive substring on the name
//    with a live result count
//  - row actions: clicking the Name opens that agent's vault file in a tab;
//    a Run button opens the bound run popup
//  - per-row add-label + add-meta affordances that rewrite the source file's
//    leading `---…---` frontmatter (preserving the body + other fields), so the
//    new label / key-value becomes immediately filterable/sortable.
//
// Scope guard (P2): table + sort + filter + open/run + add-label/add-meta only.
// No triggers/tools/sandbox, no versioning UI (just shows `version`), no
// provider routing.

import { vault, type Invocation, type McpGrant, type MdFile } from "./api";
import { type AgentDef, type AgentIndex, mcpRequestHash } from "./agents";
import { openAgentPopup } from "./agentPopup";
import {
  formatRelativeTime,
  formatSchedule,
  type InvocationIndex,
  nextFireMs,
} from "./invocations";
import { showError } from "./modal";

// ── sortable columns ─────────────────────────────────────────────────────────

type SortKey = "name" | "kind" | "model" | "labels" | "version" | "path";

interface ColumnDef {
  key: SortKey;
  label: string;
  /** The string used for sorting this column on a given def. */
  sortValue: (def: AgentDef) => string;
}

const COLUMNS: ColumnDef[] = [
  { key: "name", label: "Name", sortValue: (d) => d.name.toLowerCase() },
  { key: "kind", label: "Kind", sortValue: (d) => d.kind },
  { key: "model", label: "Model", sortValue: (d) => d.model.toLowerCase() },
  {
    key: "labels",
    label: "Labels",
    sortValue: (d) => d.labels.join(",").toLowerCase(),
  },
  { key: "version", label: "Version", sortValue: (d) => d.version ?? "" },
  { key: "path", label: "Path", sortValue: (d) => d.path.toLowerCase() },
];

// ── query parsing ────────────────────────────────────────────────────────────

interface Query {
  kinds: Array<"agent" | "skill">;
  labels: string[];
  meta: Array<{ key: string; value: string }>;
  terms: string[];
}

/** Parse the query bar into AND-combined predicates. Whitespace-separated
 *  tokens; `kind:` / `label:` / `key=value` are recognized, the rest are bare
 *  substring terms matched against the name. */
function parseQuery(raw: string): Query {
  const q: Query = { kinds: [], labels: [], meta: [], terms: [] };
  for (const tok of raw.split(/\s+/)) {
    if (tok.length === 0) continue;
    const lower = tok.toLowerCase();
    if (lower.startsWith("kind:")) {
      const v = lower.slice("kind:".length);
      if (v === "agent" || v === "skill") q.kinds.push(v);
      continue;
    }
    if (lower.startsWith("label:")) {
      const v = tok.slice("label:".length);
      if (v.length > 0) q.labels.push(v.toLowerCase());
      continue;
    }
    // `key=value` (the `=` must not be the first char so a bare `=foo` is a term)
    const eq = tok.indexOf("=");
    if (eq > 0) {
      q.meta.push({ key: tok.slice(0, eq), value: tok.slice(eq + 1) });
      continue;
    }
    q.terms.push(lower);
  }
  return q;
}

/** Does `def` satisfy every clause of `q` (AND semantics)? */
function matches(def: AgentDef, q: Query): boolean {
  if (q.kinds.length > 0 && !q.kinds.includes(def.kind)) return false;
  const lowerLabels = def.labels.map((l) => l.toLowerCase());
  for (const want of q.labels) {
    if (!lowerLabels.includes(want)) return false;
  }
  for (const { key, value } of q.meta) {
    const have = def.meta[key];
    if (have === undefined || String(have) !== value) return false;
  }
  const name = def.name.toLowerCase();
  for (const term of q.terms) {
    if (!name.includes(term)) return false;
  }
  return true;
}

// ── frontmatter rewriting (add label / add meta) ─────────────────────────────

/** Split a file body into [frontmatterLines, bodyAfterClose] or null if there
 *  is no leading `---…---` block. Keeps the inner lines verbatim so unrelated
 *  fields survive a rewrite untouched. */
function splitFrontmatter(
  body: string,
): { inner: string[]; rest: string } | null {
  if (!body.startsWith("---")) return null;
  const lines = body.split("\n");
  if (lines[0].trim() !== "---") return null;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i].trim() === "---") {
      return { inner: lines.slice(1, i), rest: lines.slice(i + 1).join("\n") };
    }
  }
  return null;
}

/** Re-emit a `---\n…\n---\n<rest>` body from frontmatter inner lines. */
function joinFrontmatter(inner: string[], rest: string): string {
  return ["---", ...inner, "---", rest].join("\n");
}

/** Parse an inline `[a, b, c]` array line value into its elements (unquoted),
 *  or null if it isn't an inline array. */
function parseInlineArray(value: string): string[] | null {
  const s = value.trim();
  if (!s.startsWith("[") || !s.endsWith("]")) return null;
  const inner = s.slice(1, -1).trim();
  if (inner.length === 0) return [];
  return inner
    .split(",")
    .map((p) => unquote(p.trim()))
    .filter((p) => p.length > 0);
}

function unquote(s: string): string {
  if (
    s.length >= 2 &&
    ((s[0] === '"' && s[s.length - 1] === '"') ||
      (s[0] === "'" && s[s.length - 1] === "'"))
  ) {
    return s.slice(1, -1);
  }
  return s;
}

/** Quote a token for an inline YAML array/map only when it needs it. */
function quoteIfNeeded(s: string): string {
  return /^[A-Za-z0-9_.\-/]+$/.test(s) ? s : JSON.stringify(s);
}

/** Render a labels list as an inline `[a, b]` array value. */
function renderArray(items: string[]): string {
  return `[${items.map(quoteIfNeeded).join(", ")}]`;
}

/** Add `label` to the file's `labels:` frontmatter (creating the field if
 *  missing). No-op (returns null) if the label is already present. Preserves the
 *  body + every other frontmatter field. */
function addLabelToBody(body: string, label: string): string | null {
  const split = splitFrontmatter(body);
  if (!split) return null;
  const { inner, rest } = split;
  const idx = inner.findIndex((l) => /^labels\s*:/.test(l));
  if (idx === -1) {
    inner.push(`labels: ${renderArray([label])}`);
    return joinFrontmatter(inner, rest);
  }
  const value = inner[idx].slice(inner[idx].indexOf(":") + 1);
  const current = parseInlineArray(value) ?? [];
  if (current.some((l) => l.toLowerCase() === label.toLowerCase())) return null;
  current.push(label);
  inner[idx] = `labels: ${renderArray(current)}`;
  return joinFrontmatter(inner, rest);
}

/** Set `key: value` inside the file's inline `meta: {…}` map (creating the
 *  field if missing). Overwrites an existing key. Preserves the body + other
 *  fields. */
function addMetaToBody(body: string, key: string, value: string): string | null {
  const split = splitFrontmatter(body);
  if (!split) return null;
  const { inner, rest } = split;
  const pair = `${quoteIfNeeded(key)}: ${quoteIfNeeded(value)}`;
  const idx = inner.findIndex((l) => /^meta\s*:/.test(l));
  if (idx === -1) {
    inner.push(`meta: {${pair}}`);
    return joinFrontmatter(inner, rest);
  }
  // Parse the existing inline map, set/overwrite the key, re-emit.
  const raw = inner[idx].slice(inner[idx].indexOf(":") + 1).trim();
  const map = new Map<string, string>();
  if (raw.startsWith("{") && raw.endsWith("}")) {
    const innerMap = raw.slice(1, -1).trim();
    if (innerMap.length > 0) {
      for (const part of innerMap.split(",")) {
        const c = part.indexOf(":");
        if (c === -1) continue;
        const k = unquote(part.slice(0, c).trim());
        if (k.length === 0) continue;
        map.set(k, unquote(part.slice(c + 1).trim()));
      }
    }
  }
  map.set(key, value);
  const rendered = [...map.entries()]
    .map(([k, v]) => `${quoteIfNeeded(k)}: ${quoteIfNeeded(v)}`)
    .join(", ");
  inner[idx] = `meta: {${rendered}}`;
  return joinFrontmatter(inner, rest);
}

// ── view options ─────────────────────────────────────────────────────────────

export interface AgentsViewCallbacks {
  /** Open the given vault file id in a note tab (wired to tabs.openNote). */
  openNote: (fileId: string) => void;
  /** Look up the live MdFile for a def's fileId (so add-label/add-meta can read
   *  the current body before rewriting it). */
  fileById: (fileId: string) => MdFile | undefined;
  /** Open the create-agent flow (the same path `/agent` uses). Wired by main.ts
   *  to the shared create popup so the Agents view is a discoverable on-ramp
   *  (#9/#10) — not just a passive table of what already exists. */
  newAgent: () => void;
  /** Tools/MCP T1: the live grant records from the vault state frame (keyed by
   *  agent name). Used to derive each agent's effective MCP status. */
  mcpGrants: () => McpGrant[];
  /** Tools/MCP T1: the live fleet app names, so the view can dim a requested
   *  server that isn't present on this host (nice-to-have). May be empty before
   *  the first fleet poll. */
  fleetApps: () => string[];
  /** I3: the live replicated invocation index (the scheduled-invocation source
   *  of truth, rebuilt on each vault state alongside the agent index). */
  invocations: () => InvocationIndex;
  /** I3: the display title for a host note id (its first heading or path
   *  basename) so the Host-note column reads naturally; null if the file is
   *  gone (orphaned handle awaiting the component's next prune tick). */
  hostNoteTitle: (fileId: string) => string | null;
  /** I3: resolve an agent definition by name (case-insensitive) so the
   *  Runs table's Agent cell links to the def file. Null if unindexed. */
  agentByName: (name: string) => AgentDef | null;
  /** Run an agent once now (the row "Run" action, embedded-runs R3). With no
   *  editor host note for a chip, this runs the agent once (`run_agent`) and
   *  appends its output as a callout to the agent's own note. */
  onRun: (name: string) => void;
}

// ── Tools/MCP T1: effective per-agent MCP status ──────────────────────────────

/** The UI-side effective status of an agent's MCP request — mirrors
 *  `Vault::mcp_status` in `apps/tangram/src/lib.rs`. `stale` means a decision
 *  exists but the def's request changed since (re-approval required; treated
 *  like `pending`). */
type McpEffectiveStatus = "pending" | "approved" | "denied" | "stale";

interface McpStatus {
  status: McpEffectiveStatus;
  /** The canonical servers currently REQUESTED (the live def's `mcp_servers`). */
  requested: string[];
  /** The hash of the current request (what `approve_mcp` is called with). */
  requestedHash: string;
  /** The servers currently APPROVED (empty unless `status === "approved"`). */
  approved: string[];
}

/** Derive an agent's effective MCP status from the live def request + the
 *  recorded grant, exactly as the component's `mcp_status` read does. */
function effectiveMcpStatus(def: AgentDef, grants: McpGrant[]): McpStatus {
  const requested = def.mcpServers;
  const requestedHash = mcpRequestHash(requested);
  const grant = grants.find(
    (g) => g.agent.trim().toLowerCase() === def.name.trim().toLowerCase(),
  );
  if (!grant) return { status: "pending", requested, requestedHash, approved: [] };
  if (grant.requested_hash !== requestedHash) {
    return { status: "stale", requested, requestedHash, approved: [] };
  }
  if (grant.status === "approved") {
    return { status: "approved", requested, requestedHash, approved: grant.approved };
  }
  if (grant.status === "denied") {
    return { status: "denied", requested, requestedHash, approved: [] };
  }
  return { status: "pending", requested, requestedHash, approved: [] };
}

// Sort + query state is module-local so it survives re-renders driven by vault
// state frames (the index rebuilds, but the user's sort/filter should persist).
let sortKey: SortKey = "name";
let sortAsc = true;
let query = "";

// ── I3: invocations table (row-model + sort/filter) ──────────────────────────

/** A flattened, display-ready row for one scheduled invocation. Pure data
 *  derived from the replicated index + the agent/file lookups, so the sort,
 *  filter, and rendering can all work off the same precomputed strings. */
export interface InvocationRow {
  id: string;
  agent: string;
  /** The host note id where the `agent://<id>` link lives (row click opens it). */
  hostFileId: string;
  /** A human-readable label for the host note (heading/basename), or its id
   *  when the file is gone (orphaned handle awaiting prune). */
  hostNote: string;
  /** Whether the host note still exists (a missing file dims + disables open). */
  hostExists: boolean;
  /** Human schedule summary (e.g. "Daily at 09:00 UTC"); never empty. */
  trigger: string;
  status: string;
  lastRunMs: number | null;
  /** Projected next fire (epoch ms), or null when it can't be computed. */
  nextFireMs: number | null;
}

/** Build the display rows for the invocations table from the replicated index.
 *  Pure (given the lookups + `nowMs`) so it can be unit-tested. */
export function buildInvocationRows(
  invocations: Invocation[],
  nowMs: number,
  hostNoteTitle: (fileId: string) => string | null,
): InvocationRow[] {
  return invocations.map((inv) => {
    const title = hostNoteTitle(inv.host_file_id);
    return {
      id: inv.id,
      agent: inv.agent,
      hostFileId: inv.host_file_id,
      hostNote: title ?? inv.host_file_id,
      hostExists: title !== null,
      trigger: formatSchedule(inv.trigger),
      status: inv.status,
      lastRunMs: inv.last_run_ms,
      nextFireMs: nextFireMs(inv.trigger, nowMs, inv.last_run_ms),
    };
  });
}

type InvSortKey = "agent" | "trigger" | "hostNote" | "status" | "lastRun" | "nextFire";

interface InvColumnDef {
  key: InvSortKey;
  label: string;
  /** Sort comparator value: a string sorts lexically, a number numerically
   *  (with null sorting last regardless of direction). */
  sortValue: (r: InvocationRow) => string | number | null;
}

const INV_COLUMNS: InvColumnDef[] = [
  { key: "agent", label: "Agent", sortValue: (r) => r.agent.toLowerCase() },
  { key: "trigger", label: "Schedule", sortValue: (r) => r.trigger.toLowerCase() },
  { key: "hostNote", label: "Host note", sortValue: (r) => r.hostNote.toLowerCase() },
  { key: "status", label: "Status", sortValue: (r) => r.status.toLowerCase() },
  // "Last/next execution" — the Run's most recent / next Execution (R1 wording).
  { key: "lastRun", label: "Last execution", sortValue: (r) => r.lastRunMs },
  { key: "nextFire", label: "Next execution", sortValue: (r) => r.nextFireMs },
];

/** Sort invocation rows by the active column/direction. Nulls (e.g. a never-run
 *  interval's next-fire) always sort last so the populated rows lead. Stable for
 *  equal keys (preserves the replicated index order). */
export function sortInvocationRows(
  rows: InvocationRow[],
  key: InvSortKey,
  asc: boolean,
): InvocationRow[] {
  const col = INV_COLUMNS.find((c) => c.key === key)!;
  return rows
    .map((r, i) => ({ r, i }))
    .sort((a, b) => {
      const av = col.sortValue(a.r);
      const bv = col.sortValue(b.r);
      if (av === null && bv === null) return a.i - b.i;
      if (av === null) return 1; // nulls last, both directions
      if (bv === null) return -1;
      let cmp: number;
      if (typeof av === "number" && typeof bv === "number") cmp = av - bv;
      else cmp = String(av).localeCompare(String(bv));
      if (cmp === 0) return a.i - b.i;
      return asc ? cmp : -cmp;
    })
    .map((x) => x.r);
}

/** Filter invocation rows by a bare case-insensitive substring over the
 *  agent / trigger / host-note / status fields (mirrors the agents table's bare
 *  AND-tokens, simplified — no key: clauses for invocations). */
export function filterInvocationRows(rows: InvocationRow[], raw: string): InvocationRow[] {
  const terms = raw.toLowerCase().split(/\s+/).filter((t) => t.length > 0);
  if (terms.length === 0) return rows;
  return rows.filter((r) => {
    const hay = `${r.agent} ${r.trigger} ${r.hostNote} ${r.status}`.toLowerCase();
    return terms.every((t) => hay.includes(t));
  });
}

// Invocations table sort + filter state, module-local like the agents table's.
let invSortKey: InvSortKey = "agent";
let invSortAsc = true;
let invQuery = "";

// The invocation id to scroll-to + briefly highlight after the next render (set
// by the Trigger popup's "Open in Agents" deep-link, consumed once).
let pendingFocusInvocationId: string | null = null;

/** Request that the Runs table scroll to + highlight the row for `id` on
 *  its next render (the Run-editor → Agents-tab deep-link). Also switches the
 *  view to the Runs sub-tab, since the table now lives behind a tab.
 *  Consumed once. */
export function focusInvocationRow(id: string): void {
  pendingFocusInvocationId = id;
  setSubTab("triggers");
}

// ── sub-tabs (Agents | Runs) ──────────────────────────────────────────────────

/** Which sub-tab is shown inside the Agents view. The agents list/table and the
 *  scheduled-Runs table each live behind one tab; only one is visible at a
 *  time. Defaults to "agents". Module-local so it survives re-renders driven by
 *  vault state frames. (The `"triggers"` token is an internal id, kept for the
 *  persisted-choice key + CSS hooks; the user-facing label is "Runs".) */
type SubTab = "agents" | "triggers";

const SUBTAB_STORAGE_KEY = "tangram.agentsView.subTab";

function loadSubTab(): SubTab {
  try {
    const v = localStorage.getItem(SUBTAB_STORAGE_KEY);
    if (v === "triggers" || v === "agents") return v;
  } catch {
    // localStorage unavailable (private mode / tests) — fall through to default.
  }
  return "agents";
}

let activeSubTab: SubTab = loadSubTab();

/** Set the active sub-tab (persisted best-effort). Exposed so the deep-link can
 *  activate the Runs tab before focusing a row. Does NOT re-render — the
 *  caller (or the next vault-state render) paints the new selection. */
export function setSubTab(tab: SubTab): void {
  activeSubTab = tab;
  try {
    localStorage.setItem(SUBTAB_STORAGE_KEY, tab);
  } catch {
    // best-effort persistence only
  }
}

// ── render ───────────────────────────────────────────────────────────────────

function el(tag: string, cls?: string, text?: string): HTMLElement {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

/** Render the Agents table into `host` from the live index. Safe to call
 *  repeatedly (replaces the host's children). */
export function renderAgentsView(
  host: HTMLElement,
  index: AgentIndex,
  cb: AgentsViewCallbacks,
): void {
  host.replaceChildren();
  const wrap = el("div", "agents-view");

  // Sub-tab bar (Agents | Runs): a segmented control reusing the popup-v2
  // pill language. The Agents tab shows the agents list/table; the Runs tab
  // shows the scheduled-Runs table. Only one panel is visible at a time;
  // default is Agents. A pending deep-link focus (set by the Run editor's
  // "Open in Agents") will already have flipped `activeSubTab` to "triggers".
  const subtabs = el("div", "agents-subtabs");
  const seg = el("div", "agent-seg agents-subtab-seg");
  seg.setAttribute("role", "tablist");
  seg.setAttribute("aria-label", "Agents view sections");
  const subTabBtn = (tab: SubTab, label: string): HTMLButtonElement => {
    const b = el("button", "agent-seg-btn agents-subtab-btn", label) as HTMLButtonElement;
    b.type = "button";
    b.dataset.subtab = tab;
    b.setAttribute("role", "tab");
    const active = activeSubTab === tab;
    b.classList.toggle("active", active);
    b.setAttribute("aria-selected", active ? "true" : "false");
    b.addEventListener("click", () => {
      if (activeSubTab === tab) return;
      setSubTab(tab);
      renderAgentsView(host, index, cb);
    });
    return b;
  };
  seg.append(subTabBtn("agents", "Agents"), subTabBtn("triggers", "Runs"));
  subtabs.appendChild(seg);
  wrap.appendChild(subtabs);

  // The Agents panel (the agents list/table) — visible only on the Agents tab.
  const agentsPanel = el("div", "agents-panel agents-panel-agents");
  agentsPanel.setAttribute("role", "tabpanel");
  if (activeSubTab !== "agents") agentsPanel.hidden = true;

  // Header: a title row (title + the "+ New agent" CTA, the discoverable
  // on-ramp #9) over the GitHub-issues-style query bar + a live count.
  const head = el("div", "agents-head");
  const titleRow = el("div", "agents-title-row");
  titleRow.appendChild(el("h1", "agents-title", "Agents"));
  const newBtn = el("button", "agents-new-btn", "+ New agent") as HTMLButtonElement;
  newBtn.type = "button";
  newBtn.title = "Create a new agent or skill";
  newBtn.addEventListener("click", () => cb.newAgent());
  titleRow.appendChild(newBtn);
  head.appendChild(titleRow);

  const bar = el("div", "agents-bar");
  const input = document.createElement("input");
  input.type = "text";
  input.className = "agents-query";
  input.placeholder = "Filter… e.g. kind:skill label:demo cost_tier=low review";
  input.value = query;
  input.spellcheck = false;
  bar.appendChild(input);
  const count = el("div", "agents-count micro");
  bar.appendChild(count);
  head.appendChild(bar);
  agentsPanel.appendChild(head);

  const tableWrap = el("div", "agents-table-wrap");
  const table = el("table", "agents-table");
  const thead = el("thead");
  const headRow = el("tr");
  for (const col of COLUMNS) {
    const th = el("th", "agents-th", col.label);
    th.dataset.key = col.key;
    if (col.key === sortKey) {
      th.classList.add("sorted");
      th.appendChild(el("span", "agents-sort-caret", sortAsc ? " ▲" : " ▼"));
    }
    th.addEventListener("click", () => {
      if (sortKey === col.key) sortAsc = !sortAsc;
      else {
        sortKey = col.key;
        sortAsc = true;
      }
      renderAgentsView(host, index, cb);
    });
    headRow.appendChild(th);
  }
  headRow.appendChild(el("th", "agents-th agents-th-mcp", "Tools / MCP")); // T1
  headRow.appendChild(el("th", "agents-th agents-th-actions", "")); // row actions
  thead.appendChild(headRow);
  table.appendChild(thead);

  const tbody = el("tbody");
  table.appendChild(tbody);
  tableWrap.appendChild(table);
  agentsPanel.appendChild(tableWrap);
  wrap.appendChild(agentsPanel);

  // The Runs panel (the scheduled-Run table, user-facing "Runs") — visible only
  // on the Runs tab. Rendered from the live replicated index; re-rendered on
  // each vault state along with the rest of the view. (The data model — the
  // `invocations` index, `invocationId` internals — is unchanged; this is a UI
  // relabel: "Invocations" → "Triggers" → "Runs".)
  const triggersPanel = el("div", "agents-panel agents-panel-triggers");
  triggersPanel.setAttribute("role", "tabpanel");
  if (activeSubTab !== "triggers") triggersPanel.hidden = true;
  renderInvocationsSection(triggersPanel, cb, host, index);
  wrap.appendChild(triggersPanel);

  host.appendChild(wrap);

  // After the view is in the DOM, honour a pending deep-link focus (the Trigger
  // popup's "Open in Agents"): scroll to + briefly highlight the row.
  if (pendingFocusInvocationId !== null) {
    const id = pendingFocusInvocationId;
    pendingFocusInvocationId = null;
    const row = host.querySelector<HTMLElement>(`[data-invocation-id="${CSS.escape(id)}"]`);
    if (row) {
      row.scrollIntoView({ block: "center", behavior: "smooth" });
      row.classList.add("invocations-row-flash");
      setTimeout(() => row.classList.remove("invocations-row-flash"), 1600);
    }
  }

  const col = COLUMNS.find((c) => c.key === sortKey)!;

  // Re-render only the rows + count when the query/sort/index change, without
  // tearing down the (focused) query input.
  const paint = () => {
    const q = parseQuery(query);
    const rows = index.all
      .filter((d) => matches(d, q))
      .sort((a, b) => {
        const cmp = col.sortValue(a).localeCompare(col.sortValue(b));
        return sortAsc ? cmp : -cmp;
      });
    count.textContent = `${rows.length} ${rows.length === 1 ? "agent" : "agents"}`;
    tbody.replaceChildren();
    if (rows.length === 0) {
      const tr = el("tr");
      const td = el("td", "agents-empty") as HTMLTableCellElement;
      td.colSpan = COLUMNS.length + 2;
      if (index.all.length === 0) {
        // First-run CTA (#9/#10): turn the passive "nothing here" into an
        // on-ramp — a primary action plus the one-line hint that teaches the
        // two ways to get an agent going.
        const cta = el("div", "agents-empty-cta");
        cta.appendChild(el("div", "agents-empty-title", "No agents or skills yet"));
        const ctaBtn = el("button", "agents-new-btn", "+ New agent") as HTMLButtonElement;
        ctaBtn.type = "button";
        ctaBtn.addEventListener("click", () => cb.newAgent());
        cta.appendChild(ctaBtn);
        const hint = el("div", "agents-empty-hint");
        hint.append(
          document.createTextNode("Type "),
          el("code", "agents-kbd", "/"),
          document.createTextNode(" in any note to create or run an agent, or click "),
          el("strong", undefined, "+ New agent"),
          document.createTextNode("."),
        );
        cta.appendChild(hint);
        td.appendChild(cta);
      } else {
        td.textContent = "No matches";
      }
      tr.appendChild(td);
      tbody.appendChild(tr);
      return;
    }
    for (const def of rows) tbody.appendChild(renderRow(def, cb, host, index));
  };

  input.addEventListener("input", () => {
    query = input.value;
    paint();
  });

  paint();
}

/** Render the Runs section (header + query bar + sortable table) into `wrap`.
 *  This is the user-facing "Runs" view over the replicated *invocation* index
 *  (the data model is unchanged — only the label changed). Mirrors the agents
 *  table's sort/filter UX. Row click opens the host note; the Agent cell links
 *  to the agent definition. */
function renderInvocationsSection(
  wrap: HTMLElement,
  cb: AgentsViewCallbacks,
  host: HTMLElement,
  index: AgentIndex,
): void {
  const section = el("section", "invocations-view");

  const head = el("div", "agents-head invocations-head");
  const titleRow = el("div", "agents-title-row");
  titleRow.appendChild(el("h2", "invocations-title", "Runs"));
  head.appendChild(titleRow);

  const bar = el("div", "agents-bar");
  const input = document.createElement("input");
  input.type = "text";
  input.className = "agents-query";
  input.placeholder = "Filter runs… e.g. standup daily scheduled";
  input.value = invQuery;
  input.spellcheck = false;
  bar.appendChild(input);
  const count = el("div", "agents-count micro");
  bar.appendChild(count);
  head.appendChild(bar);
  section.appendChild(head);

  const tableWrap = el("div", "agents-table-wrap");
  const table = el("table", "agents-table invocations-table");
  const thead = el("thead");
  const headRow = el("tr");
  for (const col of INV_COLUMNS) {
    const th = el("th", "agents-th", col.label);
    th.dataset.key = col.key;
    if (col.key === invSortKey) {
      th.classList.add("sorted");
      th.appendChild(el("span", "agents-sort-caret", invSortAsc ? " ▲" : " ▼"));
    }
    th.addEventListener("click", () => {
      if (invSortKey === col.key) invSortAsc = !invSortAsc;
      else {
        invSortKey = col.key;
        invSortAsc = true;
      }
      renderAgentsView(host, index, cb);
    });
    headRow.appendChild(th);
  }
  thead.appendChild(headRow);
  table.appendChild(thead);
  const tbody = el("tbody");
  table.appendChild(tbody);
  tableWrap.appendChild(table);
  section.appendChild(tableWrap);
  wrap.appendChild(section);

  const now = Date.now();

  const paint = () => {
    const all = buildInvocationRows(cb.invocations().all, now, cb.hostNoteTitle);
    const filtered = filterInvocationRows(all, invQuery);
    const rows = sortInvocationRows(filtered, invSortKey, invSortAsc);
    count.textContent = `${rows.length} ${rows.length === 1 ? "run" : "runs"}`;
    tbody.replaceChildren();
    if (rows.length === 0) {
      const tr = el("tr");
      const td = el("td", "agents-empty") as HTMLTableCellElement;
      td.colSpan = INV_COLUMNS.length;
      td.textContent =
        all.length === 0 ? "No scheduled runs yet" : "No matches";
      tr.appendChild(td);
      tbody.appendChild(tr);
      return;
    }
    for (const row of rows) tbody.appendChild(renderInvocationRow(row, cb, now));
  };

  input.addEventListener("input", () => {
    invQuery = input.value;
    paint();
  });

  paint();
}

/** Render one invocations table row. The whole row opens the host note (the
 *  same path quick-open/sidebar uses); the Agent cell additionally links to the
 *  agent definition file. */
function renderInvocationRow(
  row: InvocationRow,
  cb: AgentsViewCallbacks,
  nowMs: number,
): HTMLElement {
  const tr = el("tr", "agents-row invocations-row");
  tr.dataset.invocationId = row.id;
  // Row click → open the host note (skip when the click was on the Agent link,
  // which has its own target — the agent definition).
  if (row.hostExists) {
    tr.classList.add("invocations-row-clickable");
    tr.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest(".agents-name-link")) return;
      cb.openNote(row.hostFileId);
    });
  }

  // Agent → links to the agent definition file (reuse the agents-list link
  // styling). Disabled when no def with that name is indexed.
  const agentTd = el("td", "agents-cell agents-cell-name");
  const def = cb.agentByName(row.agent);
  const link = el("button", "agents-name-link", row.agent) as HTMLButtonElement;
  if (def?.fileId) {
    link.title = `Open ${def.path}`;
    link.addEventListener("click", (e) => {
      e.stopPropagation();
      cb.openNote(def.fileId);
    });
  } else {
    link.disabled = true;
    link.title = `No agent definition named "${row.agent}"`;
  }
  agentTd.appendChild(link);
  tr.appendChild(agentTd);

  // Trigger (human schedule summary).
  tr.appendChild(el("td", "agents-cell", row.trigger));

  // Host note (the note carrying the agent:// link).
  const hostTd = el("td", "agents-cell invocations-cell-host");
  const hostSpan = el(
    "span",
    `invocations-host${row.hostExists ? "" : " invocations-host-missing"}`,
    row.hostNote,
  );
  if (!row.hostExists) hostSpan.title = "Host note not found (orphaned handle)";
  hostTd.appendChild(hostSpan);
  tr.appendChild(hostTd);

  // Status chip.
  const statusTd = el("td", "agents-cell");
  statusTd.appendChild(
    el(
      "span",
      `invocations-status invocations-status-${row.status.toLowerCase().replace(/[^a-z0-9]+/g, "-")}`,
      row.status,
    ),
  );
  tr.appendChild(statusTd);

  // Last run (relative).
  const lastTd = el("td", "agents-cell agents-cell-mono", formatRelativeTime(row.lastRunMs, nowMs));
  if (row.lastRunMs !== null) lastTd.title = new Date(row.lastRunMs).toLocaleString();
  tr.appendChild(lastTd);

  // Next fire (computed from the schedule grammar, or "—" when unprojectable).
  const nextTd = el("td", "agents-cell agents-cell-mono", formatRelativeTime(row.nextFireMs, nowMs));
  if (row.nextFireMs !== null) nextTd.title = new Date(row.nextFireMs).toLocaleString();
  tr.appendChild(nextTd);

  return tr;
}

function renderRow(
  def: AgentDef,
  cb: AgentsViewCallbacks,
  host: HTMLElement,
  index: AgentIndex,
): HTMLElement {
  const tr = el("tr", "agents-row");

  // Name → opens the source vault file in a tab.
  const nameTd = el("td", "agents-cell agents-cell-name");
  const nameLink = el("button", "agents-name-link", def.name) as HTMLButtonElement;
  nameLink.title = `Open ${def.path}`;
  if (def.fileId) {
    nameLink.addEventListener("click", () => cb.openNote(def.fileId));
  } else {
    nameLink.disabled = true;
  }
  nameTd.appendChild(nameLink);
  tr.appendChild(nameTd);

  // Kind chip.
  const kindTd = el("td", "agents-cell");
  kindTd.appendChild(el("span", `agents-kind agents-kind-${def.kind}`, def.kind));
  tr.appendChild(kindTd);

  // Model.
  tr.appendChild(el("td", "agents-cell agents-cell-mono", def.model));

  // Labels chips + an inline "+ label" affordance.
  const labelsTd = el("td", "agents-cell agents-cell-labels");
  const chips = el("div", "agents-chips");
  for (const l of def.labels) chips.appendChild(el("span", "agents-chip", l));
  labelsTd.appendChild(chips);
  if (def.fileId) {
    labelsTd.appendChild(
      inlineAdder("+ label", "label", (raw) => {
        const label = raw.trim();
        if (!label) return;
        void applyEdit(def, host, cb, index, (body) => addLabelToBody(body, label));
      }),
    );
  }
  tr.appendChild(labelsTd);

  // Version.
  tr.appendChild(el("td", "agents-cell agents-cell-mono", def.version ?? "—"));

  // Path + an inline "+ meta key=value" affordance.
  const pathTd = el("td", "agents-cell agents-cell-path");
  pathTd.appendChild(el("span", "agents-path", def.path));
  if (def.fileId) {
    pathTd.appendChild(
      inlineAdder("+ meta", "key=value", (raw) => {
        const eq = raw.indexOf("=");
        if (eq <= 0) return;
        const key = raw.slice(0, eq).trim();
        const value = raw.slice(eq + 1).trim();
        if (!key) return;
        void applyEdit(def, host, cb, index, (body) => addMetaToBody(body, key, value));
      }),
    );
  }
  tr.appendChild(pathTd);

  // Tools / MCP (T1): the access request + the user's grant decision.
  const mcpTd = el("td", "agents-cell agents-cell-mcp");
  mcpTd.appendChild(renderMcpCell(def, cb, host, index));
  tr.appendChild(mcpTd);

  // Row actions: Run.
  const actTd = el("td", "agents-cell agents-cell-actions");
  const run = el("button", "agents-run", "Run");
  run.title = `Run ${def.name}`;
  run.addEventListener("click", () =>
    openAgentPopup(def, {
      // No editor host note here, so submit runs the agent once (`run_agent`),
      // appending a callout to the agent's own note (embedded-runs R3).
      onSubmit: () => cb.onRun(def.name),
      onClose: () => {},
    }),
  );
  actTd.appendChild(run);
  tr.appendChild(actTd);

  return tr;
}

/** Render the Tools / MCP cell for one agent (T1). Skills and agents that
 *  request no servers show a muted "—". Otherwise show the requested servers as
 *  chips (unknown servers dimmed) plus the grant status and the matching
 *  Approve / Deny / Revoke affordance. */
function renderMcpCell(
  def: AgentDef,
  cb: AgentsViewCallbacks,
  host: HTMLElement,
  index: AgentIndex,
): HTMLElement {
  // Only `kind: agent` with a non-empty request participates (skills ignore it).
  if (def.kind !== "agent" || def.mcpServers.length === 0) {
    return el("span", "agents-mcp-none micro", "—");
  }

  const wrap = el("div", "agents-mcp");
  const st = effectiveMcpStatus(def, cb.mcpGrants());
  const fleet = new Set(cb.fleetApps().map((n) => n.toLowerCase()));

  // Status pill.
  const pillText: Record<McpEffectiveStatus, string> = {
    pending: "requests access",
    stale: "request changed — re-approve",
    approved: "approved",
    denied: "denied",
  };
  const pill = el(
    "span",
    `agents-mcp-pill agents-mcp-pill-${st.status}`,
    pillText[st.status],
  );
  wrap.appendChild(pill);

  // The server chips: for approved, show the approved set; otherwise the
  // requested set. Dim any server name not present in the live fleet.
  const shown = st.status === "approved" ? st.approved : st.requested;
  const chips = el("div", "agents-mcp-chips");
  for (const s of shown) {
    const known = fleet.size === 0 || fleet.has(s.toLowerCase());
    const chip = el(
      "span",
      `agents-mcp-server${known ? "" : " agents-mcp-server-unknown"}`,
      s,
    );
    if (!known) chip.title = `No app named "${s}" on this host`;
    chips.appendChild(chip);
  }
  wrap.appendChild(chips);

  // Decision affordances per status (pending/stale → Approve+Deny; approved →
  // Revoke; denied → Approve to reconsider).
  const actions = el("div", "agents-mcp-actions");
  const approveBtn = (label: string) => {
    const b = el("button", "agents-mcp-btn agents-mcp-approve", label) as HTMLButtonElement;
    b.type = "button";
    b.addEventListener("click", () =>
      runMcp(host, cb, index, () => vault.approveMcp(def.name, st.requestedHash)),
    );
    return b;
  };
  const denyBtn = (label: string) => {
    const b = el("button", "agents-mcp-btn agents-mcp-deny", label) as HTMLButtonElement;
    b.type = "button";
    b.addEventListener("click", () =>
      runMcp(host, cb, index, () => vault.denyMcp(def.name)),
    );
    return b;
  };
  const revokeBtn = () => {
    const b = el("button", "agents-mcp-btn agents-mcp-revoke", "Revoke") as HTMLButtonElement;
    b.type = "button";
    b.addEventListener("click", () =>
      runMcp(host, cb, index, () => vault.revokeMcp(def.name)),
    );
    return b;
  };

  if (st.status === "pending" || st.status === "stale") {
    actions.append(approveBtn("Approve"), denyBtn("Deny"));
  } else if (st.status === "approved") {
    actions.append(revokeBtn());
  } else {
    // denied — offer reconsidering.
    actions.append(approveBtn("Approve"));
  }
  wrap.appendChild(actions);

  return wrap;
}

/** Run an MCP grant action; on success the vault state frame re-renders the
 *  view (the grants change there), and on failure route the error through the
 *  themed toast and re-render from the current index. */
async function runMcp(
  host: HTMLElement,
  cb: AgentsViewCallbacks,
  index: AgentIndex,
  action: () => Promise<unknown>,
): Promise<void> {
  try {
    await action();
  } catch (e) {
    showError(String(e instanceof Error ? e.message : e));
    renderAgentsView(host, index, cb);
  }
}

/** A compact toggle button that swaps for a small inline input on click. The
 *  input commits on Enter (calling `onCommit`), cancels on Esc / blur. */
function inlineAdder(
  buttonLabel: string,
  placeholder: string,
  onCommit: (value: string) => void,
): HTMLElement {
  const wrap = el("span", "agents-adder");
  const btn = el("button", "agents-adder-btn", buttonLabel);
  wrap.appendChild(btn);

  btn.addEventListener("click", () => {
    const input = document.createElement("input");
    input.type = "text";
    input.className = "agents-adder-input";
    input.placeholder = placeholder;
    input.spellcheck = false;
    wrap.replaceChildren(input);
    input.focus();

    let done = false;
    const restore = () => {
      if (done) return;
      done = true;
      wrap.replaceChildren(btn);
    };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        const v = input.value;
        done = true; // a vault round-trip re-renders the row anyway
        onCommit(v);
        wrap.replaceChildren(btn);
      } else if (e.key === "Escape") {
        e.preventDefault();
        restore();
      }
    });
    input.addEventListener("blur", restore);
  });

  return wrap;
}

/** Read the def's current source body, apply a frontmatter rewrite, and persist
 *  it (vault.writeFile). The vault state frame that follows rebuilds the index
 *  and re-renders the view, so the new label/meta becomes filterable. A no-op
 *  rewrite (returns null) is silently ignored. */
async function applyEdit(
  def: AgentDef,
  host: HTMLElement,
  cb: AgentsViewCallbacks,
  index: AgentIndex,
  rewrite: (body: string) => string | null,
): Promise<void> {
  const file = cb.fileById(def.fileId);
  if (!file) return;
  const next = rewrite(file.body);
  if (next === null || next === file.body) return;
  try {
    await vault.writeFile(def.fileId, next);
  } catch (e) {
    showError(String(e instanceof Error ? e.message : e));
    // Re-render from the (unchanged) index so the inline input resets cleanly.
    renderAgentsView(host, index, cb);
  }
}
