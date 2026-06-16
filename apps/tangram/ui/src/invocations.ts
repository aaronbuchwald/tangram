// Agent INVOCATIONS, parsed from ```agent fenced blocks in any vault note (R1).
//
// R1 — the trigger belongs to the INVOCATION, not the definition. A definition
// (agents.ts) is a pure capability (kind/name/model/instructions/labels); the
// thing that decides WHEN and HOW an agent runs is a durable instance — a fenced
// block inside a markdown file — that links to a definition via `use:` and owns
// the `trigger` + `prompt`:
//
//     ```agent
//     use: <definition-name>
//     trigger: cron every 1h          # or "one-time"
//     prompt: <prompt text, may span
//     multiple lines until the fence>
//     ```
//
// The block is the source of truth and is INDEXED (derived from the file text),
// so editing or removing it self-cleans — no stray refs. Each invocation gets a
// stable `invocationId` = a hash of {hostFileId + use + trigger + prompt}; an
// unedited block keeps its id, editing it produces a new id, removing it drops
// it. UI display of invocations is not required for R1 — the component is the
// consumer; this module just provides the parser/index so the UI and the
// component (apps/tangram/src/agents.rs) agree on the format BYTE-FOR-BYTE.

import type { MdFile } from "./api";

/** One parsed ```agent invocation block. */
export interface Invocation {
  /** The host file this block lives in (its stable id). */
  hostFileId: string;
  /** The definition this invocation runs (the `use:` field — a def name). */
  use: string;
  /** The raw `trigger:` text, e.g. `cron every 1h`, `cron @daily`, `one-time`. */
  trigger: string;
  /** The `prompt:` text (may span multiple lines until the closing fence). */
  prompt: string;
  /** Stable hash of {hostFileId + use + trigger + prompt} — stray-ref-safe. */
  invocationId: string;
}

/**
 * A stable id for an invocation: a hex 64-bit FNV-1a hash of
 * `hostFileId\0use\0trigger\0prompt`. This mirrors `invocation_id` in
 * `apps/tangram/src/agents.rs` EXACTLY (same fields, same NUL separator, same
 * FNV-1a constants, same 16-hex-digit zero-padded output) so the UI and the
 * component derive identical ids for the same block.
 */
export function invocationId(
  hostFileId: string,
  use: string,
  trigger: string,
  prompt: string,
): string {
  const key = `${hostFileId}\0${use}\0${trigger}\0${prompt}`;
  return fnv1aHex(key);
}

// 64-bit FNV-1a over the UTF-8 bytes, lowercase hex (16 digits, zero-padded).
// BigInt keeps the full 64-bit width (Number would lose precision past 2^53).
function fnv1aHex(s: string): string {
  const OFFSET = 0xcbf29ce484222325n;
  const PRIME = 0x00000100000001b3n;
  const MASK = 0xffffffffffffffffn;
  const bytes = new TextEncoder().encode(s);
  let hash = OFFSET;
  for (const b of bytes) {
    hash ^= BigInt(b);
    hash = (hash * PRIME) & MASK;
  }
  return hash.toString(16).padStart(16, "0");
}

/**
 * Parse every ```agent invocation block in `body`, in document order. Mirrors
 * `parse_invocations` in `apps/tangram/src/agents.rs`: `use`/`trigger`/`prompt`
 * are flat `key: value` lines at the top of the block; everything after the
 * `prompt:` line (until the closing fence) is part of the prompt (multi-line). A
 * block missing `use` is skipped (it cannot resolve a definition); `trigger`
 * defaults to `one-time`.
 */
export function parseInvocations(hostFileId: string, body: string): Invocation[] {
  const out: Invocation[] = [];
  const lines = (body ?? "").split("\n");
  let i = 0;
  while (i < lines.length) {
    if (lines[i].trim() !== "```agent") {
      i++;
      continue;
    }
    // Collect block lines until the closing fence.
    let j = i + 1;
    const block: string[] = [];
    let closed = false;
    while (j < lines.length) {
      if (lines[j].trim() === "```") {
        closed = true;
        break;
      }
      block.push(lines[j]);
      j++;
    }
    const inv = parseInvocationBlock(hostFileId, block);
    if (inv) out.push(inv);
    i = closed ? j + 1 : j;
  }
  return out;
}

