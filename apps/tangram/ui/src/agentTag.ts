// The CM6 editor wiring for the inline "@agent" LLM trigger (demo).
//
//  1. agentTagHighlight — a ViewPlugin (à la livePreview.ts) that marks every
//     literal `@agent` occurrence with a `.cm-agent-tag` chip (the blue accent).
//  2. agentTrigger(onTrigger) — a high-precedence Enter keymap that, when the
//     caret sits immediately after a `@agent` token, SUPPRESSES the newline and
//     fires onTrigger(from, to) for that token; everywhere else Enter is normal.
//  3. agentClickToReopen(onTrigger) — a mousedown DOM handler that maps a click
//     onto a `@agent` token (if any) and fires onTrigger(from, to) so a
//     highlighted token re-opens the popup after a dismiss.
//
// `onTrigger(from, to)` receives the document range of the matched `@agent` so
// the caller can replace exactly that token on Save / keep it on Exit.

import { Prec, RangeSetBuilder } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  type KeyBinding,
  ViewPlugin,
  type ViewUpdate,
  keymap,
} from "@codemirror/view";

export const AGENT_TAG = "@agent";

/** Fired with the document range [from, to) of a matched `@agent` token. */
export type AgentTriggerHandler = (from: number, to: number) => void;

const agentMark = Decoration.mark({ class: "cm-agent-tag" });

// Mark every literal `@agent` in the document. A small ViewPlugin like
// livePreview.ts: scan the visible ranges' text and emit a mark decoration per
// occurrence. Rebuilds on doc/viewport change.
function buildAgentDecorations(view: EditorView): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  for (const { from, to } of view.visibleRanges) {
    const text = view.state.sliceDoc(from, to);
    let idx = text.indexOf(AGENT_TAG);
    while (idx !== -1) {
      const start = from + idx;
      builder.add(start, start + AGENT_TAG.length, agentMark);
      idx = text.indexOf(AGENT_TAG, idx + AGENT_TAG.length);
    }
  }
  return builder.finish();
}

export const agentTagHighlight = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = buildAgentDecorations(view);
    }
    update(update: ViewUpdate) {
      if (update.docChanged || update.viewportChanged) {
        this.decorations = buildAgentDecorations(update.view);
      }
    }
  },
  { decorations: (v) => v.decorations },
);

/** If `pos` falls on (or right after) an `@agent` token, return its range. */
export function agentTagAt(
  doc: string,
  pos: number,
): { from: number; to: number } | null {
  // Find the nearest `@agent` whose span covers pos, or that ends exactly at
  // pos (caret right after it — the Enter-trigger case).
  let search = 0;
  for (;;) {
    const start = doc.indexOf(AGENT_TAG, search);
    if (start === -1) return null;
    const end = start + AGENT_TAG.length;
    if (pos >= start && pos <= end) return { from: start, to: end };
    search = end;
  }
}

/**
 * High-precedence Enter handler. Returns true (handled, newline suppressed)
 * only when the primary caret is collapsed immediately after `@agent`; fires
 * onTrigger with that token's range. Returns false everywhere else so normal
 * Enter is unaffected.
 */
export function agentTrigger(onTrigger: AgentTriggerHandler) {
  const binding: KeyBinding = {
    key: "Enter",
    run: (view) => {
      const sel = view.state.selection.main;
      if (!sel.empty) return false;
      const cursor = sel.head;
      const text = view.state.sliceDoc(0, cursor);
      if (!text.endsWith(AGENT_TAG)) return false;
      const from = cursor - AGENT_TAG.length;
      onTrigger(from, cursor);
      return true; // suppress the newline
    },
  };
  // Above the default keymap (which also binds Enter) so this wins.
  return Prec.highest(keymap.of([binding]));
}

/**
 * mousedown handler: clicking a highlighted `@agent` token re-opens the popup.
 * Maps the click coordinates onto a document position and, if it lands on an
 * `@agent`, fires onTrigger for that token (and prevents the default so the
 * click doesn't just move the caret into the token).
 */
export function agentClickToReopen(onTrigger: AgentTriggerHandler) {
  return EditorView.domEventHandlers({
    mousedown: (event, view) => {
      const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
      if (pos == null) return false;
      const range = agentTagAt(view.state.doc.toString(), pos);
      if (!range) return false;
      // Only treat a click that actually lands inside the token as a reopen.
      if (pos < range.from || pos > range.to) return false;
      event.preventDefault();
      onTrigger(range.from, range.to);
      return true;
    },
  });
}
