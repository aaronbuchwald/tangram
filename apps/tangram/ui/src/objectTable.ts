// Smart objects SO3 — inline DERIVED-OBJECT TABLES (the meal-plan UX fix).
//
// A derived `grocery-list` / `cart-preview` smart object used to render as the
// same compact `obj://` chip every other object gets — you had to CLICK it to
// see anything. That hides the whole point of the reactive recipe→grocery→cart
// chain (the "document recalculates itself" moment). This module renders those
// two types INLINE, as an auto-derived table/aisle BLOCK in place of the chip,
// so the content is visible without a click and recomputes live in the doc.
//
// Modelled on `callout.ts` (R3): a block `Decoration.replace` that legitimately
// spans the chip's line MUST come from a StateField, not a ViewPlugin (CM6
// forbids a ViewPlugin from sourcing a decoration that crosses a line break).
// So this is a StateField, parallel to `runCalloutCard`. It owns ONLY the
// table-typed objects (grocery-list / cart-preview); the inline chip ViewPlugin
// (`objectChip` in objectLink.ts) skips those ids via {@link isTableObjectType}
// so the two never double-decorate the same range.
//
// Live update: the StateField recomputes on `tr.docChanged || tr.selection`
// (the editable-on-the-active-line reveal, like livePreview) AND on the shared
// `refreshObjectChips` effect — the SAME no-doc-change nudge the chip plugin
// listens for — so a table re-renders the instant the reactivity engine
// recomputes the derived `data` (including a recompute driven by another
// replica, where this doc body is unchanged).

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
import type { SmartObject } from "./api";
import {
  DERIVED_BADGE,
  DERIVED_ERROR_GLYPH,
  type ObjectLinkOpener,
  type ObjectResolver,
  glyphForType,
  isTableObjectType,
  parseObjectLinks,
  refreshObjectChips,
} from "./objectLink";

// ── the parsed `data` shapes (mirror objectPopup.ts / the component) ──────────

/** One aggregated grocery row (a grocery-list's computed `data.rows`). */
interface GroceryRow {
  name?: string;
  quantity?: number;
  unit?: string;
  sources?: string[];
  category?: string;
}
/** One aisle group (a cart-preview's computed `data.aisles`). */
interface CartAisle {
  category?: string;
  items?: { name?: string; quantity?: number; unit?: string }[];
}

/** Best-effort parse of an object's JSON `data` — never throws (a malformed
 *  payload yields null and the block falls back to an empty render). */
function parseData<T>(data: string): T | null {
  try {
    const v = JSON.parse(data || "null");
    return v && typeof v === "object" ? (v as T) : null;
  } catch {
    return null;
  }
}

/** Format a quantity + unit compactly (`4 tbsp`, `3 ct`, `5`); a unitless
 *  count drops the unit. */
function qtyLabel(quantity: number | undefined, unit: string | undefined): string {
  const q = typeof quantity === "number" ? String(quantity) : "?";
  const u = (unit ?? "").trim();
  return u ? `${q} ${u}` : q;
}

/** The label inside a chip's `[ … ]` with the leading glyph stripped, used as
 *  the block's heading. */
function nameFromLabel(label: string): string {
  return label.replace(/^\s*[◆🛒🧺]\s*/u, "").trim();
}

// ── DOM rendering (shared by the editor block widget) ─────────────────────────

/** Build the "auto-synced ↻" badge row that heads a derived table — also the
 *  click affordance that opens the object popup (the table is otherwise inert,
 *  unlike the chip it replaced). */
