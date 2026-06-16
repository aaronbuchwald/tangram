// Unit test for the quick-open fuzzy scorer/filter (the pure part of #6). Guards
// the subsequence semantics + ranking so the switcher's ordering stays sane.

import { describe, expect, it } from "vitest";
import { filterItems, fuzzyScore, type QuickOpenItem } from "./quickOpen";

const item = (id: string, label: string, haystack = label): QuickOpenItem => ({
  id,
  kind: "note",
  label,
  haystack,
});

describe("fuzzyScore", () => {
  it("matches a case-insensitive subsequence", () => {
    expect(fuzzyScore("Daily Notes", "dn")).not.toBeNull();
    expect(fuzzyScore("Daily Notes", "dln")).not.toBeNull();
  });

  it("rejects a non-subsequence", () => {
    expect(fuzzyScore("Daily Notes", "xyz")).toBeNull();
    // order matters: "nd" is not a subsequence of "Daily Notes"
    expect(fuzzyScore("Daily", "ld")).toBeNull();
  });

  it("scores an empty query with a constant neutral value", () => {
    expect(fuzzyScore("abc", "")).toBe(0);
    expect(fuzzyScore("longer haystack", "")).toBe(0);
  });

  it("prefers an earlier, more contiguous match (lower score)", () => {
    const contiguous = fuzzyScore("report", "rep")!; // hits 0,1,2 — no gaps
    const gappy = fuzzyScore("receipt", "rep")!; // r..e....p — gaps
    expect(contiguous).toBeLessThan(gappy);
  });
});

describe("filterItems", () => {
  it("returns all items in input order for an empty query", () => {
    const items = [item("a", "Alpha"), item("b", "Beta"), item("c", "Gamma")];
    expect(filterItems(items, "").map((i) => i.id)).toEqual(["a", "b", "c"]);
  });

  it("filters out non-matches and ranks matches", () => {
    const items = [
      item("notes", "meeting-notes", "meeting-notes notes/meeting-notes.md"),
      item("nt", "nt", "nt nt.md"),
      item("other", "agenda", "agenda agenda.md"),
    ];
    const out = filterItems(items, "nt");
    expect(out.map((i) => i.id)).toContain("nt");
    expect(out.map((i) => i.id)).not.toContain("other");
    // The tight 2-char name should outrank the long path that merely contains
    // the same subsequence.
    expect(out[0].id).toBe("nt");
  });

  it("is stable for equal scores (preserves input order)", () => {
    const items = [item("x", "ab"), item("y", "ab")];
    expect(filterItems(items, "ab").map((i) => i.id)).toEqual(["x", "y"]);
  });
});
