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
  OBJECT_GLYPH,
  buildObjectLink,
  parseObjectLinks,
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
    expect(chip?.textContent).toContain(OBJECT_GLYPH);
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
