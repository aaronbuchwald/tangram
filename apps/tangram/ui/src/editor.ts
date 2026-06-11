// CodeMirror 6 markdown editor for the tangram shell.
//
// Per ADR-0007 / the shell redesign (Decision A2, Phase S4), the shell is the
// one app with a build pipeline, so it bundles CodeMirror 6 (multi-package,
// dedup-sensitive — Vite resolves a single `@codemirror/state`). This replaces
// the plain textarea with a real source editor: markdown syntax highlighting,
// a dark theme matching the performance-console language, and Obsidian-style
// in-editor heading/emphasis styling, paired with the live rendered preview.
//
// Echo-safety: an SSE `state` frame can arrive mid-edit. `MdEditor.syncRemote`
// only patches the document when the incoming body differs from what's in the
// editor AND differs from what we last wrote — so a remote change lands without
// clobbering the user's in-progress text, and our own echoed write is a no-op.

import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { markdown } from "@codemirror/lang-markdown";
import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import {
  EditorView,
  drawSelection,
  highlightActiveLine,
  keymap,
} from "@codemirror/view";
import { tags as t } from "@lezer/highlight";

// Obsidian-ish in-editor markdown styling: headings get scale/weight,
// emphasis renders italic/bold, code/links get the accent. Source stays
// visible (markers shown) — a CodeMirror source pane, not a hide-marks WYSIWYG.
const mdHighlight = HighlightStyle.define([
  { tag: t.heading1, fontSize: "1.5em", fontWeight: "600", color: "#f2f4f7" },
  { tag: t.heading2, fontSize: "1.3em", fontWeight: "600", color: "#f2f4f7" },
  { tag: t.heading3, fontSize: "1.15em", fontWeight: "600", color: "#e6e9ee" },
  { tag: [t.heading4, t.heading5, t.heading6], fontWeight: "600" },
  { tag: t.emphasis, fontStyle: "italic" },
  { tag: t.strong, fontWeight: "700", color: "#f2f4f7" },
  { tag: t.link, color: "#009eee", textDecoration: "underline" },
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
      fontSize: "13px",
    },
    ".cm-scroller": {
      fontFamily: "var(--mono)",
      lineHeight: "1.6",
      padding: "8px 0",
    },
    ".cm-content": { padding: "8px 18px", caretColor: "var(--blue)" },
    "&.cm-focused": { outline: "none" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--blue)" },
    ".cm-activeLine": { backgroundColor: "rgba(255,255,255,0.025)" },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection":
      { backgroundColor: "#182634" },
    ".cm-gutters": { display: "none" },
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
        markdown(),
        syntaxHighlighting(mdHighlight),
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
