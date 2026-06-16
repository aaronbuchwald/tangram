// The tangram shell: a persistent left sidebar (vault folder tree + live
// apps list) and a main window with an Obsidian-style tab strip. A tab is a
// rendered/edited markdown file, an app embedded as an iframe, or the home
// view. Phase S1; deferred phases are listed in ui/README.md.

import "./styles.css";
import {
  fetchFleet,
  SHELL_APP,
  subscribeVault,
  vault,
  type FleetApp,
  type McpGrant,
  type MdFile,
  type VaultState,
} from "./api";
import { isAgentPopupOpen, openAgentPopup } from "./agentPopup";
import { isTriggerPopupOpen, openTriggerPopup } from "./triggerPopup";
import {
  isQuickOpenOpen,
  openQuickOpen,
  type QuickOpenItem,
} from "./quickOpen";
import {
  DEFAULT_MODEL,
  buildAgentIndex,
  type AgentDef,
  type AgentIndex,
} from "./agents";
import { renderAgentsView } from "./agentsView";
import { buildLinkIndex, type LinkIndex } from "./links";
import {
  buildAgentLink,
  buildInvocationIndex,
  parseSchedule,
  type InvocationIndex,
} from "./invocations";
import { wikiCandidatesFromFiles } from "./wikiComplete";
import {
  type CreatedAgent,
  isCreateAgentPopupOpen,
  openCreateAgentPopup,
} from "./createAgentPopup";
import { loadAuthState, renderLogin, renderPrincipalChip } from "./auth";
import { MdEditor } from "./editor";
import { CREATE_WORD } from "./slashTrigger";
import { registry } from "./manage";
import { confirmAction, promptName, showError } from "./modal";
import { TabStore, type Tab } from "./tabs";
import { buildTree, type TreeNode } from "./tree";
import { mountChatPanel, setActiveApp } from "./chatPanel";

// ── icons ────────────────────────────────────────────────────────────────────
// Inline 16px stroke icons (Lucide-style, currentColor) so vault affordances
// read as quiet glyphs rather than clunky bordered buttons — the Obsidian
// "typography + minimal iconography" idiom. Kept tiny and stroke-only so they
// inherit the surrounding text colour and animate with it on hover.
const ICON = {
  // file with a small plus — "new note"
  file: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 3v4a1 1 0 0 0 1 1h4"/><path d="M11.5 21H6a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h8l6 6v3"/><path d="M16 18h6"/><path d="M19 15v6"/></svg>`,
  // folder with a small plus — "new folder"
  folderPlus: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.93a2 2 0 0 1 1.66.9l.82 1.2a2 2 0 0 0 1.66.9H20a2 2 0 0 1 2 2v3"/><path d="M16 19h6"/><path d="M19 16v6"/></svg>`,
  // pencil — "rename"
  pencil: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>`,
  // trash — "delete"
  trash: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/></svg>`,
};

/** Build a quiet, hover-revealed row-action button (icon glyph). */
function rowAction(icon: string, title: string, danger = false): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.className = danger ? "row-action danger" : "row-action";
  btn.type = "button";
  btn.title = title;
  btn.setAttribute("aria-label", title);
  btn.innerHTML = icon;
  return btn;
}

function displayName(slug: string): string {
  return slug.split("-").map(w => w.charAt(0).toUpperCase() + w.slice(1)).join(" ");
}

function displayFileName(name: string): string {
  return name.endsWith(".md") ? name.slice(0, -3) : name;
}

// ── shared mutable shell state ──────────────────────────────────────────────

let files: MdFile[] = [];
const filesById = new Map<string, MdFile>();
let fleet: FleetApp[] = [];
// Tools/MCP T1: the live MCP-access grant records from the vault state frame,
// read by the Agents view to derive each agent's effective grant status.
let mcpGrants: McpGrant[] = [];
// The agent/skill index, rebuilt over the vault on every state frame. The
// editor's `/<name>` resolver reads through this live reference (a stable
// closure), so a freshly-created definition becomes invocable on the next
// vault state without re-mounting the editor.
let agentIndex: AgentIndex = buildAgentIndex([]);
// The vault wikilink/backlink index (Connected Vault, G1), rebuilt over the
// vault on every state frame alongside `agentIndex` — the single rebuild point.
// The editor's `[[ ]]` resolver and the backlinks panel read through this live
// reference (a stable closure), so links re-resolve and backlinks refresh on
// the next vault state without re-mounting the editor.
let linkIndex: LinkIndex = buildLinkIndex([]);
// The scheduled-invocation index (the redesign): the app's REPLICATED
// `invocations` records, carried on the vault state frame and rebuilt here on
// every state alongside the other indexes. The inline `agent://<id>` link in a
// note is only the handle; the trigger/prompt/last-run live in this index. The
// Trigger popup reads an invocation from here by id.
let invocationIndex: InvocationIndex = buildInvocationIndex([]);
const collapsed = new Set<string>(); // collapsed folder paths
const tabs = new TabStore();

// The live CodeMirror editor for the active note tab. Held here so an SSE
// `state` frame can patch it in place (echo-safely) instead of tearing it
// down — a rebuild would drop focus/cursor and clobber an in-progress edit.
interface ActiveEditor {
  fileId: string;
  editor: MdEditor;
  saveTimer?: number;
}
let activeEditor: ActiveEditor | null = null;

// ── sidebar state ─────────────────────────────────────────────────────────────
// Vault/Apps default COLLAPSED (intentional), but the user's open/closed choice
// persists across reloads (#12) — mirroring how `sidebar-width` is persisted.
// A first-time user (no stored state) still gets the collapsed default; only an
// explicit toggle writes the stored set, so the default is never overridden.
const SECTION_COLLAPSE_KEY = "sidebar-collapsed-sections";
function loadCollapsedSections(): Set<string> {
  try {
    const raw = localStorage.getItem(SECTION_COLLAPSE_KEY);
    if (raw === null) return new Set(["vault", "apps"]); // first-time default
    const parsed = JSON.parse(raw) as unknown;
    if (Array.isArray(parsed)) {
      return new Set(parsed.filter((s): s is string => typeof s === "string"));
    }
  } catch {
    /* fall through to default on malformed/blocked storage */
  }
  return new Set(["vault", "apps"]);
}
const collapsedSections = loadCollapsedSections();
function persistCollapsedSections() {
  try {
    localStorage.setItem(SECTION_COLLAPSE_KEY, JSON.stringify([...collapsedSections]));
  } catch {
    /* storage may be unavailable (private mode); persistence is best-effort */
  }
}
let sidebarOpen = true;
let sidebarWidth = parseInt(localStorage.getItem("sidebar-width") ?? "268", 10);

function applySidebarOpen() {
  const sidebar = document.getElementById("sidebar") as HTMLElement;
  if (sidebarOpen) {
    sidebar.style.width = `${sidebarWidth}px`;
    sidebar.style.minWidth = `${sidebarWidth}px`;
    sidebar.style.overflow = "";
  } else {
    sidebar.style.width = "0";
    sidebar.style.minWidth = "0";
    sidebar.style.overflow = "hidden";
  }
}

function applySectionState() {
  const vaultBody = document.getElementById("vault-body") as HTMLElement;
  const appsBody = document.getElementById("apps-body") as HTMLElement;
  const vaultTwisty = document.getElementById("vault-twisty") as HTMLElement;
  const appsTwisty = document.getElementById("apps-twisty") as HTMLElement;
  vaultBody.style.display = collapsedSections.has("vault") ? "none" : "";
  appsBody.style.display = collapsedSections.has("apps") ? "none" : "";
  vaultTwisty.textContent = collapsedSections.has("vault") ? "▸" : "▾";
  appsTwisty.textContent = collapsedSections.has("apps") ? "▸" : "▾";
}

