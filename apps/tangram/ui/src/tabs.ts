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

  // Tabs are single-instance per target: opening a target that already has a
  // tab focuses the existing one instead of pushing a duplicate (issue #13 —
  // clicking an app a second time must switch to its open tab, not stack a new
  // one). `match` recognizes an existing tab for the requested target; `create`
  // builds a fresh tab only when none is open. Either path leaves it active.
  private openOrFocus<T extends Tab>(
    match: (t: Tab) => boolean,
    create: () => T,
  ): void {
    const existing = this.tabs.find(match);
    if (existing) {
      this.activate(existing.id);
      return;
    }
    const tab = create();
    this.tabs.push(tab);
    this.activate(tab.id);
  }

  /** Open (or focus, if already open) the home tab. */
  openHome() {
    this.openOrFocus(
      (t) => t.kind === "home",
      (): HomeTab => ({ kind: "home", id: nextId() }),
    );
  }

  /** Open (or focus) a note tab for a vault file id, keyed by that file id. */
  openNote(fileId: string) {
    this.openOrFocus(
      (t) => t.kind === "note" && t.fileId === fileId,
      (): NoteTab => ({ kind: "note", id: nextId(), fileId }),
    );
  }

  /** Open (or focus) an app tab, keyed by the stable app name. */
  openApp(app: string) {
    this.openOrFocus(
      (t) => t.kind === "app" && t.app === app,
      (): AppTab => ({ kind: "app", id: nextId(), app }),
    );
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
