// The CM6 editor wiring for the inline `/` agent/skill trigger (P1). It
// replaces P0's `@agent` sigil: a single `/` sigil now drives everything.
//
//   `/agent`         → reserved literal: opens the CREATE/DEFINE popup.
//   `/<name>`        → if <name> resolves to a saved agent/skill, INVOKE it.
//   unknown `/<word>`→ left alone (normal Enter; not hijacked).
//
// A definition is decoupled from its triggers (see agents.ts): the saved note
// is the entity; an inline `/<name>` is just a one-time trigger that invokes it.
//
//  1. slashTagHighlight(resolve) — a ViewPlugin (à la livePreview.ts) marking
//     every `/agent` and every resolved `/<name>` token with `.cm-agent-tag`
//     (the blue accent chip carried over from P0). Unknown `/<word>`s are not
//     highlighted.
//  2. slashTrigger(onTrigger) — a high-precedence Enter keymap that, when the
//     caret sits immediately after a `/<word>` token, classifies the word and
//     fires onTrigger; everywhere else Enter is normal.
//  3. slashClickToReopen(onTrigger) — a mousedown handler mapping a click onto
//     a highlighted `/agent` or `/<name>` token back to a trigger.
//
// `resolve(word)` tells the highlighter/trigger whether a word is invocable;
// `onTrigger(kind, word, from, to)` hands the caller the matched token range so
// it can replace exactly that token (Save) or keep it (Exit).

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
import { clickWithinRange, posOnToken } from "./wikiLink";

/** The reserved literal that opens the create/define popup. */
export const CREATE_WORD = "agent";

/** What kind of trigger a `/<word>` token resolved to. */
export type SlashKind = "create" | "run";

/** Tells the trigger machinery whether a bare word names a saved agent/skill. */
export type SlashResolver = (word: string) => boolean;

/** Fired with the resolved kind, the matched word, and its document range. */
export type SlashTriggerHandler = (
  kind: SlashKind,
  word: string,
  from: number,
  to: number,
) => void;

const slashMark = Decoration.mark({ class: "cm-agent-tag" });

// `/agent` or `/word` where word is [A-Za-z0-9_-]+. The leading `/` must not be
// part of a longer run of slashes (so `http://` etc. is not matched) — we check
// the preceding char when scanning.
const SLASH_TOKEN = /\/([A-Za-z0-9_-]+)/g;

/** True if the char before `start` is a token boundary (so `/x` is a real
 *  sigil, not the tail of `a/b` or `http://`). Start-of-doc counts. */
function boundaryBefore(text: string, start: number): boolean {
  if (start === 0) return true;
  const prev = text[start - 1];
  return /\s/.test(prev);
}

// Mark every `/agent` and every resolved `/<name>` in the visible ranges.
function buildSlashDecorations(
  view: EditorView,
  resolve: SlashResolver,
): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  for (const { from, to } of view.visibleRanges) {
    const text = view.state.sliceDoc(from, to);
    SLASH_TOKEN.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = SLASH_TOKEN.exec(text)) !== null) {
      const word = m[1];
      const localStart = m.index;
      if (!boundaryBefore(text, localStart)) continue;
      const known = word.toLowerCase() === CREATE_WORD || resolve(word);
      if (!known) continue;
      const start = from + localStart;
      builder.add(start, start + m[0].length, slashMark);
    }
  }
  return builder.finish();
}

/** The highlight ViewPlugin. `resolve` decides which `/<name>` tokens light up;
 *  the plugin rebuilds on every doc/viewport change. */
export function slashTagHighlight(resolve: SlashResolver) {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = buildSlashDecorations(view, resolve);
      }
      update(update: ViewUpdate) {
        if (update.docChanged || update.viewportChanged) {
          this.decorations = buildSlashDecorations(update.view, resolve);
        }
      }
    },
    { decorations: (v) => v.decorations },
  );
}

/** If `pos` falls on (or right after) a `/<word>` token, return the word and
 *  its range. Used by both the Enter trigger and click-to-reopen. */
export function slashTokenAt(
  doc: string,
  pos: number,
): { word: string; from: number; to: number } | null {
  SLASH_TOKEN.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = SLASH_TOKEN.exec(doc)) !== null) {
    const start = m.index;
    if (!boundaryBefore(doc, start)) continue;
    const end = start + m[0].length;
    if (pos >= start && pos <= end) return { word: m[1], from: start, to: end };
  }
  return null;
}

/** Classify a word: the reserved literal → "create"; a resolvable name →
 *  "run"; anything else → null (not a trigger). */
function classify(word: string, resolve: SlashResolver): SlashKind | null {
  if (word.toLowerCase() === CREATE_WORD) return "create";
  if (resolve(word)) return "run";
  return null;
}

/**
 * High-precedence Enter handler. Returns true (newline suppressed) only when
 * the caret is collapsed immediately after a `/<word>` that classifies to a
 * trigger; fires onTrigger with the kind + word + token range. Returns false
 * everywhere else so normal Enter (and unknown `/<word>`) is unaffected.
 */