function renderHeader(
  obj: SmartObject,
  label: string,
  open: ObjectLinkOpener,
): HTMLElement {
  const head = document.createElement("div");
  head.className = "object-table-head";

  const title = document.createElement("span");
  title.className = "object-table-title";
  title.textContent = `${glyphForType(obj.type)} ${label}`;
  head.append(title);

  const badge = document.createElement("span");
  if (obj.derive_error) {
    badge.className = "object-table-badge object-table-badge-error";
    badge.textContent = `${DERIVED_ERROR_GLYPH} derive error`;
    badge.title = obj.derive_error;
  } else {
    badge.className = "object-table-badge";
    badge.textContent = `auto-synced ${DERIVED_BADGE}`;
    badge.title = "Derived — recomputed automatically from its dependencies";
  }
  head.append(badge);

  // The whole header is the click affordance that opens the object popup (the
  // chip used to be the only handle; the table keeps a way back to the editor).
  const open_ = document.createElement("button");
  open_.type = "button";
  open_.className = "object-table-open";
  open_.textContent = "Edit";
  open_.title = "Open the smart object";
  open_.addEventListener("mousedown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    open(obj.id);
  });
  head.append(open_);
  return head;
}

/** A grocery-list as an Item | Qty | From table (From = "N recipes" from
 *  `sources`). Returns the table body element appended into the card. */
function renderGroceryBody(obj: SmartObject): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "object-table-body";
  const data = parseData<{ rows?: GroceryRow[] }>(obj.data);
  const rows = data?.rows ?? [];
  if (rows.length === 0) {
    const empty = document.createElement("div");
    empty.className = "object-table-empty";
    empty.textContent = "No items — include a recipe in the plan.";
    wrap.append(empty);
    return wrap;
  }
  const table = document.createElement("table");
  table.className = "object-table-grocery";
  const thead = document.createElement("thead");
  const htr = document.createElement("tr");
  for (const h of ["Item", "Qty", "From"]) {
    const th = document.createElement("th");
    th.textContent = h;
    htr.append(th);
  }
  thead.append(htr);
  table.append(thead);
  const tbody = document.createElement("tbody");
  for (const r of rows) {
    const tr = document.createElement("tr");
    const item = document.createElement("td");
    item.textContent = (r.name ?? "").trim() || "item";
    const qty = document.createElement("td");
    qty.textContent = qtyLabel(r.quantity, r.unit);
    const from = document.createElement("td");
    const n = r.sources?.length ?? 0;
    from.textContent = `${n} recipe${n === 1 ? "" : "s"}`;
    from.title = (r.sources ?? []).join(", ");
    tr.append(item, qty, from);
    tbody.append(tr);
  }
  table.append(tbody);
  wrap.append(table);
  return wrap;
}

/** A cart-preview grouped by aisle (`category`): each aisle a small labeled
 *  group of its items. */
function renderCartBody(obj: SmartObject): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "object-table-body";
  const data = parseData<{ aisles?: CartAisle[] }>(obj.data);
  const aisles = data?.aisles ?? [];
  if (aisles.length === 0) {
    const empty = document.createElement("div");
    empty.className = "object-table-empty";
    empty.textContent = "Empty cart — add items to the grocery list.";
    wrap.append(empty);
    return wrap;
  }
  for (const aisle of aisles) {
    const group = document.createElement("div");
    group.className = "object-table-aisle";
    const name = document.createElement("div");
    name.className = "object-table-aisle-name";
    name.textContent = (aisle.category ?? "").trim() || "Other";
    group.append(name);
    const list = document.createElement("ul");
    list.className = "object-table-aisle-items";
    for (const it of aisle.items ?? []) {
      const li = document.createElement("li");
      li.textContent = `${(it.name ?? "").trim() || "item"} — ${qtyLabel(it.quantity, it.unit)}`;
      list.append(li);
    }
    group.append(list);
    wrap.append(group);
  }
  return wrap;
}

/** Build the whole derived-table card for one resolved object (shared by the
 *  block widget). `label` is the chip's display label; `open` opens the popup. */
export function renderObjectTableCard(
  obj: SmartObject,
  label: string,
  open: ObjectLinkOpener,
): HTMLElement {
  const card = document.createElement("div");
  card.className = `object-table-card object-table-${obj.type}`;
  if (obj.derive_error) card.classList.add("object-table-error");
  card.dataset.objectId = obj.id;
  card.dataset.objectType = obj.type;
  card.append(renderHeader(obj, label, open));
  card.append(obj.type === "cart-preview" ? renderCartBody(obj) : renderGroceryBody(obj));
  return card;
}

