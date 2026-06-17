// Smart objects SO1 — the inline `[<label>](obj://<id>)` chip: the typed-graph
// object's HANDLE (docs/design/smart-objects.md). This GENERALIZES the
// embedded-runs `agent://` Run chip (agentLink.ts) into one primitive, built
// ALONGSIDE agents/runs. The source stays a plain, portable markdown link
// (degrades to a link); in the editor it renders as an ATOMIC, click-to-edit
// chip with a per-type glyph (`◆`) distinct from the agent `⚡`:
//
//  1. objectChip(resolve) — a ViewPlugin that regex-scans the visible ranges for
//     `[<label>](obj://<id>)` links and REPLACES each with a widget. The replaced
//     range is also published as an `EditorView.atomicRanges` set, so the cursor
//     STEPS OVER the chip and cannot enter to text-edit it (the chip is opaque by
//     design; editing is the object popup).
//  2. objectLinkClick(open) — a mousedown handler: clicking a chip opens the
//     object popup for its id. Uses the same non-clamping, EOF-safe hit-test as
//     wikiLinkClick / agentLinkClick (posOnToken / clickWithinRange).
//
// The link is only a handle: `<id>` keys the replicated `objects` store that owns
// the type/data/links/render. We never rewrite the link text. Mirrors
// `parse_object_links` in `apps/tangram/src/agents.rs` so both sides agree on the
// handle format.

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
import type { SmartObject } from "./api";

/** The glyph the smart-object chip leads with — distinct from the agent `⚡`. */
export const OBJECT_GLYPH = "◆";

/** Opens the object popup for the object with the given id (a chip click). */
export type ObjectLinkOpener = (id: string) => void;

/** Resolves a chip's id to its live SmartObject (or null when the store has no
 *  backing entry yet — e.g. just-inserted, or deleted on another device). Reads
 *  through the live replicated store so the chip restyles as the object changes
 *  without re-mounting the editor (the same closure idiom the agent/wiki
 *  resolvers use). */
export type ObjectResolver = (id: string) => SmartObject | null;

/** One inline `[<label>](obj://<id>)` link occurrence in a note body. */
export interface ObjectLink {
  /** The object id embedded in the link target (`obj://<id>`). */
  id: string;
  /** The character offset of the opening `[`. */
  from: number;
  /** The character offset just past the closing `)`. */
  to: number;
}

// `[<label>](obj://<id>)` — label excludes `]`; the id excludes `)`/whitespace.
// Global so we can scan a body for every occurrence.
const OBJECT_LINK = /\[([^\]\n]*)\]\(obj:\/\/([^)\s]+)\)/g;

/**
 * Parse every inline `[<label>](obj://<id>)` link in `body`, in document order.
 * Mirrors `parse_object_links` in `apps/tangram/src/agents.rs` so the UI and
 * component agree on the handle format.
 */
export function parseObjectLinks(body: string): ObjectLink[] {
  const out: ObjectLink[] = [];
  OBJECT_LINK.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = OBJECT_LINK.exec(body ?? "")) !== null) {
    const id = m[2].trim();
    if (id.length === 0) continue;
    out.push({ id, from: m.index, to: m.index + m[0].length });
  }
  return out;
}

/**
 * If `pos` lands ON an `obj://` link token (opening boundary inclusive, closing
 * boundary exclusive — see `posOnToken` in wikiLink.ts), return its id + range.
 * Used by the click handler in editor.ts to open the object popup.
 */
export function objectLinkAt(body: string, pos: number): ObjectLink | null {
  for (const link of parseObjectLinks(body)) {
    if (pos >= link.from && pos < link.to) return link;
  }
  return null;
}

/**
 * Build the inline link text inserted into the note when a smart object is
 * minted via the `@` picker (the handle). The `◆` glyph + chip decoration mark
 * it as a smart-object link, distinct from a `[[ ]]` wikilink and the `⚡` agent
 * chip. `id` is a UUID the caller mints. The label is the object's display name.
 */
export function buildObjectLink(label: string, id: string): string {
  return `[${OBJECT_GLYPH} ${label}](obj://${id})`;
}

/** A read-only index of the vault's smart objects (the replicated store carried
 *  on the vault state frame). */
export interface ObjectIndex {
  /** All objects from the replicated store, in stored order. */
  readonly all: SmartObject[];
  /** Look up an object by its stable id. */
  byId(id: string): SmartObject | null;
}

