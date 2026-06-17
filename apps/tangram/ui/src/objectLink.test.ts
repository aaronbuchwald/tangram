// Unit + DOM tests for the inline `obj://` smart-object chip (Smart Objects SO1):
//
//  - parseObjectLinks / buildObjectLink: the `[<label>](obj://<id>)` handle
//    parse + build (the id scheme the `@` picker mints, mirroring the component).
//  - atomicity: a chip mounted in a real MdEditor is registered as an
//    `EditorView.atomicRanges` set so the cursor STEPS OVER it (cannot enter to
//    text-edit). We assert the published atomic range, the rendered widget (the
//    per-type class hook), and that the cursor moving right skips past the chip.

import { describe, expect, it } from "vitest";
import { EditorSelection } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import {
  DERIVED_BADGE,
  DERIVED_ERROR_GLYPH,
  OBJECT_GLYPH,
  buildObjectLink,
  derivedValueLabel,
  glyphForType,
  parseObjectLinks,
  stripObjectLinkFromBody,
} from "./objectLink";
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

describe("object link parse + build (the @ chip handle)", () => {
  it("builds a portable `[◆ label](obj://id)` chip", () => {
    expect(buildObjectLink("urgent", "abc-123")).toBe(
      `[${OBJECT_GLYPH} urgent](obj://abc-123)`,
    );
  });

  it("parses every `obj://` link in document order with offsets", () => {
    const body = "a [◆ x](obj://id-1) b [y](obj://id-2) c";
    const links = parseObjectLinks(body);
    expect(links.map((l) => l.id)).toEqual(["id-1", "id-2"]);
    // Offsets bracket the full `[...](obj://...)` token.
    expect(body.slice(links[0].from, links[0].to)).toBe("[◆ x](obj://id-1)");
  });

  it("ignores empty ids and `agent://` links (distinct schemes)", () => {
    expect(parseObjectLinks("[x](obj://)").length).toBe(0);
    expect(parseObjectLinks("[⚡ a](agent://r1)").length).toBe(0);
  });
});

describe("glyphForType (#109 fix 2 — per-type chip glyph)", () => {
  it("returns a distinct glyph per resolved type", () => {
    expect(glyphForType("recipe")).toBe("🍳");
    expect(glyphForType("grocery-list")).toBe("🛒");
    expect(glyphForType("cart-preview")).toBe("🧺");
    expect(glyphForType("note-ref")).toBe("🔗");
    expect(glyphForType("tag")).toBe("🏷");
    expect(glyphForType("rollup")).toBe("∑");
  });
  it("falls back to the default `◆` for an unregistered/unknown type", () => {
    expect(glyphForType("unknown")).toBe(OBJECT_GLYPH);
    expect(glyphForType("whatever")).toBe(OBJECT_GLYPH);
  });
  it("matches case-insensitively", () => {
    expect(glyphForType("Recipe")).toBe("🍳");
  });
});

describe("stripObjectLinkFromBody (#109 fix 1 — strip the WHOLE span)", () => {
  it("removes the entire `[label](obj://id)` span, not just the target", () => {
    const body = "before [◆ urgent](obj://o1) after";
    // The whole `[◆ urgent](obj://o1)` span goes (and one surrounding space),
    // leaving no orphaned `◆ urgent` text.
    expect(stripObjectLinkFromBody(body, "o1")).toBe("before after");
  });
  it("leaves other links intact and only strips the matching id", () => {
    const body = "[◆ a](obj://o1) and [◆ b](obj://o2)";
    expect(stripObjectLinkFromBody(body, "o2")).toBe("[◆ a](obj://o1) and");
  });
  it("strips a span inside a list item without orphaning the glyph", () => {
    const body = "- [◆ Tomato Pasta](obj://recipe-pasta)\n- next";
    // The span (and one surrounding space) goes — no orphaned `◆ Tomato Pasta`.
    expect(stripObjectLinkFromBody(body, "recipe-pasta")).toBe("-\n- next");
  });
  it("returns null when the id has no inline link", () => {
    expect(stripObjectLinkFromBody("nothing here", "o1")).toBeNull();
  });
});

// ── DOM: the chip is an ATOMIC widget the cursor steps over ───────────────────

