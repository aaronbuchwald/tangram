// Shell tab state. A tab is either a note (a vault `.md` file, keyed by its
// stable id) or an app (an iframe to `/<app>/`). Tab layout is shell state;
// per Decision D it stays LOCAL (not replicated) — markdown files replicate,
// workspace layout does not.

import { focusInvocationRow } from "./agentsView";

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

// The Agents view (P2): a single-instance tab rendering the sortable/filterable
// table of every indexed agent/skill. Like home, it carries no target id.
export interface AgentsTab {
  kind: "agents";
  id: string;
}

export type Tab = NoteTab | AppTab | HomeTab | AgentsTab;

let counter = 0;
const nextId = () => `tab-${++counter}`;

export class TabStore {
  tabs: Tab[] = [];
  activeId: string | null = null;
  private listeners = new Set<() => void>();
  private closeListeners = new Set<(tab: Tab) => void>();

  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  /** Notified with the closed tab whenever any close path removes it (the chip
   *  ✕, middle-click, the overflow menu, or pruneNotes). The chat panel uses
   *  this to wipe a closed tab's persisted conversation (#35). */
  subscribeClose(fn: (tab: Tab) => void): () => void {
    this.closeListeners.add(fn);
    return () => this.closeListeners.delete(fn);
  }

  private emit() {
    for (const fn of this.listeners) fn();
  }

  private emitClose(tab: Tab) {
    for (const fn of this.closeListeners) fn(tab);
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

  /** Open (or focus, if already open) the Agents view tab. When `focusInvocationId`
   *  is given (the Trigger-popup "Open in Agents" deep-link), the Invocations
   *  table scrolls to + briefly highlights that row on its next render (I3). */
  openAgents(focusInvocationId?: string) {
    if (focusInvocationId !== undefined) focusInvocationRow(focusInvocationId);
    this.openOrFocus(
      (t) => t.kind === "agents",
      (): AgentsTab => ({ kind: "agents", id: nextId() }),
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

  /** Move the tab with `id` to position `toIndex` (clamped). Reorder only — the
   *  active tab is unchanged. No-op if the id is unknown or already in place. */
  move(id: string, toIndex: number): void {
    const from = this.tabs.findIndex((t) => t.id === id);
    if (from < 0) return;
    const clamped = Math.max(0, Math.min(toIndex, this.tabs.length - 1));
    if (from === clamped) return;
    const [moved] = this.tabs.splice(from, 1);
    this.tabs.splice(clamped, 0, moved);
    this.emit();
  }

  close(id: string) {
    const idx = this.tabs.findIndex((t) => t.id === id);
    if (idx < 0) return;
    const [closed] = this.tabs.splice(idx, 1);
    if (this.activeId === id) {
      const fallback = this.tabs[idx] ?? this.tabs[idx - 1] ?? null;
      this.activeId = fallback ? fallback.id : null;
    }
    this.emitClose(closed);
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