export function slashTrigger(
  onTrigger: SlashTriggerHandler,
  resolve: SlashResolver,
) {
  const binding: KeyBinding = {
    key: "Enter",
    run: (view) => {
      const sel = view.state.selection.main;
      if (!sel.empty) return false;
      const cursor = sel.head;
      // The token must end exactly at the caret on the current line.
      const line = view.state.doc.lineAt(cursor);
      const upto = view.state.sliceDoc(line.from, cursor);
      const m = /(^|\s)\/([A-Za-z0-9_-]+)$/.exec(upto);
      if (!m) return false;
      const word = m[2];
      const kind = classify(word, resolve);
      if (!kind) return false; // unknown `/word` → normal Enter
      const from = cursor - (word.length + 1); // include the leading `/`
      onTrigger(kind, word, from, cursor);
      return true; // suppress the newline
    },
  };
  // Above the default keymap (which also binds Enter) so this wins.
  return Prec.highest(keymap.of([binding]));
}

/** True if the char at `idx` (or end-of-doc) is a token boundary — used by the
 *  auto-open listener to require a *complete* `/agent` token (so `/agentic`
 *  doesn't fire while it's still being typed). */
function boundaryAfter(doc: string, idx: number): boolean {
  if (idx >= doc.length) return true;
  return /\s/.test(doc[idx]);
}

/**
 * Auto-open extension (Fix 1): an updateListener that fires `onTrigger("create",
 * …)` the instant the caret lands immediately after a *complete* `/agent` token
 * — no Enter or click required. "Complete" means the char after the caret is a
 * word boundary or end-of-line, so `/agentic` (still being typed) never fires.
 *
 * It is `create`-only by design: a `/<name>` run popup would be intrusive to
 * pop the moment the caret brushes the token, whereas `/agent` is an explicit
 * "I want to make one" gesture. Re-fire is guarded two ways:
 *   - `isOpen()` — never fire while a popup is already up; and
 *   - a per-token-instance latch keyed on the token's [from,to) range, so it
 *     fires once per `/agent` occurrence and won't re-fire as the caret moves
 *     around within/after the same token.
 * The latch disarms whenever the caret leaves the token (or the token's range
 * shifts via an edit), so a later, distinct `/agent` still auto-opens.
 */
export function slashAutoOpen(
  onTrigger: SlashTriggerHandler,
  isOpen: () => boolean,
) {
  // The [from,to) of the token instance we last auto-fired for. Null = armed.
  let firedFor: { from: number; to: number } | null = null;
  return EditorView.updateListener.of((update: ViewUpdate) => {
    // Only react to caret moves / edits, not pure viewport scrolls.
    if (!update.docChanged && !update.selectionSet) return;
    const sel = update.state.selection.main;
    if (!sel.empty) {
      firedFor = null;
      return;
    }
    const doc = update.state.doc.toString();
    const hit = slashTokenAt(doc, sel.head);
    // Caret not on an `/agent` token → disarm so the next one can fire.
    if (!hit || hit.word.toLowerCase() !== CREATE_WORD) {
      firedFor = null;
      return;
    }
    // The caret must sit at the END of a complete token (boundary/EOL after).
    if (sel.head !== hit.to || !boundaryAfter(doc, hit.to)) return;
    // Already fired for this exact token instance, or a popup is open → skip.
    if (firedFor && firedFor.from === hit.from && firedFor.to === hit.to) return;
    if (isOpen()) return;
    firedFor = { from: hit.from, to: hit.to };
    onTrigger("create", hit.word, hit.from, hit.to);
  });
}

/**
 * mousedown handler: clicking a highlighted `/agent` or `/<name>` token
 * re-fires its trigger (reopen after a dismiss). Only `/agent` and resolvable
 * names react; an unknown `/word` click just moves the caret as usual.
 *
 * Same non-clamping hit-test as `wikiLinkClick` so a click to the right of /
 * below a trailing `/<name>` token at EOF places the caret PAST it with one
 * click instead of clamping onto the token and re-firing the popup:
 *  - `posAtCoords(coords, false)` returns null for clicks in empty space → fall
 *    through to CM's default caret placement;
 *  - a resolved position must land ON the token (exclusive end, {@link
 *    posOnToken}) — a click at/after `to` is "past the token", not on it; and
 *  - a defensive rendered-rect check ({@link clickWithinRange}). `slashTokenAt`
 *    keeps its inclusive-end membership (the Enter trigger / auto-open need the
 *    caret to sit at `to`); the precise gating lives here, click-only.
 */
export function slashClickToReopen(
  onTrigger: SlashTriggerHandler,
  resolve: SlashResolver,
) {
  return EditorView.domEventHandlers({
    mousedown: (event, view) => {
      const coords = { x: event.clientX, y: event.clientY };
      const pos = view.posAtCoords(coords, false);
      if (pos == null) return false;
      const hit = slashTokenAt(view.state.doc.toString(), pos);
      if (!hit) return false;
      if (!posOnToken(pos, hit.from, hit.to)) return false;
      if (!clickWithinRange(view, coords, hit.from, hit.to)) return false;
      const kind = classify(hit.word, resolve);
      if (!kind) return false;
      event.preventDefault();
      onTrigger(kind, hit.word, hit.from, hit.to);
      return true;
    },
  });
}
