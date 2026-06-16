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
  placeholder,
} from "@codemirror/view";
import { autocompletion } from "@codemirror/autocomplete";
import { tags as t } from "@lezer/highlight";
import {
  type SlashResolver,
  type SlashTriggerHandler,
  slashAutoOpen,
  slashClickToReopen,
  slashTagHighlight,
  slashTrigger,
} from "./slashTrigger";
import {
  type SlashCandidateProvider,
  slashCompletionSource,
} from "./slashComplete";
import {
  type WikiLinkOpener,
  type WikiLinkResolver,
  wikiLinkClick,
  wikiLinkHighlight,
} from "./wikiLink";
import {
  type AgentLinkOpener,
  agentLinkClick,
  agentLinkHighlight,
} from "./agentLink";
import {
  type WikiCandidateProvider,
  wikiCompletionSource,
} from "./wikiComplete";
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
    // The inline `/` trigger token (`/agent` or a resolved `/<name>`) — a
    // subtle blue accent chip, so it reads as an actionable affordance (Enter /
    // click fires the create/run popup).
    ".cm-agent-tag": {
      color: "var(--blue)",
      backgroundColor: "rgba(0,158,238,0.12)",
      borderRadius: "4px",
      padding: "0.05em 0.25em",
      fontWeight: "600",
      cursor: "pointer",
    },
    // `[[ ]]` wikilinks (Connected Vault, G1). A resolved link reads as a
    // clickable blue link (the same accent family as `.cm-agent-tag`, but
    // underlined like a link rather than chipped); an unresolved "ghost" target
    // is dimmed/dashed so a typo or a not-yet-created note is visibly distinct.
    ".cm-wikilink": {
      color: "var(--blue)",
      textDecoration: "underline",
      textDecorationStyle: "dotted",
      cursor: "pointer",
    },
    ".cm-wikilink-unresolved": {
      color: "var(--dim)",
      textDecoration: "underline",
      textDecorationStyle: "dashed",
      opacity: "0.75",
    },
    // Inline `[⚡ <agent>](agent://<id>)` agent links (the scheduled-invocation
    // handle). DARK-BLUE so they read as a distinct affordance from `[[ ]]`
    // wikilinks (accent blue) — a click opens the Trigger popup. A subtle tinted
    // chip + bolt glyph marks it as an actionable agent schedule.
    ".cm-agent-link": {
      color: "#1746a2",
      backgroundColor: "rgba(23,70,162,0.14)",
      borderRadius: "4px",
      padding: "0.05em 0.3em",
      fontWeight: "600",
      textDecoration: "none",
      cursor: "pointer",
    },
    // Blockquote: a left bar + dim text, drawn on the line so it survives the
    // concealed `>` marker.
    ".cm-lp-quote": {
      borderLeft: "3px solid var(--line)",
      paddingLeft: "0.8em",
      color: "var(--dim)",
      fontStyle: "italic",
    },
    // The empty-note slash hint (#10): the default CM6 placeholder colour is too
    // loud, so dim it; the `/`/`[[` chips read as quiet key glyphs.
    ".cm-placeholder": { color: "var(--faint)", fontStyle: "normal" },
    ".cm-slash-hint-kbd": {
      fontFamily: "var(--mono)",
      color: "var(--blue)",
      backgroundColor: "var(--panel-2)",
      borderRadius: "4px",
      padding: "0.05em 0.3em",
    },
  },
  { dark: true },
);

// Build the empty-note placeholder (#10): "Type / to run an agent · [[ to link
// a note", with the `/` and `[[` tokens rendered as subtle key chips. Returned
// as a detached DOM node so CM6's `placeholder` extension mounts it verbatim;
// it auto-hides the instant the document is non-empty.
function slashHintPlaceholder(): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "cm-slash-hint";
  const kbd = (text: string): HTMLElement => {
    const k = document.createElement("code");
    k.className = "cm-slash-hint-kbd";
    k.textContent = text;
    return k;
  };
  wrap.append(
    document.createTextNode("Type "),
    kbd("/"),
    document.createTextNode(" to run an agent · "),
    kbd("[["),
    document.createTextNode(" to link a note"),
  );
  return wrap;
}

export class MdEditor {
  readonly view: EditorView;
  // The body we last pushed to the model (and thus expect to echo back over
  // SSE). Lets syncRemote distinguish "our own write" from a real remote edit.
  private lastWritten: string;

