// The tangram shell: a persistent left sidebar (vault folder tree + live
// apps list) and a main window with an Obsidian-style tab strip. A tab is a
// rendered/edited markdown file, an app embedded as an iframe, or the home
// view. Phase S1; deferred phases are listed in ui/README.md.

import "./styles.css";
import {
  fetchFleet,
  subscribeVault,
  vault,
  type FleetApp,
  type MdFile,
  type VaultState,
} from "./api";
import { renderMarkdown } from "./markdown";
import { TabStore, type Tab } from "./tabs";
import { buildTree, type TreeNode } from "./tree";

// ── shared mutable shell state ──────────────────────────────────────────────

let files: MdFile[] = [];
const filesById = new Map<string, MdFile>();
let fleet: FleetApp[] = [];
const collapsed = new Set<string>(); // collapsed folder paths
const tabs = new TabStore();

// ── DOM scaffold ─────────────────────────────────────────────────────────────

const root = document.getElementById("app")!;
root.innerHTML = `
  <div class="shell">
    <header class="topbar">
      <div class="brand">tangram</div>
      <div class="status"><span class="dot" id="live-dot"></span><span id="live-label">connecting…</span></div>
    </header>
    <div class="body">
      <aside class="sidebar">
        <section class="side-section">
          <div class="side-head">
            <span class="micro">Vault</span>
            <div class="side-actions">
              <button class="ghost" id="new-note" title="New note">+ note</button>
              <button class="ghost" id="new-folder" title="New folder">+ folder</button>
            </div>
          </div>
          <div class="tree" id="tree"></div>
        </section>
        <section class="side-section">
          <div class="side-head"><span class="micro">Apps</span></div>
          <div class="applist" id="applist"></div>
        </section>
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
    treeEl.appendChild(el("div", "empty", "no notes yet"));
    return;
  }
  for (const node of nodes) treeEl.appendChild(renderNode(node, 0));
}

function renderNode(node: TreeNode, depth: number): HTMLElement {
  if (node.kind === "folder") {
    const wrap = el("div", "tree-folder");
    const row = el("div", "tree-row folder-row");
    row.style.paddingLeft = `${depth * 14 + 8}px`;
    const isCollapsed = collapsed.has(node.path);
    row.appendChild(el("span", "twisty", isCollapsed ? "▸" : "▾"));
    row.appendChild(el("span", "label", node.name));
    const del = el("button", "row-del", "✕");
    del.title = `Delete folder ${node.path}`;
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      void deleteFolder(node.path);
    });
    row.appendChild(del);
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
  row.style.paddingLeft = `${depth * 14 + 8}px`;
  if (tabs.active?.kind === "note" && tabs.active.fileId === node.file.id) {
    row.classList.add("active");
  }
  row.appendChild(el("span", "label", node.name));
  const del = el("button", "row-del", "✕");
  del.title = `Delete ${node.path}`;
  del.addEventListener("click", (e) => {
    e.stopPropagation();
    void deleteFile(node.file.id);
  });
  row.appendChild(del);
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
    applistEl.appendChild(el("div", "empty", "no apps"));
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
    row.appendChild(el("span", "label", app.name));
    if (app.name === "tangram") row.appendChild(el("span", "tag", "shell"));
    row.addEventListener("click", () => tabs.openApp(app.name));
    applistEl.appendChild(row);
  }
}

// ── main window: tab strip + content ────────────────────────────────────────

function tabTitle(tab: Tab): string {
  if (tab.kind === "home") return "tangram";
  if (tab.kind === "app") return tab.app;
  const file = filesById.get(tab.fileId);
  if (!file) return "(missing)";
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
    tabstripEl.appendChild(chip);
  }
}

function renderContent() {
  const tab = tabs.active;
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
  wrap.appendChild(el("h1", undefined, "tangram"));
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

// A simple textarea editor + rendered preview (CodeMirror live-preview is a
// deferred phase, see ui/README.md). Edits debounce-write to the model; the
// preview re-renders from the textarea live.
function renderNoteTab(fileId: string) {
  const file = filesById.get(fileId);
  if (!file) {
    contentEl.appendChild(el("div", "empty", "this note no longer exists"));
    return;
  }
  const wrap = el("div", "note");
  const bar = el("div", "note-bar");
  bar.appendChild(el("span", "note-path", file.path));
  const renameBtn = el("button", "ghost", "rename");
  renameBtn.addEventListener("click", () => void renameFile(file));
  bar.appendChild(renameBtn);
  wrap.appendChild(bar);

  const split = el("div", "note-split");
  const editor = document.createElement("textarea");
  editor.className = "editor";
  editor.value = file.body;
  editor.spellcheck = false;
  const preview = el("div", "preview markdown");
  preview.innerHTML = renderMarkdown(file.body);

  let timer: number | undefined;
  editor.addEventListener("input", () => {
    preview.innerHTML = renderMarkdown(editor.value);
    if (timer) window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      void vault.writeFile(fileId, editor.value).catch((e) => console.error(e));
    }, 400);
  });

  split.appendChild(editor);
  split.appendChild(preview);
  wrap.appendChild(split);
  contentEl.appendChild(wrap);
}

// ── vault operations (with light prompts; richer UX is a later phase) ────────

async function newNote(folder: string) {
  const suggestion = folder ? `${folder}/untitled.md` : "untitled.md";
  const path = window.prompt("New note path", suggestion);
  if (!path) return;
  try {
    const id = await vault.createFile(path, `# ${path.split("/").pop()}\n\n`);
    tabs.openNote(id);
  } catch (e) {
    window.alert(String(e));
  }
}

async function newFolder(parent: string) {
  const suggestion = parent ? `${parent}/folder` : "folder";
  const path = window.prompt("New folder path", suggestion);
  if (!path) return;
  try {
    await vault.createFolder(path);
  } catch (e) {
    window.alert(String(e));
  }
}

async function renameFile(file: MdFile) {
  const next = window.prompt("Rename / move file", file.path);
  if (!next || next === file.path) return;
  try {
    await vault.renameFile(file.id, next);
  } catch (e) {
    window.alert(String(e));
  }
}

async function deleteFile(id: string) {
  const file = filesById.get(id);
  if (file && !window.confirm(`Delete ${file.path}?`)) return;
  try {
    await vault.deleteFile(id);
  } catch (e) {
    window.alert(String(e));
  }
}

async function deleteFolder(path: string) {
  if (!window.confirm(`Delete folder ${path} and everything in it?`)) return;
  try {
    await vault.deleteFolder(path);
  } catch (e) {
    window.alert(String(e));
  }
}

// ── live state plumbing ──────────────────────────────────────────────────────

function setLive(on: boolean) {
  liveDot.classList.toggle("on", on);
  liveLabel.textContent = on ? "live" : "offline";
}

function onVaultState(state: VaultState) {
  setLive(true);
  files = state.files ?? [];
  filesById.clear();
  for (const f of files) filesById.set(f.id, f);
  tabs.pruneNotes(new Set(files.map((f) => f.id)));
  renderTree();
  // active note tab may need its editor synced if the body changed remotely;
  // re-render content keeps it correct (last-writer-wins on the model).
  renderContent();
}

async function refreshFleet() {
  try {
    const f = await fetchFleet();
    fleet = (f.apps ?? []).sort((a, b) => a.name.localeCompare(b.name));
    renderApps();
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

setLive(false);
tabs.openHome();
renderTabs();
renderContent();
renderTree();
renderApps();

subscribeVault(onVaultState);
void refreshFleet();
window.setInterval(() => void refreshFleet(), 5000);
