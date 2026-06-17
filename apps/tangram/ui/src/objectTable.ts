// Smart objects SO3/SO5 — inline SMART-OBJECT BLOCK CARDS (the §8 meal-plan
// mockup, docs/design/smart-objects.md §8).
//
// A meal-plan smart object (`recipe` / `grocery-list` / `cart-preview`) used to
// render as the same compact `obj://` chip every other object gets — you had to
// CLICK it to see anything, hiding the whole point of the reactive
// recipe→grocery→cart chain (the "document recalculates itself" moment). This
// module renders those types INLINE, as a styled BLOCK in place of the chip, so
// the content is visible without a click and recomputes live in the doc:
//
//   - recipe       → a §8 recipe CARD: a header row (an include-in-plan
//                    checkbox + the purple chip pill `🍳 Name` + a chevron) over
//                    an expandable ingredient list. SO5.
//   - grocery-list → an Item | Qty | From "N recipes" table with an auto-synced
//                    badge. SO3, refined to §8 here.
//   - cart-preview → grouped-by-aisle, PLUS the §8 ACTION footer: a full-width
//                    "Fill Whole Foods cart · N items" button that streams the §3
//                    pipeline phases and ends on a review-only terminus (a STUB —
//                    never purchases), and a light chat affordance. SO5.
//
// Modelled on `callout.ts` (R3): a block `Decoration.replace` that spans the
// chip's line MUST come from a StateField, not a ViewPlugin (CM6 forbids a
// ViewPlugin from sourcing a decoration that crosses a line break). So this is a
// StateField, parallel to `runCalloutCard`. It owns the meal-plan-typed objects
// (recipe / grocery-list / cart-preview); the inline chip ViewPlugin
// (`objectChip` in objectLink.ts) skips those ids via {@link isCardObjectType}
// so the two never double-decorate the same range.
//
// Live update: the StateField recomputes on `tr.docChanged || tr.selection`
// (the editable-on-the-active-line reveal, like livePreview) AND on the shared
// `refreshObjectChips` effect — the SAME no-doc-change nudge the chip plugin
// listens for — so a card re-renders the instant the reactivity engine
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
import type { CartFillResult, SmartObject } from "./api";
import {
  DERIVED_BADGE,
  DERIVED_ERROR_GLYPH,
  type ObjectLinkOpener,
  type ObjectResolver,
  glyphForType,
  isCardObjectType,
  parseObjectLinks,
  refreshObjectChips,
} from "./objectLink";

// ── §8 meal-plan callbacks (SO5) ──────────────────────────────────────────────

/** The live-store + action hooks the §8 meal-plan cards need from the shell.
 *  All optional — with none wired the recipe card's checkbox/the cart Action are
 *  inert (a harmless no-op for non-vault editors, and for the SO3 fallback). */
export interface MealPlanCallbacks {
  /** Every smart object in the live store — used by the recipe card to find the
   *  derived `grocery-list` to toggle inclusion against, and by the chat
   *  affordance. */
  allObjects?: () => SmartObject[];
  /** Toggle a recipe in/out of a derived grocery-list's plan (the include
   *  checkbox). `recipeId` is the recipe object id; `groceryListId` the derived
   *  grocery-list; `include` the new state. Drives the live recompute. */
  onToggleRecipe?: (groceryListId: string, recipeId: string, include: boolean) => void;
  /** Run the SO5 cart-fill STUB action for the cart-preview `cartId`. Resolves
   *  with the §3 phases + the review-only terminus message + the live item
   *  count. NEVER purchases (the stub does no I/O). Drives the §8 Action stream. */
  onFillCart?: (cartId: string) => Promise<CartFillResult>;
  /** The §8 "Add 'Tacos' via chat" demo: inject a 4th recipe and toggle it into
   *  the plan, re-syncing the chain (chat = graph mutation). The card supplies
   *  the grocery-list id to toggle against. */
  onAddViaChat?: (groceryListId: string) => void;
}