// ── DOM scaffold ─────────────────────────────────────────────────────────────

const root = document.getElementById("app")!;
root.innerHTML = `
  <div class="shell">
    <header class="topbar">
      <div class="topbar-left">
        <button class="sidebar-btn" id="sidebar-toggle" title="Toggle sidebar" aria-label="Toggle sidebar">
          <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
            <rect x="1" y="1" width="14" height="14" rx="2"/>
            <line x1="5" y1="1" x2="5" y2="15"/>
          </svg>
        </button>
        <div class="brand">Tangram</div>
      </div>
      <div class="topbar-right">
        <div class="status"><span class="dot" id="live-dot"></span><span id="live-label">Connecting…</span></div>
        <div id="principal-slot"></div>
      </div>
    </header>
    <div class="body">
      <aside class="sidebar" id="sidebar">
        <section class="side-section">
          <div class="side-head" id="vault-head">
            <span class="micro">Vault</span>
            <span class="section-caret" id="vault-twisty">▸</span>
            <div class="side-actions">
              <button class="head-action" id="new-note" title="New note" aria-label="New note">${ICON.file}</button>
              <button class="head-action" id="new-folder" title="New folder" aria-label="New folder">${ICON.folderPlus}</button>
            </div>
          </div>
          <div class="side-body" id="vault-body">
            <div class="tree" id="tree"></div>
          </div>
        </section>
        <section class="side-section">
          <div class="side-head" id="apps-head">
            <span class="micro">Apps</span>
            <span class="section-caret" id="apps-twisty">▸</span>
            <div class="side-actions">
              <button class="ghost" id="open-marketplace" title="Browse the marketplace">+ Install</button>
            </div>
          </div>
          <div class="side-body" id="apps-body">
            <div class="applist" id="applist"></div>
          </div>
        </section>
        <section class="side-section">
          <div class="side-head" id="agents-head" title="Open the Agents view">
            <span class="micro">Agents</span>
            <span class="agents-badge" id="agents-count-badge">0</span>
          </div>
        </section>
        <div class="sidebar-resizer" id="sidebar-resizer"></div>
      </aside>
      <main class="main">
        <div class="tabstrip" id="tabstrip"></div>
        <div class="content" id="content"></div>
      </main>
      <div id="chat-slot"></div>
    </div>
  </div>
`;

// Right-sidebar app chat (DeepSeek + the active app's MCP tools). All of its
// logic lives in chatPanel.ts / mcpClient.ts / llmChat.ts; main.ts only mounts
// it and notifies it on active-tab changes (see tabs.subscribe below).
mountChatPanel(document.getElementById("chat-slot")!);

const treeEl = document.getElementById("tree")!;
const applistEl = document.getElementById("applist")!;
const tabstripEl = document.getElementById("tabstrip")!;
const contentEl = document.getElementById("content")!;
const liveDot = document.getElementById("live-dot")!;
const liveLabel = document.getElementById("live-label")!;

document.getElementById("new-note")!.addEventListener("click", () => void newNote(""));
document.getElementById("new-folder")!.addEventListener("click", () => void newFolder(""));

// "+ install" / browse marketplace: the marketplace is itself an app, so the
// sidebar's job is just the entry point — open it in a tab (Decision E). The
// per-listing install flow lives in the marketplace app; we don't reimplement.
document
  .getElementById("open-marketplace")!
  .addEventListener("click", () => tabs.openApp("marketplace"));

// Sidebar open/close toggle
document.getElementById("sidebar-toggle")!.addEventListener("click", () => {
  sidebarOpen = !sidebarOpen;
  applySidebarOpen();
});

// Section header toggles — skip if click landed on an action button inside the head
document.getElementById("vault-head")!.addEventListener("click", (e) => {
  if ((e.target as HTMLElement).closest(".head-action")) return;
  if (collapsedSections.has("vault")) collapsedSections.delete("vault");
  else collapsedSections.add("vault");
  persistCollapsedSections();
  applySectionState();
});
document.getElementById("apps-head")!.addEventListener("click", (e) => {
  if ((e.target as HTMLElement).closest(".ghost")) return;
  if (collapsedSections.has("apps")) collapsedSections.delete("apps");
  else collapsedSections.add("apps");
  persistCollapsedSections();
  applySectionState();
});

// The Agents section header is an entry point, not a collapsible list — clicking
// it opens the Agents table tab (the sortable/filterable view; P2). The badge
// shows the indexed count and updates on each vault state.
document.getElementById("agents-head")!.addEventListener("click", () => {
  tabs.openAgents();
});

// Drag-to-resize the sidebar
document.getElementById("sidebar-resizer")!.addEventListener("mousedown", (startEvt) => {
  startEvt.preventDefault();
  const startX = startEvt.clientX;
  const startW = sidebarWidth;
  const sidebar = document.getElementById("sidebar") as HTMLElement;

  const onMove = (e: MouseEvent) => {
    sidebarWidth = Math.max(180, Math.min(520, startW + e.clientX - startX));
    sidebar.style.width = `${sidebarWidth}px`;
    sidebar.style.minWidth = `${sidebarWidth}px`;
    localStorage.setItem("sidebar-width", String(sidebarWidth));
  };
  const onUp = () => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
  };
  document.body.style.cursor = "col-resize";
  document.body.style.userSelect = "none";
  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
});

// Apply initial states
applySidebarOpen();
applySectionState();

// ── sidebar: vault tree ──────────────────────────────────────────────────────

