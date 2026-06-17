// Smart objects SO1 — the basic object popup (docs/design/smart-objects.md): the
// modal opened by clicking an inline `[<label>](obj://<id>)` chip. It loads the
// object from the replicated `objects` store by id and presents a simple
// view/edit form for its `type`, `data`, and `links`. Save → `update_object`;
// Delete → `delete_object` (+ the caller strips the inline link).
//
// This is the SO1 minimum (the generalized analogue of the embedded-runs Run
// editor's Config tab). The richer per-type panels (recipe, grocery-list) +
// reactivity are SO2/SO3. Reuses modal.ts / triggerPopup.ts's overlay language
// (a single `.modal-overlay` appended to <body>, single-instance, Esc/backdrop
// to dismiss).

import type { DeriveSpec, ObjLink, ObjectType, SmartObject } from "./api";

/** Side effects the object popup needs from the shell. */
export interface ObjectPopupCallbacks {
  /** Persist the edited type/data/links/render/derive (`update_object`). The
   *  popup forwards the object's existing `derive` unchanged (SO2 does not edit
   *  the derive rule from this basic popup), so a save never drops the
   *  derived-role wiring and the engine recomputes the cached data. */
  onSave: (
    objType: string,
    data: string,
    links: ObjLink[],
    render: string,
    derive: DeriveSpec | null,
  ) => void;
  /** Delete the object (and let the caller strip the inline link). */
  onDelete: () => void;
  /** Close with no change (Esc / backdrop / Cancel). */
  onClose: () => void;
  /** The known types for the type <select> (read live off the registry). */
  objectTypes: () => ObjectType[];
  /** SO3: all smart objects in the live store (so the recipe card can find the
   *  grocery-list(s) to toggle inclusion against, and a derived view can resolve
   *  a dependency's display name). Optional — SO1/SO2 popups don't need it. */
  allObjects?: () => SmartObject[];
  /** SO3: toggle a `recipe` (this object) in/out of a derived `grocery-list`'s
   *  included set (`toggle_recipe_in_plan`), driving the live recompute of the
   *  grocery-list + cart-preview. Optional — only the recipe card uses it. */
  onToggleRecipe?: (groceryListId: string, include: boolean) => void;
}

let current: { dismiss: () => void } | null = null;

/** True while the object popup is open (used by the editor's click guard). */
export function isObjectPopupOpen(): boolean {
  return current !== null;
}

/** Serialize `links` to the editable one-per-line `rel target [url]` text. */
function linksToText(links: ObjLink[]): string {
  return links
    .map((l) => [l.rel, l.target, l.url ?? ""].filter((s, i) => i < 2 || s).join(" ").trim())
    .join("\n");
}

/** Parse the editable `rel target [url]` lines back into ObjLinks. Blank lines
 *  and lines without at least a rel + target are dropped. */
export function parseLinksText(text: string): ObjLink[] {
  const out: ObjLink[] = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const [rel, target, ...rest] = line.split(/\s+/);
    if (!rel || !target) continue;
    const url = rest.join(" ").trim();
    out.push(url ? { rel, target, url } : { rel, target });
  }
  return out;
}

/**
 * Open the basic object popup for an existing smart object, pre-filled from its
 * store record. Single-instance: any open popup is dismissed first.
 */