// ── the CM6 block widget + StateField ─────────────────────────────────────────

/** The block widget rendering one derived-object table. Recreated (CM6 reuses
 *  the DOM only on `eq`) when the object's id/type/data/error changes — so a
 *  recompute re-renders the table. */
class ObjectTableWidget extends WidgetType {
  constructor(
    readonly obj: SmartObject,
    readonly label: string,
    readonly open: ObjectLinkOpener,
  ) {
    super();
  }
  eq(other: ObjectTableWidget): boolean {
    return (
      other.obj.id === this.obj.id &&
      other.obj.type === this.obj.type &&
      other.obj.data === this.obj.data &&
      (other.obj.derive_error ?? "") === (this.obj.derive_error ?? "") &&
      other.label === this.label
    );
  }
  toDOM(): HTMLElement {
    return renderObjectTableCard(this.obj, this.label, this.open);
  }
  ignoreEvent(): boolean {
    return false;
  }
}

/** Build the block table replace-decorations from a state. A link renders as a
 *  block table only when (a) its resolved object is a table type (grocery-list
 *  / cart-preview) AND (b) the link is the SOLE non-whitespace content of its
 *  line (so a whole-line block replace is well-formed). The raw source (the
 *  inline link) is revealed while the selection touches the line, so it stays
 *  editable + the chip click still works there — exactly like the callout. */
function buildTableDecorations(
  state: EditorState,
  resolve: ObjectResolver,
  open: ObjectLinkOpener,
): DecorationSet {
  const doc = state.doc.toString();
  const ranges: Range<Decoration>[] = [];
  for (const link of parseObjectLinks(doc)) {
    const obj = resolve(link.id);
    if (!obj || !isTableObjectType(obj.type)) continue;
    const line = state.doc.lineAt(link.from);
    // The link must span the line's full non-whitespace content (a chip sitting
    // alone on its line, as the meal-plan demo lays them out). Otherwise leave
    // it to the inline chip so we never block-replace mid-paragraph.
    if (link.to > line.to) continue; // (defensive — a link never crosses lines)
    const before = doc.slice(line.from, link.from);
    const after = doc.slice(link.to, line.to);
    if (before.trim().length > 0 || after.trim().length > 0) continue;
    // Reveal the raw source on the active line (editable + chip-clickable).
    const touched = state.selection.ranges.some(
      (r) => r.from <= line.to && r.to >= line.from,
    );
    if (touched) continue;
    const inner = /^\[([^\]\n]*)\]/.exec(doc.slice(link.from, link.to))?.[1] ?? "";
    const label = nameFromLabel(inner) || obj.type;
    ranges.push(
      Decoration.replace({
        widget: new ObjectTableWidget(obj, label, open),
        block: true,
      }).range(line.from, line.to),
    );
  }
  const builder = new RangeSetBuilder<Decoration>();
  for (const r of ranges) builder.add(r.from, r.to, r.value);
  return builder.finish();
}

/**
 * The inline derived-object-table extension: a StateField that replaces a
 * grocery-list / cart-preview `obj://` chip-line with a styled table block.
 * Recomputed on every doc/selection change (the selection touch reveals the raw
 * source on the active line) AND on the shared `refreshObjectChips` effect (the
 * live derived-value update path — the SAME nudge the chip plugin listens for).
 * Sourced from a StateField because a block decoration that spans the line may
 * not come from a ViewPlugin (the callout's lesson). `resolve` reads the live
 * replicated store so the table reflects the object's current `data`.
 */
export function objectTable(
  resolve: ObjectResolver,
  open: ObjectLinkOpener,
): Extension {
  const field = StateField.define<DecorationSet>({
    create: (state) => buildTableDecorations(state, resolve, open),
    update(value, tr) {
      const refreshed = tr.effects.some((e) => e.is(refreshObjectChips));
      if (tr.docChanged || tr.selection || refreshed) {
        return buildTableDecorations(tr.state, resolve, open);
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
