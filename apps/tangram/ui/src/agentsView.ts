// The Agents view (P2): a GitHub-issues-style sortable, filterable table of
// every agent/skill indexed across the vault (the P1 frontmatter index in
// agents.ts). Shell-UI-only — rendered into the main content area as an
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

import { vault, type MdFile } from "./api";
import { type AgentDef, type AgentIndex } from "./agents";
import { openAgentPopup } from "./agentPopup";

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
}

// Sort + query state is module-local so it survives re-renders driven by vault
// state frames (the index rebuilds, but the user's sort/filter should persist).
let sortKey: SortKey = "name";
let sortAsc = true;
let query = "";

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

  // Header: title + the GitHub-issues-style query bar + a live count.
  const head = el("div", "agents-head");
  head.appendChild(el("h1", "agents-title", "Agents"));

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
  wrap.appendChild(head);

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
  headRow.appendChild(el("th", "agents-th agents-th-actions", "")); // row actions
  thead.appendChild(headRow);
  table.appendChild(thead);

  const tbody = el("tbody");
  table.appendChild(tbody);
  tableWrap.appendChild(table);
  wrap.appendChild(tableWrap);
  host.appendChild(wrap);

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
      td.colSpan = COLUMNS.length + 1;
      td.textContent = index.all.length === 0 ? "No agents or skills yet" : "No matches";
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

  // Row actions: Run.
  const actTd = el("td", "agents-cell agents-cell-actions");
  const run = el("button", "agents-run", "Run");
  run.title = `Run ${def.name}`;
  run.addEventListener("click", () =>
    openAgentPopup(def, { onSave: () => {}, onClose: () => {} }),
  );
  actTd.appendChild(run);
  tr.appendChild(actTd);

  return tr;
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
    window.alert(String(e instanceof Error ? e.message : e));
    // Re-render from the (unchanged) index so the inline input resets cleanly.
    renderAgentsView(host, index, cb);
  }
}