// ── the parsed `data` shapes (mirror objectPopup.ts / the component) ──────────

/** One ingredient line on a recipe's `data` (the SO3 manual-entry shape). */
interface RecipeIngredient {
  canonicalName?: string;
  name?: string;
  quantity?: number;
  unit?: string;
  category?: string;
}
interface RecipeData {
  name?: string;
  servings?: number;
  ingredients?: RecipeIngredient[];
  source?: string;
}
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
  return label.replace(/^\s*[◆🍳🛒🧺]\s*/u, "").trim();
}

/** The live grocery row count across the store — the §8 "· N items" figure (the
 *  number of rows the cart would hold). Reads the derived grocery-list a cart
 *  depends on, else the first derived grocery-list. Best-effort (0 when none). */
function liveItemCount(cart: SmartObject, all: SmartObject[]): number {
  const depId = cart.derive?.deps?.[0];
  const gl =
    (depId ? all.find((o) => o.id === depId) : undefined) ??
    all.find((o) => o.type === "grocery-list" && !!o.derive);
  const rows = gl ? parseData<{ rows?: GroceryRow[] }>(gl.data)?.rows ?? [] : [];
  return rows.length;
}

// ── the §8 derived purple chip pill (shared header element) ───────────────────

/** The §8 purple chip pill (`🍳/🛒/🧺 Name` with the `#534AB7` dot) that heads a
 *  card — the smart-object accent affordance. */
function chipPill(obj: SmartObject, label: string): HTMLElement {
  const pill = document.createElement("span");
  pill.className = "object-card-pill";
  const dot = document.createElement("span");
  dot.className = "object-card-dot";
  pill.append(dot, `${glyphForType(obj.type)} ${label}`);
  return pill;
}

/** The "auto-synced ↻" success badge (or a derive-error badge) for a derived
 *  card. */
function syncedBadge(obj: SmartObject): HTMLElement {
  const badge = document.createElement("span");
  if (obj.derive_error) {
    badge.className = "object-card-badge object-card-badge-error";
    badge.textContent = `${DERIVED_ERROR_GLYPH} derive error`;
    badge.title = obj.derive_error;
  } else {
    badge.className = "object-card-badge";
    badge.textContent = `auto-synced ${DERIVED_BADGE}`;
    badge.title = "Derived — recomputed automatically from its dependencies";
  }
  return badge;
}

/** A small "Edit" affordance that opens the object popup (a card is otherwise
 *  inert, unlike the chip it replaced). */
function editButton(id: string, open: ObjectLinkOpener): HTMLElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "object-card-open";
  btn.textContent = "Edit";
  btn.title = "Open the smart object";
  btn.addEventListener("mousedown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    open(id);
  });
  return btn;
}

// ── §8 recipe CARD (SO5) ──────────────────────────────────────────────────────

/**
 * Build a §8 recipe CARD: a header row (an include-in-plan accent checkbox + the
 * purple chip pill + a right-aligned chevron) over an expandable indented
 * ingredient list. Clicking the HEADER (not the checkbox) toggles expansion;
 * the CHECKBOX toggles inclusion in the plan → the chain recomputes. Exported so
 * it can be unit-tested directly.
 */