function el(tag: string, cls?: string, text?: string): HTMLElement {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

function renderTree() {
  treeEl.replaceChildren();
  treeEl.setAttribute("role", "tree");
  const nodes = buildTree(files);
  if (nodes.length === 0) {
    treeEl.appendChild(el("div", "empty", "No notes yet"));
    return;
  }
  for (const node of nodes) treeEl.appendChild(renderNode(node, 0));
}

// Keyboard navigation across the VISIBLE tree rows (#6). Rows are focusable
// (tabindex=0) and carry tree-item semantics; ↑/↓ move focus between visible
// rows, Enter/→ open a file or expand a folder, ← collapses a folder (or, on a
// file/already-collapsed folder, steps to its parent). Click behaviour is
// untouched. Re-deriving the visible row list from the DOM each keystroke keeps
// it correct across the re-render that expand/collapse triggers.
function visibleTreeRows(): HTMLElement[] {
  return Array.from(treeEl.querySelectorAll<HTMLElement>(".tree-row"));
}

function focusTreeRow(row: HTMLElement | undefined) {
  row?.focus();
}

function onTreeRowKey(e: KeyboardEvent, row: HTMLElement, node: TreeNode) {
  const rows = visibleTreeRows();
  const idx = rows.indexOf(row);
  if (e.key === "ArrowDown") {
    e.preventDefault();
    focusTreeRow(rows[idx + 1]);
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    focusTreeRow(rows[idx - 1]);
  } else if (e.key === "Enter") {
    e.preventDefault();
    row.click();
  } else if (e.key === "ArrowRight") {
    e.preventDefault();
    if (node.kind === "folder") {
      if (collapsed.has(node.path)) {
        row.click(); // expand
        // After re-render, focus the same folder row so a second → can descend.
        requestAnimationFrame(() => focusFolderRow(node.path));
      } else {
        focusTreeRow(rows[idx + 1]); // already open → step into first child
      }
    }
  } else if (e.key === "ArrowLeft") {
    e.preventDefault();
    if (node.kind === "folder" && !collapsed.has(node.path)) {
      row.click(); // collapse
      requestAnimationFrame(() => focusFolderRow(node.path));
    } else {
      // file, or already-collapsed folder → step up to the parent folder row.
      focusTreeRow(parentFolderRow(node.path));
    }
  }
}

function focusFolderRow(path: string) {
  for (const row of visibleTreeRows()) {
    if (row.dataset.treePath === path) {
      row.focus();
      return;
    }
  }
}

function parentFolderRow(path: string): HTMLElement | undefined {
  const parent = path.split("/").slice(0, -1).join("/");
  if (!parent) return undefined;
  return visibleTreeRows().find((r) => r.dataset.treePath === parent);
}

function renderNode(node: TreeNode, depth: number): HTMLElement {
  if (node.kind === "folder") {
    const wrap = el("div", "tree-folder");
    const row = el("div", "tree-row folder-row");
    row.style.paddingLeft = `${depth * 20 + 8}px`;
    const isCollapsed = collapsed.has(node.path);
    row.tabIndex = 0;
    row.dataset.treePath = node.path;
    row.setAttribute("role", "treeitem");
    row.setAttribute("aria-expanded", String(!isCollapsed));
    row.addEventListener("keydown", (e) => onTreeRowKey(e, row, node));
    // Obsidian-style disclosure chevron: a single right-pointing chevron that
    // rotates 90° down when the folder is open (the `.open` class drives a CSS
    // transform/transition in styles.css). Dim/subtle, tight to the name —
    // clearly ▶ when collapsed, ▼ when expanded.
    const twisty = el("span", "twisty", "›");
    if (!isCollapsed) twisty.classList.add("open");
    row.appendChild(twisty);
    row.appendChild(el("span", "label", node.name));

    // Hover-revealed folder actions, ordered by frequency: new note inside,
    // new subfolder, rename, delete. Each carries the folder's path as context
    // so creation targets THIS folder (the #14 fix — folders previously had no
    // create affordance, so notes could only be made at the vault root).
    const actions = el("div", "row-actions");
    const addNote = rowAction(ICON.file, `New note in ${node.path}`);
    addNote.addEventListener("click", (e) => {
      e.stopPropagation();
      void newNote(node.path);
    });
    const addFolder = rowAction(ICON.folderPlus, `New folder in ${node.path}`);
    addFolder.addEventListener("click", (e) => {
      e.stopPropagation();
      void newFolder(node.path);
    });
    const ren = rowAction(ICON.pencil, `Rename folder ${node.path}`);
    ren.addEventListener("click", (e) => {
      e.stopPropagation();
      void renameFolder(node.path);
    });
    const del = rowAction(ICON.trash, `Delete folder ${node.path}`, true);
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      void deleteFolder(node.path);
    });
    actions.append(addNote, addFolder, ren, del);
    row.appendChild(actions);
    row.addEventListener("click", () => {
      if (collapsed.has(node.path)) collapsed.delete(node.path);
      else collapsed.add(node.path);
      renderTree();
    });
    wrap.appendChild(row);
    if (!isCollapsed) {
      const kids = el("div", "tree-children");
      for (const child of node.children) kids.appendChild(renderNode(child, depth + 1));
      wrap.appendChild(kids);
    }
    return wrap;
  }
  const row = el("div", "tree-row file-row");
  row.style.paddingLeft = `${depth * 20 + 8}px`;
  row.tabIndex = 0;
  row.dataset.treePath = node.path;
  row.setAttribute("role", "treeitem");
  row.addEventListener("keydown", (e) => onTreeRowKey(e, row, node));
  if (tabs.active?.kind === "note" && tabs.active.fileId === node.file.id) {
    row.classList.add("active");
  }
  row.appendChild(el("span", "twisty")); // spacer — aligns label with folder labels at same depth
  row.appendChild(el("span", "label", displayFileName(node.name)));
  const actions = el("div", "row-actions");
  const ren = rowAction(ICON.pencil, `Rename / move ${node.path}`);
  ren.addEventListener("click", (e) => {
    e.stopPropagation();
    void renameFile(node.file);
  });
  const del = rowAction(ICON.trash, `Delete ${node.path}`, true);
  del.addEventListener("click", (e) => {
    e.stopPropagation();
    void deleteFile(node.file.id);
  });
  actions.append(ren, del);
  row.appendChild(actions);
  row.addEventListener("click", () => tabs.openNote(node.file.id));
  return row;
}

// ── sidebar: apps list ───────────────────────────────────────────────────────

function statusClass(app: FleetApp): string {
  if (!app.enabled) return "parked";
  if (app.error) return "error";
  if (app.healthy) return "healthy";
  return "parked";
}

function renderApps() {
  applistEl.replaceChildren();
  if (fleet.length === 0) {
    applistEl.appendChild(el("div", "empty", "No apps"));
    return;
  }
  for (const app of fleet) {
    const row = el("div", "app-row");
    if (tabs.active?.kind === "app" && tabs.active.app === app.name) {
      row.classList.add("active");
    }
    const dot = el("span", `dot ${statusClass(app)}`);
    if (app.error) dot.title = app.error;
    row.appendChild(dot);
    const label = el("span", "label", displayName(app.name));
    label.addEventListener("click", () => tabs.openApp(app.name));
    row.appendChild(label);

    // Management controls (Phase S2c): only registry-managed apps
    // (source === "registry") can be toggled/removed — apps.toml bootstrap
    // apps are host-owned and not in the registry's replicated doc, so the
    // registry actions can't act on them (mirrors the standalone registry UI,
    // which lists only registry-doc apps).
    if (app.source === "registry") {
      const ctls = el("div", "app-ctls");
      const toggle = el("button", "ctl", app.enabled ? "disable" : "enable");
      toggle.title = app.enabled ? `Disable ${displayName(app.name)}` : `Enable ${displayName(app.name)}`;
      toggle.addEventListener("click", (e) => {
        e.stopPropagation();
        void manageApp(() => registry.setEnabled(app.name, !app.enabled));
      });
      ctls.appendChild(toggle);
      const remove = el("button", "ctl danger", "remove");
      remove.title = `Remove ${displayName(app.name)}`;
      remove.addEventListener("click", (e) => {
        e.stopPropagation();
        void (async () => {
          const ok = await confirmAction({
            title: "Remove app",
            message: `Remove "${displayName(app.name)}" from the fleet?`,
            confirmLabel: "Remove",
          });
          if (!ok) return;
          void manageApp(() => registry.removeApp(app.name));
        })();
      });
      ctls.appendChild(remove);
      row.appendChild(ctls);
    }
    applistEl.appendChild(row);
  }
}

// Reflect the indexed agent/skill count in the sidebar "Agents" header badge,
// and mark the header active when the Agents tab is the active one (mirrors the
// vault/app row active styling).
function renderAgentsBadge() {
  const badge = document.getElementById("agents-count-badge");
  if (badge) badge.textContent = String(agentIndex.all.length);
  const head = document.getElementById("agents-head");
  if (head) {
    head.classList.toggle("active", tabs.active?.kind === "agents");
    // Surface the scheduled-invocation count (R1: ```agent blocks across the
    // vault) in the header tooltip — a small, honest read of the invocation
    // index so it stays in lockstep with the component's scheduler view.
    // A scheduled invocation is one whose trigger parses to a RECURRING
    // schedule (interval / daily / weekly / legacy `cron …`). The recurrence
    // picker writes bare triggers (`2h`, `daily at … <tz>`, `weekly on … <tz>`)
    // with no `cron` prefix, so a literal `startsWith("cron")` undercounted —
    // route through the shared parser instead (one-time triggers don't parse).
    const scheduled = invocationIndex.all.filter(
      (inv) => parseSchedule(inv.trigger) !== null,
    ).length;
    head.title =
      scheduled > 0
        ? `Open the Agents view — ${scheduled} scheduled invocation${scheduled === 1 ? "" : "s"}`
        : "Open the Agents view";
  }
}