/** Parse the inner lines of one ```agent block; null when `use` is missing. */
function parseInvocationBlock(hostFileId: string, block: string[]): Invocation | null {
  let use: string | null = null;
  let trigger: string | null = null;
  let prompt: string | null = null;

  for (let k = 0; k < block.length; k++) {
    const line = block[k];
    const idx = line.indexOf(":");
    if (idx === -1) continue;
    const key = line.slice(0, idx).trim().toLowerCase();
    const val = line.slice(idx + 1).trim();
    if (key === "use" && use === null) {
      use = val;
    } else if (key === "trigger" && trigger === null) {
      trigger = val;
    } else if (key === "prompt" && prompt === null) {
      // The prompt runs from this line's value to the end of the block.
      const parts = [val, ...block.slice(k + 1)];
      prompt = parts.join("\n").trim();
      break;
    }
  }

  const useTrimmed = (use ?? "").trim();
  if (useTrimmed.length === 0) return null;
  const triggerVal = trigger ?? "one-time";
  const promptVal = prompt ?? "";
  return {
    hostFileId,
    use: useTrimmed,
    trigger: triggerVal,
    prompt: promptVal,
    invocationId: invocationId(hostFileId, useTrimmed, triggerVal, promptVal),
  };
}

/** A read-only index of the vault's agent invocations. */
export interface InvocationIndex {
  /** All parsed invocations, in input (file, then document) order. */
  readonly all: Invocation[];
  /** Look up an invocation by its stable id. */
  byId(id: string): Invocation | null;
  /** Every invocation whose `use:` names the given definition (case-insensitive). */
  forDef(name: string): Invocation[];
}

/**
 * Build the invocation index over the current vault files. Rebuilt on each vault
 * state alongside the agent/link indexes (the single rebuild point in
 * `main.ts`'s `onVaultState`). Because it is derived from each file's body, an
 * edited/removed block self-cleans on the next state.
 */
export function buildInvocationIndex(files: MdFile[]): InvocationIndex {
  const all: Invocation[] = [];
  const byId = new Map<string, Invocation>();
  for (const f of files) {
    for (const inv of parseInvocations(f.id, f.body ?? "")) {
      all.push(inv);
      byId.set(inv.invocationId, inv);
    }
  }
  return {
    all,
    byId: (id) => byId.get(id) ?? null,
    forDef: (name) => {
      const needle = name.trim().toLowerCase();
      return all.filter((inv) => inv.use.trim().toLowerCase() === needle);
    },
  };
}

/**
 * Render a durable ```agent block (the cron-invocation text written into the
 * file when the user picks a non-one-time trigger in the run popup). The shape
 * matches `parseInvocations` above and the component's parser exactly.
 */
export function buildInvocationBlock(
  use: string,
  trigger: string,
  prompt: string,
): string {
  return ["```agent", `use: ${use}`, `trigger: ${trigger}`, `prompt: ${prompt}`, "```"].join(
    "\n",
  );
}

// ── schedule grammar v2 (mirrors apps/tangram/src/agents.rs parse_schedule) ──
//
// The component (`agents.rs`) is the authority on due-ness; this UI side parses
// the SAME grammar so the recurrence picker can validate before it writes a
// block, and so `parseScheduleMs` keeps working for interval-only callers. Forms:
//
//   - Interval shorthand:  2m / 2h / 2d  (also `every 2m`/`every 2h`/`every 2d`)
//   - Back-compat aliases: @hourly (1h) / @daily (24h)
//   - Daily at a time:     daily at HH:MM <IANA-tz>
//   - Custom weekly:       weekly on <days> at HH:MM <IANA-tz>
//
// A legacy `cron ` prefix is accepted (back-compat), exactly like the component.

/** A parsed v2 schedule (the UI-side mirror of the component's `Schedule`). */
export type Schedule =
  | { kind: "interval"; ms: number }
  | { kind: "daily"; hh: number; mm: number; tz: string }
  | { kind: "weekly"; days: Weekday[]; hh: number; mm: number; tz: string };

/** Three-letter weekday tokens, in canonical week order. */
export type Weekday = "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun";

/** Canonical week order for normalising/rendering selected days. */
export const WEEKDAYS: readonly Weekday[] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/**
 * Parse a v2 schedule string into a `Schedule`, or `null` for an
 * unknown/disabled schedule. Mirrors `parse_schedule` in `agents.rs` (same
 * forms, same `cron `-prefix back-compat). Timezone validity is checked with
 * the browser `Intl` API (the component checks against the IANA database; an
 * unknown zone disables on both sides).
 */
