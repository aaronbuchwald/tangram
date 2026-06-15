// CodeMirror 6 markdown editor for the tangram shell.
//
// Per ADR-0007 / the shell redesign (Decision A2, Phase S4), the shell is the
// one app with a build pipeline, so it bundles CodeMirror 6 (multi-package,
// dedup-sensitive — Vite resolves a single `@codemirror/state`).
//
// Issue #11 — Obsidian "Live Preview": there is no longer a side-by-side
// rendered pane. The single editable view *is* the rendered document. The
// `livePreview` extension (see livePreview.ts) conceals markdown syntax markers
// off the active line and styles the prose inline, revealing the raw syntax
// only on the line the caret/selection touches. `syntaxHighlighting` colours
// tokens (URLs, code, etc.); `livePreview` does the conceal/inline rendering.
//
// Echo-safety: an SSE `state` frame can arrive mid-edit. `MdEditor.syncRemote`
// only patches the document when the incoming body differs from what's in the
// editor AND differs from what we last wrote — so a remote change lands without
// clobbering the user's in-progress text, and our own echoed write is a no-op.

import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import {
  EditorView,
  drawSelection,
  highlightActiveLine,
  keymap,
} from "@codemirror/view";
import { tags as t } from "@lezer/highlight";
import { livePreview } from "./livePreview";

// Token colouring (Lezer highlight tags). The heading scale/weight and the
// emphasis/code/link inline rendering are driven by the `livePreview`
// decorations + styles.css; this style sheet handles the remaining token
// colours (URLs, list markers, quote text) that show on the active line.
const mdHighlight = HighlightStyle.define([
  // Heading scale/weight is applied per-line by `livePreview` (.cm-lp-h*) so it
  // spans the whole line including the concealed `#`s; here we only colour the
  // remaining inline tokens. The emphasis/strong/strike/code/link rendering is
  // also driven by livePreview marks — these tags are kept as the on-the-
  // active-line fallback colouring (e.g. so revealed `**` reads as syntax).
  { tag: t.emphasis, fontStyle: "italic" },
  { tag: t.strong, fontWeight: "700", color: "#f2f4f7" },
  { tag: t.link, color: "#009eee" },
  { tag: t.url, color: "#5d6470" },
  { tag: t.monospace, color: "#1fd286" },
  { tag: t.quote, color: "#8d95a2", fontStyle: "italic" },
  { tag: [t.processingInstruction, t.meta], color: "#5d6470" },
  { tag: t.list, color: "#8d95a2" },
  { tag: t.strikethrough, textDecoration: "line-through", color: "#8d95a2" },
]);

const theme = EditorView.theme(
  {
    "&": {
      height: "100%",
      color: "var(--text)",
      backgroundColor: "var(--bg)",
      fontSize: "16px",
    },
    ".cm-scroller": {
      fontFamily: "var(--sans)",
      lineHeight: "1.7",
      padding: "8px 0",
    },
    ".cm-content": { padding: "8px 18px", caretColor: "var(--blue)" },
    "&.cm-focused": { outline: "none" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--blue)" },
    ".cm-activeLine": { backgroundColor: "rgba(255,255,255,0.025)" },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection":
      { backgroundColor: "#182634" },
    ".cm-gutters": { display: "none" },

    // ── Live Preview (issue #11) — inline rendering of markdown constructs ──
    // Headings: scale/weight applied to the whole line so it holds even when
    // the leading `#`s are concealed. line-height tightened a touch per level.
    ".cm-lp-h1": { fontSize: "1.7em", fontWeight: "700", lineHeight: "1.3" },
    ".cm-lp-h2": { fontSize: "1.45em", fontWeight: "700", lineHeight: "1.3" },
    ".cm-lp-h3": { fontSize: "1.25em", fontWeight: "600", lineHeight: "1.35" },
    ".cm-lp-h4": { fontSize: "1.1em", fontWeight: "600" },
    ".cm-lp-h5": { fontWeight: "600" },
    ".cm-lp-h6": { fontWeight: "600", color: "var(--dim)" },
    // Emphasis rendered inline (the `*`/`_` are concealed off the active line).
    ".cm-lp-strong": { fontWeight: "700", color: "#f2f4f7" },
    ".cm-lp-em": { fontStyle: "italic" },
    ".cm-lp-strike": { textDecoration: "line-through", color: "var(--dim)" },
    // Inline code keeps a subtle chip even with the backticks hidden.
    ".cm-lp-code": {
      fontFamily: "var(--mono)",
      color: "var(--green)",
      backgroundColor: "var(--panel-2)",
      borderRadius: "4px",
      padding: "0.05em 0.3em",
    },
    // Link text reads as a link; the URL/brackets conceal off the active line.
    ".cm-lp-link": { color: "var(--blue)", textDecoration: "underline" },
    // The `•` glyph that stands in for an unordered list marker off the line.
    ".cm-lp-bullet": { color: "var(--dim)" },
    // Blockquote: a left bar + dim text, drawn on the line so it survives the
    // concealed `>` marker.
    ".cm-lp-quote": {
      borderLeft: "3px solid var(--line)",
      paddingLeft: "0.8em",
      color: "var(--dim)",
      fontStyle: "italic",
    },
  },
  { dark: true },
);

export class MdEditor {
  readonly view: EditorView;
  // The body we last pushed to the model (and thus expect to echo back over
  // SSE). Lets syncRemote distinguish "our own write" from a real remote edit.
  private lastWritten: string;

  constructor(
    parent: HTMLElement,
    initialDoc: string,
    onChange: (doc: string) => void,
  ) {
    this.lastWritten = initialDoc;
    const state = EditorState.create({
      doc: initialDoc,
      extensions: [
        history(),
        drawSelection(),
        highlightActiveLine(),
        keymap.of([...defaultKeymap, ...historyKeymap]),
        // Extended (GFM) dialect so strikethrough, task lists, tables, etc.
        // parse — the live-preview decorations key off their Lezer nodes.
        markdown({ base: markdownLanguage }),
        syntaxHighlighting(mdHighlight),
        livePreview,
        theme,
        EditorView.lineWrapping,
        EditorView.updateListener.of((u) => {
          if (u.docChanged) onChange(u.state.doc.toString());
        }),
      ],
    });
    this.view = new EditorView({ state, parent });
  }

  /** The current editor contents. */
  get doc(): string {
    return this.view.state.doc.toString();
  }

  /** Record the body we just wrote to the model (an upcoming SSE echo). */
  markWritten(doc: string): void {
    this.lastWritten = doc;
  }

  /**
   * Apply a remote body from an SSE `state` frame WITHOUT clobbering an
   * in-progress edit. We replace the document only when the remote body
   * differs from both the editor's current text and our last write — i.e. it
   * is a genuine remote change we haven't seen. Cursor/selection are preserved
   * where possible.
   */
  syncRemote(remote: string): void {
    const current = this.doc;
    if (remote === current) {
      // already in sync (commonly: our own echoed write)
      this.lastWritten = remote;
      return;
    }
    if (current !== this.lastWritten) {
      // the user has typed since our last write — don't stomp their edit.
      // last-writer-wins on the model reconciles on the next write.
      return;
    }
    // current === lastWritten but remote differs: a real remote edit. Adopt it.
    const sel = this.view.state.selection;
    const anchor = Math.min(sel.main.anchor, remote.length);
    const head = Math.min(sel.main.head, remote.length);
    this.view.dispatch({
      changes: { from: 0, to: current.length, insert: remote },
      selection: { anchor, head },
    });
    this.lastWritten = remote;
  }

  destroy(): void {
    this.view.destroy();
  }
}
