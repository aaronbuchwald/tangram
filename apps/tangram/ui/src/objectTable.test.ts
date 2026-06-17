// Tests for the SO3 inline derived-object TABLES + the full-chip click + the
// restored mac Cmd-Backspace line-delete (the smart-object-ux fix bundle):
//
//  - objectTable renders a grocery-list as an Item | Qty | From table block and
//    a cart-preview grouped by aisle, from the object's cached `data` (no
//    component change — UI reads the existing shape).
//  - the inline-chip ViewPlugin SKIPS those table types (so the two decoration
//    sources never collide), and the table still opens the popup on its Edit
//    affordance + updates live on a refreshObjectChips nudge.
//  - a click ANYWHERE on a (non-table) chip opens the popup.
//  - the editor binds Cmd-Backspace → delete-to-line-start.

import { describe, expect, it, vi } from "vitest";
import { keymap } from "@codemirror/view";
import { renderObjectTableCard } from "./objectTable";
import { buildObjectLink, isTableObjectType } from "./objectLink";
import { MdEditor } from "./editor";
import type { SmartObject } from "./api";

const obj = (over: Partial<SmartObject> = {}): SmartObject => ({
  id: "o1",
  type: "tag",
  data: "",
  links: [],
  render: "chip",
  ...over,
});

const grocery = (over: Partial<SmartObject> = {}): SmartObject =>
  obj({
    id: "meal-plan-grocery",
    type: "grocery-list",
    derive: { kind: "grocery-list", deps: ["recipe-pasta", "recipe-soup"] },
    data: JSON.stringify({
      rows: [
        { name: "olive oil", quantity: 5, unit: "tbsp", sources: ["recipe-pasta", "recipe-soup"] },
        { name: "onion", quantity: 2, unit: "ct", sources: ["recipe-pasta"] },
      ],
    }),
    ...over,
  });

const cart = (over: Partial<SmartObject> = {}): SmartObject =>
  obj({
    id: "meal-plan-cart",
    type: "cart-preview",
    derive: { kind: "cart-preview", deps: ["meal-plan-grocery"] },
    data: JSON.stringify({
      aisles: [
        { category: "Produce", items: [{ name: "onion", quantity: 2, unit: "ct" }] },
        { category: "Pantry", items: [{ name: "olive oil", quantity: 5, unit: "tbsp" }] },
      ],
    }),
    ...over,
  });

describe("isTableObjectType", () => {
  it("matches the two derived table types (case-insensitive) and nothing else", () => {
    expect(isTableObjectType("grocery-list")).toBe(true);
    expect(isTableObjectType("cart-preview")).toBe(true);
    expect(isTableObjectType("Grocery-List")).toBe(true);
    expect(isTableObjectType("recipe")).toBe(false);
    expect(isTableObjectType("tag")).toBe(false);
    expect(isTableObjectType(null)).toBe(false);
    expect(isTableObjectType(undefined)).toBe(false);
  });
});

describe("renderObjectTableCard — grocery-list (Item | Qty | From)", () => {
  it("renders the cached rows as a table with a From = 'N recipes' column", () => {
    const card = renderObjectTableCard(grocery(), "Grocery list", () => {});
    expect(card.classList.contains("object-table-grocery-list")).toBe(true);
    const head = card.querySelectorAll("thead th");
    expect([...head].map((th) => th.textContent)).toEqual(["Item", "Qty", "From"]);
    const rows = card.querySelectorAll("tbody tr");
    expect(rows.length).toBe(2);
    const firstCells = [...rows[0].querySelectorAll("td")].map((td) => td.textContent);
    expect(firstCells).toEqual(["olive oil", "5 tbsp", "2 recipes"]);
    expect([...rows[1].querySelectorAll("td")].map((td) => td.textContent)).toEqual([
      "onion",
      "2 ct",
      "1 recipe",
    ]);
    // The auto-synced (derived) badge is present.
    expect(card.querySelector(".object-table-badge")?.textContent).toContain("auto-synced");
  });

  it("shows an empty hint when the derived data has no rows", () => {
    const card = renderObjectTableCard(grocery({ data: '{"rows":[]}' }), "Grocery list", () => {});
    expect(card.querySelector(".object-table-empty")?.textContent).toContain("include a recipe");
  });

  it("the Edit affordance opens the popup for the object id", () => {
    const open = vi.fn();
    const card = renderObjectTableCard(grocery(), "Grocery list", open);
    const btn = card.querySelector<HTMLElement>(".object-table-open");
    btn?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(open).toHaveBeenCalledWith("meal-plan-grocery");
  });
});

describe("renderObjectTableCard — cart-preview (grouped by aisle)", () => {
  it("renders one labeled group per aisle with its items", () => {
    const card = renderObjectTableCard(cart(), "Cart preview", () => {});
    expect(card.classList.contains("object-table-cart-preview")).toBe(true);
    const aisles = card.querySelectorAll(".object-table-aisle");
    expect(aisles.length).toBe(2);
    expect(aisles[0].querySelector(".object-table-aisle-name")?.textContent).toBe("Produce");
    expect(aisles[0].querySelectorAll("li").length).toBe(1);
    expect(aisles[0].querySelector("li")?.textContent).toBe("onion — 2 ct");
    expect(aisles[1].querySelector(".object-table-aisle-name")?.textContent).toBe("Pantry");
  });

  it("shows an error badge for a derived object in error", () => {
    const card = renderObjectTableCard(
      cart({ derive_error: "dependency cycle" }),
      "Cart preview",
      () => {},
    );
    expect(card.classList.contains("object-table-error")).toBe(true);
    expect(card.querySelector(".object-table-badge-error")?.textContent).toContain("derive error");
  });
});

