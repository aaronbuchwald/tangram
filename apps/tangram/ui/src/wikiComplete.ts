// Autocomplete for the `[[ ]]` wikilink trigger (Connected Vault). As the user
// types `[[<partial>` inside an open wikilink, a popup lists the matching vault
// notes; accepting completes the token to `[[<chosen>]]` and the existing
// `[[ ]]` decoration/click logic (wikiLink.ts) takes over from there.
//
// This module is *completion only*: it never opens/creates anything, it just
// rewrites the open `[[…` token to a closed `[[<name>]]`. It mirrors
// slashComplete.ts's two-condition gating so it never fires on a bare `[[`:
//   1. the caret sits after a `[[` anchor (matchBefore enforces this), and
//   2. at least one character follows the `[[` (a bare `[[` shows nothing).
//
// Candidates = the vault notes, read through a live provider (closing over the
// rebuilt-on-vault-state `files`), so newly-created notes appear without a
// reload — the same closure idiom the slash autocomplete and the wikilink
// resolver use.
//
// Insertion follows Obsidian's "shortest path" spirit: insert the bare basename
// when it is unique vault-wide, else insert the folder-qualified `folder/note`
// so the resulting link is unambiguous (and resolves via links.ts, which keys
// full paths authoritatively over basenames).

import {
  type Completion,
  type CompletionContext,
  type CompletionResult,
  autocompletion,
} from "@codemirror/autocomplete";

/** One candidate vault note surfaced in the `[[ ]]` popup. */
export interface WikiCandidate {
  /** The note's stable file id (not used for insertion; kept for parity/debug). */
  id: string;
  /** The full vault path WITHOUT the trailing `.md`, e.g. `folder/My Note`. */
  path: string;
  /** The basename (last `/`-segment) WITHOUT `.md`, e.g. `My Note`. */
  basename: string;
}

/** Supplies the live candidate set (read fresh on every keystroke so newly
 *  created notes appear without re-mounting the editor). */
export type WikiCandidateProvider = () => WikiCandidate[];

