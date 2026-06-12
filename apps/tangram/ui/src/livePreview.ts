// Obsidian-style "Live Preview" decorations for the CodeMirror 6 markdown
// editor (issue #11). There is no separate rendered pane: the single editable
// view *is* the rendered document. Markdown syntax markers (the `#` of a
// heading, the `**`/`_` around emphasis, the `` ` `` around code, list
// bullets, blockquote `>`, link brackets/URLs) are concealed by default and
// the surrounding text is styled to match, so the prose reads like rendered
// output. The raw syntax for a construct is *revealed* only when the cursor or
// selection touches it — that's the line you're editing — so editing always
// operates on the true source text. Nothing here mutates the document; it is
// purely a view-layer projection (decorations), so the CRDT/save path is
// untouched.
//
// Implementation: a ViewPlugin walks the Lezer markdown syntax tree over the
// visible ranges and emits two kinds of decorations — `Decoration.replace` to
// conceal a marker (zero-width, no widget) and `Decoration.mark` to style the
// styled span. A construct is "active" (revealed) when the selection overlaps
// the *line range* it sits on, mirroring Obsidian: put the caret on a line and
// its markup springs back into view.

import { syntaxTree } from "@codemirror/language";
import {
  type EditorState,
  type Range,
  RangeSetBuilder,
} from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  ViewPlugin,
  type ViewUpdate,
  WidgetType,
} from "@codemirror/view";
import type { SyntaxNodeRef } from "@lezer/common";

// A rendered bullet glyph that stands in for an unordered list marker
// (`-`/`*`/`+`) when its line isn't being edited — Obsidian shows a dot.
class BulletWidget extends WidgetType {
  eq(): boolean {
    return true;
  }
  toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = "cm-lp-bullet";
    span.textContent = "•";
    return span;
  }
  ignoreEvent(): boolean {
    return false;
  }
}
const bulletDeco = Decoration.replace({ widget: new BulletWidget() });

// Marker node names whose text is concealed when the line is not active. These
// are the pure-syntax tokens of each construct.
const MARKER_NODES = new Set<string>([
  "HeaderMark", // leading `#`s of an ATX heading (+ the trailing space)
  "EmphasisMark", // `*` / `_` around emphasis/strong
  "CodeMark", // backticks of inline code / fenced-code fences
  "QuoteMark", // `>`
  "StrikethroughMark", // `~~`
  "LinkMark", // `[` `]` `(` `)` around links/images
]);

// A concealing replace decoration (zero-width: it hides the marked range).
const conceal = Decoration.replace({});

// Inline styling marks (paired with the dark-theme CSS in styles.css). Block
// styling (heading scale, blockquote bar) is handled by line decorations.
const strongMark = Decoration.mark({ class: "cm-lp-strong" });
const emphasisMark = Decoration.mark({ class: "cm-lp-em" });
const strikeMark = Decoration.mark({ class: "cm-lp-strike" });
const codeMark = Decoration.mark({ class: "cm-lp-code" });
const linkMark = Decoration.mark({ class: "cm-lp-link" });

const headingLine = [1, 2, 3, 4, 5, 6].map((n) =>
  Decoration.line({ class: `cm-lp-h${n}` }),
);
const quoteLine = Decoration.line({ class: "cm-lp-quote" });

// Does the selection touch the line(s) this node sits on? If so we reveal the
// node's raw syntax (Obsidian's "edit the line you're on" behaviour).
function selectionTouchesNode(
  state: EditorState,
  from: number,
  to: number,
): boolean {
  const firstLine = state.doc.lineAt(from);
  const lastLine = state.doc.lineAt(to);
  for (const range of state.selection.ranges) {
    if (range.from <= lastLine.to && range.to >= firstLine.from) return true;
  }
  return false;
}