// Open the create-agent flow OUTSIDE the editor (from the Agents view's
// "+ New agent" button or the quick-open switcher) — the same popup `/agent`
// uses, but with no editor token to swap. On create we open the new definition's
// source note in a tab (the create popup writes it to the vault by `path`, and a
// vault state frame indexes it shortly after — we resolve the freshly-written
// file by path here so the user lands on what they just made). The discoverable
// on-ramp for #9/#10.
function createAgentStandalone() {
  openCreateAgentPopup({
    isNameTaken: (name) => agentIndex.has(name),
    onCreated: (created: CreatedAgent) => {
      // The note may not be in `files` yet (the vault round-trip is async); try
      // to open it by path, and if it isn't indexed yet the agents badge/table
      // will pick it up on the next state frame regardless.
      const file = files.find((f) => normalizePath(f.path) === normalizePath(created.path));
      if (file) tabs.openNote(file.id);
    },
    onClose: () => {},
  });
}

// ── quick-open (Ctrl/Cmd-P) ──────────────────────────────────────────────────

// Build the switcher's item set from the live vault + agent index + fleet. Read
// fresh each time quick-open is invoked (the closures over `files`/`agentIndex`/
// `fleet` always see the current frame), so new notes/agents/apps appear without
// any extra subscription. Folder sentinels (`.keep`) are excluded.
function quickOpenItems(): QuickOpenItem[] {
  const items: QuickOpenItem[] = [];
  for (const f of files) {
    if (f.path.endsWith("/.keep") || f.path === ".keep") continue;
    const name = (f.path.split("/").pop() ?? f.path).replace(/\.md$/i, "");
    items.push({
      id: f.id,
      kind: "note",
      label: name,
      detail: f.path,
      haystack: `${name} ${f.path}`,
    });
  }
  for (const def of agentIndex.all) {
    items.push({
      id: def.name,
      kind: def.kind,
      label: def.name,
      detail: def.path,
      haystack: `${def.name} ${def.path}`,
    });
  }
  for (const app of fleet) {
    items.push({
      id: app.name,
      kind: "app",
      label: displayName(app.name),
      detail: "app",
      haystack: `${app.name} ${displayName(app.name)}`,
    });
  }
  return items;
}

// Dispatch a quick-open pick: notes/apps open in a tab; an agent opens its bound
// run popup (the highest-value action — run it now), falling back to its source
// note if the def somehow can't be located.
function quickOpenPick(item: QuickOpenItem): void {
  if (item.kind === "note") {
    tabs.openNote(item.id);
    return;
  }
  if (item.kind === "app") {
    tabs.openApp(item.id);
    return;
  }
  // agent | skill — open the run popup bound to the def.
  const def = agentIndex.findAgent(item.id);
  if (def) {
    openAgentPopup(def, { onSave: () => {}, onClose: () => {} });
  } else {
    tabs.openAgents();
  }
}

// Global Ctrl/Cmd-P → toggle the quick-open switcher. Capture phase + preventing
// default so it beats the browser's native print dialog. Single-instance: if the
// switcher is already up, the shortcut closes it (handled inside quickOpen's own
// Esc/keydown), so we only open when not already open.
document.addEventListener(
  "keydown",
  (e) => {
    const mod = e.metaKey || e.ctrlKey;
    if (mod && !e.altKey && (e.key === "p" || e.key === "P")) {
      e.preventDefault();
      e.stopPropagation();
      if (isQuickOpenOpen()) return;
      openQuickOpen({ items: quickOpenItems, onPick: quickOpenPick });
    }
  },
  true,
);

// Run a registry mutation, then refresh the fleet so the change reflects in
// the sidebar (and /api/fleet) without waiting for the 5s poll. Errors (most
// commonly a missing/invalid token → 401) surface as a themed toast.
async function manageApp(action: () => Promise<unknown>) {
  try {
    await action();
    // The host converges in a beat; give it a moment, then refresh.
    window.setTimeout(() => void refreshFleet(), 800);
  } catch (e) {
    showError(String(e instanceof Error ? e.message : e));
  }
}

// ── main window: tab strip + content ────────────────────────────────────────

function tabTitle(tab: Tab): string {
  if (tab.kind === "home") return "Tangram";
  if (tab.kind === "agents") return "Agents";
  if (tab.kind === "app") return displayName(tab.app);
  const file = filesById.get(tab.fileId);
  if (!file) return "(Missing)";
  const name = file.path.split("/").pop() ?? file.path;
  return name.replace(/\.md$/i, "");
}

function renderTabs() {
  tabstripEl.replaceChildren();
  tabstripEl.setAttribute("role", "tablist");
  const home = el("button", "tab-home", "⌂");
  home.title = "Home";
  home.setAttribute("role", "tab");
  home.setAttribute("aria-label", "Home");
  home.addEventListener("click", () => tabs.openHome());
  tabstripEl.appendChild(home);
  let activeChip: HTMLElement | null = null;
  for (const tab of tabs.tabs) {
    const chip = el("div", "tab");
    chip.setAttribute("role", "tab");
    chip.setAttribute("aria-selected", String(tab.id === tabs.activeId));
    if (tab.id === tabs.activeId) {
      chip.classList.add("active");
      activeChip = chip;
    }
    chip.appendChild(el("span", "tab-title", tabTitle(tab)));
    const close = el("button", "tab-close", "✕");
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      tabs.close(tab.id);
    });
    chip.appendChild(close);
    chip.addEventListener("click", () => tabs.activate(tab.id));
    // Middle-click (auxclick, button 1) closes the tab — browser-tab idiom.
    chip.addEventListener("auxclick", (e) => {
      if (e.button !== 1) return;
      e.preventDefault();
      e.stopPropagation();
      tabs.close(tab.id);
    });
    // Suppress the default middle-click autoscroll so the close reads cleanly.
    chip.addEventListener("mousedown", (e) => {
      if (e.button === 1) e.preventDefault();
    });

    // Drag-to-reorder (browser-style). HTML5 DnD does not fire a click on
    // drop, so dragging never accidentally activates; a plain click still does.
    chip.draggable = true;
    chip.dataset.tabId = tab.id;
    chip.addEventListener("dragstart", (e) => {
      e.dataTransfer?.setData("text/plain", tab.id);
      if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
      chip.classList.add("dragging");
    });
    chip.addEventListener("dragend", () => {
      chip.classList.remove("dragging");
      tabstripEl
        .querySelectorAll(".tab.drop-before, .tab.drop-after")
        .forEach((n) => n.classList.remove("drop-before", "drop-after"));
    });
    chip.addEventListener("dragover", (e) => {
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
      const rect = chip.getBoundingClientRect();
      const after = e.clientX > rect.left + rect.width / 2;
      chip.classList.toggle("drop-after", after);
      chip.classList.toggle("drop-before", !after);
    });
    chip.addEventListener("dragleave", () => {
      chip.classList.remove("drop-before", "drop-after");
    });
    chip.addEventListener("drop", (e) => {
      e.preventDefault();
      const draggedId = e.dataTransfer?.getData("text/plain");
      chip.classList.remove("drop-before", "drop-after");
      if (!draggedId || draggedId === tab.id) return;
      const rect = chip.getBoundingClientRect();
      const after = e.clientX > rect.left + rect.width / 2;
      const ids = tabs.tabs.map((t) => t.id);
      let targetIndex = ids.indexOf(tab.id);
      if (after) targetIndex += 1;
      // Adjust for removal of the dragged item if it sits before the target.
      const fromIndex = ids.indexOf(draggedId);
      if (fromIndex < targetIndex) targetIndex -= 1;
      tabs.move(draggedId, targetIndex);
    });

    tabstripEl.appendChild(chip);
  }

  // Overflow affordance: when the strip can't show every tab, a trailing "⋯"
  // button opens a menu listing all open tabs (each row activates / closes its
  // tab). Pinned to the strip's right edge so it stays reachable. Only shown
  // when the content actually overflows (measured post-layout).
  const overflowBtn = el("button", "tab-overflow", "⋯");
  overflowBtn.title = "All tabs";
  overflowBtn.setAttribute("aria-label", "All open tabs");
  overflowBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    openTabOverflowMenu(overflowBtn);
  });
  tabstripEl.appendChild(overflowBtn);

  // Defer to next frame so layout is settled before we measure overflow and
  // scroll the active tab into view (it must never sit off-screen, #7a).
  requestAnimationFrame(() => {
    const overflowing = tabstripEl.scrollWidth > tabstripEl.clientWidth + 1;
    overflowBtn.style.display = overflowing ? "" : "none";
    if (activeChip) {
      activeChip.scrollIntoView({ inline: "nearest", block: "nearest" });
    }
  });
}