export function openObjectPopup(
  obj: SmartObject,
  callbacks: ObjectPopupCallbacks,
): void {
  current?.dismiss();

  const overlay = document.createElement("div");
  overlay.className = "modal-overlay";
  const dialog = document.createElement("div");
  dialog.className = "modal object-popup";
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-label", `Edit smart object ${obj.id}`);
  overlay.appendChild(dialog);

  const title = document.createElement("div");
  title.className = "modal-title";
  title.textContent = "Smart object";
  dialog.appendChild(title);

  // ── type select ──────────────────────────────────────────────────────────
  const typeRow = document.createElement("label");
  typeRow.className = "object-popup-field";
  typeRow.append("Type");
  const typeSelect = document.createElement("select");
  typeSelect.className = "object-popup-type";
  const known = callbacks.objectTypes();
  // Ensure the object's current type is selectable even if unregistered.
  const names = new Set(known.map((t) => t.name));
  const opts: { name: string; label: string }[] = known.map((t) => ({
    name: t.name,
    label: t.label,
  }));
  if (!names.has(obj.type)) opts.unshift({ name: obj.type, label: `${obj.type} (custom)` });
  for (const o of opts) {
    const opt = document.createElement("option");
    opt.value = o.name;
    opt.textContent = o.label;
    if (o.name === obj.type) opt.selected = true;
    typeSelect.appendChild(opt);
  }
  typeRow.appendChild(typeSelect);
  dialog.appendChild(typeRow);

  // ── derived banner (SO2) ───────────────────────────────────────────────────
  // A derived object's `data` is computed + cached by the reactivity engine, so
  // it is shown READ-ONLY here with a clear "derived / auto" notice (and the
  // cycle/error state when broken). Editing the rule itself is out of scope for
  // this basic popup; the existing `derive` is forwarded unchanged on save.
  const isDerived = !!obj.derive;
  if (isDerived) {
    const banner = document.createElement("div");
    banner.className = obj.derive_error
      ? "object-popup-derived object-popup-derived-error"
      : "object-popup-derived";
    if (obj.derive_error) {
      banner.textContent = `⚠ Derive error: ${obj.derive_error}`;
    } else {
      const deps = obj.derive?.deps ?? [];
      banner.textContent = `↻ Derived (${obj.derive?.kind}) — auto-computed from ${deps.length} dependency object${deps.length === 1 ? "" : "ies"}.`;
    }
    dialog.appendChild(banner);
  }

  // ── SO3 rich per-type view (recipe card / grocery table / cart by aisle) ────
  // A functional render toward the §8 mockup (the full two-column + chat-panel
  // mockup is SO4): the recipe shows an expandable card + an Include-in-plan
  // toggle; the grocery-list shows an Item | Qty | From table; the cart-preview
  // groups by aisle. The raw `data` textarea stays below for power-editing.
  const richView = renderRichView(obj, callbacks);
  if (richView) dialog.appendChild(richView);

  // ── data textarea ─────────────────────────────────────────────────────────
  const dataRow = document.createElement("label");
  dataRow.className = "object-popup-field";
  dataRow.append(isDerived ? "Computed data (read-only)" : "Data");
  const dataArea = document.createElement("textarea");
  dataArea.className = "object-popup-data";
  dataArea.rows = 5;
  dataArea.value = obj.data;
  dataArea.placeholder = "Opaque payload (JSON or plain text)";
  // A derived object's data is engine-owned — never hand-edited (a save would be
  // overwritten by the next recompute), so make it read-only.
  if (isDerived) dataArea.readOnly = true;
  dataRow.appendChild(dataArea);
  dialog.appendChild(dataRow);

  // ── links textarea (one `rel target [url]` per line) ───────────────────────
  const linksRow = document.createElement("label");
  linksRow.className = "object-popup-field";
  linksRow.append("Links");
  const linksArea = document.createElement("textarea");
  linksArea.className = "object-popup-links";
  linksArea.rows = 3;
  linksArea.value = linksToText(obj.links);
  linksArea.placeholder = "rel target [url] — one per line";
  linksRow.appendChild(linksArea);
  dialog.appendChild(linksRow);

  const hint = document.createElement("div");
  hint.className = "modal-hint";
  hint.textContent = `id ${obj.id} · render ${obj.render || "chip"}`;
  dialog.appendChild(hint);

  // ── buttons (reuse modal.ts's `.modal-actions` / `.modal-btn` language) ─────
  const buttons = document.createElement("div");
  buttons.className = "modal-actions object-popup-buttons";

  const deleteBtn = document.createElement("button");
  deleteBtn.type = "button";
  deleteBtn.className = "modal-btn danger object-popup-delete";
  deleteBtn.textContent = "Delete";

  const cancelBtn = document.createElement("button");
  cancelBtn.type = "button";
  cancelBtn.className = "modal-btn object-popup-cancel";
  cancelBtn.textContent = "Cancel";

  const saveBtn = document.createElement("button");
  saveBtn.type = "button";
  saveBtn.className = "modal-btn primary object-popup-save";
  saveBtn.textContent = "Save";

  // Delete sits left, Cancel/Save right — push them apart.
  const spacer = document.createElement("div");
  spacer.style.flex = "1";
  buttons.append(deleteBtn, spacer, cancelBtn, saveBtn);
  dialog.appendChild(buttons);

  document.body.appendChild(overlay);
  typeSelect.focus();

  let settled = false;
  const dismiss = () => {
    if (settled) return;
    settled = true;
    document.removeEventListener("keydown", onKey, true);
    overlay.remove();
    if (current?.dismiss === dismiss) current = null;
  };
  const close = () => {
    dismiss();
    callbacks.onClose();
  };
  const save = () => {
    const objType = typeSelect.value.trim() || obj.type;
    // A derived object's data is engine-owned: send the existing cached value so
    // the round-trip is faithful (the component recomputes it anyway).
    const data = isDerived ? obj.data : dataArea.value;
    const links = parseLinksText(linksArea.value);
    // Keep the existing render hint (SO1 has no render picker; the type default
    // applies on the component side when blank) AND forward the existing derive
    // wiring unchanged (SO2 — a save must not drop the derived role).
    callbacks.onSave(objType, data, links, obj.render, obj.derive ?? null);
    dismiss();
  };
  const del = () => {
    callbacks.onDelete();
    dismiss();
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  };
  document.addEventListener("keydown", onKey, true);
  overlay.addEventListener("mousedown", (e) => {
    if (e.target === overlay) close();
  });
  cancelBtn.addEventListener("click", close);
  saveBtn.addEventListener("click", save);
  deleteBtn.addEventListener("click", del);

  current = { dismiss };
}