function buildDecorations(view: EditorView): DecorationSet {
  const { state } = view;
  // Collect raw ranges first, then sort — replace/mark/line decorations must
  // be added to a RangeSetBuilder in (from, startSide) order, and a single
  // tree walk yields them out of order.
  const marks: Range<Decoration>[] = [];
  const lines: Range<Decoration>[] = [];

  for (const { from, to } of view.visibleRanges) {
    syntaxTree(state).iterate({
      from,
      to,
      enter: (node: SyntaxNodeRef) => {
        const name = node.name;
        const active = selectionTouchesNode(state, node.from, node.to);

        // Block-level line styling (heading scale, blockquote bar). Applied
        // regardless of active state — the styling itself reads as "rendered",
        // and revealing only swaps the marker glyph back in.
        const headingMatch = /^(?:ATXHeading|SetextHeading)([1-6])$/.exec(name);
        if (headingMatch) {
          const level = Number(headingMatch[1]);
          const line = state.doc.lineAt(node.from);
          lines.push(headingLine[level - 1].range(line.from));
          return;
        }
        if (name === "Blockquote") {
          // A blockquote can span multiple lines; decorate each.
          let pos = node.from;
          while (pos <= node.to) {
            const line = state.doc.lineAt(pos);
            lines.push(quoteLine.range(line.from));
            if (line.to + 1 > node.to) break;
            pos = line.to + 1;
          }
          return;
        }

        // Inline emphasis/code/link styling: style the whole construct span.
        if (name === "StrongEmphasis") {
          marks.push(strongMark.range(node.from, node.to));
          return;
        }
        if (name === "Emphasis") {
          marks.push(emphasisMark.range(node.from, node.to));
          return;
        }
        if (name === "Strikethrough") {
          marks.push(strikeMark.range(node.from, node.to));
          return;
        }
        if (name === "InlineCode") {
          marks.push(codeMark.range(node.from, node.to));
          return;
        }
        if (name === "Link" || name === "Image") {
          marks.push(linkMark.range(node.from, node.to));
          return;
        }

        // Unordered list bullet: swap `-`/`*`/`+` for a `•` glyph off the
        // active line; reveal the raw marker on it. Ordered-list numbers
        // (`1.`) are left as-is — they carry meaning the reader wants to see.
        if (name === "ListMark" && !active && node.to > node.from) {
          const marker = state.doc.sliceString(node.from, node.to);
          if (marker === "-" || marker === "*" || marker === "+") {
            marks.push(bulletDeco.range(node.from, node.to));
          }
          return;
        }

        // Concealable markers: hide the raw syntax unless the line is active.
        if (MARKER_NODES.has(name) && !active && node.to > node.from) {
          marks.push(conceal.range(node.from, node.to));
          return;
        }

        // The URL/title inside a link is noise once rendered; hide it off the
        // active line so only the link text shows (Obsidian behaviour).
        if (
          (name === "URL" || name === "LinkTitle") &&
          !active &&
          node.to > node.from
        ) {
          marks.push(conceal.range(node.from, node.to));
          return;
        }
      },
    });
  }

  const builder = new RangeSetBuilder<Decoration>();
  // Line decorations sort before marks at the same position (startSide), so
  // merge the two streams and add in (from, startSide) order.
  const all = [...lines, ...marks].sort(
    (a, b) => a.from - b.from || a.value.startSide - b.value.startSide,
  );
  for (const r of all) builder.add(r.from, r.to, r.value);
  return builder.finish();
}

export const livePreview = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = buildDecorations(view);
    }
    update(update: ViewUpdate) {
      // Rebuild when the doc, the selection (active line moved), or the
      // viewport changes — any of which can change what is concealed/revealed.
      if (update.docChanged || update.selectionSet || update.viewportChanged) {
        this.decorations = buildDecorations(update.view);
      }
    }
  },
  {
    decorations: (v) => v.decorations,
    // Concealing replace decorations participate in atomic-range handling so
    // arrow keys step over a hidden marker instead of landing inside it.
    provide: (plugin) =>
      EditorView.atomicRanges.of(
        (view) => view.plugin(plugin)?.decorations ?? Decoration.none,
      ),
  },
);