export function renderRecipeCard(
  obj: SmartObject,
  label: string,
  open: ObjectLinkOpener,
  cbs: MealPlanCallbacks,
): HTMLElement {
  const recipe = parseData<RecipeData>(obj.data);
  const card = document.createElement("div");
  card.className = "object-card object-card-recipe";
  card.dataset.objectId = obj.id;
  card.dataset.objectType = obj.type;

  // The grocery-list this recipe toggles into (the first derived one). The
  // checkbox is shown only when a target grocery-list + toggle hook exist.
  const all = cbs.allObjects?.() ?? [];
  const grocery = all.find((o) => o.type === "grocery-list" && !!o.derive);
  const included = (grocery?.derive?.deps ?? []).includes(obj.id);

  // ── header row ──────────────────────────────────────────────────────────
  const head = document.createElement("div");
  head.className = "object-card-head object-recipe-head";

  if (grocery && cbs.onToggleRecipe) {
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.className = "object-recipe-include";
    cb.checked = included;
    cb.title = "Include in plan";
    cb.setAttribute("aria-label", `Include ${recipe?.name ?? "recipe"} in plan`);
    // The checkbox toggles inclusion — and must NOT also toggle expansion.
    cb.addEventListener("mousedown", (e) => e.stopPropagation());
    cb.addEventListener("click", (e) => e.stopPropagation());
    cb.addEventListener("change", () => {
      cbs.onToggleRecipe?.(grocery.id, obj.id, cb.checked);
    });
    head.append(cb);
  }

  const servings = recipe?.servings ? ` · serves ${recipe.servings}` : "";
  head.append(chipPill(obj, `${nameFromLabel(label)}${servings}`));

  const chevron = document.createElement("span");
  chevron.className = "object-recipe-chevron";
  chevron.textContent = "▾";
  head.append(chevron);
  head.append(editButton(obj.id, open));
  card.append(head);

  // ── expandable ingredient list ──────────────────────────────────────────
  const list = document.createElement("ul");
  list.className = "object-recipe-ingredients";
  const ingredients = recipe?.ingredients ?? [];
  if (ingredients.length === 0) {
    const li = document.createElement("li");
    li.className = "object-card-empty";
    li.textContent = "No ingredients yet — Edit to add some.";
    list.append(li);
  } else {
    for (const ing of ingredients) {
      const li = document.createElement("li");
      const name = (ing.canonicalName ?? ing.name ?? "").trim() || "ingredient";
      li.append(`${name} — ${qtyLabel(ing.quantity, ing.unit)}`);
      if (ing.category) {
        const tag = document.createElement("span");
        tag.className = "object-recipe-aisle";
        tag.textContent = ing.category;
        li.append(" ", tag);
      }
      list.append(li);
    }
  }
  card.append(list);

  // Clicking the header (but not the checkbox/Edit) expands/collapses.
  let expanded = true;
  const setExpanded = (v: boolean) => {
    expanded = v;
    card.classList.toggle("object-recipe-collapsed", !expanded);
    chevron.textContent = expanded ? "▾" : "▸";
  };
  setExpanded(true);
  head.addEventListener("mousedown", (e) => {
    const t = e.target as HTMLElement;
    if (t.closest(".object-recipe-include") || t.closest(".object-card-open")) return;
    e.preventDefault();
    setExpanded(!expanded);
  });

  return card;
}

// ── grocery-list TABLE (SO3, refined to §8) ───────────────────────────────────

/** A grocery-list as an Item | Qty | From table (From = "N recipes" from
 *  `sources`). */
function renderGroceryBody(obj: SmartObject): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "object-card-body";
  const data = parseData<{ rows?: GroceryRow[] }>(obj.data);
  const rows = data?.rows ?? [];
  if (rows.length === 0) {
    const empty = document.createElement("div");
    empty.className = "object-card-empty";
    empty.textContent = "No items — include a recipe in the plan.";
    wrap.append(empty);
    return wrap;
  }
  const table = document.createElement("table");
  table.className = "object-card-grocery";
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

// ── cart-preview (SO3) + the §8 ACTION footer (SO5) ───────────────────────────

/** A cart-preview grouped by aisle (`category`): each aisle a small labeled
 *  group of its items. */