// ── SO3 rich per-type rendering (docs/design/smart-objects.md §8) ─────────────

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

/** Best-effort parse of an object's JSON `data` (never throws — a malformed
 *  payload yields null and the rich view is skipped). */
function parseData<T>(data: string): T | null {
  try {
    const v = JSON.parse(data || "null");
    return v && typeof v === "object" ? (v as T) : null;
  } catch {
    return null;
  }
}

/** Format a quantity + unit compactly (`4 tbsp`, `3 ct`, `5`); a unitless count
 *  drops the unit. */
function qtyLabel(quantity: number | undefined, unit: string | undefined): string {
  const q = typeof quantity === "number" ? String(quantity) : "?";
  const u = (unit ?? "").trim();
  return u ? `${q} ${u}` : q;
}

/** Build the SO3 rich view for a recipe / grocery-list / cart-preview object,
 *  or null for a type with no dedicated view (the raw data textarea suffices). */
function renderRichView(
  obj: SmartObject,
  callbacks: ObjectPopupCallbacks,
): HTMLElement | null {
  switch (obj.type) {
    case "recipe":
      return renderRecipeCard(obj, callbacks);
    case "grocery-list":
      return renderGroceryTable(obj);
    case "cart-preview":
      return renderCartPreview(obj);
    default:
      return null;
  }
}

/** A recipe as an expandable card: name + servings, the ingredient list, and an
 *  Include-in-plan toggle per derived grocery-list that could include it (SO3). */
