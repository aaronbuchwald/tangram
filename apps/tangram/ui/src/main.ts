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
  type MdFile,
  type VaultState,
} from "./api";
import { loadAuthState, renderLogin, renderPrincipalChip } from "./auth";
import { MdEditor } from "./editor";
import { registry } from "./manage";
import { confirmAction, promptName } from "./modal";
import { TabStore, type Tab } from "./tabs";
import { buildTree, type TreeNode } from "./tree";

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
const collapsedSections = new Set<string>(["vault", "apps"]); // both collapsed by default
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
        <div class="sidebar-resizer" id="sidebar-resizer"></div>
      </aside>
      <main class="main">
        <div class="tabstrip" id="tabstrip"></div>
        <div class="content" id="content"></div>
      </main>
    </div>
  </div>
`;

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
  applySectionState();
});
document.getElementById("apps-head")!.addEventListener("click", (e) => {
  if ((e.target as HTMLElement).closest(".ghost")) return;
  if (collapsedSections.has("apps")) collapsedSections.delete("apps");
  else collapsedSections.add("apps");
  applySectionState();
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
  const nodes = buildTree(files);
  if (nodes.length === 0) {
    treeEl.appendChild(el("div", "empty", "No notes yet"));
    return;
  }
  for (const node of nodes) treeEl.appendChild(renderNode(node, 0));
}

function renderNode(node: TreeNode, depth: number): HTMLElement {
  if (node.kind === "folder") {
    const wrap = el("div", "tree-folder");
    const row = el("div", "tree-row folder-row");
    row.style.paddingLeft = `${depth * 20 + 8}px`;
    const isCollapsed = collapsed.has(node.path);
    row.appendChild(el("span", "twisty", isCollapsed ? "▸" : "▾"));
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

// Run a registry mutation, then refresh the fleet so the change reflects in
// the sidebar (and /api/fleet) without waiting for the 5s poll. Errors (most
// commonly a missing/invalid token → 401) surface as an alert.
async function manageApp(action: () => Promise<unknown>) {
  try {
    await action();
    // The host converges in a beat; give it a moment, then refresh.
    window.setTimeout(() => void refreshFleet(), 800);
  } catch (e) {
    window.alert(String(e instanceof Error ? e.message : e));
  }
}

// ── main window: tab strip + content ────────────────────────────────────────

function tabTitle(tab: Tab): string {
  if (tab.kind === "home") return "Tangram";
  if (tab.kind === "app") return displayName(tab.app);
  const file = filesById.get(tab.fileId);
  if (!file) return "(Missing)";
  const name = file.path.split("/").pop() ?? file.path;
  return name.replace(/\.md$/i, "");
}

function renderTabs() {
  tabstripEl.replaceChildren();
  const home = el("button", "tab-home", "⌂");
  home.title = "Home";
  home.addEventListener("click", () => tabs.openHome());
  tabstripEl.appendChild(home);
  for (const tab of tabs.tabs) {
    const chip = el("div", "tab");
    if (tab.id === tabs.activeId) chip.classList.add("active");
    chip.appendChild(el("span", "tab-title", tabTitle(tab)));
    const close = el("button", "tab-close", "✕");
    close.addEventListener("click", (e) => {
      e.stopPropagation();
      tabs.close(tab.id);
    });
    chip.appendChild(close);
    chip.addEventListener("click", () => tabs.activate(tab.id));

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

// Convert heading text into a safe filename base (no extension, no slashes,
// no path-hostile/invalid chars). Returns null if nothing usable remains.
function headingToBaseName(heading: string): string | null {
  const base = heading
    .replace(/[/<>:"\\|?*\x00-\x1f]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return base.length ? base : null;
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
  const editor = new MdEditor(editorHost, file.body, (doc) => {
    if (state.saveTimer) window.clearTimeout(state.saveTimer);
    state.saveTimer = window.setTimeout(() => {
      editor.markWritten(doc); // expect this body to echo back over SSE
      void vault.writeFile(fileId, doc).catch((e) => console.error(e));
      maybeRenameFromHeading(fileId, doc);
    }, 400);
  });
  state.editor = editor;

  wrap.appendChild(editorHost);
  contentEl.appendChild(wrap);
  activeEditor = state;
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
    window.alert(String(e));
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
    window.alert(String(e));
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
    window.alert(String(e));
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
    window.alert(String(e));
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
    window.alert(String(e));
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
    window.alert(String(e));
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
  filesById.clear();
  for (const f of files) filesById.set(f.id, f);
  tabs.pruneNotes(new Set(files.map((f) => f.id)));
  renderTree();
  // A header-driven rename re-derives the active tab's title from the new path,
  // so refresh the tab strip too (otherwise it would go stale after a rename).
  renderTabs();
  // Patch the live note editor in place from the new snapshot (echo-safe),
  // then refresh content. renderContent reuses the mounted editor for the
  // active note, so this never clobbers an in-progress edit.
  syncActiveNote();
  renderContent();
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