const LINK = `[${OBJECT_GLYPH} urgent](obj://o1)`;
const DOC = `before ${LINK} after`;
const CHIP_FROM = "before ".length;
const CHIP_TO = CHIP_FROM + LINK.length;

/** Mount an MdEditor with the object-link extensions wired and a resolver that
 *  returns `record` for the chip's id. Mirrors how main.ts wires it (the `@`
 *  args sit at the tail of the positional constructor). */
function mountWithObjectChip(record: SmartObject | null): MdEditor {
  const host = document.createElement("div");
  document.body.appendChild(host);
  return new MdEditor(
    host,
    DOC,
    () => {}, // onChange
    () => {}, // onSlashTrigger (present → agent extensions wired)
    () => false, // resolveAgent
    () => false, // isPopupOpen
    () => [], // slash candidates
    () => null, // wiki resolver
    () => {}, // open wikilink
    () => [], // wiki candidates
    () => null, // current note path
    () => {}, // onOpenAgentLink
    () => null, // resolveRunStatus
    () => {}, // onScrollToOutput
    () => {}, // onCalloutBacklink
    () => [], // objectTypes
    () => {}, // onMintObject
    () => {}, // onOpenObjectLink
    () => record, // resolveObject
  );
}

describe("object chip atomicity + render (DOM)", () => {
  it("renders the chip as a widget with the per-type class + glyph", () => {
    const editor = mountWithObjectChip(obj({ type: "tag" }));
    const chip = editor.view.dom.querySelector<HTMLElement>(".cm-object-link");
    expect(chip).not.toBeNull();
    expect(chip?.classList.contains("cm-object-link-tag")).toBe(true);
    expect(chip?.dataset.objectId).toBe("o1");
    expect(chip?.dataset.objectType).toBe("tag");
    // #109 fix 2: the chip leads with its RESOLVED type's glyph (tag → 🏷).
    expect(chip?.textContent).toContain(glyphForType("tag"));
    editor.destroy();
  });

  it("renders a per-type glyph + class for the resolved type (#109 fix 2)", () => {
    // Use a NON-card type (`rollup`) — the meal-plan card types (recipe /
    // grocery-list / cart-preview) now render as inline BLOCK cards, owned by
    // objectTable.ts, so they are intentionally NOT inline chips (SO5).
    const editor = mountWithObjectChip(obj({ type: "rollup" }));
    const chip = editor.view.dom.querySelector<HTMLElement>(".cm-object-link");
    expect(chip?.dataset.objectType).toBe("rollup");
    expect(chip?.classList.contains("cm-object-link-rollup")).toBe(true);
    // The rollup glyph (∑), NOT the generic `◆`.
    expect(chip?.textContent).toContain(glyphForType("rollup"));
    expect(chip?.textContent).not.toContain(OBJECT_GLYPH);
    editor.destroy();
  });

  it("publishes the chip range as an atomic range", () => {
    const editor = mountWithObjectChip(obj());
    let covered = false;
    for (const provider of editor.view.state.facet(EditorView.atomicRanges)) {
      const set = provider(editor.view);
      set.between(CHIP_FROM, CHIP_TO, (from, to) => {
        if (from === CHIP_FROM && to === CHIP_TO) covered = true;
      });
    }
    expect(covered).toBe(true);
    editor.destroy();
  });

  it("steps the cursor OVER the chip (cannot land inside it)", () => {
    const editor = mountWithObjectChip(obj());
    editor.view.dispatch({ selection: EditorSelection.cursor(CHIP_FROM) });
    const moved = editor.view.moveByChar(editor.view.state.selection.main, true);
    expect(moved.head).toBeGreaterThanOrEqual(CHIP_TO);
    editor.destroy();
  });

  it("renders an `unknown`-type chip when the store has no record yet", () => {
    const editor = mountWithObjectChip(null);
    const chip = editor.view.dom.querySelector<HTMLElement>(".cm-object-link");
    expect(chip?.dataset.objectType).toBe("unknown");
    editor.destroy();
  });
});

// ── SO2: derived-value rendering + the cycle/error chip ───────────────────────

