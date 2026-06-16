// CM6 editor wiring for inline `[<label>](agent://<id>)` agent links (the
// scheduled-invocation handle). A direct clone of wikiLink.ts:
//
//  1. agentLinkHighlight() — a ViewPlugin that regex-scans the visible ranges
//     for `[<label>](agent://<id>)` links and marks each with `.cm-agent-link`
//     (a DARK-BLUE link, distinct from `.cm-wikilink`'s accent blue).
//  2. agentLinkClick(open) — a mousedown handler: clicking an agent link opens
//     the Trigger popup for its invocation id. Uses the same non-clamping,
//     EOF-safe hit-test as wikiLinkClick (posOnToken / clickWithinRange) so a
//     click to the right of / below a trailing link places the caret past it
//     rather than opening the popup.
//
// The link is only a handle: `<id>` keys the replicated `invocations` index that
// owns the trigger/prompt/last-run. We never rewrite the link text.

import { RangeSetBuilder } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  ViewPlugin,
  type ViewUpdate,
} from "@codemirror/view";
import { clickWithinRange, posOnToken } from "./wikiLink";
import { agentLinkAt, parseAgentLinks } from "./invocations";

/** Opens the Trigger popup for the invocation with the given id (a click). */
export type AgentLinkOpener = (id: string) => void;

const agentLinkMark = Decoration.mark({ class: "cm-agent-link" });

/** Mark every `[<label>](agent://<id>)` in the visible ranges. */
function buildAgentLinkDecorations(view: EditorView): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  for (const { from, to } of view.visibleRanges) {
    const text = view.state.sliceDoc(from, to);
    for (const link of parseAgentLinks(text)) {
      builder.add(from + link.from, from + link.to, agentLinkMark);
    }
  }
  return builder.finish();
}

/** The highlight ViewPlugin; rebuilds on every doc/viewport change. */
export function agentLinkHighlight() {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = buildAgentLinkDecorations(view);
      }
      update(update: ViewUpdate) {
        if (update.docChanged || update.viewportChanged) {
          this.decorations = buildAgentLinkDecorations(update.view);
        }
      }
    },
    { decorations: (v) => v.decorations },
  );
}

/**
 * mousedown handler: clicking an `agent://` link opens the Trigger popup for its
 * invocation id. Same non-clamping, EOF-safe hit-test as `wikiLinkClick` (see
 * its doc): `posAtCoords(coords, false)` returns null in empty space; a resolved
 * position must land ON the token (exclusive end, {@link posOnToken}) and within
 * the token's rendered box ({@link clickWithinRange}).
 */
export function agentLinkClick(open: AgentLinkOpener) {
  return EditorView.domEventHandlers({
    mousedown: (event, view) => {
      const coords = { x: event.clientX, y: event.clientY };
      const pos = view.posAtCoords(coords, false);
      if (pos == null) return false;
      const hit = agentLinkAt(view.state.doc.toString(), pos);
      if (!hit) return false;
      if (!posOnToken(pos, hit.from, hit.to)) return false;
      if (!clickWithinRange(view, coords, hit.from, hit.to)) return false;
      event.preventDefault();
      open(hit.id);
      return true;
    },
  });
}
