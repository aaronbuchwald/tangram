// Autocomplete for the inline `/` agent/skill trigger (P2). As the user types
// `/<partial>` at a fresh token boundary, a small popup lists the matching
// slash-commands; accepting completes the token text to `/<name>` and the
// existing trigger logic (slashTrigger.ts) takes over from there — `/agent`
// auto-opens the create popup, a resolved `/<name>` runs on Enter/click.
//
// This module is *completion only*: it never opens/runs/creates anything, it
// just rewrites the token to the full `/<name>`. It reuses the same two gating
// conditions as the trigger so it never fires inside `a/b`, `http://`, etc.:
//   1. a space (or start-of-line) immediately precedes the `/`, and
//   2. at least one character follows the `/` (a bare `/` shows nothing).
//
// Candidates = every indexed agent/skill name PLUS the reserved `agent` create
// command. The candidate list is read through a live provider (closing over the
// rebuilt-on-vault-state index), so newly-created definitions appear without a
// reload — the same pattern as the trigger's `resolveAgent`.

import type {
  Completion,
  CompletionContext,
  CompletionResult,
} from "@codemirror/autocomplete";

/** One candidate slash-command surfaced in the popup. */
export interface SlashCandidate {
  /** The bare name (no leading `/`). */
  name: string;
  /** What it resolves to: a saved agent, a saved skill, or the create command. */
  kind: "agent" | "skill" | "create";
}

/** Supplies the live candidate set (read fresh on every keystroke so newly
 *  created definitions appear without re-mounting the editor). */
export type SlashCandidateProvider = () => SlashCandidate[];

// A `/` followed by zero-or-more name chars, optionally preceded by one
// whitespace char we use to verify the token boundary. We match the optional
// boundary char so `from` can be adjusted to point at the `/` itself.
const SLASH_BEFORE = /(^|\s)\/[\w-]*/;

/** Human label for the kind chip. The reserved create command reads as
 *  "new agent/skill" rather than a concrete kind. */
function kindLabel(kind: SlashCandidate["kind"]): string {
  return kind === "create" ? "new agent/skill" : kind;
}

/** Rank `partial` against `name`: 0 = prefix match, 1 = substring, -1 = none.
 *  Case-insensitive; callers pass an already-lowercased `partial`. */
function matchRank(name: string, partial: string): number {
  const lower = name.toLowerCase();
  if (lower.startsWith(partial)) return 0;
  if (lower.includes(partial)) return 1;
  return -1;
}

/**
 * A CodeMirror `CompletionSource` for the `/<partial>` slash trigger. Returns
 * null (no popup) unless the caret sits in a `/<partial>` token that satisfies
 * both gating conditions and `partial` is non-empty.
 */
export function slashCompletionSource(
  candidates: SlashCandidateProvider,
): (context: CompletionContext) => CompletionResult | null {
  return (context: CompletionContext): CompletionResult | null => {
    // Condition 1 (boundary) is enforced by `matchBefore`: CM anchors the regex
    // to the cursor (`…$`) and searches the line text, so `(^|\s)\/` only
    // matches a `/` at start-of-line or after whitespace — `a/b`, `http://`,
    // `a/b/c` never match (verified against @codemirror/autocomplete's
    // matchBefore semantics).
    const before = context.matchBefore(SLASH_BEFORE);
    if (!before) return null;
    // `before.text` is e.g. " /foo" or "/foo" (start-of-line). The optional
    // leading boundary char precedes the `/`; locate the `/` and split.
    const slashIdx = before.text.indexOf("/");
    if (slashIdx === -1) return null;
    const tokenFrom = before.from + slashIdx; // points at the `/`
    const partial = before.text.slice(slashIdx + 1); // chars after `/`
    // Condition 2: at least one character must follow the `/` (bare `/` → none).
    if (partial.length === 0) return null;

    const needle = partial.toLowerCase();
    const ranked = candidates()
      .map((c) => ({ c, rank: matchRank(c.name, needle) }))
      .filter((r) => r.rank >= 0)
      // prefix matches (rank 0) before substring matches (rank 1); then
      // alphabetical so the list is stable.
      .sort((a, b) => a.rank - b.rank || a.c.name.localeCompare(b.c.name));
    if (ranked.length === 0) return null;

    const options: Completion[] = ranked.map(({ c }) => ({
      label: `/${c.name}`,
      detail: kindLabel(c.kind),
      type: c.kind === "create" ? "keyword" : c.kind,
      // Accepting completes the token text to the full `/<name>`; the existing
      // trigger logic then applies on the completed token.
      apply: `/${c.name}`,
    }));

    return {
      from: tokenFrom,
      to: context.pos,
      options,
      // Our own filter/rank already produced the ordered list; don't let CM's
      // default fuzzy filter re-order or drop our substring matches.
      filter: false,
    };
  };
}