function renderCartBody(obj: SmartObject): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "object-card-body";
  const data = parseData<{ aisles?: CartAisle[] }>(obj.data);
  const aisles = data?.aisles ?? [];
  if (aisles.length === 0) {
    const empty = document.createElement("div");
    empty.className = "object-card-empty";
    empty.textContent = "Empty cart — add items to the grocery list.";
    wrap.append(empty);
    return wrap;
  }
  for (const aisle of aisles) {
    const group = document.createElement("div");
    group.className = "object-card-aisle";
    const name = document.createElement("div");
    name.className = "object-card-aisle-name";
    name.textContent = (aisle.category ?? "").trim() || "Other";
    group.append(name);
    const ul = document.createElement("ul");
    ul.className = "object-card-aisle-items";
    for (const it of aisle.items ?? []) {
      const li = document.createElement("li");
      li.textContent = `${(it.name ?? "").trim() || "item"} — ${qtyLabel(it.quantity, it.unit)}`;
      ul.append(li);
    }
    group.append(ul);
    wrap.append(group);
  }
  return wrap;
}

/**
 * Stream the §8 cart-fill phases into `status`, one green-check line per phase,
 * ending on a lock-icon review terminus. Exported + returns a Promise so a test
 * can await the full sequence deterministically. `delayMs` paces the stream
 * (0 in tests for an instant resolve). NEVER triggers a purchase — it only
 * renders the STUB result the component returned.
 */
export async function streamCartFill(
  status: HTMLElement,
  result: CartFillResult,
  delayMs = 380,
): Promise<void> {
  status.replaceChildren();
  status.classList.add("object-action-status-active");
  const wait = (ms: number) =>
    ms > 0 ? new Promise<void>((r) => setTimeout(r, ms)) : Promise.resolve();
  for (const phase of result.phases) {
    const line = document.createElement("div");
    line.className = "object-action-phase";
    const check = document.createElement("span");
    check.className = "object-action-check";
    check.textContent = "✓";
    line.append(check, ` ${phase}`);
    status.append(line);
    // biome-ignore lint/nursery/noAwaitInLoop: a paced stream is the point.
    await wait(delayMs);
  }
  // The review-only terminus: a lock icon + "nothing purchased" message. Always
  // shown — even though `purchased` is structurally always false, we surface it
  // so the no-purchase guarantee is visible.
  const review = document.createElement("div");
  review.className = "object-action-review";
  review.textContent = `🔒 ${result.review_message}`;
  status.append(review);
}

/** Build the §8 ACTION footer for a cart-preview card: a full-width accent
 *  button "Fill Whole Foods cart · N items" that streams the phase list into a
 *  status area on click and re-enables after. A STUB — never purchases. */
function renderActionFooter(
  obj: SmartObject,
  all: SmartObject[],
  cbs: MealPlanCallbacks,
): HTMLElement {
  const footer = document.createElement("div");
  footer.className = "object-action";

  const count = liveItemCount(obj, all);
  const button = document.createElement("button");
  button.type = "button";
  button.className = "object-action-btn";
  const setLabel = (n: number) => {
    button.textContent = `🛒 Fill Whole Foods cart · ${n} item${n === 1 ? "" : "s"}`;
  };
  setLabel(count);

  const status = document.createElement("div");
  status.className = "object-action-status";

  button.addEventListener("mousedown", (e) => {
    // Don't let the click bubble to the editor's caret placement / the card
    // header. The button owns the gesture.
    e.preventDefault();
    e.stopPropagation();
  });
  button.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (button.disabled || !cbs.onFillCart) return;
    button.disabled = true;
    void cbs
      .onFillCart(obj.id)
      .then((result) => streamCartFill(status, result))
      .catch((err) => {
        status.replaceChildren();
        const line = document.createElement("div");
        line.className = "object-action-review object-action-error";
        line.textContent = `Cart-fill failed: ${err instanceof Error ? err.message : String(err)}`;
        status.append(line);
      })
      .finally(() => {
        button.disabled = false;
      });
  });

  footer.append(button, status);
  return footer;
}

/** Build the §8 light CHAT affordance shown under the cart Action: two seeded
 *  copilot messages + an "Add 'Tacos' via chat" button that injects a 4th recipe
 *  and re-syncs the chain (chat = graph mutation). Kept INSIDE the document
 *  column (not a second sidebar) so it never competes with the existing
 *  right-sidebar copilot. */
