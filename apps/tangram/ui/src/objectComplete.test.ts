// Tests for the `@` smart-object type-picker completion source (Smart Objects
// SO1). The source must: open on a boundary `@`, list the registered types
// (ranked), NOT fire mid-word/in an email, and invoke the mint handler (not a
// text apply) on accept.

import { describe, expect, it, vi } from "vitest";
import { EditorState } from "@codemirror/state";
import { CompletionContext } from "@codemirror/autocomplete";
import { objectCompletionSource } from "./objectComplete";
import type { ObjectType } from "./api";

const TYPES: ObjectType[] = [
  { name: "note-ref", label: "Note reference", render: "chip" },
  { name: "tag", label: "Tag", render: "chip" },
];

/** Run the completion source against a doc with the caret at `pos` (explicit). */
function complete(doc: string, pos: number, mint = () => {}) {
  const state = EditorState.create({ doc });
  const ctx = new CompletionContext(state, pos, true);
  return objectCompletionSource(() => TYPES, mint)(ctx);
}

describe("@ type-picker completion source", () => {
  it("opens on a bare `@` at start-of-line and lists all types", () => {
    const res = complete("@", 1);
    expect(res).not.toBeNull();
    expect(res?.options.map((o) => o.label)).toEqual(["@note-ref", "@tag"]);
    expect(res?.from).toBe(0);
  });

  it("opens after whitespace and filters/ranks by the partial", () => {
    const doc = "see @ta";
    const res = complete(doc, doc.length);
    expect(res).not.toBeNull();
    expect(res?.options.map((o) => o.label)).toEqual(["@tag"]);
    // The replaced token starts at the `@`.
    expect(res?.from).toBe("see ".length);
  });

  it("does NOT fire mid-word or in an email address", () => {
    expect(complete("foo@bar", 7)).toBeNull();
    expect(complete("a@b.com", 3)).toBeNull();
  });

  it("returns null when the partial matches no type", () => {
    const doc = "@zzz";
    expect(complete(doc, doc.length)).toBeNull();
  });

  it("invokes the mint handler (a side-effecting apply, not a text apply)", () => {
    const mint = vi.fn();
    const res = complete("@tag", 4, mint);
    const apply = res?.options[0].apply;
    expect(typeof apply).toBe("function");
    // The apply is a (view, completion, from, to) callback — calling it mints.
    const view = {} as never;
    (apply as (v: unknown, c: unknown, f: number, t: number) => void)(
      view,
      res!.options[0],
      0,
      4,
    );
    expect(mint).toHaveBeenCalledTimes(1);
    expect(mint).toHaveBeenCalledWith(view, TYPES[1], 0, 4);
  });
});