export function parseSchedule(schedule: string): Schedule | null {
  let s = schedule.trim();
  // Optional legacy `cron ` prefix (a `cron` token + whitespace).
  const cronMatch = /^cron(\s+)(.*)$/.exec(s);
  if (cronMatch) s = cronMatch[2].trim();

  if (s === "@hourly") return { kind: "interval", ms: 60 * 60 * 1000 };
  if (s === "@daily") return { kind: "interval", ms: 24 * 60 * 60 * 1000 };

  const daily = /^daily at (.+)$/.exec(s);
  if (daily) {
    const t = parseTimeTz(daily[1].trim());
    return t ? { kind: "daily", hh: t.hh, mm: t.mm, tz: t.tz } : null;
  }
  const weekly = /^weekly on (.+?) at (.+)$/.exec(s);
  if (weekly) {
    const days = parseDays(weekly[1].trim());
    const t = parseTimeTz(weekly[2].trim());
    return days && t ? { kind: "weekly", days, hh: t.hh, mm: t.mm, tz: t.tz } : null;
  }

  // Interval shorthand, optionally with an `every ` prefix.
  const bare = s.startsWith("every ") ? s.slice("every ".length).trim() : s;
  const ms = parseIntervalMs(bare);
  return ms === null ? null : { kind: "interval", ms };
}

/** Parse an interval shorthand (`<N>m`/`<N>h`/`<N>d`) into milliseconds. */
function parseIntervalMs(s: string): number | null {
  const m = /^(\d+)\s*([mhd])$/.exec(s.trim());
  if (!m) return null;
  const n = Number(m[1]);
  if (!Number.isInteger(n) || n <= 0) return null;
  const unit = { m: 60 * 1000, h: 60 * 60 * 1000, d: 24 * 60 * 60 * 1000 }[m[2]];
  return unit ? n * unit : null;
}

/** Parse a `HH:MM <IANA-tz>` tail (24-hour) into `{hh, mm, tz}`. */
function parseTimeTz(s: string): { hh: number; mm: number; tz: string } | null {
  const idx = s.search(/\s/);
  if (idx === -1) return null;
  const t = parseHhMm(s.slice(0, idx).trim());
  const tz = s.slice(idx + 1).trim();
  if (!t || !isValidTz(tz)) return null;
  return { hh: t.hh, mm: t.mm, tz };
}

/** Parse a strict `HH:MM` 24-hour time (00:00..=23:59). */
function parseHhMm(s: string): { hh: number; mm: number } | null {
  const m = /^(\d{1,2}):(\d{2})$/.exec(s);
  if (!m) return null;
  const hh = Number(m[1]);
  const mm = Number(m[2]);
  if (hh > 23 || mm > 59) return null;
  return { hh, mm };
}

/** Parse a comma list of weekday tokens into a de-duplicated, ordered list. */
function parseDays(s: string): Weekday[] | null {
  const out: Weekday[] = [];
  for (const raw of s.split(",")) {
    const tok = raw.trim().toLowerCase();
    if (!(WEEKDAYS as readonly string[]).includes(tok)) return null;
    const day = tok as Weekday;
    if (!out.includes(day)) out.push(day);
  }
  return out.length === 0 ? null : out;
}

/** Whether `tz` is a valid IANA timezone name per the browser `Intl` API. */
export function isValidTz(tz: string): boolean {
  if (!tz) return false;
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: tz });
    return true;
  } catch {
    return false;
  }
}

/**
 * The popup-side validity check + interval projection. Returns the interval in
 * ms for interval schedules, a positive sentinel (`1`) for valid calendar
 * schedules (so callers can keep using `!== null` as "valid"), or `null` for an
 * unknown schedule. Calendar callers should prefer `parseSchedule`.
 */
export function parseScheduleMs(schedule: string): number | null {
  const s = parseSchedule(schedule);
  if (!s) return null;
  return s.kind === "interval" ? s.ms : 1;
}

/** The browser's current IANA timezone (the picker's default zone). */
export function browserTz(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

/** The picker's emitted recurrence (one-time is handled outside the picker). */
export type Recurrence =
  | { mode: "interval"; n: number; unit: "m" | "h" | "d" }
  | { mode: "daily"; time: string; tz: string }
  | { mode: "weekly"; days: Weekday[]; time: string; tz: string };

/**
 * Build the `trigger:` string from the picker's recurrence selection — the
 * single shared builder, so the emitted string round-trips through
 * `parseSchedule` (and matches the component's grammar). `time` is `HH:MM`.
 */
export function buildTrigger(rec: Recurrence): string {
  switch (rec.mode) {
    case "interval":
      return `${rec.n}${rec.unit}`;
    case "daily":
      return `daily at ${rec.time} ${rec.tz}`;
    case "weekly": {
      const days = WEEKDAYS.filter((d) => rec.days.includes(d)).join(",");
      return `weekly on ${days} at ${rec.time} ${rec.tz}`;
    }
  }
}