function renderChatAffordance(
  all: SmartObject[],
  cbs: MealPlanCallbacks,
): HTMLElement | null {
  const grocery = all.find((o) => o.type === "grocery-list" && !!o.derive);
  if (!grocery || !cbs.onAddViaChat) return null;

  const wrap = document.createElement("div");
  wrap.className = "object-chat-demo";

  const head = document.createElement("div");
  head.className = "object-chat-demo-head";
  head.textContent = "💬 Copilot";
  wrap.append(head);

  for (const [role, text] of [
    ["assistant", "Your plan looks balanced. Want me to add a protein?"],
    ["user", "Add tacos for Friday."],
  ] as const) {
    const msg = document.createElement("div");
    msg.className = `object-chat-demo-msg object-chat-demo-${role}`;
    msg.textContent = text;
    wrap.append(msg);
  }

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "object-chat-demo-btn";
  btn.textContent = "Add 'Tacos' via chat";
  btn.title = "Inject a Tacos recipe and re-sync the chain";
  btn.addEventListener("mousedown", (e) => {
    e.preventDefault();
    e.stopPropagation();
  });
  btn.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    cbs.onAddViaChat?.(grocery.id);
  });
  wrap.append(btn);
  return wrap;
}

// ── the shared card builder ───────────────────────────────────────────────────

/** Build the whole meal-plan card for one resolved object (shared by the block
 *  widget). Dispatches on type: a §8 recipe card, a grocery table, or a cart
 *  preview WITH the §8 Action footer + chat affordance. `label` is the chip's
 *  display label; `open` opens the popup; `cbs` carries the SO5 hooks. */
export function renderObjectTableCard(
  obj: SmartObject,
  label: string,
  open: ObjectLinkOpener,
  cbs: MealPlanCallbacks = {},
): HTMLElement {
  if (obj.type === "recipe") return renderRecipeCard(obj, label, open, cbs);

  const card = document.createElement("div");
  card.className = `object-card object-card-${obj.type}`;
  if (obj.derive_error) card.classList.add("object-card-error");
  card.dataset.objectId = obj.id;
  card.dataset.objectType = obj.type;

  // Header: the purple chip pill + the auto-synced badge + an Edit affordance.
  const head = document.createElement("div");
  head.className = "object-card-head";
  head.append(chipPill(obj, nameFromLabel(label)), syncedBadge(obj), editButton(obj.id, open));
  card.append(head);

  card.append(obj.type === "cart-preview" ? renderCartBody(obj) : renderGroceryBody(obj));

  // §8 ACTION + chat affordance live on the cart-preview (the chain terminus).
  if (obj.type === "cart-preview") {
    const all = cbs.allObjects?.() ?? [];
    card.append(renderActionFooter(obj, all, cbs));
    const chat = renderChatAffordance(all, cbs);
    if (chat) card.append(chat);
  }
  return card;
}

// ── the CM6 block widget + StateField ─────────────────────────────────────────

/** A small structural signature of an object that, when it changes, must
 *  re-render the card. For a cart-preview we also fold in the live grocery row
 *  count so the Action button's "· N items" tracks the plan. */
function cardSignature(obj: SmartObject, cbs: MealPlanCallbacks): string {
  let extra = "";
  if (obj.type === "recipe") {
    const grocery = (cbs.allObjects?.() ?? []).find(
      (o) => o.type === "grocery-list" && !!o.derive,
    );
    extra = `inc:${(grocery?.derive?.deps ?? []).includes(obj.id)}`;
  } else if (obj.type === "cart-preview") {
    extra = `n:${liveItemCount(obj, cbs.allObjects?.() ?? [])}`;
  }
  return `${obj.id}|${obj.type}|${obj.data}|${obj.derive_error ?? ""}|${extra}`;
}

/** The block widget rendering one meal-plan card. Recreated (CM6 reuses the DOM
 *  only on `eq`) when the object's signature changes — so a recompute / a plan
 *  toggle re-renders. */