// ── editor integration: the table block decoration + the chip skip ────────────

/** Mount an MdEditor whose resolver returns `record` for ANY id, with the
 *  doc holding a single `obj://o1` link on its own line. */
function mountWithDoc(doc: string, resolve: (id: string) => SmartObject | null, onOpen = () => {}) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  return new MdEditor(
    host,
    doc,
    () => {},
    () => {},
    () => false,
    () => false,
    () => [],
    () => null,
    () => {},
    () => [],
    () => null,
    () => {},
    () => null,
    () => {},
    () => {},
    () => [],
    () => {},
    onOpen, // onOpenObjectLink
    resolve, // resolveObject
  );
}

const LINK = buildObjectLink("Grocery list", "meal-plan-grocery");

describe("objectTable editor integration", () => {
  it("replaces a grocery-list chip line with a table BLOCK (not the inline chip)", () => {
    // The link sits alone on its own line below a paragraph (the demo layout).
    const doc = `## Grocery list\n\n${LINK}\n\nmore`;
    const editor = mountWithDoc(doc, () => grocery());
    // The block table rendered; the inline chip did NOT (skipped by the plugin).
    expect(editor.view.dom.querySelector(".object-table-card")).not.toBeNull();
    expect(editor.view.dom.querySelector(".cm-object-link")).toBeNull();
    // Item | Qty | From is visible without a click.
    expect(editor.view.dom.querySelector(".object-table-grocery")).not.toBeNull();
    editor.destroy();
  });

  it("a non-table object keeps the inline chip (no table block)", () => {
    const doc = `before ${buildObjectLink("urgent", "o1")} after`;
    const editor = mountWithDoc(doc, () => obj({ id: "o1", type: "tag" }));
    expect(editor.view.dom.querySelector(".object-table-card")).toBeNull();
    expect(editor.view.dom.querySelector(".cm-object-link")).not.toBeNull();
    editor.destroy();
  });

  it("updates the table live when the resolved derived data recomputes", () => {
    let record = grocery({ data: '{"rows":[{"name":"onion","sources":["a"]}]}' });
    // The link sits on its own line, NOT the active (cursor) line, so the block
    // table renders (the active line reveals raw source, like livePreview).
    const doc = `head\n\n${LINK}\n\ntail`;
    const host = document.createElement("div");
    document.body.appendChild(host);
    const editor = new MdEditor(
      host,
      doc,
      () => {},
      () => {},
      () => false,
      () => false,
      () => [],
      () => null,
      () => {},
      () => [],
      () => null,
      () => {},
      () => null,
      () => {},
      () => {},
      () => [],
      () => {},
      () => {},
      () => record,
    );
    expect(editor.view.dom.querySelectorAll(".object-table-grocery tbody tr").length).toBe(1);
    // The engine recomputed → two rows now. The refreshObjectChips nudge (the
    // same path the chip plugin uses) rebuilds the table StateField.
    record = grocery({
      data: '{"rows":[{"name":"onion","sources":["a"]},{"name":"tomato","sources":["a","b"]}]}',
    });
    editor.refreshObjectChips();
    expect(editor.view.dom.querySelectorAll(".object-table-grocery tbody tr").length).toBe(2);
    editor.destroy();
  });
});

// ── Fix 2: the WHOLE chip is a click target ───────────────────────────────────

describe("full-chip click (fix: clicking anywhere on a chip opens the popup)", () => {
  it("opens the popup from a mousedown anywhere inside the chip element", () => {
    const open = vi.fn();
    const doc = `before ${buildObjectLink("urgent", "o1")} after`;
    const editor = mountWithDoc(doc, () => obj({ id: "o1", type: "tag" }), () => open("hit"));
    const chip = editor.view.dom.querySelector<HTMLElement>(".cm-object-link");
    expect(chip?.dataset.objectId).toBe("o1");
    // A mousedown originating from inside the chip (incl. the right half, which
    // the old posAtCoords hit-test missed) must still open it.
    chip?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(open).toHaveBeenCalledWith("hit");
    editor.destroy();
  });

  it("a mousedown OUTSIDE any chip does not open the popup", () => {
    const open = vi.fn();
    const doc = `plain text, no chip`;
    const editor = mountWithDoc(doc, () => null, () => open("nope"));
    const content = editor.view.dom.querySelector<HTMLElement>(".cm-content");
    content?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(open).not.toHaveBeenCalled();
    editor.destroy();
  });
});

// ── Fix 3: Cmd-Backspace is bound to delete-to-line-start ──────────────────────

describe("mac line-delete keymap (fix: Cmd+Backspace regression)", () => {
  it("binds Cmd-Backspace (mac Mod-Backspace) somewhere in the editor keymap", () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const editor = new MdEditor(host, "hello world", () => {}, () => {});
    // The keymap facet holds arrays of key bindings; assert a mac Mod-Backspace
    // binding is present (our explicit high-precedence restore) and an
    // Alt-Backspace (Option, word-delete) is too.
    const bindings = editor.view.state.facet(keymap).reduce((a, b) => a.concat(b), [] as any[]);
    const hasCmdBksp = bindings.some(
      (b: any) => b.mac === "Mod-Backspace" || b.key === "Mod-Backspace",
    );
    const hasOptBksp = bindings.some(
      (b: any) => b.mac === "Alt-Backspace" || b.key === "Mod-Backspace",
    );
    expect(hasCmdBksp).toBe(true);
    expect(hasOptBksp).toBe(true);
    editor.destroy();
  });
});
