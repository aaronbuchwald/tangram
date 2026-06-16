// Agent/skill definitions, parsed from a vault note's leading YAML frontmatter.
// R1 — the trigger belongs to the INVOCATION, not the definition: a definition
// is a PURE CAPABILITY (kind/name/model/instructions/labels) and carries NO
// trigger. The thing that decides when/how an agent runs is a ```agent
// invocation block (see invocations.ts), indexed separately. An inline
// `/<name>` in any file invokes a def — one-time (runs now) or, via the run
// popup's options, written as a durable cron invocation block. Any stray
// `trigger:`/`tools`/`sandbox` keys left in a definition's frontmatter are
// parse-and-ignored so a richer frontmatter doesn't break the index.
//
// A file is an agent/skill iff its frontmatter carries `kind: agent|skill` AND
// a non-empty `name`. The YAML parser is intentionally minimal and
// self-contained (no dependency): flat scalars, `[a, b]` inline arrays, and
// `{k: v}` inline maps — enough for the fields P1 defines.

import type { MdFile } from "./api";

/** The default model used when a definition omits `model`. */
export const DEFAULT_MODEL = "deepseek-chat";

/** A parsed agent/skill definition — a pure capability; triggers live on the
 *  ```agent invocation (see invocations.ts), never here. */
export interface AgentDef {
  kind: "agent" | "skill";
  name: string;
  model: string;
  labels: string[];
  meta: Record<string, unknown>;
  version: string | null;
  /** The body after the closing `---` — the system prompt / task. */
  instructions: string;
  /** The source file's id and path (so the UI can locate/open the def). */
  fileId: string;
  path: string;
}

// ── minimal YAML (flat scalars + inline [..] arrays + inline {..} maps) ───────

/** Strip a single layer of matching quotes from a scalar, if present. */
function unquote(raw: string): string {
  const s = raw.trim();
  if (s.length >= 2) {
    const q = s[0];
    if ((q === '"' || q === "'") && s[s.length - 1] === q) {
      return s.slice(1, -1);
    }
  }
  return s;
}

/** Parse a single scalar token into string | number | boolean | null. */
function parseScalar(raw: string): unknown {
  const s = raw.trim();
  if (s.length === 0) return "";
  // Quoted → always a string (keep it verbatim sans the quotes).
  if (
    (s[0] === '"' && s[s.length - 1] === '"') ||
    (s[0] === "'" && s[s.length - 1] === "'")
  ) {
    return s.slice(1, -1);
  }
  if (s === "true") return true;
  if (s === "false") return false;
  if (s === "null" || s === "~") return null;
  if (/^-?\d+(\.\d+)?$/.test(s)) return Number(s);
  return s;
}

/** Split on `sep` at the top level only (ignoring separators inside quotes). */
function splitTopLevel(input: string, sep: string): string[] {
  const out: string[] = [];
  let buf = "";
  let quote: string | null = null;
  for (const ch of input) {
    if (quote) {
      if (ch === quote) quote = null;
      buf += ch;
    } else if (ch === '"' || ch === "'") {
      quote = ch;
      buf += ch;
    } else if (ch === sep) {
      out.push(buf);
      buf = "";
    } else {
      buf += ch;
    }
  }
  out.push(buf);
  return out;
}

/** Parse a YAML value: inline array, inline map, or scalar. */
function parseValue(raw: string): unknown {
  const s = raw.trim();
  if (s.startsWith("[") && s.endsWith("]")) {
    const inner = s.slice(1, -1).trim();
    if (inner.length === 0) return [];
    return splitTopLevel(inner, ",")
      .map((part) => parseScalar(part))
      .filter((v) => !(typeof v === "string" && v.length === 0));
  }
  if (s.startsWith("{") && s.endsWith("}")) {
    const inner = s.slice(1, -1).trim();
    const map: Record<string, unknown> = {};
    if (inner.length === 0) return map;
    for (const part of splitTopLevel(inner, ",")) {
      const idx = part.indexOf(":");
      if (idx === -1) continue;
      const key = unquote(part.slice(0, idx));
      if (key.length === 0) continue;
      map[key] = parseScalar(part.slice(idx + 1));
    }
    return map;
  }
  return parseScalar(s);
}

