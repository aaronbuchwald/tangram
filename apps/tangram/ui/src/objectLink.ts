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

import { RangeSetBuilder, StateEffect } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  ViewPlugin,
  type ViewUpdate,
  WidgetType,
} from "@codemirror/view";
import type { SmartObject } from "./api";

/** The default glyph a smart-object chip leads with — distinct from the agent
 *  `⚡`. Used for an unregistered type or one without a dedicated glyph. */
export const OBJECT_GLYPH = "◆";

/** Per-type chip glyphs (#109 fix 2): the chip reflects its RESOLVED type with a
 *  distinct glyph, not a generic `◆`/`unknown`. Resolved from the object store
 *  (the chip resolver), falling back to {@link OBJECT_GLYPH} for an unregistered
 *  type or a not-yet-resolved chip. Keep in sync with the SO type registry in
 *  `apps/tangram/src/lib.rs` (`KNOWN_OBJECT_TYPES`). */
const TYPE_GLYPHS: Record<string, string> = {
  recipe: "🍳",
  "grocery-list": "🛒",
  "cart-preview": "🧺",
  "note-ref": "🔗",
  tag: "🏷",
  rollup: "∑",
};

/** The glyph for a resolved smart-object `type` — its per-type glyph, or the
 *  default `◆` for an unregistered/unknown type. */
export function glyphForType(objType: string): string {
  return TYPE_GLYPHS[objType.trim().toLowerCase()] ?? OBJECT_GLYPH;
}

/** The smart-object types rendered INLINE as a §8 BLOCK card (the meal-plan
 *  mockup, see objectTable.ts) rather than the compact inline chip. The chip
 *  ViewPlugin SKIPS these ids so the block StateField in objectTable.ts owns
 *  them — the two never double-decorate the same range. `recipe` is a §8 card
 *  (SO5); `grocery-list`/`cart-preview` are derived tables (SO3). */
const CARD_TYPES: ReadonlySet<string> = new Set(["recipe", "grocery-list", "cart-preview"]);

/** True when a smart object of `objType` renders as an inline §8 block card (and
 *  is therefore skipped by the inline-chip ViewPlugin). Case-insensitive. */
export function isCardObjectType(objType: string | null | undefined): boolean {
  return CARD_TYPES.has((objType ?? "").trim().toLowerCase());
}

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

/**
 * #109 fix 1 — strip the ENTIRE `[label](obj://<id>)` span from `body`, not just
 * the `](obj://id)` target (which used to orphan the leading `◆ label` text).
 * Returns the new body, or null when the id has no inline link (nothing to do).
 * Collapses one surrounding space so removing the span doesn't leave a double
 * space (prefers eating a trailing space; else a leading one). Pure — the editor
 * caller applies the result.
 */
