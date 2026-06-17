// CM6 editor wiring for the inline `[⚡ <agent>](agent://<id>)` chip — the
// embedded-runs "Run" handle (docs/design/embedded-runs.md, R1). The source
// stays a plain, portable markdown link (degrades gracefully); in the editor it
// renders as an ATOMIC, CLICK-TO-EDIT status chip:
//
//  1. agentChip(status) — a ViewPlugin that regex-scans the visible ranges for
//     `[<label>](agent://<id>)` links and REPLACES each with a widget (a
//     `Decoration.replace`) that renders the Run's live status from the index
//     (Idle / Running / Done / Error). The replaced range is also published as an
//     `EditorView.atomicRanges` set, so the cursor STEPS OVER the chip and cannot
//     enter to text-edit it (deliberately NOT the "reveal raw source on cursor
//     entry" behaviour — the chip is opaque by design; editing is the Run editor).
//  2. agentLinkClick(open) — a mousedown handler: clicking a chip opens the Run
//     editor for its id. Uses the same non-clamping, EOF-safe hit-test as
//     wikiLinkClick (posOnToken / clickWithinRange) so a click to the right of /
//     below a trailing chip places the caret past it rather than opening it. The
//     `↓` affordance on a Done chip scrolls to the Run's latest appended output.
//
// The link is only a handle: `<id>` keys the replicated `invocations` index that
// owns the trigger/prompt/last-run/status. We never rewrite the link text.

import { RangeSetBuilder } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  ViewPlugin,
  type ViewUpdate,
  WidgetType,
} from "@codemirror/view";
import { clickWithinRange, posOnToken } from "./wikiLink";
import { agentLinkAt, parseAgentLinks } from "./invocations";
import { formatRelativeTime } from "./invocations";
import type { Invocation } from "./api";

/** Opens the Run editor for the Run with the given id (a chip click). */
export type AgentLinkOpener = (id: string) => void;

/** Scrolls the note to the Run's latest appended output (the Done chip's `↓`
 *  affordance). Best-effort in R1; the formal output callout + block-id backlink
 *  is R3 (docs/design/embedded-runs.md). */
export type ScrollToOutput = (id: string) => void;

/** Resolves a chip's id to its live Run record (or null when the index has no
 *  backing entry yet — e.g. just-inserted, or deleted on another device). Reads
 *  through the live replicated index so the chip restyles as status changes
 *  without re-mounting the editor (the same closure idiom the wiki/agent
 *  resolvers use). */
export type RunStatusResolver = (id: string) => Invocation | null;

/** The four resting/live states a chip can render, derived from the Run's
 *  string `status` (mirrors the component's STATUS_* values in lib.rs). */
export type RunChipState = "idle" | "running" | "done" | "error";

/** A rendered chip: its display text, a state class hook, and whether it carries
 *  the `↓` scroll-to-output affordance (Done only). Pure given the Run + now.
 *  `label` is the full single-line text (including the trailing ` · ↓` when
 *  scrollable, so the pure render reads naturally); the widget splits the `↓`
 *  into its own clickable element. */
export interface RunChip {
  state: RunChipState;
  /** The full chip label, e.g. `⚡ standup 🔒` / `◌ standup · running…` /
   *  `✓ standup · 5m ago · ↓`. */
  label: string;
  /** The label WITHOUT the trailing ` · ↓` (what precedes the affordance). For a
   *  non-scrollable chip this equals `label`. */
  base: string;
  /** True when the chip should offer the `↓` jump-to-latest-output affordance
   *  (a Run that has produced output — Done). */
  scrollable: boolean;
}

/** Map a Run's stored status string to a chip state. Unknown / missing →
 *  Idle (a freshly-inserted chip with no index entry yet reads as Idle). */
function chipState(inv: Invocation | null): RunChipState {
  switch (inv?.status) {
    case "running":
      return "running";
    case "ran":
      return "done";
    case "error":
      return "error";
    // "scheduled" and anything unknown/missing → Idle.
    default:
      return "idle";
  }
}

/**
 * Build the chip's display from the Run record + the current time. Pure (so it
 * unit-tests cleanly). `agent` falls back to the link's own label-derived name
 * when there is no index entry yet. States:
 *
 *   - Idle:    `⚡ <name> 🔒`
 *   - Running: `◌ <name> · running…`
 *   - Done:    `✓ <name> · <relative time> · ↓`
 *   - Error:   `! <name> · failed`
 */
export function runStatusChip(
  inv: Invocation | null,
  agentName: string,
  nowMs: number,
): RunChip {
  const name = (inv?.agent ?? agentName).trim() || "agent";
  const state = chipState(inv);
  switch (state) {
    case "running": {
      const base = `◌ ${name} · running…`;
      return { state, label: base, base, scrollable: false };
    }
    case "done": {
      const when = formatRelativeTime(inv?.last_run_ms ?? null, nowMs);
      const base = `✓ ${name} · ${when}`;
      return { state, label: `${base} · ↓`, base, scrollable: true };
    }
    case "error": {
      const base = `! ${name} · failed`;
      return { state, label: base, base, scrollable: false };
    }
    case "idle": {
      const base = `⚡ ${name} 🔒`;
      return { state, label: base, base, scrollable: false };
    }
  }
}

