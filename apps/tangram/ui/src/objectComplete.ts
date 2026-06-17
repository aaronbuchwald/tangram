// Smart objects SO1 — autocomplete for the inline `@` type-picker
// (docs/design/smart-objects.md). Typing `@<partial>` at a fresh token boundary
// opens a popup listing the known smart-object TYPES (the type registry). On
// select, the `apply` callback mints a UUID, replaces the `@<partial>` token
// with the atomic chip `[<label>](obj://<id>)`, and calls `create_object` — the
// end-to-end `@`→chip→store loop. This mirrors slashComplete.ts's two-condition
// gating so it never fires inside an email/`a@b`/mid-word `@`:
//   1. a space (or start-of-line) immediately precedes the `@`, and
//   2. the partial after `@` is a type-name fragment (word chars / `-`).
//
// Candidates = the registered types, read through a live provider (closing over
// the rebuilt-on-vault-state registry) so a newly-registered type appears
// without a reload — the same closure idiom the slash/wiki autocompletes use.
//
// IMPORTANT: this source is added to the EXISTING single
// `autocompletion({override: [...]})` array in editor.ts — NEVER a second
// `autocompletion()` (a duplicate throws "Config merge conflict" and breaks the
// editor; see editor.ts + editor.smoke.test.ts).

import type {
  Completion,
  CompletionContext,
  CompletionResult,
} from "@codemirror/autocomplete";
import type { EditorView } from "@codemirror/view";
import type { ObjectType } from "./api";

/** Supplies the live registered-type set (read fresh on every keystroke). */
export type ObjectTypeProvider = () => ObjectType[];

/**
 * Mint a smart object of `objType` and insert its chip. Called by the picker's
 * `apply`: it should mint a UUID, replace [from, to) (the `@<partial>` token)
 * with the atomic `[<label>](obj://<id>)` chip, and persist via `create_object`.
 * The shell wires this in main.ts; here it is just the seam.
 */
export type ObjectMintHandler = (
  view: EditorView,
  objType: ObjectType,
  from: number,
  to: number,
) => void;

// An `@` followed by zero-or-more name chars, optionally preceded by one
// whitespace char we use to verify the token boundary (so `a@b` never matches).
const AT_BEFORE = /(^|\s)@[\w-]*/;

/** Rank `partial` against `name`: 0 = prefix, 1 = substring, -1 = none.
 *  Case-insensitive; callers pass an already-lowercased `partial`. */
function matchRank(name: string, partial: string): number {
  const lower = name.toLowerCase();
  if (lower.startsWith(partial)) return 0;
  if (lower.includes(partial)) return 1;
  return -1;
}

/**
 * A CodeMirror `CompletionSource` for the `@<partial>` smart-object type picker.
 * Returns null (no popup) unless the caret sits in an `@<partial>` token that
 * satisfies the boundary condition. A bare `@` (empty partial) DOES open the
 * picker (so `@` alone shows the full type list) — unlike the slash source,
 * which requires a non-empty partial, because `@` is itself the dedicated
 * smart-object trigger and a bare `@` is always intentional.
 */
export function objectCompletionSource(
  types: ObjectTypeProvider,
  mint: ObjectMintHandler,
): (context: CompletionContext) => CompletionResult | null {
  return (context: CompletionContext): CompletionResult | null => {
    // Condition 1 (boundary) is enforced by `matchBefore`: CM anchors the regex
    // to the cursor and searches the line, so `(^|\s)@` only matches an `@` at
    // start-of-line or after whitespace — `a@b`, `foo@bar.com` never match.
    const before = context.matchBefore(AT_BEFORE);
    if (!before) return null;
    const atIdx = before.text.indexOf("@");
    if (atIdx === -1) return null;
    const tokenFrom = before.from + atIdx; // points at the `@`
    const partial = before.text.slice(atIdx + 1); // chars after `@`

    // An explicit `@` typed with no preceding partial only auto-activates if the
    // user actually typed `@` (not a stray match); CM's `explicit` flag is true
    // when invoked manually. A bare `@` while typing should still show the list,
    // so we do NOT gate on a non-empty partial here.
    const needle = partial.toLowerCase();
    const ranked = types()
      .map((t) => ({ t, rank: needle.length === 0 ? 0 : matchRank(t.name, needle) }))
      .filter((r) => r.rank >= 0)
      // prefix (0) before substring (1); then alphabetical for a stable order.
      .sort((a, b) => a.rank - b.rank || a.t.name.localeCompare(b.t.name));
    if (ranked.length === 0) return null;

    const options: Completion[] = ranked.map(({ t }) => ({
      label: `@${t.name}`,
      detail: t.label,
      type: "type",
      // Accepting mints the object + inserts the chip via the shell's handler,
      // replacing the whole `@<partial>` token. We do NOT set `apply` to a
      // string — the side-effecting callback owns the replacement.
      apply: (view: EditorView, _completion: Completion, from: number, to: number) => {
        mint(view, t, from, to);
      },
    }));

    return {
      from: tokenFrom,
      to: context.pos,
      options,
      // Our own filter/rank already ordered the list; don't let CM re-order it.
      filter: false,
    };
  };
}
