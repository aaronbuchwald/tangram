// Unit tests for the pure hit-test helper behind the "click past a trailing
// link" fix. The bug: a click to the right of / below a trailing `[[link]]` was
// clamped onto the link's end and opened it, so the caret could never be placed
// after the link with a single click. The fix uses non-clamping `posAtCoords`
// (returns null in empty space) AND requires the resolved pos to land ON the
// token with an EXCLUSIVE end — `pos === to` is "past the link", not "on it".
//
// The layout-dependent parts (non-clamping coords, the rendered-rect refinement
// in `clickWithinRange`) need real layout, which jsdom does not provide; those
// are covered by a manual browser click. The decisive range-membership rule is
// pure and is what we unit-test here.

import { describe, expect, it } from "vitest";
import { posOnToken } from "./wikiLink";

describe("posOnToken (on-link hit test)", () => {
  // A trailing token `[[Note]]` occupying [0, 8) at the very end of the doc.
  const from = 0;
  const to = 8;

  it("is true strictly inside the token", () => {
    expect(posOnToken(0, from, to)).toBe(true); // opening boundary is ON
    expect(posOnToken(4, from, to)).toBe(true); // interior
    expect(posOnToken(7, from, to)).toBe(true); // last char of `]]`
  });

  it("is false exactly at the END boundary (the EOF-click bug)", () => {
    // pos === to means the caret sits AFTER the trailing link — clamping
    // posAtCoords used to land a right-of/below click here and open the link.
    expect(posOnToken(to, from, to)).toBe(false);
  });

  it("is false past the end and before the start", () => {
    expect(posOnToken(to + 1, from, to)).toBe(false);
    expect(posOnToken(-1, from, to)).toBe(false);
  });

  it("treats an empty range as never on-token", () => {
    expect(posOnToken(5, 5, 5)).toBe(false);
  });
});
