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
  | { kind: "once" }
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

  // The one-shot kind (embedded-runs R3): a `once` Run fires exactly once.
  // Distinct from the legacy `one-time` sentinel (which means "no index entry").
  if (s === "once") return { kind: "once" };
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

/** The picker's emitted recurrence. `once` is the unified one-time Run
 *  (embedded-runs R3): it is now a first-class trigger (a Run with a `once`
 *  schedule), not handled outside the picker. */
export type Recurrence =
  | { mode: "once" }
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
    case "once":
      return "once";
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

// ── human-readable rendering for the Agents-tab invocations table ────────────

/** Full weekday names for the schedule summary, keyed by canonical token. */
const WEEKDAY_NAMES: Record<Weekday, string> = {
  mon: "Mon",
  tue: "Tue",
  wed: "Wed",
  thu: "Thu",
  fri: "Fri",
  sat: "Sat",
  sun: "Sun",
};

/** Render an interval (ms) as a compact `every <n> <unit>` phrase. */
function formatInterval(ms: number): string {
  const DAY = 24 * 60 * 60 * 1000;
  const HOUR = 60 * 60 * 1000;
  const MIN = 60 * 1000;
  const plural = (n: number, unit: string) => `every ${n} ${unit}${n === 1 ? "" : "s"}`;
  if (ms % DAY === 0) return plural(ms / DAY, "day");
  if (ms % HOUR === 0) return plural(ms / HOUR, "hour");
  return plural(Math.max(1, Math.round(ms / MIN)), "minute");
}

/** Pad an hour/minute to two digits for an `HH:MM` time render. */
function hhmm(hh: number, mm: number): string {
  return `${String(hh).padStart(2, "0")}:${String(mm).padStart(2, "0")}`;
}

/**
 * A human-readable summary of a `trigger` string for the invocations table
 * (e.g. `every 2 hours`, `Daily at 09:00 UTC`, `Weekly on Mon, Wed at 18:00
 * America/New_York`). Falls back to the raw string for an unparseable trigger
 * (so nothing renders empty), mirroring the v2 grammar in `parseSchedule`.
 */
export function formatSchedule(trigger: string): string {
  const s = parseSchedule(trigger);
  if (!s) return trigger.trim() || "—";
  switch (s.kind) {
    case "once":
      return "One-time";
    case "interval":
      return formatInterval(s.ms);
    case "daily":
      return `Daily at ${hhmm(s.hh, s.mm)} ${s.tz}`;
    case "weekly": {
      const days = s.days.map((d) => WEEKDAY_NAMES[d]).join(", ");
      return `Weekly on ${days} at ${hhmm(s.hh, s.mm)} ${s.tz}`;
    }
  }
}

/**
 * Compute the wall-clock hour/minute that `nowMs` maps to in IANA zone `tz`,
 * plus the weekday index in WEEKDAYS order (mon=0 … sun=6). Returns null if the
 * zone can't be formatted (matches `isValidTz` failing on both sides).
 */
function zonedNow(nowMs: number, tz: string): { hh: number; mm: number; dow: number } | null {
  try {
    const parts = new Intl.DateTimeFormat("en-US", {
      timeZone: tz,
      hour12: false,
      hour: "2-digit",
      minute: "2-digit",
      weekday: "short",
    }).formatToParts(new Date(nowMs));
    const get = (t: string) => parts.find((p) => p.type === t)?.value ?? "";
    let hh = Number(get("hour"));
    if (hh === 24) hh = 0; // some engines emit "24" for midnight
    const mm = Number(get("minute"));
    const map: Record<string, number> = { Mon: 0, Tue: 1, Wed: 2, Thu: 3, Fri: 4, Sat: 5, Sun: 6 };
    const dow = map[get("weekday")];
    if (!Number.isFinite(hh) || !Number.isFinite(mm) || dow === undefined) return null;
    return { hh, mm, dow };
  } catch {
    return null;
  }
}

/**
 * Compute the next fire time (epoch ms) for a `trigger`, relative to `nowMs`,
 * or null when it can't be projected (unknown schedule, unparseable zone, or an
 * interval invocation that has never run — there's no anchor without a
 * `last_run_ms`). This is a DISPLAY projection only — the component
 * (`agents.rs`) remains the authority on actual due-ness; we never schedule.
 *
 *   - interval: `lastRunMs + ms` (null if it has never run — no anchor).
 *   - daily:    the next occurrence of HH:MM in the trigger's zone.
 *   - weekly:   the next occurrence of HH:MM on one of the selected days.
 */
export function nextFireMs(
  trigger: string,
  nowMs: number,
  lastRunMs: number | null,
): number | null {
  const s = parseSchedule(trigger);
  if (!s) return null;
  // A `once` Run has no future fire once it has run; if it hasn't, it fires on
  // the next tick (no projectable wall-clock instant) — so render no next-fire.
  if (s.kind === "once") return null;
  if (s.kind === "interval") {
    return lastRunMs === null ? null : lastRunMs + s.ms;
  }
  const z = zonedNow(nowMs, s.tz);
  if (!z) return null;
  const MIN = 60 * 1000;
  const nowMins = z.hh * 60 + z.mm;
  const targetMins = s.hh * 60 + s.mm;
  if (s.kind === "daily") {
    // Minutes until the target time today, or the same time tomorrow if passed.
    const delta = targetMins > nowMins ? targetMins - nowMins : targetMins - nowMins + 24 * 60;
    return nowMs + delta * MIN;
  }
  // weekly: find the smallest non-negative day offset whose (day, time) is in
  // the future. Today counts only if the target time hasn't passed yet.
  const selected = new Set(s.days.map((d) => WEEKDAYS.indexOf(d)));
  for (let add = 0; add < 7; add++) {
    const dow = (z.dow + add) % 7;
    if (!selected.has(dow)) continue;
    if (add === 0 && targetMins <= nowMins) continue; // already passed today
    const delta = add * 24 * 60 + (targetMins - nowMins);
    return nowMs + delta * MIN;
  }
  // Every selected day is "today, already passed" — wrap to the same weekday
  // next week (7 days out at the target time).
  return nowMs + (7 * 24 * 60 + (targetMins - nowMins)) * MIN;
}

/**
 * Format an absolute epoch-ms instant as a short relative phrase ("just now",
 * "5m ago", "3h ago", "2d ago") for the Last-run / Next-fire columns. Future
 * instants render with an "in " prefix ("in 2h"). `null` renders "—".
 */
export function formatRelativeTime(ms: number | null, nowMs: number): string {
  if (ms === null) return "—";
  const diff = ms - nowMs;
  const abs = Math.abs(diff);
  const MIN = 60 * 1000;
  const HOUR = 60 * MIN;
  const DAY = 24 * HOUR;
  if (abs < 45 * 1000) return "just now";
  let phrase: string;
  if (abs < HOUR) phrase = `${Math.round(abs / MIN)}m`;
  else if (abs < DAY) phrase = `${Math.round(abs / HOUR)}h`;
  else phrase = `${Math.round(abs / DAY)}d`;
  return diff < 0 ? `${phrase} ago` : `in ${phrase}`;
}
