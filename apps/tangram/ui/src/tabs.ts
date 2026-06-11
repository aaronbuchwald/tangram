// Shell tab state. A tab is either a note (a vault `.md` file, keyed by its
// stable id) or an app (an iframe to `/<app>/`). Tab layout is shell state;
// per Decision D it stays LOCAL (not replicated) — markdown files replicate,
// workspace layout does not.

export interface NoteTab {
  kind: "note";
  id: string; // tab id
  fileId: string; // the MdFile id this tab renders/edits
}

export interface AppTab {
  kind: "app";
  id: string; // tab id
  app: string; // app name, embedded as <iframe src="/<app>/">
}

export interface HomeTab {
  kind: "home";
  id: string;
}

export type Tab = NoteTab | AppTab | HomeTab;

let counter = 0;
const nextId = () => `tab-${++counter}`;

export class TabStore {
  tabs: Tab[] = [];
  activeId: string | null = null;
  private listeners = new Set<() => void>();

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private emit() {
    for (const fn of this.listeners) fn();
  }

  get active(): Tab | null {
    return this.tabs.find((t) => t.id === this.activeId) ?? null;
  }

  activate(id: string) {
    this.activeId = id;
    this.emit();
  }

  /** Open (or focus, if already open) the home tab. */
  openHome() {
    const existing = this.tabs.find((t) => t.kind === "home");
    if (existing) {
      this.activate(existing.id);
      return;
    }
    const tab: HomeTab = { kind: "home", id: nextId() };
    this.tabs.push(tab);
    this.activate(tab.id);
  }

  /** Open (or focus) a note tab for a vault file id. */
  openNote(fileId: string) {
    const existing = this.tabs.find(
      (t): t is NoteTab => t.kind === "note" && t.fileId === fileId,
    );
    if (existing) {
      this.activate(existing.id);
      return;
    }
    const tab: NoteTab = { kind: "note", id: nextId(), fileId };
    this.tabs.push(tab);
    this.activate(tab.id);
  }

  /** Open (or focus) an app tab. */
  openApp(app: string) {
    const existing = this.tabs.find(
      (t): t is AppTab => t.kind === "app" && t.app === app,
    );
    if (existing) {
      this.activate(existing.id);
      return;
    }
    const tab: AppTab = { kind: "app", id: nextId(), app };
    this.tabs.push(tab);
    this.activate(tab.id);
  }

  close(id: string) {
    const idx = this.tabs.findIndex((t) => t.id === id);
    if (idx < 0) return;
    this.tabs.splice(idx, 1);
    if (this.activeId === id) {
      const fallback = this.tabs[idx] ?? this.tabs[idx - 1] ?? null;
      this.activeId = fallback ? fallback.id : null;
    }
    this.emit();
  }

  /** A note tab whose file disappeared from the vault must be closed. */
  pruneNotes(liveFileIds: Set<string>) {
    const stale = this.tabs.filter(
      (t) => t.kind === "note" && !liveFileIds.has((t as NoteTab).fileId),
    );
    for (const t of stale) this.close(t.id);
  }
}
