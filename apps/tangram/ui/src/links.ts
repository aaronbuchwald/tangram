// The vault wikilink index (Connected Vault, G1) — Obsidian-style `[[ ]]`
// links + a persisted reverse backlink map.
//
// DESIGN: resolve links by the STABLE `MdFile.id`, not by path.
// ---------------------------------------------------------------
// A wikilink's text is a *name* (a basename like `My Note`, or a full vault
// path like `folder/My Note`). We resolve that name to the target file's
// stable `id` via a case-insensitive name→id map built over the vault. Once
// resolved, every edge in the index is keyed by id, so a link keeps resolving
// even after the target is renamed/moved — the id never changes. This
// sidesteps Obsidian's rename-rewrite problem (where renaming a note forces a
// rewrite of every `[[ ]]` that referenced it) in our CRDT store: we never
// rewrite the link text, the id-keyed edges just follow the file.
//
// The backlink map is PERSISTED (built once per vault state, alongside the
// agent index) rather than re-scanned on demand — on-demand re-scan is
// Obsidian's known bottleneck. The single rebuild point is `main.ts`'s
// `onVaultState`, mirroring `buildAgentIndex(files)`.
//
// Parsing is intentionally minimal/self-contained (a regex over the body),
// matching the agents.ts no-dependency posture. We strip fenced code blocks
// (```…```) before scanning so links inside code fences don't count.

import type { MdFile } from "./api";

/** One parsed wikilink occurrence within a source file's body. */
export interface WikiLink {
  /** The resolved target file id, or null when the name doesn't resolve. */
  targetId: string | null;
  /** The name as written inside `[[ ]]` (target portion, alias/heading stripped). */
  name: string;
  /** The full raw token as it appears in the body, e.g. `[[Other|alias]]`. */
  original: string;
  /** The character offset of the token's opening `[` within `body`. */
  pos: number;
  /** True for an embed (`![[ ]]`) vs a plain link (`[[ ]]`). */
  embed: boolean;
}

/** A single backlink: a source file that links TO the indexed target. */
export interface Backlink {
  sourceId: string;
  /** The raw `[[ ]]` token in the source (for the panel snippet). */
  original: string;
  /** The token's offset within the source body. */
  pos: number;
}

/** A read-only index of the vault's wikilinks + reverse backlink map. */
export interface LinkIndex {
  /** Forward: source id → (target id → occurrence count). */
  readonly resolvedLinks: Map<string, Map<string, number>>;
  /** Forward, unresolved: source id → (ghost target name → occurrence count). */
  readonly unresolvedLinks: Map<string, Map<string, number>>;
  /** Reverse (persisted): target id → the source occurrences linking to it. */
  readonly backlinks: Map<string, Backlink[]>;
  /** Backlinks pointing at `fileId` (empty array if none). */
  backlinksFor(fileId: string): Backlink[];
  /** Resolve a wikilink name to a target file id, or null if unresolved. */
  resolve(name: string): string | null;
}

// `[[Target]]`, `[[Target|alias]]`, `[[Target#heading]]`, and the embed forms
// prefixed with `!`. We capture the optional `!`, then the inner text up to the
// closing `]]`. Alias (`|…`) and heading (`#…`) are split out of the inner text
// when resolving — only the target portion participates in resolution.
const WIKILINK = /(!?)\[\[([^\][\n]+?)\]\]/g;

/** Strip the leading `.md` (case-insensitive) from a basename/path. */
function stripMd(s: string): string {
  return s.replace(/\.md$/i, "");
}

/** The basename (last `/`-segment) of a vault path, without `.md`. */
function basenameOf(path: string): string {
  const seg = path.split("/").pop() ?? path;
  return stripMd(seg);
}

/**
 * The target portion of a wikilink's inner text: drop an `|alias` suffix and a
 * `#heading` fragment, trim, and strip a trailing `.md` the user may have
 * typed. Returns "" when nothing usable remains (a malformed `[[ ]]`).
 */
function linkTargetName(inner: string): string {
  let s = inner;
  const pipe = s.indexOf("|");
  if (pipe !== -1) s = s.slice(0, pipe);
  const hash = s.indexOf("#");
  if (hash !== -1) s = s.slice(0, hash);
  return stripMd(s.trim());
}

