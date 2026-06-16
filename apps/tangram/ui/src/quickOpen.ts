// Ctrl/Cmd-P quick-open (#6 — the headline keyboard affordance). A single-
// instance fuzzy switcher over the vault notes + indexed agents (+ apps),
// rendered with the shared modal overlay/card language (modal.ts / styles.css).
//
// Behaviour:
//   - type to filter — case-insensitive subsequence match over the item's
//     "haystack" (note path / agent name+path / app name), ranked so tighter,
//     earlier, more-contiguous matches float to the top;
//   - ↑/↓ (and Tab/Shift-Tab) move the active row, Enter opens it, Esc or a
//     backdrop click dismisses;
//   - clicking a row opens it; hovering a row makes it active.
//
// The list is provided by the caller (so this module stays vault/agent-agnostic
// and testable); `onPick` dispatches the open (note → tab, agent → run/open,
// app → tab). The fuzzy scorer (`fuzzyScore` / `filterItems`) is a pure function
// exported for a focused unit test.

/** One searchable target in the switcher. */
export interface QuickOpenItem {
  /** Stable id, opaque to this module (e.g. a file id, agent name, app name). */
  id: string;
  /** What kind of thing this is — drives the kind chip + the caller's dispatch. */
  kind: "note" | "agent" | "skill" | "app";
  /** The primary label shown in the row (e.g. the note/agent/app name). */
  label: string;
  /** A dim secondary line (e.g. the vault path) — optional. */
  detail?: string;
  /** The text the query is matched against (label + any extra context). */
  haystack: string;
}

export interface QuickOpenCallbacks {
  /** The live item set, read fresh each time the switcher opens (so newly
   *  created notes/agents are present without any subscription bookkeeping). */
  items: () => QuickOpenItem[];
  /** Open the chosen item. The switcher closes first, then calls this. */
  onPick: (item: QuickOpenItem) => void;
}

/** Score `item` against the lower-cased `query`, or null if it doesn't match.
 *  Lower is better. The match is a case-insensitive SUBSEQUENCE of the haystack;
 *  the score rewards an early first hit, contiguity (adjacent matched chars),
 *  and a shorter haystack so a tight name beats a long path that merely
 *  contains the letters. An empty query matches everything with a constant
 *  neutral score, so the caller's stable sort preserves the natural input
 *  order. */
export function fuzzyScore(haystack: string, query: string): number | null {
  if (query.length === 0) return 0;
  const h = haystack.toLowerCase();
  const q = query.toLowerCase();
  let hi = 0;
  let firstHit = -1;
  let gaps = 0;
  let prevHit = -1;
  for (let qi = 0; qi < q.length; qi++) {
    const ch = q[qi];
    let found = -1;
    for (; hi < h.length; hi++) {
      if (h[hi] === ch) {
        found = hi;
        hi++;
        break;
      }
    }
    if (found === -1) return null; // ran out of haystack → not a subsequence
    if (firstHit === -1) firstHit = found;
    if (prevHit !== -1 && found !== prevHit + 1) gaps++;
    prevHit = found;
  }
  // Weighted: earlier first hit + fewer gaps + shorter haystack = lower score.
  return firstHit * 2 + gaps * 4 + h.length * 0.1;
}

/** Filter + rank `items` by `query` (empty query → all, in input order). Stable:
 *  equal scores keep their original relative order. */
export function filterItems(
  items: QuickOpenItem[],
  query: string,
): QuickOpenItem[] {
  const scored: Array<{ item: QuickOpenItem; score: number; idx: number }> = [];
  items.forEach((item, idx) => {
    const score = fuzzyScore(item.haystack, query);
    if (score !== null) scored.push({ item, score, idx });
  });
  scored.sort((a, b) => a.score - b.score || a.idx - b.idx);
  return scored.map((s) => s.item);
}

const KIND_LABEL: Record<QuickOpenItem["kind"], string> = {
  note: "note",
  agent: "agent",
  skill: "skill",
  app: "app",
};