// A lightweight popup listing every open tab — the overflow fallback when the
// strip scrolls. Each row activates its tab (and closes the menu); a small ✕
// closes the tab in place. Dismisses on outside-click / Esc. Single-instance.
function openTabOverflowMenu(anchor: HTMLElement) {
  document.getElementById("tab-overflow-menu")?.remove();
  const menu = el("div", "tab-overflow-menu");
  menu.id = "tab-overflow-menu";
  const rect = anchor.getBoundingClientRect();
  menu.style.top = `${rect.bottom + 4}px`;
  menu.style.right = `${window.innerWidth - rect.right}px`;

  for (const tab of tabs.tabs) {
    const row = el("div", "tab-overflow-row");
    if (tab.id === tabs.activeId) row.classList.add("active");
    const label = el("span", "tab-overflow-label", tabTitle(tab));
    label.addEventListener("click", () => {
      tabs.activate(tab.id);
      close();
    });
    const x = el("button", "tab-overflow-close", "✕");
    x.title = "Close tab";
    x.addEventListener("click", (e) => {
      e.stopPropagation();
      tabs.close(tab.id);
      // Re-render keeps the menu in sync; rebuild if any tabs remain.
      close();
      if (tabs.tabs.length > 0) openTabOverflowMenu(anchor);
    });
    row.append(label, x);
    menu.appendChild(row);
  }
  if (tabs.tabs.length === 0) {
    menu.appendChild(el("div", "tab-overflow-empty", "No open tabs"));
  }

  function close() {
    menu.remove();
    document.removeEventListener("mousedown", onDoc, true);
    document.removeEventListener("keydown", onKey, true);
  }
  function onDoc(e: MouseEvent) {
    if (!menu.contains(e.target as Node) && e.target !== anchor) close();
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") close();
  }
  document.addEventListener("mousedown", onDoc, true);
  document.addEventListener("keydown", onKey, true);
  document.body.appendChild(menu);
}

function disposeActiveEditor() {
  if (!activeEditor) return;
  if (activeEditor.saveTimer) window.clearTimeout(activeEditor.saveTimer);
  activeEditor.editor.destroy();
  activeEditor = null;
}

function renderContent() {
  const tab = tabs.active;
  // If the active note tab is already mounted with its CodeMirror editor,
  // leave it in place — tearing it down to re-render would drop the cursor
  // and clobber an in-progress edit. SSE updates flow through syncActiveNote.
  if (
    tab?.kind === "note" &&
    activeEditor?.fileId === tab.fileId &&
    contentEl.contains(activeEditor.editor.view.dom)
  ) {
    return;
  }
  disposeActiveEditor();
  contentEl.replaceChildren();
  if (!tab) {
    renderHome();
    return;
  }
  if (tab.kind === "home") {
    renderHome();
    return;
  }
  if (tab.kind === "agents") {
    // The Agents view reads through the live index (rebuilt on each vault
    // state); onVaultState re-renders it so new/edited agents appear without a
    // manual refresh. Row actions open the source note / the bound run popup.
    renderAgentsView(contentEl, agentIndex, {
      openNote: (fileId) => tabs.openNote(fileId),
      fileById: (fileId) => filesById.get(fileId),
      newAgent: () => createAgentStandalone(),
      // Tools/MCP T1: the live grants + fleet names drive the approval UI.
      mcpGrants: () => mcpGrants,
      fleetApps: () => fleet.map((a) => a.name),
      // I3: the invocations table reads through the live replicated index and
      // resolves host-note titles / agent defs from the live vault indexes.
      invocations: () => invocationIndex,
      hostNoteTitle: (fileId) => hostNoteTitle(fileId),
      agentByName: (name) => agentIndex.findAgent(name),
    });
    return;
  }
  if (tab.kind === "app") {
    const frame = document.createElement("iframe");
    frame.className = "app-frame";
    // Relative src: the shell is mounted under /tangram/, so "../<app>/"
    // resolves to the host's per-app surface regardless of mount prefix.
    frame.src = `../${tab.app}/`;
    frame.title = tab.app;
    frame.setAttribute("loading", "lazy");
    contentEl.appendChild(frame);
    return;
  }
  renderNoteTab(tab.fileId);
}

function renderHome() {
  const wrap = el("div", "home");
  wrap.appendChild(el("h1", undefined, "Tangram"));
  wrap.appendChild(
    el("p", "sub", "Manage the set of apps (tangrams) on your device."),
  );
  const stats = el("div", "home-stats");
  const notesCount = files.filter((f) => !f.path.endsWith("/.keep")).length;
  stats.appendChild(stat(`${notesCount}`, "notes"));
  stats.appendChild(stat(`${fleet.length}`, "apps"));
  stats.appendChild(stat(`${fleet.filter((a) => a.healthy).length}`, "healthy"));
  wrap.appendChild(stats);
  wrap.appendChild(
    el(
      "p",
      "hint",
      "Pick a note from the Vault to read or edit it, or an app to embed it in a tab.",
    ),
  );
  contentEl.appendChild(wrap);
}

function stat(value: string, label: string): HTMLElement {
  const s = el("div", "stat");
  s.appendChild(el("div", "stat-value", value));
  s.appendChild(el("div", "stat-label micro", label));
  return s;
}

// The note's title is its first heading line (Obsidian-style). Returns the
// heading text, or null if the first non-empty line isn't an ATX heading.
function firstHeading(body: string): string | null {
  for (const raw of body.split("\n")) {
    const line = raw.trim();
    if (line.length === 0) continue;
    const m = /^#{1,6}\s+(.+?)\s*#*$/.exec(line);
    return m ? (m[1].trim() || null) : null; // first non-empty line decides
  }
  return null;
}

// I3: a display title for a host note (the Agents-tab invocations table's
// "Host note" column). Prefers the note's first heading, falls back to its path
// basename (sans `.md`). Returns null when the file is gone (an orphaned
// `agent://` handle awaiting the component's next prune tick) so the table can
// dim the cell and disable its row click.
function hostNoteTitle(fileId: string): string | null {
  const file = filesById.get(fileId);
  if (!file) return null;
  const heading = firstHeading(file.body);
  if (heading) return heading;
  const base = file.path.split("/").pop() ?? file.path;
  return base.replace(/\.md$/i, "") || file.path;
}

