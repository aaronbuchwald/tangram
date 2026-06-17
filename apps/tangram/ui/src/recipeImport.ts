// Smart objects SO4 — the recipe-URL import affordance
// (docs/design/smart-objects.md §6 ingestion).
//
// Picking `@recipe` in the type-picker offers TWO paths:
//   * paste a URL → IMPORT: mint a UUID, insert a placeholder recipe chip,
//     call `ingest_recipe` (host fetch → schema.org JSON-LD → LLM normalize →
//     recipe object, cached by URL+JSON-LD hash). The chip relabels to the
//     imported recipe name when the object lands on the next vault state.
//   * leave it blank → the SO3 MANUAL path (an empty recipe object + the popup).
//
// This module owns the small testable pieces (URL validation + the chip text)
// so the orchestration in main.ts stays a thin wire-up; the network call itself
// is the `vault.ingestRecipe` action (no live calls in tests).

/** A placeholder chip label shown while an import is in flight (before the
 *  normalized recipe name arrives on the next vault state). */
export const IMPORTING_LABEL = "importing…";

/**
 * Validate a pasted recipe URL for import. Returns the trimmed URL when it is a
 * well-formed http(s) URL, or an error string to surface in the prompt. We only
 * admit http/https here (defense in depth — the host egress gate is the real
 * fence, but a `file:`/`javascript:` URL should never reach it).
 */
export function validateRecipeUrl(raw: string): { url: string } | { error: string } {
  const trimmed = raw.trim();
  if (trimmed === "") return { error: "paste a recipe URL" };
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return { error: "that doesn't look like a URL (include https://)" };
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return { error: "only http(s) recipe URLs can be imported" };
  }
  return { url: trimmed };
}