export function stripObjectLinkFromBody(body: string, id: string): string | null {
  const link = parseObjectLinks(body).find((l) => l.id === id);
  if (!link) return null;
  let { from, to } = link;
  if (body[to] === " ") {
    to += 1;
  } else if (from > 0 && body[from - 1] === " ") {
    from -= 1;
  }
  return body.slice(0, from) + body.slice(to);
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

/** The "derived / auto" affordance glyph shown on a healthy derived chip — a
 *  small recompute marker (mirrors the design's auto-synced badge). */
export const DERIVED_BADGE = "↻";

/** The error glyph shown on a broken derived chip (a dependency cycle / unknown
 *  derive kind — Smart Objects SO2 §2). */
export const DERIVED_ERROR_GLYPH = "⚠";

/** Format a derived object's cached `data` (the engine's computed JSON) as a
 *  compact inline value for the chip — e.g. a rollup `{"op":"sum","sum":8,...}`
 *  renders as `8`, a `count` as `8`. Falls back to a trimmed raw string for an
 *  opaque payload. Best-effort + never throws (a chip must always render). */
export function derivedValueLabel(data: string): string {
  const raw = (data ?? "").trim();
  if (!raw) return "…";
  try {
    const v = JSON.parse(raw);
    if (v && typeof v === "object") {
      // SO3 grocery-list: `{ rows: [...] }` → "N items".
      if (Array.isArray(v.rows)) {
        const n = v.rows.length;
        return `${n} item${n === 1 ? "" : "s"}`;
      }
      // SO3 cart-preview: `{ aisles: [...] }` → "N aisles".
      if (Array.isArray(v.aisles)) {
        const n = v.aisles.length;
        return `${n} aisle${n === 1 ? "" : "s"}`;
      }
      // Prefer the headline aggregate field for the known rollup ops.
      if (typeof v.sum === "number") return String(v.sum);
      if (Array.isArray(v.values)) return v.values.join(", ") || "(empty)";
      if (typeof v.count === "number") return String(v.count);
    }
    if (typeof v === "number" || typeof v === "string") return String(v);
  } catch {
    // not JSON — fall through to the raw payload
  }
  return raw.length > 40 ? `${raw.slice(0, 40)}…` : raw;
}

/** The CM6 widget that renders one smart-object chip. Recreated when the chip
 *  text/type/derived-state changes (so the decoration set rebuilds and the
 *  derived value updates live when a dependency changes). */
class ObjectChipWidget extends WidgetType {
  constructor(
    readonly id: string,
    readonly label: string,
    readonly objType: string,
    /** SO2: true when the object carries a `derive` (a derived object). */
    readonly derived: boolean,
    /** SO2: the cached derived value (when `derived`, healthy) to show inline. */
    readonly value: string,
    /** SO2: the cached derive error (cycle / unknown kind), or "" when healthy. */
    readonly error: string,
  ) {
    super();
  }

  // Two widgets are equal (CM6 reuses the DOM) only when EVERY rendered input
  // matches — so a recomputed derived value / a new error forces a re-render.
  eq(other: ObjectChipWidget): boolean {
    return (
      other.id === this.id &&
      other.label === this.label &&
      other.objType === this.objType &&
      other.derived === this.derived &&
      other.value === this.value &&
      other.error === this.error
    );
  }

  toDOM(): HTMLElement {
    const span = document.createElement("span");
    const classes = ["cm-object-link", `cm-object-link-${typeSlug(this.objType)}`];
    if (this.derived) classes.push("cm-object-link-derived");
    if (this.error) classes.push("cm-object-link-derived-error");
    span.className = classes.join(" ");
    span.dataset.objectId = this.id;
    span.dataset.objectType = this.objType;

    // #109 fix 2: the chip leads with its RESOLVED type's glyph (not a generic
    // `◆`); an unregistered/unresolved type falls back to `◆`.
    const glyph = glyphForType(this.objType);

    if (this.derived && this.error) {
      // A broken derived object: an unmissable error chip (SO2 §2).
      span.dataset.derived = "error";
      span.setAttribute(
        "aria-label",
        `Smart object: ${this.label} (${this.objType}) — derive error: ${this.error}`,
      );
      span.title = this.error;
      span.append(`${glyph} ${this.label} ${DERIVED_ERROR_GLYPH}`);
    } else if (this.derived) {
      // A healthy derived object: show the computed value + the auto badge.
      span.dataset.derived = "auto";
      span.setAttribute(
        "aria-label",
        `Smart object: ${this.label} (${this.objType}) — derived, auto-computed value ${this.value}`,
      );
      span.title = "Derived — recomputed automatically from its dependencies";
      span.append(`${glyph} ${this.label}: ${this.value} `);
      const badge = document.createElement("span");
      badge.className = "cm-object-link-badge";
      badge.textContent = DERIVED_BADGE;
      span.append(badge);
    } else {
      span.setAttribute("aria-label", `Smart object: ${this.label} (${this.objType})`);
      span.append(`${glyph} ${this.label}`);
    }
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
      // A §8 card type (recipe / grocery-list / cart-preview) renders as an
      // inline BLOCK card, owned by the StateField in objectTable.ts — skip it
      // here so the two decoration sources never collide on the same range.
      if (isCardObjectType(obj?.type)) continue;
      const raw = text.slice(link.from, link.to);
      const inner = /^\[([^\]\n]*)\]/.exec(raw)?.[1] ?? "";
      const label = nameFromLabel(inner) || "object";
      const objType = obj?.type ?? "unknown";
      // SO2: a derived object renders its cached value + an auto badge (or an
      // error chip on a cycle). The value comes from the live store, so the chip
      // updates the moment the engine recomputes a dependency.
      const derived = !!obj?.derive;
      const error = obj?.derive_error ?? "";
      const value = derived ? derivedValueLabel(obj?.data ?? "") : "";
      builder.add(
        start,
        end,
        Decoration.replace({
          widget: new ObjectChipWidget(link.id, label, objType, derived, value, error),
        }),
      );
    }
  }
  return builder.finish();
}

/**
 * A no-payload effect that forces the object-chip plugin to rebuild its
 * decorations from the live store WITHOUT a doc change (Smart Objects SO2). The
 * shell dispatches it on every vault state frame so a derived chip's cached value
 * refreshes the moment the reactivity engine recomputes a dependency — including
 * a recompute driven by ANOTHER replica (where the local doc body is unchanged).
 */
export const refreshObjectChips = StateEffect.define<null>();

/**
 * The object-chip ViewPlugin: replaces each `obj://` link with an atomic widget
 * (the cursor steps over it) carrying the object's per-type glyph/style and, for
 * a derived object, its cached value + auto badge (or an error chip on a cycle).
 * Rebuilds on every doc/viewport change AND on a `refreshObjectChips` effect (the
 * live derived-value update path). `resolve` reads the live replicated store so a
 * chip reflects the object's current type/value without re-mounting.
 */
export function objectChip(resolve: ObjectResolver) {
  return ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;
      constructor(view: EditorView) {
        this.decorations = buildChipDecorations(view, resolve);
      }
      update(update: ViewUpdate) {
        const refreshed = update.transactions.some((tr) =>
          tr.effects.some((e) => e.is(refreshObjectChips)),
        );
        if (update.docChanged || update.viewportChanged || refreshed) {
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
 * mousedown handler: clicking ANYWHERE on a chip opens the object popup for its
 * id (#fix smart-object-ux 2). The previous position-based hit-test
 * (`posAtCoords` + `posOnToken` + `clickWithinRange`, cloned from the
 * wiki/agent link handlers) only registered toward the chip's LEFT edge: the
 * atomic widget is a single replaced range, and `posAtCoords` resolves a click
 * over the right portion of the rendered glyph to the EXCLUSIVE end (`pos ===
 * to`), which `posOnToken` rejects as "past the chip" — so most of the chip was
 * dead. A smart-object chip is an OPAQUE widget (no nested affordances like the
 * Run chip's `↓`), so the whole rendered element is one click target: resolve
 * the chip straight off the event's DOM target via `.cm-object-link[data-object-id]`,
 * which covers the entire chip box. Falls back to nothing when the click did not
 * land inside a chip element (normal caret placement).
 */
export function objectLinkClick(open: ObjectLinkOpener) {
  return EditorView.domEventHandlers({
    mousedown: (event, _view) => {
      const target = event.target as HTMLElement | null;
      const chip = target?.closest<HTMLElement>(".cm-object-link");
      const id = chip?.dataset.objectId;
      if (!id) return false;
      event.preventDefault();
      open(id);
      return true;
    },
  });
}