  constructor(
    parent: HTMLElement,
    initialDoc: string,
    onChange: (doc: string) => void,
    // Inline `/` agent trigger (P1): fired when a `/<word>` token is committed
    // (caret right after it + Enter, or a click on a highlighted token) and the
    // word classifies — `/agent` (create) or `/<name>` (run a saved def). The
    // handler gets the kind, the word, and the token's [from, to) range so the
    // popup can replace exactly it on Save (or keep it on Exit). Optional so
    // non-agent editors are unchanged.
    onSlashTrigger?: SlashTriggerHandler,
    // Resolves whether a bare word names a saved agent/skill. Defaults to "no
    // names known", so only `/agent` (create) reacts until an index is wired.
    resolveAgent: SlashResolver = () => false,
    // True while an agent popup (create or run) is already open. The auto-open
    // listener uses this to avoid re-firing the create popup over an open one.
    // Defaults to "never open" so the auto-open is unguarded if not wired.
    isPopupOpen: () => boolean = () => false,
    // Live candidate set for the `/<partial>` autocomplete popup (every indexed
    // agent/skill plus the reserved `agent` create command). Read fresh on each
    // keystroke so newly-created defs appear without re-mounting. Defaults to an
    // empty list so the autocomplete is a harmless no-op when not wired.
    slashCandidates: SlashCandidateProvider = () => [],
    // `[[ ]]` wikilink support (Connected Vault, G1). `resolveWikiLink` maps a
    // link name to a target file id (or null = ghost); it reads through the
    // live link index so links re-resolve as the vault changes without a
    // re-mount. `onOpenWikiLink` opens a resolved link's target note on click.
    // Defaults make the highlight a harmless ghost-everything no-op and clicks
    // inert, so non-vault editors are unchanged.
    resolveWikiLink: WikiLinkResolver = () => null,
    onOpenWikiLink: WikiLinkOpener = () => {},
    // Live candidate set for the `[[ ]]` wikilink autocomplete popup (the vault
    // notes). Read fresh on each keystroke so newly-created notes appear without
    // re-mounting — same closure idiom as `slashCandidates`/`resolveWikiLink`.
    // Defaults to an empty list so the autocomplete is a harmless no-op (returns
    // no popup) when not wired.
    wikiCandidates: WikiCandidateProvider = () => [],
    // The path of the note being edited (sans `.md`), used to exclude it from
    // its own `[[ ]]` autocomplete candidates. Defaults to "unknown".
    currentNotePath: () => string | null = () => null,
    // Inline `[⚡ <agent>](agent://<id>)` agent-link support (the scheduled-
    // invocation redesign). Clicking a dark-blue agent link opens the Trigger
    // popup for its invocation id. Defaults to a no-op so non-vault editors are
    // unchanged (the highlight is harmless and always on).
    onOpenAgentLink: AgentLinkOpener = () => {},
  ) {
    this.lastWritten = initialDoc;
    // The Enter trigger + click-to-reopen are only wired when a handler is
    // supplied; the highlight is harmless and always on. The auto-open listener
    // (Fix 1) pops the create popup the instant the caret lands after a complete
    // `/agent` token — Enter/click remain as fallbacks.
    const agentExtensions = onSlashTrigger
      ? [
          slashTrigger(onSlashTrigger, resolveAgent),
          slashClickToReopen(onSlashTrigger, resolveAgent),
          slashAutoOpen(onSlashTrigger, isPopupOpen),
        ]
      : [];
    const state = EditorState.create({
      doc: initialDoc,
      extensions: [
        history(),
        drawSelection(),
        highlightActiveLine(),
        // High-precedence agent Enter handler must sit before the default
        // keymap (which also binds Enter); slashTrigger wraps it in Prec.highest.
        ...agentExtensions,
        // Quiet slash/wikilink affordance (#10): a dim placeholder shown only
        // while the note is empty, surfacing the two inline powers without
        // adding chrome. CM6's `placeholder` auto-hides the moment any text is
        // typed, so it never intrudes on a real note or the live-preview render.
        placeholder(slashHintPlaceholder()),
        keymap.of([...defaultKeymap, ...historyKeymap]),
        // Extended (GFM) dialect so strikethrough, task lists, tables, etc.
        // parse — the live-preview decorations key off their Lezer nodes.
        markdown({ base: markdownLanguage }),
        syntaxHighlighting(mdHighlight),
        livePreview,
        slashTagHighlight(resolveAgent),
        // `[[ ]]` wikilink decorations + click-to-open (G1). Always on; with the
        // default resolver every link is a harmless ghost and clicks are inert.
        wikiLinkHighlight(resolveWikiLink),
        wikiLinkClick(resolveWikiLink, onOpenWikiLink),
        // Inline `agent://<id>` agent-link decoration + click-to-edit (the
        // scheduled-invocation handle). Dark-blue, distinct from `.cm-wikilink`;
        // a click opens the Trigger popup. Always on; the click is a no-op with
        // the default opener.
        agentLinkHighlight(),
        agentLinkClick(onOpenAgentLink),
        // CM6 allows `autocompletion()` exactly ONCE — two configured instances
        // throw "Config merge conflict for field override" and the editor never
        // mounts (notes stop rendering). So the slash `/<partial>` popup and the
        // `[[<partial>` wikilink popup share a SINGLE autocompletion extension,
        // each contributing its own completion source. Each source self-gates on
        // its own trigger (`/` token vs `[[` anchor) and is a harmless no-op when
        // its candidate provider is empty, so both can always be present. (The
        // slash source's create/run trigger logic still rides on slashTrigger
        // above; this is completion only — accepting just rewrites the token.)
        autocompletion({
          override: [
            slashCompletionSource(slashCandidates),
            wikiCompletionSource(wikiCandidates, currentNotePath),
          ],
          activateOnTyping: true,
          icons: false,
          maxRenderedOptions: 12,
          aboveCursor: false,
        }),
        theme,
        EditorView.lineWrapping,
        EditorView.updateListener.of((u) => {
          if (u.docChanged) onChange(u.state.doc.toString());
        }),
      ],
    });
    this.view = new EditorView({ state, parent });
  }

  /**
   * Replace the document range [from, to) with `text`, put the caret right
   * after the inserted text, and refocus. Used by the run popup's Save to swap
   * the triggering `/<name>` token for the prompt+response block (and by the
   * create popup to strip the `/agent` token). The existing debounced onChange
   * persists the new doc to the vault.
   */
  replaceRange(from: number, to: number, text: string): void {
    const docLen = this.view.state.doc.length;
    const clampedFrom = Math.min(from, docLen);
    const clampedTo = Math.min(Math.max(to, clampedFrom), docLen);
    const caret = clampedFrom + text.length;
    this.view.dispatch({
      changes: { from: clampedFrom, to: clampedTo, insert: text },
      selection: { anchor: caret },
    });
    this.view.focus();
  }

  /** Refocus the editor (e.g. after the agent popup is dismissed). */
  focus(): void {
    this.view.focus();
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
