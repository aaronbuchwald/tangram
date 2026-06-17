// Smart objects SO4 — the recipe-import URL validation (the testable piece of
// the import affordance). The orchestration (mint id → insert chip →
// `ingest_recipe`) is a thin wire-up in main.ts driven by the action; here we
// pin the URL guard so a bad paste never reaches the host egress gate.

import { describe, expect, it } from "vitest";
import { IMPORTING_LABEL, validateRecipeUrl } from "./recipeImport";

describe("validateRecipeUrl", () => {
  it("accepts a well-formed https recipe URL", () => {
    const r = validateRecipeUrl("https://example.com/recipes/pasta");
    expect(r).toEqual({ url: "https://example.com/recipes/pasta" });
  });

  it("accepts http and trims surrounding whitespace", () => {
    const r = validateRecipeUrl("  http://recipes.test/x  ");
    expect(r).toEqual({ url: "http://recipes.test/x" });
  });

  it("rejects an empty value", () => {
    expect(validateRecipeUrl("   ")).toHaveProperty("error");
  });

  it("rejects a non-URL", () => {
    expect(validateRecipeUrl("not a url")).toHaveProperty("error");
  });

  it("rejects non-http(s) schemes (defense in depth)", () => {
    expect(validateRecipeUrl("file:///etc/passwd")).toHaveProperty("error");
    expect(validateRecipeUrl("javascript:alert(1)")).toHaveProperty("error");
  });

  it("exposes a placeholder label for the in-flight chip", () => {
    expect(IMPORTING_LABEL).toBeTruthy();
  });
});