// Convert heading text into a safe filename base (no extension, no slashes,
// no path-hostile/invalid chars). Returns null if nothing usable remains.
function headingToBaseName(heading: string): string | null {
  const base = heading
    .replace(/[/<>:"\\|?*\x00-\x1f]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return base.length ? base : null;
}

// Open the Trigger popup for an inline `agent://<id>` link click. Reads the
// invocation from the live (replicated) index by id; the popup stays on the
// file. Save edits the trigger/prompt (`update_invocation`); Delete removes the
// index entry AND strips the inline link from the open note (so the handle and
// the record go together); "Open in Agents" switches to the Agents tab.
function openAgentLinkTrigger(id: string): void {
  const inv = invocationIndex.byId(id);
  if (!inv) {
    // The link has no backing record (e.g. it was just deleted on another
    // device); send the user to the Agents tab rather than a dead popup.
    tabs.openAgents();
    return;
  }
  openTriggerPopup(inv, {
    onSave: (trigger, prompt) => {
      void vault
        .updateInvocation(id, trigger, prompt)
        .catch((e) => showError(String(e instanceof Error ? e.message : e)));
      activeEditor?.editor.focus();
    },
    onOpenAgents: () => tabs.openAgents(id),
    onDelete: () => {
      stripAgentLink(id);
      void vault
        .deleteInvocation(id)
        .catch((e) => showError(String(e instanceof Error ? e.message : e)));
    },
    onClose: () => activeEditor?.editor.focus(),
  });
}

// Strip the inline `[…](agent://<id>)` link from the active note editor (used by
// the Trigger popup's Delete). Finds the token in the live doc and replaces it
// with its label text (so the sentence reads naturally), then the debounced
// onChange persists the edit and the component reconcile prunes the orphan.
function stripAgentLink(id: string): void {
  const editor = activeEditor?.editor;
  if (!editor) return;
  const doc = editor.doc;
  const token = `](agent://${id})`;
  const close = doc.indexOf(token);
  if (close === -1) return;
  const open = doc.lastIndexOf("[", close);
  if (open === -1) return;
  const label = doc.slice(open + 1, close); // the `⚡ <agent>` label
  const to = close + token.length;
  editor.replaceRange(open, to, label);
}

// A single Obsidian-style "Live Preview" CodeMirror 6 editor (issue #11): the
// editable view *is* the rendered note — markdown syntax is concealed off the
// active line and rendered inline (see editor.ts / livePreview.ts). There is no
// separate preview pane. Edits debounce-write to the model; the MdEditor is
// retained in `activeEditor` so SSE state frames patch it echo-safely (see
// syncActiveNote).
function renderNoteTab(fileId: string) {
  const file = filesById.get(fileId);
  if (!file) {
    contentEl.appendChild(el("div", "empty", "This note no longer exists"));
    return;
  }
  const wrap = el("div", "note");

  const editorHost = el("div", "editor-host");

  const state: ActiveEditor = {
    fileId,
    editor: undefined as unknown as MdEditor,
  };
  const editor = new MdEditor(
    editorHost,
    file.body,
    (doc) => {
      if (state.saveTimer) window.clearTimeout(state.saveTimer);
      state.saveTimer = window.setTimeout(() => {
        editor.markWritten(doc); // expect this body to echo back over SSE
        void vault.writeFile(fileId, doc).catch((e) => console.error(e));
        maybeRenameFromHeading(fileId, doc);
      }, 400);
    },
    // Inline `/` trigger (P1, +P1 fixes): the editor hands us the kind, the
    // word, and the matched token's [from, to). `/agent` opens the CREATE popup;
    // on Create we (Fix 2) swap the `/agent` token for `/<new-name>` and chain
    // straight into the RUN popup bound to the new def. A resolved `/<name>`
    // opens the RUN popup bound to that def; on Save we replace the token with
    // the prompt+response block (the debounced onChange persists it), on
    // Exit/dismiss we just refocus (the `/<name>` reference is left untouched).
    (kind, word, from, to) => {
      // Open the RUN popup for `def`, with its `/<name>` token at [tokFrom,
      // tokTo). Save swaps that token for the completion block (Fix 3 backlink);
      // Exit/dismiss leaves the live `/<name>` reference in place.
      const openRun = (def: AgentDef, tokFrom: number, tokTo: number) => {
        openAgentPopup(def, {
          onSave: (block) => editor.replaceRange(tokFrom, tokTo, block),
          // Schedule submit: mint a UUID, swap the `/<name>` token for the inline
          // `[⚡ <agent>](agent://<id>)` handle, and record the invocation in the
          // replicated index. The scheduler picks it up; the popup did not run.
          onSchedule: (trigger, prompt) => {
            const id = crypto.randomUUID();
            const link = buildAgentLink(def.name, id);
            editor.replaceRange(tokFrom, tokTo, link);
            void vault
              .createInvocation(id, def.name, trigger, prompt, fileId)
              .catch((e) => showError(String(e instanceof Error ? e.message : e)));
          },
          onClose: () => editor.focus(),
        });
      };

      if (kind === "create") {
        openCreateAgentPopup({
          isNameTaken: (name) => agentIndex.has(name),
          // Fix 2: keep the reference inline as `/<new-name>` and chain into its
          // run popup, bound to the just-saved fields (works before the vault
          // round-trip rebuilds the index).
          onCreated: (created: CreatedAgent) => {
            const replacement = `/${created.name}`;
            editor.replaceRange(from, to, replacement);
            const def: AgentDef = {
              kind: created.kind,
              name: created.name,
              model: created.model || DEFAULT_MODEL,
              labels: created.labels,
              meta: {},
              version: null,
              // A freshly-created def requests no MCP servers (Tools/MCP T1); the
              // user adds `mcp_servers:` by editing the frontmatter later.
              mcpServers: [],
              instructions: created.instructions,
              fileId: "",
              path: created.path,
            };
            openRun(def, from, from + replacement.length);
          },
          onClose: () => editor.focus(),
        });
        return;
      }
      const def = agentIndex.findAgent(word);
      if (!def) {
        editor.focus();
        return;
      }
      openRun(def, from, to);
    },
    // The `/<name>` resolver reads through the live index (rebuilt each vault
    // state), so newly-created definitions resolve without re-mounting.
    (word) => agentIndex.findAgent(word) !== null,
    // Auto-open guard (Fix 1): don't re-pop the create popup while either agent
    // popup (create or run) is already up.
    () => isCreateAgentPopupOpen() || isAgentPopupOpen() || isTriggerPopupOpen(),
    // Live candidates for the `/<partial>` autocomplete popup: every indexed
    // agent/skill (kind from its def) plus the reserved `agent` create command.
    // Reads through the live index (rebuilt each vault state) so new defs show
    // up without re-mounting — same pattern as the resolver above.
    () => [
      { name: CREATE_WORD, kind: "create" as const },
      ...agentIndex.all.map((d) => ({ name: d.name, kind: d.kind })),
    ],
    // `[[ ]]` wikilink resolver — reads through the live `linkIndex` (rebuilt
    // each vault state), so links re-resolve as notes are added/renamed without
    // re-mounting the editor (resolution is by stable id, not link text).
    (name) => linkIndex.resolve(name),
    // Click a resolved `[[name]]` → open its target note in a tab.
    (targetId) => tabs.openNote(targetId),
    // Live candidates for the `[[ ]]` wikilink autocomplete popup: the vault
    // notes (folder sentinels excluded). Read through the live `files` array
    // (reassigned each vault state) so newly-created notes appear without
    // re-mounting the editor — same pattern as the slash candidates above.
    () => wikiCandidatesFromFiles(files),
    // The note being edited, so it's excluded from its own link autocomplete.
    // Read live from the index in case the note is renamed while open.
    () => filesById.get(fileId)?.path ?? null,
    // Click an inline `[⚡ <agent>](agent://<id>)` link → open the Trigger popup
    // for that invocation (read from the live index by id). Save edits the
    // trigger/prompt; Delete removes the index entry AND strips the inline link;
    // "Open in Agents" switches to the Agents tab (the table lands later).
    (id) => openAgentLinkTrigger(id),
  );
  state.editor = editor;

  wrap.appendChild(editorHost);
  // Backlinks panel (Connected Vault, G1): the notes that link TO this one,
  // from the persisted reverse map. Built here, then patched in place by
  // refreshBacklinksPanel on each vault state — renderContent reuses the
  // mounted editor (early-returns), so it would otherwise leave this stale.
  wrap.appendChild(renderBacklinksPanel(fileId));
  contentEl.appendChild(wrap);
  activeEditor = state;
}

// Repopulate the active note's Backlinks panel from the live index, in place
// (the note tab's editor is reused across vault states, so the panel must be
// refreshed rather than rebuilt). No-op when no note tab is mounted.
function refreshBacklinksPanel() {
  if (!activeEditor) return;
  const existing = contentEl.querySelector(".note .backlinks");
  if (!existing) return;
  existing.replaceWith(renderBacklinksPanel(activeEditor.fileId));
}

// Render the Backlinks panel for the note `fileId`: a collapsible section below
// the editor listing every note that links to it (source name + the raw `[[ ]]`
// snippet), each row opening that source note. Reads the persisted reverse map
// (`linkIndex.backlinks`) — no on-demand re-scan. Shows an empty state when
// nothing links here yet.
function renderBacklinksPanel(fileId: string): HTMLElement {
  const panel = el("div", "backlinks");
  const head = el("div", "backlinks-head");
  const links = linkIndex.backlinksFor(fileId);
  head.appendChild(el("span", "backlinks-title micro", "Linked mentions"));
  head.appendChild(el("span", "backlinks-count", String(links.length)));
  panel.appendChild(head);

  const list = el("div", "backlinks-list");
  if (links.length === 0) {
    list.appendChild(el("div", "backlinks-empty", "No backlinks yet"));
  } else {
    // One row per backlink occurrence. A note may link here more than once;
    // each occurrence is its own row so the snippet/position is preserved.
    for (const link of links) {
      const source = filesById.get(link.sourceId);
      const name = source
        ? (source.path.split("/").pop() ?? source.path).replace(/\.md$/i, "")
        : "(missing note)";
      const row = el("div", "backlinks-row");
      row.appendChild(el("span", "backlinks-name", name));
      row.appendChild(el("span", "backlinks-snippet", link.original));
      if (source) {
        row.addEventListener("click", () => tabs.openNote(link.sourceId));
      } else {
        row.classList.add("disabled");
      }
      list.appendChild(row);
    }
  }
  panel.appendChild(list);
  return panel;
}

// Apply a fresh vault snapshot to the live note editor, if any. Echo-safe:
// MdEditor.syncRemote only adopts the remote body when it differs from both
// the editor's current text and our last write (so a peer's edit lands but our
// own in-progress typing is never clobbered). The live-preview decorations
// re-render automatically from the editor's own doc/selection changes.
function syncActiveNote() {
  if (!activeEditor) return;
  const file = filesById.get(activeEditor.fileId);
  if (!file) return; // pruneNotes will close a vanished note's tab
  activeEditor.editor.syncRemote(file.body);
}

// Keep the filename in sync with the note's first heading. Renames only when
// the derived base name is valid, differs from the current name, and doesn't
// collide with another file. The parent folder is preserved and the file id is
// stable, so the open tab/editor are not disrupted (the tab title re-derives
// from the new path).
function maybeRenameFromHeading(fileId: string, body: string): void {
  const file = filesById.get(fileId);
  if (!file) return;
  const heading = firstHeading(body);
  if (!heading) return;
  const base = headingToBaseName(heading);
  if (!base) return;
  const segs = file.path.split("/");
  const currentBase = (segs[segs.length - 1] ?? "").replace(/\.md$/i, "");
  if (base === currentBase) return;
  const folder = segs.slice(0, -1).join("/");
  const newPath = folder ? `${folder}/${base}.md` : `${base}.md`;
  const normNew = normalizePath(newPath);
  if (files.some((f) => f.id !== fileId && normalizePath(f.path) === normNew)) return;
  void vault.renameFile(fileId, newPath).catch((e) => console.error(e));
}

// ── vault naming: validation + custom modal ──────────────────────────────────

// Characters the backend would either choke on or that make for hostile paths.
// The model normalizes whitespace and slashes and rejects `.`/`..` segments
// (see `normalize_path` in apps/tangram/src/lib.rs); we mirror those rules in
// the modal so the user is corrected inline rather than after a failed action,
// and additionally forbid the control/wildcard chars that have no place in a
// vault path. `/` is allowed — it nests, and is validated per-segment.
const INVALID_NAME_CHARS = /[<>:"\\|?*\x00-\x1f]/;
const NAME_HINT =
  'Use / to nest. Avoid < > : " \\ | ? *. Enter to confirm, Esc to cancel.';

// Normalize a candidate path the same way the backend does (trim segments,
// drop empties) so duplicate-detection compares apples to apples.
function normalizePath(path: string): string {
  return path
    .split("/")
    .map((s) => s.trim())
    .filter((s) => s.length > 0)
    .join("/");
}

// Validate a candidate vault path against the model's rules + known names.
// `existing` is the set of normalized paths already taken (files, and for
// folders the folder paths); `self` is the path being renamed (allowed to keep
// its own slot). Returns an error string for the hint line, or null if valid.
function validatePath(
  candidate: string,
  existing: Set<string>,
  self?: string,
): string | null {
  const segments = candidate.split("/").map((s) => s.trim());
  if (segments.some((s) => s.length === 0)) {
    return "Name can't have empty path segments";
  }
  if (segments.some((s) => s === "." || s === "..")) {
    return "Name can't contain '.' or '..' segments";
  }
  if (INVALID_NAME_CHARS.test(candidate)) {
    return 'Name can\'t contain < > : " \\ | ? * or control characters';
  }
  const normalized = normalizePath(candidate);
  if (!normalized) return "Name can't be empty";
  if (normalized !== self && existing.has(normalized)) {
    return `"${normalized}" already exists`;
  }
  return null;
}

/** Normalized paths currently taken by files. */
function takenFilePaths(): Set<string> {
  return new Set(files.map((f) => normalizePath(f.path)));
}

/** Normalized folder paths currently in the vault (every ancestor of a file). */
function takenFolderPaths(): Set<string> {
  const set = new Set<string>();
  for (const f of files) {
    const segs = normalizePath(f.path).split("/");
    segs.pop(); // drop the filename
    let acc = "";
    for (const s of segs) {
      acc = acc ? `${acc}/${s}` : s;
      set.add(acc);
    }
  }
  return set;
}

// ── vault operations (custom modal; folder-context preserved) ────────────────

// Create a note inside `folder` (empty string = vault root). We prompt for the
// filename only (via the custom modal) and join it to the target folder, so the
// file always lands in the folder whose "+ note" the user clicked — the path
// context the old root-only button never carried (#14). The user can still type
// a `/` to nest further. The modal validates against the model's path rules and
// existing names so a collision is caught inline, not after a failed action.
async function newNote(folder: string) {
  const where = folder ? `${folder}/` : "vault root";
  const taken = takenFilePaths();
  const name = await promptName({
    title: `New note in ${where}`,
    hint: NAME_HINT,
    value: "untitled.md",
    placeholder: "untitled.md",
    selection: { start: 0, end: "untitled".length },
    validate: (v) => validatePath(folder ? `${folder}/${v}` : v, taken),
  });
  if (!name) return;
  const path = folder ? `${folder}/${name}` : name;
  // Reveal the target folder so a freshly-created note isn't hidden.
  collapsed.delete(folder);
  const title = (name.split("/").pop() ?? name).replace(/\.md$/i, "");
  try {
    const id = await vault.createFile(path, `# ${title}\n\n`);
    tabs.openNote(id);
  } catch (e) {
    showError(String(e));
  }
}

// Create a folder inside `parent` (empty string = vault root). As with notes,
// we prompt for the new folder's name (modal) and join it to the parent so it
// nests where clicked.
async function newFolder(parent: string) {
  const where = parent ? `${parent}/` : "vault root";
  const taken = takenFolderPaths();
  const name = await promptName({
    title: `New folder in ${where}`,
    hint: NAME_HINT,
    placeholder: "folder",
    confirmLabel: "Create folder",
    validate: (v) => validatePath(parent ? `${parent}/${v}` : v, taken),
  });
  if (!name) return;
  const path = parent ? `${parent}/${name}` : name;
  if (parent) collapsed.delete(parent);
  try {
    await vault.createFolder(path);
  } catch (e) {
    showError(String(e));
  }
}

// Rename / move a whole folder (rewrites the prefix of every file under it).
async function renameFolder(path: string) {
  const taken = takenFolderPaths();
  const next = await promptName({
    title: "Rename / move folder",
    hint: NAME_HINT,
    value: path,
    confirmLabel: "Rename",
    selection: { start: 0, end: path.length },
    validate: (v) => validatePath(v, taken, normalizePath(path)),
  });
  if (!next) return;
  const trimmed = normalizePath(next);
  if (trimmed === normalizePath(path)) return;
  try {
    await vault.renameFolder(path, trimmed);
  } catch (e) {
    showError(String(e));
  }
}

async function renameFile(file: MdFile) {
  const taken = takenFilePaths();
  const next = await promptName({
    title: "Rename / move file",
    hint: NAME_HINT,
    value: file.path,
    confirmLabel: "Rename",
    selection: { start: 0, end: file.path.length },
    validate: (v) => validatePath(v, taken, normalizePath(file.path)),
  });
  if (!next || normalizePath(next) === normalizePath(file.path)) return;
  try {
    await vault.renameFile(file.id, next);
  } catch (e) {
    showError(String(e));
  }
}

async function deleteFile(id: string) {
  const file = filesById.get(id);
  const name = file ? (file.path.split("/").pop() ?? file.path).replace(/\.md$/i, "") : "this note";
  const ok = await confirmAction({
    title: "Delete note",
    message: `Permanently delete "${name}"? This cannot be undone.`,
    confirmLabel: "Delete",
  });
  if (!ok) return;
  try {
    await vault.deleteFile(id);
  } catch (e) {
    showError(String(e));
  }
}

async function deleteFolder(path: string) {
  const ok = await confirmAction({
    title: "Delete folder",
    message: `Permanently delete the folder "${path}" and everything in it? This cannot be undone.`,
    confirmLabel: "Delete folder",
  });
  if (!ok) return;
  try {
    await vault.deleteFolder(path);
  } catch (e) {
    showError(String(e));
  }
}

// ── live state plumbing ──────────────────────────────────────────────────────

function setLive(on: boolean) {
  liveDot.classList.toggle("on", on);
  liveLabel.textContent = on ? "Live" : "Offline";
}

function onVaultState(state: VaultState) {
  setLive(true);
  files = state.files ?? [];
  // Tools/MCP T1: the grant records ride on the same state frame (the SSE
  // `state` event serializes the full model); refresh them so the Agents view's
  // grant status reflects an approve/deny/revoke without a manual reload.
  mcpGrants = state.mcp_grants ?? [];
  filesById.clear();
  for (const f of files) filesById.set(f.id, f);
  // Rebuild the agent/skill index so `/<name>` resolution reflects the current
  // vault (newly-created definitions become invocable here). The editor's
  // resolver closes over `agentIndex`, so no editor re-mount is needed.
  agentIndex = buildAgentIndex(files);
  // Rebuild the wikilink/backlink index here too (the single rebuild point).
  // The editor's `[[ ]]` resolver closes over `linkIndex`, and the backlinks
  // panel re-renders from it via renderContent below, so both reflect the new
  // vault without an editor re-mount.
  linkIndex = buildLinkIndex(files);
  // Rebuild the scheduled-invocation index from the REPLICATED records on the
  // state frame (the source of truth — not the markdown). The Trigger popup
  // reads invocations from here by id; the component prunes orphans server-side.
  invocationIndex = buildInvocationIndex(state.invocations ?? []);
  tabs.pruneNotes(new Set(files.map((f) => f.id)));
  renderTree();
  renderAgentsBadge();
  // A header-driven rename re-derives the active tab's title from the new path,
  // so refresh the tab strip too (otherwise it would go stale after a rename).
  renderTabs();
  // Patch the live note editor in place from the new snapshot (echo-safe),
  // then refresh content. renderContent reuses the mounted editor for the
  // active note, so this never clobbers an in-progress edit.
  syncActiveNote();
  renderContent();
  // renderContent reuses the mounted note editor (early return), so the
  // backlinks panel — which lives in that retained DOM — won't be rebuilt;
  // refresh it from the freshly-built link index here.
  refreshBacklinksPanel();
}

async function refreshFleet() {
  try {
    const f = await fetchFleet();
    // Exclude the shell's own entry: it is the outer container, not a
    // selectable app. Without this, opening tangram → clicking tangram in the
    // sidebar would nest the shell inside itself. Filtering at the single
    // fleet-ingest point keeps it out of the APPS list, the tab path, and the
    // home stats alike.
    fleet = (f.apps ?? [])
      .filter((a) => a.name !== SHELL_APP)
      .sort((a, b) => a.name.localeCompare(b.name));
    renderApps();
    // Keep the landing/home stats (APPS / HEALTHY) in step with the fleet:
    // the home tab is otherwise only re-rendered on tab/vault changes, so it
    // would show 0 until one of those fired (the landing-stats bug).
    if (tabs.active?.kind === "home" || tabs.active === null) renderContent();
  } catch (e) {
    console.error("fleet refresh failed", e);
  }
}

// Re-render the chrome whenever tab state changes.
tabs.subscribe(() => {
  renderTabs();
  renderContent();
  renderTree();
  renderApps();
  renderAgentsBadge();
  // Drive the right-sidebar app chat: connect to the active app's MCP server
  // (and reset the chat) when an app tab is active; hide on note/home/agents.
  const a = tabs.active;
  if (a?.kind === "app") setActiveApp(a.app, displayName(a.app));
  else setActiveApp(null, "");
});

// ── boot ─────────────────────────────────────────────────────────────────────

// Start the live shell (vault stream + fleet polling). Called once the host has
// confirmed we are authorized (self-hosted always is; multi-tenant after a
// session cookie is in place).
function startShell() {
  setLive(false);
  tabs.openHome();
  renderTabs();
  renderContent();
  renderTree();
  renderApps();
  renderAgentsBadge();

  subscribeVault(onVaultState);
  void refreshFleet();
  window.setInterval(() => void refreshFleet(), 5000);
}

// Auth gate (auth.md §9 C5). In self-hosted mode there is no auth chrome and the
// shell starts immediately (the loopback-trusted default — unchanged). In
// multi-tenant mode an unauthenticated visitor gets the login view; once a
// session is established the principal chip appears and the shell boots.
void (async () => {
  const auth = await loadAuthState();
  if (auth.mode === "multi-tenant" && !auth.principal) {
    renderLogin(root, () => window.location.reload(), auth.oauth ?? false);
    return;
  }
  if (auth.mode === "multi-tenant" && auth.principal) {
    const slot = document.getElementById("principal-slot");
    if (slot) renderPrincipalChip(slot, auth.principal);
  }
  startShell();
})();
