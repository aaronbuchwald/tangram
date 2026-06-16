// Scheduled agent INVOCATIONS (the redesign): an inline dark-blue markdown link
// `[⚡ <agent>](agent://<id>)` in any vault note is the HANDLE; the source of
// truth (trigger/prompt/last-run/status) lives in the app's replicated
// `invocations` index, keyed by the stable UUID `<id>` embedded in the link.
//
//   - The link text survives edits/sync (the id is in the doc).
//   - Editing the trigger/prompt is a popup → `update_invocation` call; the
//     markdown never carries those fields.
//   - Removing the link makes the index entry an orphan; the component prunes it
//     on the next tick (stray-ref reconcile, like the wikilink index).
//
// This module provides: the inline-link parser + EOF-safe hit-test (so the
// decoration/click in editor.ts can find the handle), a builder for the link
// text, and an index over the REPLICATED invocations carried on the vault state
// frame. It mirrors `parse_agent_links` in `apps/tangram/src/agents.rs` so both
// sides agree on the handle format.

import type { Invocation } from "./api";

/** One inline `[<label>](agent://<id>)` link occurrence in a note body. */
export interface AgentLink {
  /** The invocation id embedded in the link target (`agent://<id>`). */
  id: string;
  /** The character offset of the opening `[`. */
  from: number;
  /** The character offset just past the closing `)`. */
  to: number;
}

// `[<label>](agent://<id>)` — label excludes `]`; the id excludes `)`/whitespace.
// Global so we can scan a body for every occurrence.
const AGENT_LINK = /\[([^\]\n]*)\]\(agent:\/\/([^)\s]+)\)/g;

/**
 * Parse every inline `[<label>](agent://<id>)` link in `body`, in document
 * order. Mirrors `parse_agent_links` in `apps/tangram/src/agents.rs` so the UI
 * and component agree on the handle format.
 */
export function parseAgentLinks(body: string): AgentLink[] {
  const out: AgentLink[] = [];
  AGENT_LINK.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = AGENT_LINK.exec(body ?? "")) !== null) {
    const id = m[2].trim();
    if (id.length === 0) continue;
    out.push({ id, from: m.index, to: m.index + m[0].length });
  }
  return out;
}

/**
 * If `pos` lands ON an `agent://` link token (opening boundary inclusive,
 * closing boundary exclusive — see `posOnToken` in wikiLink.ts), return its id +
 * range. Used by the click handler in editor.ts to open the Trigger popup.
 */
export function agentLinkAt(body: string, pos: number): AgentLink | null {
  for (const link of parseAgentLinks(body)) {
    if (pos >= link.from && pos < link.to) return link;
  }
  return null;
}

/**
 * Build the inline link text inserted into the note when a scheduled invocation
 * is created (the handle). The `⚡` glyph + dark-blue decoration mark it as an
 * agent link distinct from a `[[ ]]` wikilink. `id` is a UUID the caller mints.
 */
export function buildAgentLink(agent: string, id: string): string {
  return `[⚡ ${agent}](agent://${id})`;
}

/** A read-only index of the vault's scheduled invocations (the replicated index
 *  carried on the vault state frame). */
export interface InvocationIndex {
  /** All invocations from the replicated index, in stored order. */
  readonly all: Invocation[];
  /** Look up an invocation by its stable id. */
  byId(id: string): Invocation | null;
  /** Every invocation whose `agent` names the given definition (case-insensitive). */
  forAgent(name: string): Invocation[];
}

/**
 * Build the invocation index over the REPLICATED invocations from the vault
 * state frame. Rebuilt in `main.ts`'s `onVaultState` alongside the other
 * indexes. (The source of truth is the index, not the markdown — the inline
 * link is only the handle.)
 */
export function buildInvocationIndex(invocations: Invocation[]): InvocationIndex {
  const all = invocations ?? [];
  const byId = new Map<string, Invocation>();
  for (const inv of all) byId.set(inv.id, inv);
  return {
    all,
    byId: (id) => byId.get(id) ?? null,
    forAgent: (name) => {
      const needle = name.trim().toLowerCase();
      return all.filter((inv) => inv.agent.trim().toLowerCase() === needle);
    },
  };
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
