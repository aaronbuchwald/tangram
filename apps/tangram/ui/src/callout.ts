// Run-output CALLOUT cards (embedded-runs R3). A Run's output renders as an
// Obsidian-style `> [!run]+ …` callout below the chip's host paragraph — a CARD
// in the live-preview editor (and the reading view), not the legacy indented
// blockquote. The markdown is portable (a plain renderer shows a blockquote);
// this module owns:
//
//   - the block-id helpers (`hostBlockId` / `calloutBlockId`) shared with the
//     component (`apps/tangram/src/agents.rs`) so the chip ⇄ callout backlinks
//     resolve deterministically from the Run id;
//   - `parseRunCallouts` — find every callout block in a document body and pull
//     out its header (glyph, agent, model, when), its body text, the host block
//     id it backlinks to, and its own callout block id + char range;
//   - `runCalloutCard` — a ViewPlugin that REPLACES each callout block with a
//     styled card widget (header + body), the header carrying a `↑` backlink to
//     the chip; and
//   - `renderRunCalloutCard` — the same card as a detached DOM node for a
//     reading view.
//
// The card matches the shell's visual language (the `.cm-agent-link` chip family
// + the panel chrome). Clicking the header's `↑` scrolls to + briefly highlights
// the chip (the callout→chip backlink); the chip's `↓` jumps here (chip→callout).

import {
  type EditorState,
  type Extension,
  type Range,
  RangeSetBuilder,
  StateField,
} from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  WidgetType,
} from "@codemirror/view";

/** The host-paragraph block id for a Run (the chip's anchor, the callout's
 *  backlink target). Mirrors `host_block_id` in `agents.rs`. */
export function hostBlockId(runId: string): string {
  return `run-${runId}`;
}

/** The output-callout block id for a Run (the chip's `↓` jump target). Mirrors
 *  `callout_block_id` in `agents.rs`. */
export function calloutBlockId(runId: string): string {
  return `runout-${runId}`;
}

/** A parsed run-output callout block found in a note body. */
export interface RunCallout {
  /** The status glyph from the header (`✓` ran / `✗` error). */
  glyph: string;
  /** True when the header glyph marks an error run. */
  isError: boolean;
  /** The agent/skill name (the header's `/<agent>`). */
  agent: string;
  /** The model the run called. */
  model: string;
  /** The short "when" label (e.g. `one-time`, a trigger summary). */
  when: string;
  /** The host-paragraph block id the header backlinks to (`run-<id>`). */
  hostBlockId: string;
  /** This callout's own block id (`runout-<id>`). */
  blockId: string;
  /** The output body (newline-joined, the `> ` prefixes stripped). */
  body: string;
  /** Char offset of the block start (the `> [!run]` line). */
  from: number;
  /** Char offset just past the block (past the `> ^runout-…` line). */
  to: number;
}

// `> [!run]+ ✓ /standup · deepseek-chat · one-time [↑](#^run-abc)`
const HEADER_RE =
  /^> \[!run\][+-]? (\S+) \/([^·]+?) · ([^·]+?) · (.+?) \[↑\]\(#\^([^)]+)\)\s*$/;
// `> ^runout-abc`
const BLOCKID_RE = /^> \^(\S+)\s*$/;

/**
 * Parse every run-output callout block in `body`, in document order. A block is
 * the contiguous run of `> …` lines starting at a `> [!run]` header and ending
 * at the `> ^runout-…` block-id line. Robust to the body lines in between.
 */
export function parseRunCallouts(body: string): RunCallout[] {
  const out: RunCallout[] = [];
  const text = body ?? "";
  const lines = text.split("\n");
  // Precompute the char offset of each line start.
  const offsets: number[] = [];
  let acc = 0;
  for (const line of lines) {
    offsets.push(acc);
    acc += line.length + 1; // + the `\n`
  }

  let i = 0;
  while (i < lines.length) {
    const header = HEADER_RE.exec(lines[i]);
    if (!header) {
      i += 1;
      continue;
    }
    const from = offsets[i];
    const glyph = header[1];
    const agent = header[2].trim();
    const model = header[3].trim();
    const when = header[4].trim();
    const hostId = header[5];
    // Collect body lines until the `> ^runout-…` block-id line (or a non-quote).
    const bodyLines: string[] = [];
    let blockId = "";
    let j = i + 1;
    let end = offsets[i] + lines[i].length; // default: header only
    for (; j < lines.length; j++) {
      const idMatch = BLOCKID_RE.exec(lines[j]);
      if (idMatch) {
        blockId = idMatch[1];
        end = offsets[j] + lines[j].length;
        j += 1;
        break;
      }
      if (!lines[j].startsWith(">")) break; // ran off the callout
      // Strip the leading `> ` (or bare `>`) prefix.
      bodyLines.push(lines[j].replace(/^> ?/, ""));
    }
    out.push({
      glyph,
      isError: glyph === "✗",
      agent,
      model,
      when,
      hostBlockId: hostId,
      blockId,
      body: bodyLines.join("\n").trim(),
      from,
      to: end,
    });
    i = j;
  }
  return out;
}

/** A callout backlink click: scroll to + briefly highlight the chip whose host
 *  block id matches (the callout→chip direction). */
export type CalloutBacklink = (hostBlockId: string) => void;

/** Build the card DOM for one parsed callout (shared by the editor widget + the
 *  reading view). The header carries a `↑` backlink button. */
export function renderRunCalloutCard(
  cal: RunCallout,
  onBacklink: CalloutBacklink,
): HTMLElement {
  const card = document.createElement("div");
  card.className = `run-callout-card${cal.isError ? " run-callout-error" : ""}`;
  card.dataset.calloutBlockId = cal.blockId;

  const head = document.createElement("div");
  head.className = "run-callout-head";
  const glyph = document.createElement("span");
  glyph.className = "run-callout-glyph";
  glyph.textContent = cal.glyph;
  const titleEl = document.createElement("span");
  titleEl.className = "run-callout-title";
  titleEl.textContent = `/${cal.agent}`;
  const meta = document.createElement("span");
  meta.className = "run-callout-meta";
  meta.textContent = `${cal.model} · ${cal.when}`;
  // The ↑ backlink to the chip (callout→chip).
  const back = document.createElement("span");
  back.className = "run-callout-backlink";
  back.textContent = "↑";
  back.title = "Jump to the run chip";
  back.addEventListener("mousedown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    onBacklink(cal.hostBlockId);
  });
  head.append(glyph, titleEl, meta, back);

  const bodyEl = document.createElement("div");
  bodyEl.className = "run-callout-body";
  bodyEl.textContent = cal.body;

  card.append(head, bodyEl);
  return card;
}

