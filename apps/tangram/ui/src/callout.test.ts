// Unit + DOM tests for the run-output callout cards (embedded-runs R3):
//
//  - block-id helpers mirror the component (`agents.rs`) so the chip ⇄ callout
//    backlinks resolve from the Run id;
//  - parseRunCallouts pulls the header (glyph/agent/model/when), body, host
//    block id (backlink target) + the callout's own block id and char range;
//  - renderRunCalloutCard builds the card DOM with a working `↑` backlink;
//  - the editor wires the callout decoration so the chip's `↓` and the callout's
//    `↑` target the right block ids (the bidirectional backlink).

import { describe, expect, it, vi } from "vitest";
import {
  calloutBlockId,
  hostBlockId,
  parseRunCallouts,
  renderRunCalloutCard,
} from "./callout";
import { MdEditor } from "./editor";

// A callout exactly as the component (`build_run_callout`) emits it.
function card(id: string, agent: string, output: string, isError = false): string {
  const glyph = isError ? "✗" : "✓";
  const body = output
    .split("\n")
    .map((l) => `> ${l}`)
    .join("\n");
  return (
    `> [!run]+ ${glyph} /${agent} · deepseek-chat · one-time [↑](#^${hostBlockId(id)})\n` +
    `${body}\n` +
    `> ^${calloutBlockId(id)}\n`
  );
}

describe("block-id helpers (mirror agents.rs)", () => {
  it("derive the host + callout block ids from the Run id", () => {
    expect(hostBlockId("abc")).toBe("run-abc");
    expect(calloutBlockId("abc")).toBe("runout-abc");
  });
});

describe("parseRunCallouts", () => {
  it("parses a callout's header, body, and both block ids", () => {
    const body = `# Daily\n\nRun [⚡ standup](agent://abc) today. ^${hostBlockId("abc")}\n\n${card("abc", "standup", "all good\nsecond line")}\nmore text\n`;
    const cals = parseRunCallouts(body);
    expect(cals.length).toBe(1);
    const c = cals[0];
    expect(c.agent).toBe("standup");
    expect(c.model).toBe("deepseek-chat");
    expect(c.when).toBe("one-time");
    expect(c.glyph).toBe("✓");
    expect(c.isError).toBe(false);
    expect(c.hostBlockId).toBe("run-abc"); // the backlink target
    expect(c.blockId).toBe("runout-abc"); // its own id (the ↓ target)
    expect(c.body).toBe("all good\nsecond line");
    // The range covers exactly the callout block.
    expect(body.slice(c.from, c.to)).toBe(card("abc", "standup", "all good\nsecond line").trimEnd());
  });

  it("flags an error callout via the ✗ glyph", () => {
    const c = parseRunCallouts(card("x", "a", "boom", true))[0];
    expect(c.isError).toBe(true);
  });

  it("ignores a plain blockquote that is not a run callout", () => {
    expect(parseRunCallouts("> just a quote\n> not a callout\n")).toEqual([]);
  });
});

describe("renderRunCalloutCard (DOM)", () => {
  it("renders the header, meta, body, and a working ↑ backlink", () => {
    const cal = parseRunCallouts(card("abc", "standup", "the output"))[0];
    const onBacklink = vi.fn();
    const el = renderRunCalloutCard(cal, onBacklink);
    expect(el.querySelector(".run-callout-title")!.textContent).toBe("/standup");
    expect(el.querySelector(".run-callout-body")!.textContent).toBe("the output");
    expect(el.dataset.calloutBlockId).toBe("runout-abc");
    // The ↑ backlink fires with the host block id (callout→chip).
    const back = el.querySelector(".run-callout-backlink")!;
    back.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    expect(onBacklink).toHaveBeenCalledWith("run-abc");
  });
});

// ── the bidirectional backlink targeting via the editor ───────────────────────

const RUN_ID = "abc";
const HOST_LINE = `Run [⚡ standup](agent://${RUN_ID}) today. ^${hostBlockId(RUN_ID)}`;
const DOC = `# Daily\n\n${HOST_LINE}\n\n${card(RUN_ID, "standup", "all good")}\n`;

function mount(): MdEditor {
  const host = document.createElement("div");
  document.body.appendChild(host);
  return new MdEditor(host, DOC, () => {});
}

describe("bidirectional backlink targeting (editor)", () => {
  it("scrollToBlockId finds the callout block id (chip ↓ → callout)", () => {
    const editor = mount();
    expect(editor.scrollToBlockId(calloutBlockId(RUN_ID))).toBe(true);
    editor.destroy();
  });

  it("scrollToBlockId finds the host block id (callout ↑ → chip)", () => {
    const editor = mount();
    expect(editor.scrollToBlockId(hostBlockId(RUN_ID))).toBe(true);
    editor.destroy();
  });

  it("returns false for a block id that isn't in the doc", () => {
    const editor = mount();
    expect(editor.scrollToBlockId("runout-nope")).toBe(false);
    editor.destroy();
  });

  // embedded-runs R4: the chip's `↓` jump must FLASH the rendered callout card.
  // The `^runout-<id>` anchor sits inside the card's replaced (widget) range, so
  // the flash must target the `.run-callout-card` element, not a `.cm-line`.
  it("flashes the rendered callout card on the chip → callout jump", async () => {
    // CM6's scroll-measure calls Range#getClientRects, which jsdom doesn't
    // implement on text ranges (it throws). Polyfill it to an empty list so the
    // measure that `scrollIntoView` schedules is a harmless no-op here; this is
    // a jsdom-only gap (real browsers implement it). Restore afterwards.
    const proto = Range.prototype as unknown as {
      getClientRects?: () => DOMRectList;
    };
    const had = "getClientRects" in proto;
    const prev = proto.getClientRects;
    proto.getClientRects = () => ({ length: 0, item: () => null }) as unknown as DOMRectList;

    const editor = mount();
    const sel = `.run-callout-card[data-callout-block-id="${calloutBlockId(RUN_ID)}"]`;
    // The callout block renders as a card (the StateField decoration).
    expect(editor.view.dom.querySelector(sel)).not.toBeNull();
    expect(editor.scrollToBlockId(calloutBlockId(RUN_ID))).toBe(true);
    // The flash lands a frame after the scroll; wait a frame. Re-query the card —
    // CM may re-render the decoration DOM between scroll and flash.
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    const card = editor.view.dom.querySelector(sel);
    expect(card).not.toBeNull();
    expect(card!.classList.contains("cm-backlink-flash")).toBe(true);

    editor.destroy();
    if (had) proto.getClientRects = prev;
    else delete proto.getClientRects;
  });
});