/** The label-derived agent name for a chip when the index has no entry yet: the
 *  link's visible label with a leading bolt glyph stripped (`⚡ standup` →
 *  `standup`). Best-effort; the index `agent` is preferred when present. */
function nameFromLabel(label: string): string {
  return label.replace(/^\s*[⚡◌✓!]\s*/u, "").trim();
}

/** The CM6 widget that renders one chip. Recreated when the chip text/state
 *  changes (so the decoration set rebuilds on status change). */
class AgentChipWidget extends WidgetType {
  constructor(
    readonly id: string,
    readonly chip: RunChip,
    readonly onScroll: ScrollToOutput,
  ) {
    super();
  }

  // Two widgets are equal (CM6 reuses the DOM) only when the id AND the rendered
  // text/state match — so a status change forces a re-render.
  eq(other: AgentChipWidget): boolean {
    return (
      other.id === this.id &&
      other.chip.label === this.chip.label &&
      other.chip.state === this.chip.state
    );
  }

  toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = `cm-agent-link cm-agent-link-${this.chip.state}`;
    span.dataset.runId = this.id;
    span.setAttribute("aria-label", `Run: ${this.chip.label}`);
    span.append(this.chip.base);
    if (this.chip.scrollable) {
      // The `↓` jump-to-latest-output affordance: its own element so a click on
      // it scrolls (and stops propagation) rather than opening the Run editor
      // like a click on the chip body does.
      span.append(" · ");
      const down = document.createElement("span");
      down.className = "cm-agent-link-jump";
      down.textContent = "↓";
      down.title = "Jump to the latest output";
      down.addEventListener("mousedown", (e) => {
        e.preventDefault();
        e.stopPropagation();
        this.onScroll(this.id);
      });
      span.append(down);
    }
    return span;
  }

  // Let the chip's own DOM handle its events (the `↓` mousedown above); the
  // editor-level mousedown handler still opens the editor for a body click.
  ignoreEvent(): boolean {
    return false;
  }
}

/** Build the replace-decorations (chips) for the visible ranges from the live
 *  status resolver. */
function buildChipDecorations(
  view: EditorView,
  status: RunStatusResolver,
  onScroll: ScrollToOutput,
  nowMs: number,
): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  for (const { from, to } of view.visibleRanges) {
    const text = view.state.sliceDoc(from, to);
    for (const link of parseAgentLinks(text)) {
      const start = from + link.from;
      const end = from + link.to;
      const inv = status(link.id);
      const label = text.slice(link.from, link.to);
      // The label inside `[ … ]` — fall back name when no index entry.
      const inner = /^\[([^\]\n]*)\]/.exec(label)?.[1] ?? "";
      const chip = runStatusChip(inv, nameFromLabel(inner), nowMs);
      builder.add(
        start,
        end,
        Decoration.replace({
          widget: new AgentChipWidget(link.id, chip, onScroll),
        }),
      );
    }
  }
  return builder.finish();
}

/**
 * The chip ViewPlugin: replaces each `agent://` link with a live-status widget
 * and publishes the replaced ranges as atomic (cursor steps over them). Rebuilds
 * on every doc/viewport change. `status` reads the live replicated index so the
 * chip restyles as a Run progresses without re-mounting the editor.
 */
export function agentChip(status: RunStatusResolver, onScroll: ScrollToOutput) {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = buildChipDecorations(view, status, onScroll, Date.now());
      }
      update(update: ViewUpdate) {
        // Rebuild on doc/viewport change. A pure status flip (a new vault state
        // frame) does not fire a ViewUpdate by itself, so the editor re-mount on
        // each vault state — and any in-editor edit — refreshes the chips; the
        // running→done transition shows on the next such refresh.
        if (update.docChanged || update.viewportChanged) {
          this.decorations = buildChipDecorations(
            update.view,
            status,
            onScroll,
            Date.now(),
          );
        }
      }
    },
    {
      decorations: (v) => v.decorations,
      // Make the replaced chip ranges ATOMIC: the cursor steps over them and a
      // selection cannot land inside, so the chip can never be text-edited.
      provide: (plugin) =>
        EditorView.atomicRanges.of(
          (view) => view.plugin(plugin)?.decorations ?? Decoration.none,
        ),
    },
  );
}

/**
 * mousedown handler: clicking a chip opens the Run editor for its id. Same
 * non-clamping, EOF-safe hit-test as `wikiLinkClick` (see its doc):
 * `posAtCoords(coords, false)` returns null in empty space; a resolved position
 * must land ON the token (exclusive end, {@link posOnToken}) and within the
 * token's rendered box ({@link clickWithinRange}). The atomic chip occupies the
 * full `[from, to)` range, so the hit-test still resolves against the source
 * link offsets even though the rendered glyph is a widget.
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