/** Parse a flat frontmatter block (the text between the `---` fences) into a
 *  key→value map. Lines without a top-level `key:` (e.g. blanks, comments,
 *  nested list items) are skipped — P1 only consumes flat fields. */
function parseFrontmatter(block: string): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const line of block.split("\n")) {
    const trimmed = line.trim();
    if (trimmed.length === 0 || trimmed.startsWith("#")) continue;
    // Only treat a line as a field when the key sits at column 0 (no indent),
    // so an accidentally-indented continuation isn't read as a new key.
    if (/^\s/.test(line)) continue;
    const idx = line.indexOf(":");
    if (idx === -1) continue;
    const key = line.slice(0, idx).trim();
    if (key.length === 0) continue;
    out[key] = parseValue(line.slice(idx + 1));
  }
  return out;
}

/** Coerce a parsed value into a string[] (single scalar → one-element array). */
function toStringArray(value: unknown): string[] {
  if (Array.isArray(value)) return value.map((v) => String(v)).filter((s) => s.length > 0);
  if (value == null || value === "") return [];
  return [String(value)];
}

/**
 * Parse one file as an agent/skill definition, or null if it isn't one (no
 * leading `---\n…\n---` frontmatter, or missing `kind`/`name`). The body after
 * the closing fence becomes `instructions`.
 */
export function parseAgent(file: MdFile): AgentDef | null {
  const body = file.body ?? "";
  // Frontmatter must be the very first thing in the file.
  if (!body.startsWith("---")) return null;
  // The opening fence is a `---` line; find the closing `---` line after it.
  const lines = body.split("\n");
  if (lines[0].trim() !== "---") return null;
  let close = -1;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i].trim() === "---") {
      close = i;
      break;
    }
  }
  if (close === -1) return null;

  const fm = parseFrontmatter(lines.slice(1, close).join("\n"));
  const kindRaw = typeof fm.kind === "string" ? fm.kind.toLowerCase() : "";
  if (kindRaw !== "agent" && kindRaw !== "skill") return null;
  const name = typeof fm.name === "string" ? fm.name.trim() : "";
  if (name.length === 0) return null;

  const model =
    typeof fm.model === "string" && fm.model.trim().length > 0
      ? fm.model.trim()
      : DEFAULT_MODEL;
  const meta =
    fm.meta && typeof fm.meta === "object" && !Array.isArray(fm.meta)
      ? (fm.meta as Record<string, unknown>)
      : {};
  const version =
    fm.version == null || fm.version === "" ? null : String(fm.version);
  const instructions = lines.slice(close + 1).join("\n").trim();

  return {
    kind: kindRaw,
    name,
    model,
    labels: toStringArray(fm.labels),
    meta,
    version,
    instructions,
    fileId: file.id,
    path: file.path,
  };
}

/** A read-only index of the agent/skill definitions found across the vault. */
export interface AgentIndex {
  /** All parsed definitions, in input order. */
  readonly all: AgentDef[];
  /** Look up a definition by name (case-insensitive). */
  findAgent(name: string): AgentDef | null;
  /** Whether a name is already taken (case-insensitive) — for create validation. */
  has(name: string): boolean;
}

/** Build an index over the current vault files. Rebuilt on each vault state. */
export function buildAgentIndex(files: MdFile[]): AgentIndex {
  const all: AgentDef[] = [];
  const byName = new Map<string, AgentDef>();
  for (const file of files) {
    const def = parseAgent(file);
    if (!def) continue;
    const key = def.name.toLowerCase();
    // First definition for a name wins; later duplicates are ignored.
    if (byName.has(key)) continue;
    byName.set(key, def);
    all.push(def);
  }
  return {
    all,
    findAgent: (name) => byName.get(name.trim().toLowerCase()) ?? null,
    has: (name) => byName.has(name.trim().toLowerCase()),
  };
}