/** The CM6 block widget rendering one callout card. Recreated when the rendered
 *  text changes (the body/header differ) so a re-run refresh re-renders. */
class RunCalloutWidget extends WidgetType {
  constructor(
    readonly cal: RunCallout,
    readonly onBacklink: CalloutBacklink,
  ) {
    super();
  }
  eq(other: RunCalloutWidget): boolean {
    return (
      other.cal.blockId === this.cal.blockId &&
      other.cal.body === this.cal.body &&
      other.cal.glyph === this.cal.glyph &&
      other.cal.when === this.cal.when &&
      other.cal.model === this.cal.model &&
      other.cal.agent === this.cal.agent
    );
  }
  toDOM(): HTMLElement {
    return renderRunCalloutCard(this.cal, this.onBacklink);
  }
  ignoreEvent(): boolean {
    return false;
  }
}

/** Build the callout-card replace decorations from a state. Provided via a
 *  StateField (NOT a ViewPlugin): a block `Decoration.replace` spans line
 *  breaks, which plugins are forbidden from doing — only StateField-sourced
 *  decorations may. Reveals the raw source while the selection touches the block
 *  (so it stays editable on the active line, like livePreview). */
function buildCalloutDecorations(
  state: EditorState,
  onBacklink: CalloutBacklink,
): DecorationSet {
  const doc = state.doc.toString();
  const ranges: Range<Decoration>[] = [];
  for (const cal of parseRunCallouts(doc)) {
    const touched = state.selection.ranges.some(
      (r) => r.from <= cal.to && r.to >= cal.from,
    );
    if (touched) continue; // reveal raw source on the active block
    ranges.push(
      Decoration.replace({
        widget: new RunCalloutWidget(cal, onBacklink),
        block: true,
      }).range(cal.from, cal.to),
    );
  }
  const builder = new RangeSetBuilder<Decoration>();
  for (const r of ranges) builder.add(r.from, r.to, r.value);
  return builder.finish();
}

/**
 * The run-output callout extension (embedded-runs R3): a StateField that
 * replaces each `> [!run]+` callout block with a styled card widget (block
 * level). `onBacklink` wires the header's `↑` to scroll to + highlight the chip.
 * Recomputed on every doc/selection change (the selection touch reveals the raw
 * source on the active block). The replaced ranges are atomic (the cursor steps
 * over the card). Sourced from a StateField because block decorations that span
 * line breaks may not come from a ViewPlugin.
 */
export function runCalloutCard(onBacklink: CalloutBacklink): Extension {
  const field = StateField.define<DecorationSet>({
    create: (state) => buildCalloutDecorations(state, onBacklink),
    update(value, tr) {
      if (tr.docChanged || tr.selection) {
        return buildCalloutDecorations(tr.state, onBacklink);
      }
      return value;
    },
    provide: (f) => [
      EditorView.decorations.from(f),
      EditorView.atomicRanges.of((view) => view.state.field(f, false) ?? Decoration.none),
    ],
  });
  return field;
}
