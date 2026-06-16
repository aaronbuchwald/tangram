// CM6 editor wiring for `[[ ]]` wikilinks (Connected Vault, G1). A direct
// clone of slashTrigger.ts's `slashTagHighlight` + `slashClickToReopen`:
//
//  1. wikiLinkHighlight(resolve) — a ViewPlugin that regex-scans the visible
//     ranges for `[[Target]]` / `[[Target|alias]]` / `[[Target#heading]]` and
//     their embed forms (`![[…]]`), marking each: a RESOLVED link gets
//     `.cm-wikilink` (the blue accent, à la `.cm-agent-tag`); an UNRESOLVED
//     "ghost" target gets `.cm-wikilink-unresolved` (a dim style).
//  2. wikiLinkClick(open) — a mousedown handler: clicking a RESOLVED wikilink
//     opens that note (open(targetId)). An unresolved link is a no-op click
//     (just moves the caret) in v1.
//
// `resolve(name)` returns the target file id (or null = unresolved); it reads
// through `main.ts`'s live `linkIndex`, so links re-resolve as the vault
// changes without re-mounting the editor — the same closure idiom the agent
// resolver uses. We never rewrite link text; resolution is by stable id.

import { RangeSetBuilder } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  ViewPlugin,
  type ViewUpdate,
} from "@codemirror/view";

/** Resolves a wikilink name (basename or path, sans `.md`) to a file id. */
export type WikiLinkResolver = (name: string) => string | null;

/** Opens the note with the given file id (a click on a resolved link). */
export type WikiLinkOpener = (fileId: string) => void;

const resolvedMark = Decoration.mark({ class: "cm-wikilink" });
const unresolvedMark = Decoration.mark({ class: "cm-wikilink-unresolved" });

// `[[Target]]` / `[[Target|alias]]` / `[[Target#heading]]`, optional leading
// `!` for embeds. The inner text excludes `]`, `[`, and newlines so a stray
// bracket doesn't run a match across constructs.
const WIKILINK_TOKEN = /(!?)\[\[([^\][\n]+?)\]\]/g;

/** The target name of a wikilink's inner text (alias/heading/`.md` stripped). */
function targetName(inner: string): string {
  let s = inner;
  const pipe = s.indexOf("|");
  if (pipe !== -1) s = s.slice(0, pipe);
  const hash = s.indexOf("#");
  if (hash !== -1) s = s.slice(0, hash);
  return s.trim().replace(/\.md$/i, "");
}

/** Mark every `[[ ]]` in the visible ranges, resolved vs ghost. */
function buildWikiDecorations(
  view: EditorView,
  resolve: WikiLinkResolver,
): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  for (const { from, to } of view.visibleRanges) {
    const text = view.state.sliceDoc(from, to);
    WIKILINK_TOKEN.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = WIKILINK_TOKEN.exec(text)) !== null) {
      const name = targetName(m[2]);
      if (name.length === 0) continue;
      const start = from + m.index;
      const end = start + m[0].length;
      const mark = resolve(name) ? resolvedMark : unresolvedMark;
      builder.add(start, end, mark);
    }
  }
  return builder.finish();
}

/** The highlight ViewPlugin; rebuilds on every doc/viewport change. */
export function wikiLinkHighlight(resolve: WikiLinkResolver) {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = buildWikiDecorations(view, resolve);
      }
      update(update: ViewUpdate) {
        if (update.docChanged || update.viewportChanged) {
          this.decorations = buildWikiDecorations(update.view, resolve);
        }
      }
    },
    { decorations: (v) => v.decorations },
  );
}

/**
 * True iff `pos` lands ON a `[from, to)` token — strictly inside, INCLUDING the
 * opening boundary but EXCLUDING the closing one. The end boundary is excluded
 * so a caret placed exactly *after* a trailing token (`pos === to`) is treated
 * as "past the link", not "on the link": clamping `posAtCoords` resolves a click
 * to the right of / below a trailing token onto its end, and we must let that
 * fall through to normal caret placement rather than open the link. Pure +
 * unit-tested (the rendered-rect refinement below is layout-dependent).
 */
export function posOnToken(pos: number, from: number, to: number): boolean {
  return pos >= from && pos < to;
}

/** If `pos` lands ON a `[[ ]]` token, return its target name + range. Uses the
 *  exclusive-end membership of {@link posOnToken}. */
function wikiTokenAt(
  doc: string,
  pos: number,
): { name: string; from: number; to: number } | null {
  WIKILINK_TOKEN.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = WIKILINK_TOKEN.exec(doc)) !== null) {
    const from = m.index;
    const to = from + m[0].length;
    if (posOnToken(pos, from, to)) {
      const name = targetName(m[2]);
      if (name.length === 0) return null;
      return { name, from, to };
    }
  }
  return null;
}

/**
 * mousedown handler: clicking a RESOLVED wikilink opens its target note. An
 * unresolved `[[ ]]` click is left alone (caret moves as usual) in v1.
 *
 * Hit-testing is non-clamping (`posAtCoords(coords, false)`): a click in empty
 * space — to the right of the line or below the last line — returns null, so we
 * fall through to CodeMirror's default caret placement (e.g. caret AFTER a
 * trailing link) instead of clamping onto the nearest token. When a position is
 * returned we additionally require it to land ON the token text (exclusive end,
 * see {@link posOnToken}) and, defensively, that the click x/y is within the
 * token's rendered box — only then open. A click at/after the token's end → not
 * on the link → CM places the caret.
 */
export function wikiLinkClick(resolve: WikiLinkResolver, open: WikiLinkOpener) {
  return EditorView.domEventHandlers({
    mousedown: (event, view) => {
      const coords = { x: event.clientX, y: event.clientY };
      // Non-clamping: null when the click is outside any text (right of / below
      // the content) → let CM place the caret there.
      const pos = view.posAtCoords(coords, false);
      if (pos == null) return false;
      const hit = wikiTokenAt(view.state.doc.toString(), pos);
      if (!hit) return false;
      // Defensive rect check: confirm the click actually fell within the token's
      // rendered box, not merely resolved into its range. Guards the edge where
      // posAtCoords lands on an interior char but the cursor is past the glyphs.
      if (!clickWithinRange(view, coords, hit.from, hit.to)) return false;
      const targetId = resolve(hit.name);
      if (!targetId) return false; // ghost link → normal caret placement
      event.preventDefault();
      open(targetId);
      return true;
    },
  });
}

/**
 * True iff the pixel `coords` fall within the rendered box of the document range
 * `[from, to)`. Built from `coordsAtPos` of the endpoints (a single line spans a
 * box; we union the two endpoint rects). Returns true when coords can't be
 * resolved (e.g. jsdom has no layout) so the range-membership check stays the
 * decisive gate in tests; in a real browser it tightens the on-link decision.
 */
export function clickWithinRange(
  view: EditorView,
  coords: { x: number; y: number },
  from: number,
  to: number,
): boolean {
  const a = view.coordsAtPos(from, 1);
  const b = view.coordsAtPos(to, -1);
  if (!a || !b) return true; // no layout (jsdom) → defer to range membership
  const left = Math.min(a.left, b.left);
  const right = Math.max(a.right, b.right);
  const top = Math.min(a.top, b.top);
  const bottom = Math.max(a.bottom, b.bottom);
  return (
    coords.x >= left &&
    coords.x <= right &&
    coords.y >= top &&
    coords.y <= bottom
  );
}
