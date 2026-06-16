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

/** If `pos` falls within a `[[ ]]` token, return its target name + range. */
function wikiTokenAt(
  doc: string,
  pos: number,
): { name: string; from: number; to: number } | null {
  WIKILINK_TOKEN.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = WIKILINK_TOKEN.exec(doc)) !== null) {
    const from = m.index;
    const to = from + m[0].length;
    if (pos >= from && pos <= to) {
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
 */
export function wikiLinkClick(resolve: WikiLinkResolver, open: WikiLinkOpener) {
  return EditorView.domEventHandlers({
    mousedown: (event, view) => {
      const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
      if (pos == null) return false;
      const hit = wikiTokenAt(view.state.doc.toString(), pos);
      if (!hit) return false;
      if (pos < hit.from || pos > hit.to) return false;
      const targetId = resolve(hit.name);
      if (!targetId) return false; // ghost link → normal caret placement
      event.preventDefault();
      open(targetId);
      return true;
    },
  });
}
