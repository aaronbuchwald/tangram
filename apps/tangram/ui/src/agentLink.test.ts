// Unit + DOM tests for the inline `agent://` Run chip (embedded-runs R1):
//
//  - runStatusChip: the PURE chip-render — Idle / Running / Done / Error labels
//    and the Done `↓` scroll affordance — derived from the Run's status string.
//  - atomicity: a chip mounted in a real MdEditor is registered as an
//    `EditorView.atomicRanges` set so the cursor STEPS OVER it (cannot enter to
//    text-edit). We assert both the published atomic range and the rendered
//    widget (state class + that the cursor moving right skips past the chip).
//
// The pure render is the load-bearing logic; the DOM test is the regression
// guard for "the chip is atomic, not text-editable".

import { describe, expect, it } from "vitest";
import { EditorSelection } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { runStatusChip } from "./agentLink";
import { MdEditor } from "./editor";
import type { Invocation } from "./api";

const NOW = 1_000_000_000_000;

const inv = (over: Partial<Invocation> = {}): Invocation => ({
  id: "r1",
  agent: "standup",
  trigger: "daily at 09:00 UTC",
  prompt: "go",
  host_file_id: "f1",
  last_run_ms: null,
  status: "scheduled",
  ...over,
});

describe("runStatusChip (live status render)", () => {
  it("Idle: ⚡ <name> 🔒 (scheduled / never run), no jump affordance", () => {
    const chip = runStatusChip(inv({ status: "scheduled" }), "standup", NOW);
    expect(chip.state).toBe("idle");
    expect(chip.label).toBe("⚡ standup 🔒");
    expect(chip.base).toBe("⚡ standup 🔒");
    expect(chip.scrollable).toBe(false);
  });

  it("Running: ◌ <name> · running…", () => {
    const chip = runStatusChip(inv({ status: "running" }), "standup", NOW);
    expect(chip.state).toBe("running");
    expect(chip.label).toBe("◌ standup · running…");
    expect(chip.scrollable).toBe(false);
  });

  it("Done: ✓ <name> · <relative time> · ↓ (carries the jump affordance)", () => {
    const chip = runStatusChip(
      inv({ status: "ran", last_run_ms: NOW - 5 * 60 * 1000 }),
      "standup",
      NOW,
    );
    expect(chip.state).toBe("done");
    expect(chip.base).toBe("✓ standup · 5m ago");
    expect(chip.label).toBe("✓ standup · 5m ago · ↓");
    expect(chip.scrollable).toBe(true);
  });

  it("Error: ! <name> · failed", () => {
    const chip = runStatusChip(inv({ status: "error" }), "standup", NOW);
    expect(chip.state).toBe("error");
    expect(chip.label).toBe("! standup · failed");
    expect(chip.scrollable).toBe(false);
  });

  it("prefers the index agent name, falling back to the link label name", () => {
    // No record yet (just-inserted chip) → Idle, name from the link label.
    const fallback = runStatusChip(null, "digest", NOW);
    expect(fallback.label).toBe("⚡ digest 🔒");
    // With a record, the index `agent` wins over the passed-in label name.
    const fromIndex = runStatusChip(inv({ agent: "Standup" }), "ignored", NOW);
    expect(fromIndex.label).toBe("⚡ Standup 🔒");
  });

  it("an unknown status string reads as Idle (forward-compatible)", () => {
    expect(runStatusChip(inv({ status: "future-state" }), "x", NOW).state).toBe("idle");
  });
});

// ── DOM: the chip is an ATOMIC widget the cursor steps over ───────────────────

const LINK = "[⚡ standup](agent://r1)";
const DOC = `before ${LINK} after`;
const CHIP_FROM = "before ".length;
const CHIP_TO = CHIP_FROM + LINK.length;

/** Mount an MdEditor with the agent-link extensions wired and a status resolver
 *  that returns `record` for the chip's id. Mirrors how main.ts wires it. */
function mountWithChip(record: Invocation | null): MdEditor {
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
    () => record, // resolveRunStatus
    () => {}, // onScrollToOutput
  );
}

describe("agent chip atomicity + render (DOM)", () => {
  it("renders the chip as a widget with the live state class", () => {
    const editor = mountWithChip(inv({ status: "ran", last_run_ms: NOW }));
    const chip = editor.view.dom.querySelector<HTMLElement>(".cm-agent-link");
    expect(chip).not.toBeNull();
    expect(chip?.classList.contains("cm-agent-link-done")).toBe(true);
    expect(chip?.dataset.runId).toBe("r1");
    // The Done chip carries the `↓` jump affordance as its own element.
    expect(chip?.querySelector(".cm-agent-link-jump")?.textContent).toBe("↓");
    editor.destroy();
  });

  it("publishes the chip range as an atomic range", () => {
    const editor = mountWithChip(inv());
    // The atomicRanges facet holds provider fns; each yields a RangeSet. The
    // chip's [from, to) must be covered (a span exists at CHIP_FROM ending at
    // CHIP_TO).
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
    const editor = mountWithChip(inv());
    // Place the caret just before the chip, then move right by one character.
    editor.view.dispatch({ selection: EditorSelection.cursor(CHIP_FROM) });
    const moved = editor.view.moveByChar(
      editor.view.state.selection.main,
      true, // forward
    );
    // With the chip atomic, a single forward step lands at/after its END, never
    // strictly inside it.
    expect(moved.head).toBeGreaterThanOrEqual(CHIP_TO);
    editor.destroy();
  });
});