class ObjectTableWidget extends WidgetType {
  readonly sig: string;
  constructor(
    readonly obj: SmartObject,
    readonly label: string,
    readonly open: ObjectLinkOpener,
    readonly cbs: MealPlanCallbacks,
  ) {
    super();
    this.sig = cardSignature(obj, cbs);
  }
  eq(other: ObjectTableWidget): boolean {
    return other.sig === this.sig && other.label === this.label;
  }
  toDOM(): HTMLElement {
    return renderObjectTableCard(this.obj, this.label, this.open, this.cbs);
  }
  ignoreEvent(): boolean {
    return false;
  }
}

/** Build the block card replace-decorations from a state. A link renders as a
 *  block card only when (a) its resolved object is a meal-plan card type
 *  (recipe / grocery-list / cart-preview) AND (b) the link is the SOLE
 *  non-whitespace content of its line, allowing only a leading list marker
 *  (`- `, `* `, `1. `) before it (so a clean recipe bullet still becomes a
 *  card). The raw source is revealed while the selection touches the line, so it
 *  stays editable + the chip click still works there — exactly like the callout. */
function buildTableDecorations(
  state: EditorState,
  resolve: ObjectResolver,
  open: ObjectLinkOpener,
  cbs: MealPlanCallbacks,
): DecorationSet {
  const doc = state.doc.toString();
  const ranges: Range<Decoration>[] = [];
  for (const link of parseObjectLinks(doc)) {
    const obj = resolve(link.id);
    if (!obj || !isCardObjectType(obj.type)) continue;
    const line = state.doc.lineAt(link.from);
    if (link.to > line.to) continue; // (defensive — a link never crosses lines)
    const before = doc.slice(line.from, link.from);
    const after = doc.slice(link.to, line.to);
    // Allow only whitespace OR a leading list marker before the link; nothing
    // after it. Otherwise leave it to the inline chip (never block mid-paragraph).
    const beforeOk = /^\s*(?:[-*]\s+|\d+\.\s+)?$/.test(before);
    if (!beforeOk || after.trim().length > 0) continue;
    // Reveal the raw source on the active line (editable + chip-clickable).
    const touched = state.selection.ranges.some(
      (r) => r.from <= line.to && r.to >= line.from,
    );
    if (touched) continue;
    const inner = /^\[([^\]\n]*)\]/.exec(doc.slice(link.from, link.to))?.[1] ?? "";
    const label = nameFromLabel(inner) || obj.type;
    ranges.push(
      Decoration.replace({
        widget: new ObjectTableWidget(obj, label, open, cbs),
        block: true,
      }).range(line.from, line.to),
    );
  }
  const builder = new RangeSetBuilder<Decoration>();
  for (const r of ranges) builder.add(r.from, r.to, r.value);
  return builder.finish();
}

/**
 * The inline meal-plan-card extension: a StateField that replaces a
 * recipe / grocery-list / cart-preview `obj://` chip-line with a styled §8
 * BLOCK card. Recomputed on every doc/selection change (the selection touch
 * reveals the raw source on the active line) AND on the shared
 * `refreshObjectChips` effect (the live derived-value update path — the SAME
 * nudge the chip plugin listens for). Sourced from a StateField because a block
 * decoration that spans the line may not come from a ViewPlugin (the callout's
 * lesson). `resolve` reads the live replicated store so the card reflects the
 * object's current `data`; `cbs` carries the SO5 toggle/Action/chat hooks.
 */
export function objectTable(
  resolve: ObjectResolver,
  open: ObjectLinkOpener,
  cbs: MealPlanCallbacks = {},
): Extension {
  const field = StateField.define<DecorationSet>({
    create: (state) => buildTableDecorations(state, resolve, open, cbs),
    update(value, tr) {
      const refreshed = tr.effects.some((e) => e.is(refreshObjectChips));
      if (tr.docChanged || tr.selection || refreshed) {
        return buildTableDecorations(tr.state, resolve, open, cbs);
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
