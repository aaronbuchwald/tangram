// Editor-mount smoke test (regression guard).
//
// This mounts `MdEditor` the way `main.ts` mounts it — with the slash
// `/<partial>`, the `[[<partial>` wikilink, AND the `@<partial>` smart-object
// (Smart Objects SO1) autocomplete all wired through non-empty providers so all
// THREE completion sources are active — and asserts that constructing it does
// NOT throw and that the document renders.
//
// It exists to catch the "editor never mounts" class of regression. The
// specific bug that motivated it: the editor configured the CodeMirror 6
// `autocompletion()` extension TWICE (once for the slash popup, once for the
// wikilink popup). CM6 permits `autocompletion()` only once, so two configured
// instances throw `Config merge conflict for field override` at
// `EditorState.create`/`new EditorView` time — the editor never mounts and note
// content stops rendering. The fix folds both completion sources into a SINGLE
// `autocompletion()`; this test fails before that fix and passes after it.
//
// Fast + CI-runnable: jsdom only, no real browser.

import { describe, expect, it } from "vitest";
import { MdEditor } from "./editor";
import type { SlashCandidate } from "./slashComplete";
import type { WikiCandidate } from "./wikiComplete";
import type { ObjectType } from "./api";
import { CREATE_WORD } from "./slashTrigger";

const noop = () => {};

// Mirror main.ts: live providers that return a non-empty candidate set so both
// the slash and wiki completion sources are genuinely wired and active.
const slashCandidates = (): SlashCandidate[] => [
  { name: CREATE_WORD, kind: "create" },
  { name: "summarize", kind: "agent" },
];
const wikiCandidates = (): WikiCandidate[] => [
  { id: "n1", path: "Daily/Today", basename: "Today" },
  { id: "n2", path: "Ideas", basename: "Ideas" },
];
// Smart objects SO1: a non-empty type registry so the `@` source is active too.
const objectTypes = (): ObjectType[] => [
  { name: "note-ref", label: "Note reference", render: "chip" },
  { name: "tag", label: "Tag", render: "chip" },
];

describe("MdEditor mount (regression smoke test)", () => {
  it("constructs and mounts without throwing, with all three autocomplete sources wired", () => {
    const host = document.createElement("div");
    document.body.appendChild(host);

    const doc = "# Hello\n\nbody";
    let editor: MdEditor | undefined;

    // The actual regression: this constructor threw "Config merge conflict for
    // field override" when `autocompletion()` was configured twice.
    expect(() => {
      editor = new MdEditor(
        host,
        doc,
        noop, // onChange
        // Slash trigger handler present so the slash extensions are all wired,
        // exactly as the note editor in main.ts wires them.
        () => {},
        // resolveAgent
        (word) => word === "summarize",
        // isPopupOpen
        () => false,
        // slash autocomplete candidates (non-empty → source active)
        slashCandidates,
        // wikilink resolver
        () => null,
        // open wikilink
        noop,
        // wiki autocomplete candidates (non-empty → source active)
        wikiCandidates,
        // current note path
        () => "Daily/Today",
        // onOpenAgentLink / resolveRunStatus / onScrollToOutput / onCalloutBacklink
        noop,
        () => null,
        noop,
        noop,
        // Smart objects SO1 — the `@` type-picker source (non-empty → active).
        objectTypes,
        noop, // onMintObject
      );
    }).not.toThrow();

    expect(editor).toBeDefined();
    // The note "renders": the editor view is mounted into the host and holds
    // the document we gave it.
    expect(editor?.doc).toBe(doc);
    expect(host.querySelector(".cm-editor")).not.toBeNull();
    expect(host.querySelector(".cm-content")?.textContent).toContain("Hello");

    editor?.destroy();
  });
});