/**
 * Build the object index over the REPLICATED objects from the vault state frame.
 * Rebuilt in `main.ts`'s `onVaultState` alongside the other indexes. (The source
 * of truth is the store, not the markdown — the inline link is only the handle.)
 */
export function buildObjectIndex(objects: SmartObject[]): ObjectIndex {
  const all = objects ?? [];
  const byId = new Map<string, SmartObject>();
  for (const o of all) byId.set(o.id, o);
  return {
    all,
    byId: (id) => byId.get(id) ?? null,
  };
}

/** The label-derived name for a chip (the link's visible label with the leading
 *  `◆` glyph stripped). Best-effort fallback when the store has no entry yet. */
function nameFromLabel(label: string): string {
  return label.replace(/^\s*[◆]\s*/u, "").trim();
}

/** A class-safe slug for the chip's per-type style hook (e.g. `note-ref` →
 *  `note-ref`; unknown chars collapsed). */
function typeSlug(objType: string): string {
  return objType.trim().toLowerCase().replace(/[^a-z0-9-]+/g, "-") || "unknown";
}

/** The CM6 widget that renders one smart-object chip. Recreated when the chip
 *  text/type changes (so the decoration set rebuilds on a state change). */
class ObjectChipWidget extends WidgetType {
  constructor(
    readonly id: string,
    readonly label: string,
    readonly objType: string,
  ) {
    super();
  }

  // Two widgets are equal (CM6 reuses the DOM) only when id + label + type match.
  eq(other: ObjectChipWidget): boolean {
    return (
      other.id === this.id &&
      other.label === this.label &&
      other.objType === this.objType
    );
  }

  toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = `cm-object-link cm-object-link-${typeSlug(this.objType)}`;
    span.dataset.objectId = this.id;
    span.dataset.objectType = this.objType;
    span.setAttribute("aria-label", `Smart object: ${this.label} (${this.objType})`);
    span.append(`${OBJECT_GLYPH} ${this.label}`);
    return span;
  }

  ignoreEvent(): boolean {
    return false;
  }
}

/** Build the replace-decorations (chips) for the visible ranges from the live
 *  object resolver. */
function buildChipDecorations(
  view: EditorView,
  resolve: ObjectResolver,
): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  for (const { from, to } of view.visibleRanges) {
    const text = view.state.sliceDoc(from, to);
    for (const link of parseObjectLinks(text)) {
      const start = from + link.from;
      const end = from + link.to;
      const obj = resolve(link.id);
      const raw = text.slice(link.from, link.to);
      const inner = /^\[([^\]\n]*)\]/.exec(raw)?.[1] ?? "";
      const label = nameFromLabel(inner) || "object";
      const objType = obj?.type ?? "unknown";
      builder.add(
        start,
        end,
        Decoration.replace({ widget: new ObjectChipWidget(link.id, label, objType) }),
      );
    }
  }
  return builder.finish();
}

/**
 * The object-chip ViewPlugin: replaces each `obj://` link with an atomic widget
 * (the cursor steps over it) carrying the object's per-type glyph/style.
 * Rebuilds on every doc/viewport change. `resolve` reads the live replicated
 * store so the chip restyles as the object's type changes without re-mounting.
 */
export function objectChip(resolve: ObjectResolver) {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = buildChipDecorations(view, resolve);
      }
      update(update: ViewUpdate) {
        if (update.docChanged || update.viewportChanged) {
          this.decorations = buildChipDecorations(update.view, resolve);
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
 * mousedown handler: clicking a chip opens the object popup for its id. Same
 * non-clamping, EOF-safe hit-test as `agentLinkClick` / `wikiLinkClick`.
 */
export function objectLinkClick(open: ObjectLinkOpener) {
  return EditorView.domEventHandlers({
    mousedown: (event, view) => {
      const coords = { x: event.clientX, y: event.clientY };
      const pos = view.posAtCoords(coords, false);
      if (pos == null) return false;
      const hit = objectLinkAt(view.state.doc.toString(), pos);
      if (!hit) return false;
      if (!posOnToken(pos, hit.from, hit.to)) return false;
      if (!clickWithinRange(view, coords, hit.from, hit.to)) return false;
      event.preventDefault();
      open(hit.id);
      return true;
    },
  });
}