function renderRecipeCard(
  obj: SmartObject,
  callbacks: ObjectPopupCallbacks,
): HTMLElement {
  const recipe = parseData<RecipeData>(obj.data);
  const card = document.createElement("div");
  card.className = "object-rich object-recipe-card";

  const details = document.createElement("details");
  details.open = true;
  const summary = document.createElement("summary");
  summary.className = "object-recipe-summary";
  const servings = recipe?.servings ? ` · serves ${recipe.servings}` : "";
  summary.textContent = `${recipe?.name?.trim() || "Recipe"}${servings}`;
  details.appendChild(summary);

  const list = document.createElement("ul");
  list.className = "object-recipe-ingredients";
  const ingredients = recipe?.ingredients ?? [];
  if (ingredients.length === 0) {
    const li = document.createElement("li");
    li.className = "object-rich-empty";
    li.textContent = "No ingredients yet — edit the data below.";
    list.appendChild(li);
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
      list.appendChild(li);
    }
  }
  details.appendChild(list);
  card.appendChild(details);

  // Include-in-plan toggles: one per grocery-list in the store (SO3 — the
  // reactive meal-plan affordance). Toggling drives `toggle_recipe_in_plan`,
  // which recomputes the grocery-list + cart-preview live.
  const lists = (callbacks.allObjects?.() ?? []).filter(
    (o) => o.type === "grocery-list" && !!o.derive,
  );
  if (lists.length > 0 && callbacks.onToggleRecipe) {
    const planRow = document.createElement("div");
    planRow.className = "object-recipe-plan";
    for (const gl of lists) {
      const label = document.createElement("label");
      label.className = "object-recipe-plan-toggle";
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.className = "object-recipe-include";
      cb.checked = (gl.derive?.deps ?? []).includes(obj.id);
      cb.addEventListener("change", () => {
        callbacks.onToggleRecipe?.(gl.id, cb.checked);
      });
      label.append(cb, ` Include in plan`);
      planRow.appendChild(label);
    }
    card.appendChild(planRow);
  }
  return card;
}

/** A grocery-list as a table: Item | Qty | From "N recipes" (SO3). Shows the
 *  auto-synced affordance — it's a derived object. */
function renderGroceryTable(obj: SmartObject): HTMLElement {
  const data = parseData<{ rows?: GroceryRow[] }>(obj.data);
  const rows = data?.rows ?? [];
  const wrap = document.createElement("div");
  wrap.className = "object-rich object-grocery";

  const synced = document.createElement("div");
  synced.className = "object-rich-synced";
  synced.textContent = "↻ Auto-synced from the included recipes";
  wrap.appendChild(synced);

  if (rows.length === 0) {
    const empty = document.createElement("div");
    empty.className = "object-rich-empty";
    empty.textContent = "No items — include a recipe in the plan.";
    wrap.appendChild(empty);
    return wrap;
  }

  const table = document.createElement("table");
  table.className = "object-grocery-table";
  const thead = document.createElement("thead");
  thead.innerHTML = "<tr><th>Item</th><th>Qty</th><th>From</th></tr>";
  table.appendChild(thead);
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
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);
  wrap.appendChild(table);
  return wrap;
}

/** A cart-preview grouped by aisle (SO3): each aisle a heading + its items. */
function renderCartPreview(obj: SmartObject): HTMLElement {
  const data = parseData<{ aisles?: CartAisle[] }>(obj.data);
  const aisles = data?.aisles ?? [];
  const wrap = document.createElement("div");
  wrap.className = "object-rich object-cart";

  const synced = document.createElement("div");
  synced.className = "object-rich-synced";
  synced.textContent = "↻ Auto-synced from the grocery list";
  wrap.appendChild(synced);

  if (aisles.length === 0) {
    const empty = document.createElement("div");
    empty.className = "object-rich-empty";
    empty.textContent = "Empty cart — add items to the grocery list.";
    wrap.appendChild(empty);
    return wrap;
  }

  for (const aisle of aisles) {
    const group = document.createElement("div");
    group.className = "object-cart-aisle";
    const heading = document.createElement("div");
    heading.className = "object-cart-aisle-name";
    heading.textContent = (aisle.category ?? "").trim() || "Other";
    group.appendChild(heading);
    const list = document.createElement("ul");
    list.className = "object-cart-items";
    for (const it of aisle.items ?? []) {
      const li = document.createElement("li");
      li.textContent = `${(it.name ?? "").trim() || "item"} — ${qtyLabel(it.quantity, it.unit)}`;
      list.appendChild(li);
    }
    group.appendChild(list);
    wrap.appendChild(group);
  }
  return wrap;
}