// The open-wikilink prefix: a `[[` anchor immediately followed by the partial
// target text. The captured group excludes `[`, `]`, newline, `|` (alias), and
// `#` (heading) — those terminate the target portion — but DOES allow `/`, so a
// foldered partial like `folder/no` is matched and folder notes are findable.
const WIKI_BEFORE = /\[\[([^[\]\n|#]+)/;

/** Strip a trailing `.md` (case-insensitive) from a name/path. */
function stripMd(s: string): string {
  return s.replace(/\.md$/i, "");
}

/** The basename (last `/`-segment) of a path, without `.md`. */
function basenameOf(path: string): string {
  const seg = path.split("/").pop() ?? path;
  return stripMd(seg);
}

/** Rank `partial` against `hay`: 0 = prefix match, 1 = substring, -1 = none.
 *  Case-insensitive; callers pass an already-lowercased `partial`. */
function matchRank(hay: string, partial: string): number {
  const lower = hay.toLowerCase();
  if (lower.startsWith(partial)) return 0;
  if (lower.includes(partial)) return 1;
  return -1;
}

/** Best rank across a note's basename AND its full path (so typing a folder
 *  segment finds foldered notes). Lower is better; -1 = no match on either. */
function candidateRank(c: WikiCandidate, partial: string): number {
  const byBase = matchRank(c.basename, partial);
  const byPath = matchRank(c.path, partial);
  if (byBase === -1) return byPath;
  if (byPath === -1) return byBase;
  return Math.min(byBase, byPath);
}

/**
 * Build a CompletionSource for the `[[<partial>` wikilink trigger. Returns null
 * (no popup) unless the caret sits just after a `[[` with a non-empty partial.
 *
 * `currentPath` (optional, sans `.md`) is the note being edited; it is excluded
 * from the candidate list so a note can't autocomplete a link to itself.
 */
export function wikiCompletionSource(
  candidates: WikiCandidateProvider,
  currentPath: () => string | null = () => null,
): (context: CompletionContext) => CompletionResult | null {
  return (context: CompletionContext): CompletionResult | null => {
    // Condition 1 (anchor) is enforced by matchBefore: CM anchors the regex to
    // the cursor (`…$`) and searches the line, so the `\[\[` prefix only matches
    // when the caret follows an open `[[`. The captured partial is the text
    // between `[[` and the caret.
    const before = context.matchBefore(WIKI_BEFORE);
    if (!before) return null;
    // before.text is e.g. `[[fo`; group 1 is the partial after `[[`.
    const m = WIKI_BEFORE.exec(before.text);
    if (!m) return null;
    const partial = m[1];
    // Condition 2: at least one character after `[[` (bare `[[` → no popup).
    if (partial.length === 0) return null;
    // The token we replace starts at the `[[` itself: `before.from` is the
    // offset of the first matched char, and the regex is anchored on `\[\[`, so
    // the match begins exactly at the opening `[[`. We replace the whole
    // `[[<partial>` (brackets included) with the closed `[[<name>]]`.
    const tokenFrom = before.from;

    const self = currentPath();
    const selfKey = self ? stripMd(self).toLowerCase() : null;
    const needle = partial.toLowerCase();

    // A basename is ambiguous when two distinct notes share it (case-insensitive)
    // → those must insert the path-qualified form to stay unambiguous.
    const baseCounts = new Map<string, number>();
    for (const c of candidates()) {
      const k = c.basename.toLowerCase();
      baseCounts.set(k, (baseCounts.get(k) ?? 0) + 1);
    }

    const ranked = candidates()
      .filter((c) => c.path.length > 0)
      // Exclude the current note (no self-link autocomplete).
      .filter((c) => selfKey === null || c.path.toLowerCase() !== selfKey)
      .map((c) => ({ c, rank: candidateRank(c, needle) }))
      .filter((r) => r.rank >= 0)
      // prefix (0) before substring (1); then alphabetical by path for a stable
      // order (path so foldered duplicates sort deterministically).
      .sort((a, b) => a.rank - b.rank || a.c.path.localeCompare(b.c.path));
    if (ranked.length === 0) return null;

    // What follows the caret: skip an already-typed closing bracket so we don't
    // produce `]]]]`. Consume one `]]`, or a lone `]`, if present right after.
    const after = context.state.sliceDoc(
      context.pos,
      Math.min(context.pos + 2, context.state.doc.length),
    );
    let tokenTo = context.pos;
    if (after.startsWith("]]")) tokenTo = context.pos + 2;
    else if (after.startsWith("]")) tokenTo = context.pos + 1;

    const options: Completion[] = ranked.map(({ c }) => {
      const ambiguous = (baseCounts.get(c.basename.toLowerCase()) ?? 0) > 1;
      // Obsidian shortest-path: bare basename when unique vault-wide, else the
      // folder-qualified path so the inserted link resolves to THIS note.
      const insertName = ambiguous ? c.path : c.basename;
      return {
        // Show the basename as the headline; for an ambiguous basename, surface
        // the qualifying folder so the user sees which note they're picking.
        label: c.basename,
        detail: ambiguous || c.path !== c.basename ? c.path : undefined,
        type: "variable",
        apply: `[[${insertName}]]`,
      };
    });

    return {
      from: tokenFrom,
      to: tokenTo,
      options,
      // Our own filter/rank already ordered the list; don't let CM's default
      // fuzzy filter re-order or drop our substring/path matches.
      filter: false,
    };
  };
}

/** Build the `[[ ]]` autocomplete extension wired to the live note provider. */
export function wikiAutocomplete(
  candidates: WikiCandidateProvider,
  currentPath: () => string | null = () => null,
) {
  return autocompletion({
    override: [wikiCompletionSource(candidates, currentPath)],
    activateOnTyping: true,
    icons: false,
    maxRenderedOptions: 12,
    aboveCursor: false,
  });
}

/** Map vault files to `[[ ]]` candidates (path/basename sans `.md`). Excludes
 *  folder sentinels (`.keep`) and any empty/sentinel paths. */
export function wikiCandidatesFromFiles(
  files: { id: string; path: string }[],
): WikiCandidate[] {
  const out: WikiCandidate[] = [];
  for (const f of files) {
    if (!f.path || f.path.endsWith("/.keep") || f.path === ".keep") continue;
    const path = stripMd(f.path);
    if (path.length === 0) continue;
    out.push({ id: f.id, path, basename: basenameOf(f.path) });
  }
  return out;
}