describe("derivedValueLabel (the cached value shown inline)", () => {
  it("renders a rollup sum as its number", () => {
    expect(derivedValueLabel('{"op":"sum","sum":8,"count":2}')).toBe("8");
  });
  it("renders a rollup count when there is no sum", () => {
    expect(derivedValueLabel('{"op":"count","count":3}')).toBe("3");
  });
  it("joins concat values", () => {
    expect(derivedValueLabel('{"op":"concat","values":["a","b"]}')).toBe("a, b");
  });
  it("summarizes a grocery-list as N items (SO3)", () => {
    expect(derivedValueLabel('{"rows":[{"name":"onion"},{"name":"tomato"}]}')).toBe("2 items");
    expect(derivedValueLabel('{"rows":[{"name":"onion"}]}')).toBe("1 item");
  });
  it("summarizes a cart-preview as N aisles (SO3)", () => {
    expect(derivedValueLabel('{"aisles":[{"category":"Produce"}]}')).toBe("1 aisle");
    expect(derivedValueLabel('{"aisles":[{"category":"A"},{"category":"B"}]}')).toBe("2 aisles");
  });
  it("falls back to raw payload for non-JSON and `…` for empty", () => {
    expect(derivedValueLabel("plain text")).toBe("plain text");
    expect(derivedValueLabel("")).toBe("…");
  });
});

describe("derived chip render (DOM)", () => {
  it("shows the computed value + the auto badge for a healthy derived object", () => {
    const editor = mountWithObjectChip(
      obj({
        type: "rollup",
        data: '{"op":"sum","sum":8,"count":2}',
        derive: { kind: "rollup", deps: ["a", "b"] },
        derive_error: null,
      }),
    );
    const chip = editor.view.dom.querySelector<HTMLElement>(".cm-object-link");
    expect(chip?.classList.contains("cm-object-link-derived")).toBe(true);
    expect(chip?.dataset.derived).toBe("auto");
    expect(chip?.textContent).toContain("8");
    expect(chip?.textContent).toContain(DERIVED_BADGE);
    editor.destroy();
  });

  it("shows an error chip for a derived object in a cycle", () => {
    const editor = mountWithObjectChip(
      obj({
        type: "rollup",
        derive: { kind: "rollup", deps: ["self"] },
        derive_error: "dependency cycle: this derived object depends on itself",
      }),
    );
    const chip = editor.view.dom.querySelector<HTMLElement>(".cm-object-link");
    expect(chip?.classList.contains("cm-object-link-derived-error")).toBe(true);
    expect(chip?.dataset.derived).toBe("error");
    expect(chip?.textContent).toContain(DERIVED_ERROR_GLYPH);
    expect(chip?.title).toContain("cycle");
    editor.destroy();
  });

  it("renders a plain (non-derived) object with no badge or error class", () => {
    const editor = mountWithObjectChip(obj({ type: "tag" }));
    const chip = editor.view.dom.querySelector<HTMLElement>(".cm-object-link");
    expect(chip?.classList.contains("cm-object-link-derived")).toBe(false);
    expect(chip?.classList.contains("cm-object-link-derived-error")).toBe(false);
    expect(chip?.dataset.derived).toBeUndefined();
    expect(chip?.textContent).not.toContain(DERIVED_BADGE);
    editor.destroy();
  });

  it("updates the rendered value when the resolved object changes (live)", () => {
    // The chip reads the store through the resolver and rebuilds on a
    // refreshObjectChips nudge — the SO2 live-update path.
    let record: SmartObject = obj({
      type: "rollup",
      data: '{"sum":1}',
      derive: { kind: "rollup", deps: ["a"] },
    });
    const host = document.createElement("div");
    document.body.appendChild(host);
    const editor = new MdEditor(
      host,
      DOC,
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
    expect(editor.view.dom.querySelector(".cm-object-link")?.textContent).toContain("1");
    // The engine recomputed the dependency → the cached value changed.
    record = obj({ type: "rollup", data: '{"sum":42}', derive: { kind: "rollup", deps: ["a"] } });
    editor.refreshObjectChips();
    expect(editor.view.dom.querySelector(".cm-object-link")?.textContent).toContain("42");
    editor.destroy();
  });
});
