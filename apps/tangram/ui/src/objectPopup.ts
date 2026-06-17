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

import type { ObjLink, ObjectType, SmartObject } from "./api";

/** Side effects the object popup needs from the shell. */
export interface ObjectPopupCallbacks {
  /** Persist the edited type/data/links/render (`update_object`). */
  onSave: (objType: string, data: string, links: ObjLink[], render: string) => void;
  /** Delete the object (and let the caller strip the inline link). */
  onDelete: () => void;
  /** Close with no change (Esc / backdrop / Cancel). */
  onClose: () => void;
  /** The known types for the type <select> (read live off the registry). */
  objectTypes: () => ObjectType[];
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

  // ── data textarea ─────────────────────────────────────────────────────────
  const dataRow = document.createElement("label");
  dataRow.className = "object-popup-field";
  dataRow.append("Data");
  const dataArea = document.createElement("textarea");
  dataArea.className = "object-popup-data";
  dataArea.rows = 5;
  dataArea.value = obj.data;
  dataArea.placeholder = "Opaque payload (JSON or plain text)";
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
    const data = dataArea.value;
    const links = parseLinksText(linksArea.value);
    // Keep the existing render hint (SO1 has no render picker; the type default
    // applies on the component side when blank).
    callbacks.onSave(objType, data, links, obj.render);
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