// Single-instance: re-invoking while open closes the existing switcher first
// (and toggling with the same shortcut is handled by the caller via isOpen).
let current: { close: () => void } | null = null;

/** True while the quick-open switcher is up. */
export function isQuickOpenOpen(): boolean {
  return current !== null;
}

/** Open the quick-open switcher (single-instance). */
export function openQuickOpen(cb: QuickOpenCallbacks): void {
  current?.close();

  const all = cb.items();

  const overlay = document.createElement("div");
  overlay.className = "modal-overlay quick-open-overlay";

  const dialog = document.createElement("div");
  dialog.className = "modal quick-open";
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-label", "Quick open");
  overlay.appendChild(dialog);

  const input = document.createElement("input");
  input.className = "modal-input quick-open-input";
  input.type = "text";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.placeholder = "Search notes, agents, apps…";
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-expanded", "true");
  input.setAttribute("aria-controls", "quick-open-list");
  dialog.appendChild(input);

  const list = document.createElement("div");
  list.className = "quick-open-list";
  list.id = "quick-open-list";
  list.setAttribute("role", "listbox");
  dialog.appendChild(list);

  let results: QuickOpenItem[] = [];
  let active = 0;
  let settled = false;

  function close() {
    if (settled) return;
    settled = true;
    document.removeEventListener("keydown", onKey, true);
    overlay.remove();
    if (current?.close === close) current = null;
  }
  current = { close };

  function pick(item: QuickOpenItem) {
    close();
    cb.onPick(item);
  }

  function paint() {
    results = filterItems(all, input.value.trim());
    if (active >= results.length) active = Math.max(0, results.length - 1);
    list.replaceChildren();
    if (results.length === 0) {
      const empty = document.createElement("div");
      empty.className = "quick-open-empty";
      empty.textContent = "No matches";
      list.appendChild(empty);
      return;
    }
    results.forEach((item, i) => {
      const row = document.createElement("div");
      row.className = "quick-open-row" + (i === active ? " active" : "");
      row.setAttribute("role", "option");
      row.setAttribute("aria-selected", String(i === active));

      const chip = document.createElement("span");
      chip.className = `quick-open-kind quick-open-kind-${item.kind}`;
      chip.textContent = KIND_LABEL[item.kind];
      row.appendChild(chip);

      const labelWrap = document.createElement("span");
      labelWrap.className = "quick-open-label";
      labelWrap.textContent = item.label;
      row.appendChild(labelWrap);

      if (item.detail) {
        const detail = document.createElement("span");
        detail.className = "quick-open-detail";
        detail.textContent = item.detail;
        row.appendChild(detail);
      }

      row.addEventListener("mousemove", () => {
        if (active === i) return;
        active = i;
        repaintActive();
      });
      row.addEventListener("click", () => pick(item));
      list.appendChild(row);
    });
  }

  // Cheap active-row repaint (hover/arrow): just toggle classes, no rebuild.
  function repaintActive() {
    const rows = list.querySelectorAll<HTMLElement>(".quick-open-row");
    rows.forEach((row, i) => {
      const on = i === active;
      row.classList.toggle("active", on);
      row.setAttribute("aria-selected", String(on));
      if (on) row.scrollIntoView({ block: "nearest" });
    });
  }

  function move(delta: number) {
    if (results.length === 0) return;
    active = (active + delta + results.length) % results.length;
    repaintActive();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
    } else if (e.key === "ArrowDown" || (e.key === "Tab" && !e.shiftKey)) {
      e.preventDefault();
      e.stopPropagation();
      move(1);
    } else if (e.key === "ArrowUp" || (e.key === "Tab" && e.shiftKey)) {
      e.preventDefault();
      e.stopPropagation();
      move(-1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      e.stopPropagation();
      const item = results[active];
      if (item) pick(item);
    }
  }

  input.addEventListener("input", () => paint());
  overlay.addEventListener("mousedown", (e) => {
    if (e.target === overlay) close();
  });
  document.addEventListener("keydown", onKey, true);

  document.body.appendChild(overlay);
  paint();
  input.focus();
}