/**
 * Remove fenced code blocks (```…``` or ~~~…~~~) from a body before scanning,
 * so `[[ ]]` inside a code fence is ignored. Replaces fenced regions with
 * blank lines of equal length to PRESERVE character offsets (so the `pos` we
 * record still points into the original body). Inline code spans are left as-is
 * (acceptable for v1, per the spec).
 */
function blankFencedCode(body: string): string {
  const lines = body.split("\n");
  let inFence = false;
  let fence = "";
  for (let i = 0; i < lines.length; i++) {
    const trimmed = lines[i].trimStart();
    const m = /^(```+|~~~+)/.exec(trimmed);
    if (m) {
      if (!inFence) {
        inFence = true;
        fence = m[1][0]; // ` or ~
      } else if (trimmed.startsWith(fence)) {
        inFence = false;
        fence = "";
      }
      // Blank the fence marker line itself too (keep length for offsets).
      lines[i] = " ".repeat(lines[i].length);
      continue;
    }
    if (inFence) lines[i] = " ".repeat(lines[i].length);
  }
  return lines.join("\n");
}

/** Parse every wikilink in `body`, resolving each via `resolve`. */
export function parseWikiLinks(
  body: string,
  resolve: (name: string) => string | null,
): WikiLink[] {
  const out: WikiLink[] = [];
  const scan = blankFencedCode(body ?? "");
  WIKILINK.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = WIKILINK.exec(scan)) !== null) {
    const name = linkTargetName(m[2]);
    if (name.length === 0) continue;
    out.push({
      targetId: resolve(name),
      name,
      original: m[0],
      pos: m.index,
      embed: m[1] === "!",
    });
  }
  return out;
}

/**
 * Build the link index over the current vault files. Rebuilt on each vault
 * state (the single point in `main.ts`'s `onVaultState`). The name→id resolver
 * is case-insensitive and accepts either a basename (`My Note`) or a full vault
 * path (`folder/My Note`), with or without a trailing `.md`.
 *
 * Ambiguous basenames (two files share a basename) resolve to the first file in
 * input order; a full-path link always wins for its exact file. This keeps
 * resolution deterministic without a UI for disambiguation in v1.
 */
export function buildLinkIndex(files: MdFile[]): LinkIndex {
  // name (lowercased) → file id. Full paths are inserted so they always win
  // their exact slot; basenames are inserted first-wins so the lookup is stable.
  const byName = new Map<string, string>();
  for (const f of files) {
    const base = basenameOf(f.path).toLowerCase();
    if (base.length > 0 && !byName.has(base)) byName.set(base, f.id);
  }
  for (const f of files) {
    const full = stripMd(f.path).toLowerCase();
    // Full path is authoritative for its file (overrides a basename clash).
    if (full.length > 0) byName.set(full, f.id);
  }
  const resolve = (name: string): string | null => {
    const key = stripMd(name.trim()).toLowerCase();
    if (key.length === 0) return null;
    return byName.get(key) ?? null;
  };

  const resolvedLinks = new Map<string, Map<string, number>>();
  const unresolvedLinks = new Map<string, Map<string, number>>();
  const backlinks = new Map<string, Backlink[]>();

  const bump = (
    outer: Map<string, Map<string, number>>,
    a: string,
    b: string,
  ) => {
    let inner = outer.get(a);
    if (!inner) {
      inner = new Map<string, number>();
      outer.set(a, inner);
    }
    inner.set(b, (inner.get(b) ?? 0) + 1);
  };

  for (const f of files) {
    for (const link of parseWikiLinks(f.body, resolve)) {
      if (link.targetId) {
        // Don't index a self-link as a backlink (a note linking to itself
        // would otherwise show up in its own backlinks panel).
        bump(resolvedLinks, f.id, link.targetId);
        if (link.targetId !== f.id) {
          const list = backlinks.get(link.targetId) ?? [];
          list.push({ sourceId: f.id, original: link.original, pos: link.pos });
          backlinks.set(link.targetId, list);
        }
      } else {
        bump(unresolvedLinks, f.id, link.name);
      }
    }
  }

  return {
    resolvedLinks,
    unresolvedLinks,
    backlinks,
    backlinksFor: (fileId) => backlinks.get(fileId) ?? [],
    resolve,
  };
}
