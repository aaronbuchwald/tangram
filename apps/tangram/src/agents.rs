//! Agent/skill execution support for the vault: the definition frontmatter
//! parser, the inline `agent://<id>` link parser (the scheduled-invocation
//! handle), and the v2 schedule grammar (interval shorthand `2m`/`2h`/`2d` +
//! calendar daily/weekly-at-a-time with a DST-aware IANA timezone, plus the
//! `@hourly`/`@daily` back-compat aliases).
//!
//! The trigger belongs to the INVOCATION, not the definition:
//!
//! - A **definition** (`agents/…` note) is a pure capability: `kind`, `name`,
//!   `model`, instructions, labels. It carries NO trigger. This mirrors the
//!   fields `apps/tangram/ui/src/agents.ts` parses (any stray `trigger:` left in
//!   an old definition is parsed-and-ignored).
//! - A **scheduled invocation** is an entry in the replicated `invocations`
//!   index in `lib.rs` (the source of truth for trigger/prompt/last-run), keyed
//!   by a stable UUID that is also embedded in a note as an inline
//!   `[⚡ <agent>](agent://<id>)` link (the HANDLE). This module parses those
//!   links (`parse_agent_links`) and evaluates a trigger's due-ness
//!   (`trigger_is_due`); it mirrors `apps/tangram/ui/src/invocations.ts`.
//!
//! Everything here is pure (no I/O, no clock), so it is straightforward to
//! unit-test; the action layer in `lib.rs` supplies the wall clock and the LLM
//! egress.

/// The default model when a definition omits `model` (matches
/// `DEFAULT_MODEL` in `ui/src/agents.ts`).
pub const DEFAULT_MODEL: &str = "deepseek-chat";

/// A parsed agent/skill definition from a note's leading frontmatter. A
/// definition is a pure capability — it carries NO trigger (R1: the trigger
/// lives on the invocation). Richer frontmatter is parsed-and-ignored
/// (forward-compatible with the UI's superset, and any stray `trigger:` left in
/// an old definition).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// `agent` or `skill`.
    pub kind: String,
    /// The agent's name (the `/<name>` handle).
    pub name: String,
    /// The model id to call (defaults to [`DEFAULT_MODEL`]).
    pub model: String,
    /// The note body after the closing `---` — the system prompt / task.
    pub instructions: String,
    /// The MCP servers (apps, e.g. `nutrition`, `notes`) this definition
    /// REQUESTS access to (Tools/MCP T1). This is a *request*, not a grant —
    /// the user approves it (see the `mcp_grants` state + `approve_mcp` action
    /// in `lib.rs`). **Only `kind: agent` declares this; on `kind: skill` the
    /// `mcp_servers:` frontmatter is parsed-and-ignored** (skills do not get
    /// the tools plane in this slice). Canonicalized (trimmed, lowercased,
    /// de-duplicated, sorted) so the same set always hashes identically.
    pub mcp_servers: Vec<String>,
}

/// The schedule portion of a raw trigger string, stripped of the optional
/// back-compat `cron ` prefix. `daily at …`/`weekly on …`/`2m`/`@hourly` (with
/// or without the legacy `cron ` prefix) all return the bare schedule text;
/// `one-time`/empty returns `None`.
///
/// v2: the trigger no longer REQUIRES a `cron ` prefix — bare schedules (`2m`,
/// `daily at 09:00 America/New_York`, …) are first-class. The prefix is still
/// accepted so existing `cron @hourly` triggers keep parsing.
#[must_use]
pub fn schedule_str(trigger: &str) -> Option<&str> {
    let t = trigger.trim();
    if t.is_empty() || t == "one-time" {
        return None;
    }
    // Strip an optional leading `cron` token (back-compat with v1 triggers).
    if let Some(rest) = t.strip_prefix("cron") {
        if rest.is_empty() || !rest.starts_with(char::is_whitespace) {
            // `cronish …` is NOT a cron prefix; fall through and let the grammar
            // reject it (it won't parse), i.e. disabled.
            return Some(t);
        }
        return Some(rest.trim());
    }
    Some(t)
}

/// The parsed [`Schedule`] for a raw trigger string, if it declares one the
/// grammar understands. `None` ⇒ disabled (`one-time`, or an unknown schedule).
#[must_use]
pub fn schedule_of(trigger: &str) -> Option<Schedule> {
    schedule_str(trigger).and_then(parse_schedule)
}

// ── frontmatter parsing (a flat-scalar subset) ────────────────────────────────

/// Parse one file body as an agent/skill definition, or `None` if it is not one
/// (no leading `---\n…\n---` block, or missing `kind`/`name`). The body after
/// the closing fence becomes `instructions`. Mirrors `parseAgent` in
/// `ui/src/agents.ts` for the fields R1 cares about. Any `trigger:` in the
/// frontmatter is ignored (the trigger lives on the invocation, not here).
#[must_use]
pub fn parse_agent(body: &str) -> Option<AgentDef> {
    if !body.starts_with("---") {
        return None;
    }
    let lines: Vec<&str> = body.split('\n').collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return None;
    }
    // Find the closing `---` line after the opener.
    let close = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(i, l)| if l.trim() == "---" { Some(i) } else { None })?;

    let fm = parse_frontmatter(&lines[1..close]);
    let kind = fm.get("kind").map(|s| s.to_ascii_lowercase())?;
    if kind != "agent" && kind != "skill" {
        return None;
    }
    let name = fm.get("name").map(|s| s.trim().to_string())?;
    if name.is_empty() {
        return None;
    }
    let model = fm
        .get("model")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let instructions = lines[close + 1..].join("\n").trim().to_string();

    // Tools/MCP T1: only `kind: agent` declares an `mcp_servers:` request; a
    // skill's value (if any) is parsed-and-ignored. Canonicalize so the request
    // hashes identically regardless of source order/case/whitespace/dupes.
    let mcp_servers = if kind == "agent" {
        canonical_servers(parse_inline_array(
            fm.get("mcp_servers").unwrap_or_default(),
        ))
    } else {
        Vec::new()
    };

    Some(AgentDef {
        kind,
        name,
        model,
        instructions,
        mcp_servers,
    })
}

/// Parse an inline `[a, b, c]` YAML array value into its elements (unquoted,
/// trimmed). A non-array scalar is treated as a single-element list; an empty
/// or missing value yields no elements. Intentionally minimal (mirrors the
/// frontmatter parser's altitude + the UI's `parseInlineArray`).
fn parse_inline_array(value: &str) -> Vec<String> {
    let s = value.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let inner = if let Some(stripped) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        stripped
    } else {
        // A bare scalar `mcp_servers: nutrition` is one element.
        s
    };
    inner
        .split(',')
        .map(|p| unquote(p.trim()).trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Strip one layer of matching quotes from a scalar, if present.
fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let q = bytes[0];
        if (q == b'"' || q == b'\'') && bytes[bytes.len() - 1] == q {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Canonicalize a list of requested MCP server names: trim, lowercase,
/// de-duplicate, and sort. Canonicalization is what makes the request HASH
/// (see [`mcp_request_hash`]) stable: `[Nutrition, notes]` and `[notes,
/// nutrition]` are the same request and must approve/stale identically.
#[must_use]
pub fn canonical_servers(servers: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = servers
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A stable hash of a (canonical) requested-server set: a hex 64-bit FNV-1a
/// over the canonical servers joined by NUL. Mirrors `mcpRequestHash` in
/// `apps/tangram/ui/src/agents.ts` EXACTLY (same canonicalization, same NUL
/// separator, same FNV-1a constants, same 16-hex output) so the UI and the
/// component agree on the hash the user's approval binds to. The grant records
/// this hash; if the definition later changes `mcp_servers` the hash no longer
/// matches and the grant goes STALE → pending re-approval (the auto-todo
/// plan-hash-bound-approval precedent).
#[must_use]
pub fn mcp_request_hash(servers: &[String]) -> String {
    let canon = canonical_servers(servers.to_vec());
    fnv1a_hex(&canon.join("\0"))
}

/// Parse the frontmatter block (lines between the fences) into a flat
/// key→raw-value map. Only column-0 `key: value` lines are read (indented
/// continuations are skipped), matching the UI parser's intentional
/// minimalism. Values are kept as their raw (trimmed) text.
fn parse_frontmatter(lines: &[&str]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // A field key must sit at column 0 (no indentation).
        if line.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let Some(idx) = line.find(':') else { continue };
        let key = line[..idx].trim();
        if key.is_empty() {
            continue;
        }
        out.push((key.to_string(), line[idx + 1..].trim().to_string()));
    }
    out
}

/// A tiny lookup over the flat frontmatter pairs (first wins). Defined as a
/// trait-ish helper so callers read like a map without pulling `HashMap` (and
/// keeping the order deterministic).
trait Lookup {
    fn get(&self, key: &str) -> Option<&str>;
}
impl Lookup for Vec<(String, String)> {
    fn get(&self, key: &str) -> Option<&str> {
        self.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// 64-bit FNV-1a over the UTF-8 bytes, lowercase hex. Tiny, dependency-free,
/// and trivially portable to the TS side (so the ids match).
fn fnv1a_hex(s: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

// ── the v2 schedule grammar ───────────────────────────────────────────────────

use chrono::{Datelike, TimeZone, Weekday};
use chrono_tz::Tz;

const MINUTE_MS: i64 = 60 * 1000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

/// A parsed agent schedule. Built by [`parse_schedule`]; evaluated for
/// due-ness by [`is_due`]. Mirrors the picker output in the UI
/// (`apps/tangram/ui/src/invocations.ts` `parseScheduleMs` + the picker
/// builder) — both sides parse the SAME trigger grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Fire EXACTLY ONCE, then never again (the `once` trigger — embedded-runs
    /// R3's one-time/scheduled unification). A one-time Run is now an index
    /// entry with a `once` schedule (a chip + record like any Run), not a
    /// legacy run-now-and-discard flow. Never run ⇒ due now; once run ⇒ no
    /// next fire (the scheduler is the authority — it must never re-fire it).
    Once,
    /// Fire every `n` milliseconds since the last run (`2m`/`2h`/`2d`,
    /// `every 2m`, the `@hourly`/`@daily` back-compat aliases).
    Interval(i64),
    /// Fire once per day at local `hh:mm` in IANA timezone `tz`.
    Daily { hh: u32, mm: u32, tz: Tz },
    /// Fire on the selected `days` of the week at local `hh:mm` in IANA
    /// timezone `tz`. `days` is the de-duplicated set of selected weekdays.
    Weekly {
        days: Vec<Weekday>,
        hh: u32,
        mm: u32,
        tz: Tz,
    },
}

/// Parse a v2 schedule string into a [`Schedule`], or `None` for an
/// unknown/disabled schedule (the caller skips it). The accepted forms — kept
/// byte-identical to the UI parser:
///
/// - **Interval shorthand**: `2m` / `2h` / `2d` (minutes / hours / DAYS), plus
///   `every 2m` / `every 2h` / `every 2d` synonyms.
/// - **Back-compat aliases**: `@hourly` (1h), `@daily` (24h interval).
/// - **Daily at a time**: `daily at HH:MM <IANA-tz>` — e.g.
///   `daily at 09:00 America/New_York`.
/// - **Custom weekly**: `weekly on <days> at HH:MM <IANA-tz>` — `<days>` a comma
///   list of `mon,tue,wed,thu,fri,sat,sun` — e.g.
///   `weekly on mon,wed,fri at 14:00 America/New_York`.
///
/// `HH:MM` is 24-hour; the tz must be a known IANA name (else `None`).
#[must_use]
pub fn parse_schedule(schedule: &str) -> Option<Schedule> {
    let s = schedule.trim();

    // The one-shot kind (embedded-runs R3): a `once` Run lives in the index and
    // fires exactly once. Distinct from the legacy `one-time` sentinel, which
    // means "no index entry / no schedule" and is filtered out in `schedule_str`.
    if s == "once" {
        return Some(Schedule::Once);
    }

    // Back-compat aliases.
    match s {
        "@hourly" => return Some(Schedule::Interval(HOUR_MS)),
        "@daily" => return Some(Schedule::Interval(DAY_MS)),
        _ => {}
    }

    // Calendar forms.
    if let Some(rest) = s.strip_prefix("daily at ") {
        let (hh, mm, tz) = parse_time_tz(rest.trim())?;
        return Some(Schedule::Daily { hh, mm, tz });
    }
    if let Some(rest) = s.strip_prefix("weekly on ") {
        // `<days> at HH:MM <tz>`
        let (days_part, time_part) = rest.split_once(" at ")?;
        let days = parse_days(days_part.trim())?;
        let (hh, mm, tz) = parse_time_tz(time_part.trim())?;
        return Some(Schedule::Weekly { days, hh, mm, tz });
    }

    // Interval shorthand: `2m`/`2h`/`2d`, optionally with an `every ` prefix.
    let interval = s.strip_prefix("every ").map_or(s, str::trim);
    parse_interval_ms(interval).map(Schedule::Interval)
}

/// Parse an interval shorthand (`<N>m` / `<N>h` / `<N>d`) into milliseconds.
fn parse_interval_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: i64 = num.trim().parse().ok()?;
    if n <= 0 {
        return None;
    }
    match unit {
        "m" => n.checked_mul(MINUTE_MS),
        "h" => n.checked_mul(HOUR_MS),
        "d" => n.checked_mul(DAY_MS),
        _ => None,
    }
}

/// Parse a `HH:MM <IANA-tz>` tail (24-hour clock) into `(hh, mm, tz)`.
fn parse_time_tz(s: &str) -> Option<(u32, u32, Tz)> {
    let (time, tz_name) = s.split_once(char::is_whitespace)?;
    let (hh, mm) = parse_hh_mm(time.trim())?;
    let tz: Tz = tz_name.trim().parse().ok()?;
    Some((hh, mm, tz))
}

/// Parse a strict `HH:MM` 24-hour time (00:00..=23:59).
fn parse_hh_mm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let hh: u32 = h.parse().ok()?;
    let mm: u32 = m.parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some((hh, mm))
}

/// Parse a comma list of weekday abbreviations into a de-duplicated, ordered
/// `Vec<Weekday>`. Empty / any unknown token ⇒ `None`.
fn parse_days(s: &str) -> Option<Vec<Weekday>> {
    let mut out: Vec<Weekday> = Vec::new();
    for tok in s.split(',') {
        let day = match tok.trim().to_ascii_lowercase().as_str() {
            "mon" => Weekday::Mon,
            "tue" => Weekday::Tue,
            "wed" => Weekday::Wed,
            "thu" => Weekday::Thu,
            "fri" => Weekday::Fri,
            "sat" => Weekday::Sat,
            "sun" => Weekday::Sun,
            _ => return None,
        };
        if !out.contains(&day) {
            out.push(day);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// The epoch-ms of the most-recent scheduled occurrence at or before `now_ms`
/// for a calendar (daily/weekly) schedule, computed in `tz` so it is DST-aware
/// (chrono-tz resolves the wall-clock `hh:mm` to the correct UTC instant for
/// the day, including across spring-forward / fall-back). `None` only if the
/// clock can't be represented.
///
/// Strategy: walk back day-by-day from "today in `tz`" (at most 8 days — a full
/// week plus one for the daily case / DST slack); for each candidate day that
/// matches the schedule's weekday set, resolve its `hh:mm` wall-clock to an
/// instant and return the first one `<= now`. Walking days (not a fixed
/// interval) is what makes it DST-correct: each day's `hh:mm` is resolved
/// independently in the zone.
fn last_calendar_occurrence_ms(
    days: Option<&[Weekday]>,
    hh: u32,
    mm: u32,
    tz: Tz,
    now_ms: i64,
) -> Option<i64> {
    let now_utc = chrono::DateTime::from_timestamp_millis(now_ms)?;
    let now_local = now_utc.with_timezone(&tz);
    let today = now_local.date_naive();
    // Look back over a full week (+1 day of slack for time-of-day / DST).
    for back in 0..8 {
        let day = today - chrono::Duration::days(back);
        if let Some(allowed) = days
            && !allowed.contains(&day.weekday())
        {
            continue;
        }
        // Resolve this day's wall-clock hh:mm in the zone (DST-aware). On a
        // spring-forward gap the slot may not exist; `single()` yields None, so
        // we fall through to the previous day (the missed slot fires late, on
        // the next valid occurrence — acceptable at minute granularity).
        let Some(occ) = tz
            .with_ymd_and_hms(day.year(), day.month(), day.day(), hh, mm, 0)
            .single()
        else {
            continue;
        };
        let occ_ms = occ.timestamp_millis();
        if occ_ms <= now_ms {
            return Some(occ_ms);
        }
    }
    None
}

/// The epoch-ms of the FIRST scheduled occurrence STRICTLY AFTER `after_ms` for
/// a calendar (daily/weekly) schedule, computed in `tz` so it is DST-aware (the
/// forward twin of [`last_calendar_occurrence_ms`]). `None` only if no occurrence
/// can be represented within the look-ahead window.
///
/// Strategy: walk forward day-by-day from "the day of `after_ms` in `tz`" (at
/// most 9 days — a full week plus slack for the time-of-day not-yet-reached and
/// DST). For each candidate day that matches the schedule's weekday set, resolve
/// its `hh:mm` wall-clock to an instant and return the first one strictly
/// `> after_ms`. Walking days (not a fixed interval) is what keeps it DST-correct:
/// each day's `hh:mm` is resolved independently in the zone.
fn next_calendar_occurrence_ms(
    days: Option<&[Weekday]>,
    hh: u32,
    mm: u32,
    tz: Tz,
    after_ms: i64,
) -> Option<i64> {
    let after_utc = chrono::DateTime::from_timestamp_millis(after_ms)?;
    let after_local = after_utc.with_timezone(&tz);
    let start = after_local.date_naive();
    // Look ahead over a full week (+2 days of slack for time-of-day / DST).
    for ahead in 0..9 {
        let day = start + chrono::Duration::days(ahead);
        if let Some(allowed) = days
            && !allowed.contains(&day.weekday())
        {
            continue;
        }
        // Resolve this day's wall-clock hh:mm in the zone (DST-aware). On a
        // spring-forward gap the slot may not exist; `single()` yields None, so
        // we fall through to the next valid day (matching the backward walk's
        // "missed slot fires on the next valid occurrence" semantics).
        let Some(occ) = tz
            .with_ymd_and_hms(day.year(), day.month(), day.day(), hh, mm, 0)
            .single()
        else {
            continue;
        };
        let occ_ms = occ.timestamp_millis();
        if occ_ms > after_ms {
            return Some(occ_ms);
        }
    }
    None
}

/// The stored **next fire time** for a recurring `trigger`: the epoch-ms instant
/// at or before which the invocation becomes DUE. This is the inverse of
/// [`trigger_is_due`] — for every `now_ms`, `next_fire_ms(..) <= Some(now_ms)`
/// holds exactly when [`trigger_is_due`] is `true`, so the scheduler can select
/// "due now" entries by a stored timestamp instead of re-deriving due-ness for
/// the whole index every tick. `None` for a `one-time`/unknown trigger (never
/// scheduled), or when no calendar occurrence can be represented.
///
/// - **Interval** (`2m`/`@hourly`/…): never run ⇒ fire now (`now_ms`, so it is
///   immediately due); otherwise `last_run + interval`. This matches the
///   `now - last >= interval` due rule exactly.
/// - **Daily / Weekly**: the next occurrence STRICTLY AFTER the last run (so the
///   slot fires exactly once); never run ⇒ fire at the earliest representable
///   occurrence so a fresh invocation catches up on its first eligible tick,
///   matching the `occurrence <= now && occurrence > last_run` due rule (with no
///   last run, any past-or-present occurrence is due).
#[must_use]
pub fn next_fire_ms(trigger: &str, last_run_ms: Option<i64>, now_ms: i64) -> Option<i64> {
    schedule_of(trigger).and_then(|s| schedule_next_fire_ms(&s, last_run_ms, now_ms))
}

/// [`next_fire_ms`] over an already-parsed [`Schedule`] (the testable core).
#[must_use]
pub fn schedule_next_fire_ms(
    schedule: &Schedule,
    last_run_ms: Option<i64>,
    now_ms: i64,
) -> Option<i64> {
    match schedule {
        // One-shot: due now until it runs, then never again. `None` after a run
        // is what makes the scheduler stop selecting it (fires exactly once).
        Schedule::Once => match last_run_ms {
            None => Some(now_ms),
            Some(_) => None,
        },
        Schedule::Interval(interval) => match last_run_ms {
            // Never run ⇒ due now: a next-fire at `now` is `<= now` (immediately
            // due), matching `trigger_is_due(.., None, now) == true`.
            None => Some(now_ms),
            Some(last) => Some(last.saturating_add(*interval)),
        },
        Schedule::Daily { hh, mm, tz } => {
            calendar_next_fire(None, *hh, *mm, *tz, last_run_ms, now_ms)
        }
        Schedule::Weekly { days, hh, mm, tz } => {
            calendar_next_fire(Some(days), *hh, *mm, *tz, last_run_ms, now_ms)
        }
    }
}

/// Shared daily/weekly next-fire: the next occurrence strictly after the last
/// run. With no last run, anchor the walk just before the most-recent occurrence
/// at or before `now` (so that occurrence — which makes the invocation due — is
/// returned), falling back to the next future occurrence if none has happened
/// yet. This reproduces [`schedule_is_due`]'s calendar branch as a timestamp.
fn calendar_next_fire(
    days: Option<&[Weekday]>,
    hh: u32,
    mm: u32,
    tz: Tz,
    last_run_ms: Option<i64>,
    now_ms: i64,
) -> Option<i64> {
    let after = match last_run_ms {
        Some(last) => last,
        None => {
            // Never run: if an occurrence is already at/before now, that one is
            // due — return it by anchoring one ms before it. Otherwise fall
            // through to the first future occurrence (not yet due).
            match last_calendar_occurrence_ms(days, hh, mm, tz, now_ms) {
                Some(occ) => occ.saturating_sub(1),
                None => now_ms,
            }
        }
    };
    next_calendar_occurrence_ms(days, hh, mm, tz, after)
}

// ── inline `agent://<id>` links (the scheduled-invocation handle) ─────────────

/// One inline `[<label>](agent://<id>)` link found in a note body: the stable
/// invocation `id` it references plus the byte offset just past the link (where
/// the scheduler appends the run output). Mirrors `parseAgentLinks` in the UI's
/// `invocations.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLink {
    /// The invocation id embedded in the link target (`agent://<id>`).
    pub id: String,
    /// The byte offset just past the closing `)` of the markdown link (where the
    /// run output is appended).
    pub link_end: usize,
}

/// Parse every inline `[<label>](agent://<id>)` link in `body`, in document
/// order. The label may contain anything but `]`; the id is the non-`)` run
/// after the `agent://` scheme. Mirrors the UI's `parseAgentLinks` so both sides
/// agree on the handle format.
#[must_use]
pub fn parse_agent_links(body: &str) -> Vec<AgentLink> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let needle = b"](agent://";
    let mut i = 0usize;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        // The link must open with a `[` somewhere before the `]` on this run; a
        // bare `](agent://…)` without an opening bracket is not a markdown link.
        // Find the matching `[` by scanning back to the nearest unbalanced `[`.
        let label_close = i; // index of `]`
        let Some(open) = body[..label_close].rfind('[') else {
            i += needle.len();
            continue;
        };
        let _ = open; // presence of `[` is all we require for the handle
        // The id runs from just after `agent://` to the closing `)`.
        let id_start = i + needle.len();
        let Some(rel_close) = body[id_start..].find(')') else {
            break; // no closing paren — malformed; stop scanning
        };
        let id_end = id_start + rel_close;
        let id = body[id_start..id_end].trim().to_string();
        if !id.is_empty() {
            out.push(AgentLink {
                id,
                link_end: id_end + 1, // just past the `)`
            });
        }
        i = id_end + 1;
    }
    out
}

// ── run-output callout cards + bidirectional block ids (embedded-runs R3) ─────
//
// A Run's output renders as an Obsidian-style `> [!run]+ …` CALLOUT below the
// chip's host paragraph (not the legacy indented blockquote). The format is
// portable markdown — a renderer that doesn't know the callout still shows a
// blockquote — and carries the bidirectional backlink block ids:
//
//   The host paragraph carries a stable block id `^run-<id>` (Obsidian block
//   ref), and the callout carries `^runout-<id>`. The callout header links back
//   to the chip (`[↑](#^run-<id>)`); the chip's `↓` jumps to `^runout-<id>`.
//   Both ids derive deterministically from the Run id, so the UI and component
//   agree without storing extra state.

/// The sha256 (hex) of the **resolved effective config** that produced an
/// execution (embedded-runs R3+R4): the Agent definition (model, instructions,
/// canonical MCP servers) layered with the Run's overrides — the Run's prompt,
/// the trigger, and (R4) the Run-scoped **mounted files**. Deterministic — the
/// same effective config always hashes identically — so a stored `config_hash`
/// reproducibly identifies *which* config ran (the reproducibility seam a later
/// versioning pass builds on, embedded-runs §4). Different mounted-file sets
/// therefore yield different hashes. Each field is tagged and length-prefixed so
/// no field boundary is ambiguous; the mounted-file list is folded in as a
/// count-prefixed, length-prefixed sequence so `["a","b"]` can never collide with
/// `["ab"]` or a reordering.
#[must_use]
pub fn config_hash(def: &AgentDef, prompt: &str, trigger: &str, files: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut field = |label: &str, value: &str| {
        hasher.update(label.as_bytes());
        hasher.update(b":");
        hasher.update(value.len().to_le_bytes());
        hasher.update(b":");
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    };
    field("agent", &def.name);
    field("kind", &def.kind);
    field("model", &def.model);
    field("instructions", &def.instructions);
    field(
        "mcp_servers",
        &canonical_servers(def.mcp_servers.clone()).join(","),
    );
    field("prompt", prompt);
    field("trigger", trigger.trim());
    // Run-scoped mounted files (R4): count-prefixed so the boundary is
    // unambiguous, then each path length-prefixed via `field`. The order is the
    // Run's stored (canonical) mount order — a remount/reorder changes the hash.
    field("mounted_files_n", &files.len().to_string());
    for (i, path) in files.iter().enumerate() {
        field(&format!("mounted_file[{i}]"), path);
    }
    let digest = hasher.finalize();
    use std::fmt::Write as _;
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The host-paragraph block id for a Run (the chip's anchor, the callout's
/// backlink target). Derived from the Run id so both sides agree.
#[must_use]
pub fn host_block_id(run_id: &str) -> String {
    format!("run-{run_id}")
}

/// The output-callout block id for a Run (the `↓` jump target, refreshed in
/// place on each run so the chip always lands on the latest output).
#[must_use]
pub fn callout_block_id(run_id: &str) -> String {
    format!("runout-{run_id}")
}

/// Build the run-output callout card markdown for one execution (embedded-runs
/// R3). An Obsidian-style `> [!run]+` foldable callout whose header is
/// `<status glyph> /<agent> · <model> · <when> [↑](#^<host_block>)` and whose
/// body is the output (each line prefixed with the blockquote `>`). It carries
/// its own block id `^<callout_block>` on the last line so the chip's `↓` can
/// target it. `when` is a short human label (e.g. `manual`, a trigger summary,
/// or a relative time) the caller supplies. Degrades to a styled blockquote in
/// any plain-markdown renderer.
#[must_use]
pub fn build_run_callout(
    run_id: &str,
    agent: &str,
    model: &str,
    when: &str,
    output: &str,
    is_error: bool,
) -> String {
    let host = host_block_id(run_id);
    let out_block = callout_block_id(run_id);
    let glyph = if is_error { "✗" } else { "✓" };
    let mut s = String::new();
    s.push_str(&format!(
        "> [!run]+ {glyph} /{agent} · {model} · {when} [↑](#^{host})\n"
    ));
    // The output body — every line prefixed with the callout's `>`. A blank
    // output still yields an empty quoted line so the card has a body.
    if output.is_empty() {
        s.push_str(">\n");
    } else {
        for line in output.split('\n') {
            s.push_str("> ");
            s.push_str(line);
            s.push('\n');
        }
    }
    // The callout's own block id on its own quoted line (so a renderer keeps it
    // inside the callout and the `↓` jump targets it).
    s.push_str(&format!("> ^{out_block}\n"));
    s
}

/// The byte offset of the end of the line containing `pos` (the offset of the
/// `\n`, or `body.len()` at EOF). The host PARAGRAPH for a chip is taken as the
/// line the chip's link sits on — the callout is inserted just after it and the
/// `^run-<id>` block id is stamped at its end.
#[must_use]
pub fn line_end(body: &str, pos: usize) -> usize {
    body[pos..].find('\n').map_or(body.len(), |rel| pos + rel)
}

/// Locate an existing run-output callout for `run_id` in `body` and return the
/// byte range `[start, end)` that the whole callout block occupies (so the
/// caller can REPLACE it on a re-run rather than appending a second card). The
/// callout is identified by its trailing block-id line `> ^runout-<id>`; the
/// block starts at the preceding `> [!run]` header line. Returns `None` when no
/// such callout exists yet (the first run appends a fresh one).
#[must_use]
pub fn find_run_callout(body: &str, run_id: &str) -> Option<(usize, usize)> {
    let marker = format!("> ^{}", callout_block_id(run_id));
    // The end of the block is the end of the line carrying the block-id marker.
    let marker_at = body.find(&marker)?;
    let line_end = body[marker_at..]
        .find('\n')
        .map_or(body.len(), |rel| marker_at + rel + 1);
    // The start of the block is the `> [!run]` header line that precedes it. We
    // scan backwards over contiguous quoted (`>`) lines to the header.
    let header_marker = "> [!run]";
    let header_at = body[..marker_at].rfind(header_marker)?;
    // Back up to the start of the header's line.
    let start = body[..header_at].rfind('\n').map_or(0, |nl| nl + 1);
    Some((start, line_end))
}

/// Whether a recurring schedule described by the raw `trigger` text is DUE at
/// `now_ms` given its last run. A `one-time`/unknown trigger returns `false`.
///
/// **Reference oracle (test-only).** The scheduler now selects due invocations
/// by the stored [`next_fire_ms`] (the next-fire model — a tick consumes only
/// what is actually due instead of re-deriving this for the whole index). This
/// predicate is retained as the correctness oracle the next-fire computation is
/// validated against: for every `now_ms`, `next_fire_ms(..) <= Some(now_ms)`
/// must equal `trigger_is_due(..)`. Hence `#[cfg(test)]`.
///
/// - **Interval** (`2m`/`2h`/`2d`/`@hourly`/`@daily`): due iff never run, or at
///   least the interval has elapsed since the last run.
/// - **Daily / Weekly**: due iff the most-recent scheduled occurrence `<= now`
///   (computed in the schedule's timezone, DST-aware) is strictly AFTER the
///   last run. This fires exactly once per occurrence (a second tick within the
///   same slot sees the same occurrence ≤ last_run and is NOT due), and a missed
///   occurrence fires on the next tick (the occurrence is still > last_run).
///   Never run ⇒ due as soon as an occurrence exists at or before now.
#[cfg(test)]
#[must_use]
pub fn trigger_is_due(trigger: &str, last_run_ms: Option<i64>, now_ms: i64) -> bool {
    match schedule_of(trigger) {
        Some(schedule) => schedule_is_due(&schedule, last_run_ms, now_ms),
        None => false,
    }
}

/// Whether the raw `trigger` text names a recurring schedule the grammar
/// understands. Test-only reference oracle now that the scheduler selects by the
/// stored [`next_fire_ms`] (which is `None` exactly for an unscheduled trigger).
#[cfg(test)]
#[must_use]
pub fn trigger_is_scheduled(trigger: &str) -> bool {
    schedule_of(trigger).is_some()
}

/// [`trigger_is_due`] over an already-parsed [`Schedule`] (the testable core of
/// the reference oracle; see [`trigger_is_due`]). Test-only.
#[cfg(test)]
#[must_use]
pub fn schedule_is_due(schedule: &Schedule, last_run_ms: Option<i64>, now_ms: i64) -> bool {
    match schedule {
        // One-shot: due iff it has never run (then never again).
        Schedule::Once => last_run_ms.is_none(),
        Schedule::Interval(interval) => match last_run_ms {
            None => true,
            Some(last) => now_ms.saturating_sub(last) >= *interval,
        },
        Schedule::Daily { hh, mm, tz } => {
            match last_calendar_occurrence_ms(None, *hh, *mm, *tz, now_ms) {
                None => false,
                Some(occ) => last_run_ms.is_none_or(|last| occ > last),
            }
        }
        Schedule::Weekly { days, hh, mm, tz } => {
            match last_calendar_occurrence_ms(Some(days), *hh, *mm, *tz, now_ms) {
                None => false,
                Some(occ) => last_run_ms.is_none_or(|last| occ > last),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An interval `Schedule` for `ms`, for terse assertions.
    fn interval(ms: i64) -> Schedule {
        Schedule::Interval(ms)
    }

    #[test]
    fn parse_interval_forms() {
        // Bare shorthand (v2): m / h / d.
        assert_eq!(parse_schedule("2m"), Some(interval(2 * MINUTE_MS)));
        assert_eq!(parse_schedule("2h"), Some(interval(2 * HOUR_MS)));
        assert_eq!(parse_schedule("2d"), Some(interval(2 * DAY_MS)));
        assert_eq!(parse_schedule("15m"), Some(interval(15 * MINUTE_MS)));
        // `every <N><unit>` synonyms.
        assert_eq!(parse_schedule("every 1m"), Some(interval(MINUTE_MS)));
        assert_eq!(parse_schedule("every 2h"), Some(interval(2 * HOUR_MS)));
        assert_eq!(parse_schedule("every 3d"), Some(interval(3 * DAY_MS)));
        // Back-compat aliases.
        assert_eq!(parse_schedule("@hourly"), Some(interval(HOUR_MS)));
        assert_eq!(parse_schedule("@daily"), Some(interval(DAY_MS)));
        assert_eq!(parse_schedule("  @hourly  "), Some(interval(HOUR_MS)));
        // Unknown / unsupported → disabled.
        assert_eq!(parse_schedule("* * * * *"), None);
        assert_eq!(parse_schedule("@weekly"), None);
        assert_eq!(parse_schedule("0m"), None);
        assert_eq!(parse_schedule("-3m"), None);
        assert_eq!(parse_schedule("5s"), None);
        assert_eq!(parse_schedule("everything"), None);
    }

    #[test]
    fn parse_calendar_forms() {
        let ny: Tz = "America/New_York".parse().unwrap();
        assert_eq!(
            parse_schedule("daily at 09:00 America/New_York"),
            Some(Schedule::Daily {
                hh: 9,
                mm: 0,
                tz: ny,
            })
        );
        assert_eq!(
            parse_schedule("weekly on mon,wed,fri at 14:00 America/New_York"),
            Some(Schedule::Weekly {
                days: vec![Weekday::Mon, Weekday::Wed, Weekday::Fri],
                hh: 14,
                mm: 0,
                tz: ny,
            })
        );
        // Day order/dupes are de-duplicated, original order preserved.
        assert_eq!(
            parse_schedule("weekly on fri,mon,fri at 00:30 UTC"),
            Some(Schedule::Weekly {
                days: vec![Weekday::Fri, Weekday::Mon],
                hh: 0,
                mm: 30,
                tz: "UTC".parse().unwrap(),
            })
        );
        // Unparseable → disabled.
        assert_eq!(parse_schedule("daily at 9 America/New_York"), None);
        assert_eq!(parse_schedule("daily at 25:00 UTC"), None);
        assert_eq!(parse_schedule("daily at 09:60 UTC"), None);
        assert_eq!(parse_schedule("daily at 09:00 Not/AZone"), None);
        assert_eq!(parse_schedule("weekly on funday at 09:00 UTC"), None);
        assert_eq!(parse_schedule("weekly on mon at 09:00"), None); // no tz
    }

    #[test]
    fn definition_is_trigger_agnostic() {
        // A definition carries NO trigger; a stray `trigger:` is ignored.
        let body = "---\nkind: skill\nname: standup\nmodel: deepseek-chat\n\
                    trigger: cron @hourly\n---\n\
                    Write a one-line status note for the team.";
        let def = parse_agent(body).expect("parses");
        assert_eq!(def.kind, "skill");
        assert_eq!(def.name, "standup");
        assert_eq!(def.model, "deepseek-chat");
        assert_eq!(
            def.instructions,
            "Write a one-line status note for the team."
        );
    }

    #[test]
    fn agent_declares_mcp_servers_canonicalized() {
        // Only `kind: agent` reads `mcp_servers:`; the set is canonicalized
        // (trim/lowercase/dedupe/sort) so source order/case never matters.
        let body = "---\nkind: agent\nname: planner\n\
                    mcp_servers: [Nutrition, notes, nutrition]\n---\nPlan it.";
        let def = parse_agent(body).unwrap();
        assert_eq!(def.mcp_servers, vec!["notes", "nutrition"]);
    }

    #[test]
    fn skill_ignores_mcp_servers() {
        // `kind: skill` parses-and-ignores `mcp_servers:` (no tools plane in T1).
        let body = "---\nkind: skill\nname: summarize\n\
                    mcp_servers: [nutrition, notes]\n---\nSummarize.";
        let def = parse_agent(body).unwrap();
        assert!(def.mcp_servers.is_empty());
    }

    #[test]
    fn agent_without_mcp_servers_requests_nothing() {
        let body = "---\nkind: agent\nname: plain\n---\nDo it.";
        assert!(parse_agent(body).unwrap().mcp_servers.is_empty());
    }

    #[test]
    fn bare_scalar_mcp_servers_is_one_element() {
        let body = "---\nkind: agent\nname: x\nmcp_servers: nutrition\n---\nb";
        assert_eq!(parse_agent(body).unwrap().mcp_servers, vec!["nutrition"]);
    }

    #[test]
    fn mcp_request_hash_is_order_insensitive_and_set_sensitive() {
        // Same set, different source order/case ⇒ same hash.
        let a = mcp_request_hash(&["nutrition".into(), "notes".into()]);
        let b = mcp_request_hash(&["NOTES".into(), "Nutrition".into()]);
        assert_eq!(a, b, "canonicalized set ⇒ same hash");
        // Adding a server ⇒ a different hash (the request changed → would stale).
        let c = mcp_request_hash(&["notes".into(), "nutrition".into(), "shell".into()]);
        assert_ne!(a, c);
        // The empty request hashes too (distinct, stable).
        assert_eq!(mcp_request_hash(&[]), mcp_request_hash(&[]));
        assert_ne!(mcp_request_hash(&[]), a);
    }

    #[test]
    fn model_defaults_when_absent() {
        let body = "---\nkind: agent\nname: foo\n---\nDo it.";
        let def = parse_agent(body).unwrap();
        assert_eq!(def.model, DEFAULT_MODEL);
        assert_eq!(def.name, "foo");
    }

    #[test]
    fn non_agent_notes_are_ignored() {
        assert!(parse_agent("# just a note\n\nbody").is_none());
        assert!(parse_agent("---\nkind: note\nname: x\n---\nbody").is_none());
        // Missing name.
        assert!(parse_agent("---\nkind: skill\n---\nbody").is_none());
        // Unterminated frontmatter.
        assert!(parse_agent("---\nkind: skill\nname: x\nbody").is_none());
    }

    #[test]
    fn schedule_of_strips_cron_and_handles_one_time() {
        assert_eq!(schedule_str("cron @hourly"), Some("@hourly"));
        assert_eq!(
            schedule_of("cron @hourly"),
            Some(Schedule::Interval(HOUR_MS))
        );
        assert_eq!(schedule_str("one-time"), None);
        assert_eq!(schedule_of("one-time"), None);
        assert_eq!(schedule_str("  "), None);
    }

    #[test]
    fn cronish_prefix_is_not_a_schedule() {
        // `cronish whatever` must NOT be read as a schedule (the `cron` prefix
        // requires a whitespace separator; the whole string fails the grammar).
        assert!(!trigger_is_scheduled("cronish whatever"));
        assert_eq!(schedule_of("cronish whatever"), None);
    }

    #[test]
    fn bare_interval_trigger_without_cron_prefix() {
        // v2: a bare `2d` trigger (no `cron ` prefix) is a first-class schedule.
        assert!(trigger_is_scheduled("2d"));
        assert_eq!(schedule_of("2d"), Some(Schedule::Interval(2 * DAY_MS)));
    }

    #[test]
    fn unknown_schedule_is_disabled() {
        // @weekly is not in the grammar → not scheduled.
        assert!(!trigger_is_scheduled("cron @weekly"), "@weekly is unknown");
        assert_eq!(schedule_of("cron @weekly"), None);
        assert!(!trigger_is_due("cron @weekly", None, 1_000_000));
    }

    #[test]
    fn interval_due_logic() {
        // A legacy `cron ` prefix and the new bare form behave identically.
        let trig = "cron every 1m";
        // Never run → due.
        assert!(trigger_is_due(trig, None, 0));
        // Run just now → not due.
        assert!(!trigger_is_due(trig, Some(1_000_000), 1_000_000));
        // Run a minute ago → due.
        assert!(trigger_is_due(trig, Some(1_000_000), 1_000_000 + MINUTE_MS));
        // Run 59s ago → not yet.
        assert!(!trigger_is_due(
            trig,
            Some(1_000_000),
            1_000_000 + MINUTE_MS - 1
        ));
    }

    #[test]
    fn back_compat_hourly_trigger_still_fires() {
        // An old `cron @hourly` trigger keeps working under v2.
        let trig = "cron @hourly";
        assert_eq!(schedule_of(trig), Some(Schedule::Interval(HOUR_MS)));
        assert!(trigger_is_due(trig, None, 0));
        assert!(!trigger_is_due(
            trig,
            Some(1_000_000),
            1_000_000 + HOUR_MS - 1
        ));
        assert!(trigger_is_due(trig, Some(1_000_000), 1_000_000 + HOUR_MS));
    }

    // ── calendar (daily/weekly) due-computation, DST-aware via chrono-tz ──────

    /// epoch-ms for a wall-clock instant in a named zone (test helper).
    fn ms_at(tz_name: &str, y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        let tz: Tz = tz_name.parse().unwrap();
        tz.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn daily_fires_once_per_occurrence_across_day_boundary() {
        let sched = parse_schedule("daily at 09:00 America/New_York").unwrap();
        // "now" = 2026-03-10 10:00 ET — the 09:00 slot today has passed.
        let now = ms_at("America/New_York", 2026, 3, 10, 10, 0);
        // Never run → due (the 09:00 occurrence is ≤ now).
        assert!(schedule_is_due(&sched, None, now));
        // Last run right at today's 09:00 occurrence → not due again today.
        let today_9 = ms_at("America/New_York", 2026, 3, 10, 9, 0);
        assert!(!schedule_is_due(&sched, Some(today_9), now));
        // Last run was yesterday's 09:00 → today's occurrence is newer → due.
        let yesterday_9 = ms_at("America/New_York", 2026, 3, 9, 9, 0);
        assert!(schedule_is_due(&sched, Some(yesterday_9), now));
        // Before today's 09:00 (08:00 now), last run yesterday → the most-recent
        // occurrence ≤ now is yesterday's, == last_run → NOT due yet.
        let now_8am = ms_at("America/New_York", 2026, 3, 10, 8, 0);
        assert!(!schedule_is_due(&sched, Some(yesterday_9), now_8am));
    }

    #[test]
    fn weekly_fires_only_on_selected_days() {
        // Mon/Wed/Fri at 14:00 ET. 2026-06-15 is a Monday.
        let sched = parse_schedule("weekly on mon,wed,fri at 14:00 America/New_York").unwrap();
        // Monday 15:00 → Monday's 14:00 occurrence passed, never run → due.
        let mon_3pm = ms_at("America/New_York", 2026, 6, 15, 15, 0);
        assert!(schedule_is_due(&sched, None, mon_3pm));
        // Fired at Monday 14:00 → not due again until the next selected day.
        let mon_2pm = ms_at("America/New_York", 2026, 6, 15, 14, 0);
        assert!(!schedule_is_due(&sched, Some(mon_2pm), mon_3pm));
        // Tuesday (not selected) all day → most-recent occurrence is still
        // Monday 14:00 == last_run → NOT due.
        let tue_4pm = ms_at("America/New_York", 2026, 6, 16, 16, 0);
        assert!(!schedule_is_due(&sched, Some(mon_2pm), tue_4pm));
        // Wednesday 14:30 → Wednesday's occurrence is newer than Mon → due.
        let wed_230pm = ms_at("America/New_York", 2026, 6, 17, 14, 30);
        assert!(schedule_is_due(&sched, Some(mon_2pm), wed_230pm));
    }

    #[test]
    fn missed_occurrence_fires_on_next_tick() {
        // Daily at 09:00 UTC; the host was down and missed several days. Last
        // run was 3 days ago; "now" is past today's 09:00 → still due (the most-
        // recent occurrence is newer than last_run, fires once, catching up).
        let sched = parse_schedule("daily at 09:00 UTC").unwrap();
        let now = ms_at("UTC", 2026, 6, 15, 10, 0);
        let three_days_ago_9 = ms_at("UTC", 2026, 6, 12, 9, 0);
        assert!(schedule_is_due(&sched, Some(three_days_ago_9), now));
    }

    // ── inline `agent://<id>` link parsing + index-keyed due check ────────────

    // ── embedded-runs R3: `once` schedule + callout cards + block ids ─────────

    #[test]
    fn once_schedule_parses_and_fires_exactly_once() {
        assert_eq!(parse_schedule("once"), Some(Schedule::Once));
        assert!(trigger_is_scheduled("once"));
        // Never run ⇒ due now; once run ⇒ never again (no next fire).
        assert!(schedule_is_due(&Schedule::Once, None, 1_000));
        assert!(!schedule_is_due(&Schedule::Once, Some(1_000), 9_999_999));
        assert_eq!(
            schedule_next_fire_ms(&Schedule::Once, None, 1_000),
            Some(1_000)
        );
        assert_eq!(
            schedule_next_fire_ms(&Schedule::Once, Some(1_000), 2_000),
            None
        );
        // The legacy `one-time` sentinel is still "no schedule".
        assert_eq!(schedule_of("one-time"), None);
    }

    #[test]
    fn build_run_callout_is_a_portable_card_with_block_ids() {
        let card = build_run_callout(
            "abc",
            "standup",
            "deepseek-chat",
            "one-time",
            "line1\nline2",
            false,
        );
        // Obsidian-style foldable callout header with the status glyph + backlink.
        assert!(
            card.starts_with("> [!run]+ ✓ /standup · deepseek-chat · one-time [↑](#^run-abc)\n")
        );
        // Each output line is quoted; the callout carries its own block id.
        assert!(card.contains("> line1\n"));
        assert!(card.contains("> line2\n"));
        assert!(card.contains("> ^runout-abc\n"));
        // An error run uses the ✗ glyph.
        let err = build_run_callout("abc", "a", "m", "one-time", "boom", true);
        assert!(err.contains("> [!run]+ ✗ /a"));
    }

    #[test]
    fn find_run_callout_locates_the_block_for_replacement() {
        let host_line = "Run [⚡ standup](agent://abc) every day. ^run-abc";
        let card = build_run_callout("abc", "standup", "m", "one-time", "out", false);
        let body = format!("# Daily\n\n{host_line}\n\n{card}\nmore text\n");
        let (start, end) = find_run_callout(&body, "abc").expect("callout present");
        // The located span is exactly the callout card.
        assert_eq!(&body[start..end], &card[..]);
        // A body with no callout for the id returns None.
        assert_eq!(find_run_callout("no callout here", "abc"), None);
    }

    #[test]
    fn config_hash_is_deterministic_and_config_sensitive() {
        let def = AgentDef {
            kind: "skill".into(),
            name: "standup".into(),
            model: "deepseek-chat".into(),
            instructions: "Write a status.".into(),
            mcp_servers: vec![],
        };
        let a = config_hash(&def, "prompt", "once", &[]);
        let b = config_hash(&def, "prompt", "once", &[]);
        assert_eq!(a, b, "same config ⇒ same hash");
        assert_eq!(a.len(), 64, "sha256 hex");
        // Any change to the effective config changes the hash.
        assert_ne!(a, config_hash(&def, "other", "once", &[]));
        assert_ne!(a, config_hash(&def, "prompt", "daily at 09:00 UTC", &[]));
        let mut def2 = def.clone();
        def2.model = "deepseek-reasoner".into();
        assert_ne!(a, config_hash(&def2, "prompt", "once", &[]));
        // embedded-runs R4: the Run-scoped mounted files fold into the hash —
        // a different mount set (or a reorder) yields a different hash.
        let one = config_hash(&def, "prompt", "once", &["notes/a.md".into()]);
        assert_ne!(a, one, "mounting a file changes the hash");
        let two = config_hash(
            &def,
            "prompt",
            "once",
            &["notes/a.md".into(), "notes/b.md".into()],
        );
        assert_ne!(one, two, "a different mount set changes the hash");
        let reordered = config_hash(
            &def,
            "prompt",
            "once",
            &["notes/b.md".into(), "notes/a.md".into()],
        );
        assert_ne!(two, reordered, "mount order is part of the hash");
        // Stable for the same mount set.
        assert_eq!(
            one,
            config_hash(&def, "prompt", "once", &["notes/a.md".into()])
        );
    }

    #[test]
    fn parse_agent_links_finds_inline_handles() {
        let body = "Daily check: [⚡ standup](agent://abc123) then more text.\n\
                    Another [run me](agent://def456).";
        let links = parse_agent_links(body);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].id, "abc123");
        assert_eq!(links[1].id, "def456");
        // link_end points just past the closing `)`.
        assert_eq!(&body[links[0].link_end..links[0].link_end + 5], " then");
    }

    #[test]
    fn parse_agent_links_ignores_non_links() {
        // No opening `[` → not a markdown link; bare scheme is ignored.
        assert!(parse_agent_links("see agent://abc for details").is_empty());
        // Empty id is skipped.
        assert!(parse_agent_links("[x](agent://)").is_empty());
        // A normal wikilink/url is not an agent link.
        assert!(parse_agent_links("[note](other://abc) [[wiki]]").is_empty());
    }

    #[test]
    fn trigger_is_due_matches_block_path() {
        // A bare interval trigger via the index behaves like the block path.
        assert!(trigger_is_due("1m", None, 0));
        assert!(!trigger_is_due("1m", Some(1_000_000), 1_000_000));
        assert!(trigger_is_due("1m", Some(1_000_000), 1_000_000 + MINUTE_MS));
        // one-time / unknown → never due.
        assert!(!trigger_is_due("one-time", None, 0));
        assert!(!trigger_is_due("@weekly", None, 0));
        assert!(trigger_is_scheduled("daily at 09:00 UTC"));
        assert!(!trigger_is_scheduled("one-time"));
    }

    // ── next_fire_ms: the stored next-fire, inverse of the due check ──────────

    /// Assert the next-fire/due equivalence at a given `now`: `next_fire <= now`
    /// iff `trigger_is_due`. This is the invariant the scheduler relies on.
    fn assert_next_fire_matches_due(trigger: &str, last_run: Option<i64>, now: i64) {
        let nf = next_fire_ms(trigger, last_run, now);
        let due = trigger_is_due(trigger, last_run, now);
        let nf_due = nf.is_some_and(|t| t <= now);
        assert_eq!(
            nf_due, due,
            "next_fire {nf:?} <= now {now} ({nf_due}) must equal trigger_is_due ({due}) \
             for {trigger:?} last_run {last_run:?}"
        );
    }

    #[test]
    fn next_fire_interval_advances_by_interval() {
        // Never run ⇒ fire now (immediately due).
        assert_eq!(next_fire_ms("every 1m", None, 1_000_000), Some(1_000_000));
        // Run at T ⇒ next fire is T + interval.
        assert_eq!(
            next_fire_ms("every 1m", Some(1_000_000), 1_000_000),
            Some(1_000_000 + MINUTE_MS)
        );
        assert_eq!(
            next_fire_ms("@hourly", Some(5_000_000), 5_000_000),
            Some(5_000_000 + HOUR_MS)
        );
        // one-time / unknown ⇒ no next fire.
        assert_eq!(next_fire_ms("one-time", None, 0), None);
        assert_eq!(next_fire_ms("@weekly", None, 0), None);
    }

    #[test]
    fn next_fire_matches_due_for_intervals() {
        // Sweep the boundary around `last + interval` for an interval schedule.
        let last = 1_000_000;
        for delta in [0, MINUTE_MS - 1, MINUTE_MS, MINUTE_MS + 1, 5 * MINUTE_MS] {
            assert_next_fire_matches_due("every 1m", Some(last), last + delta);
        }
        // Never run is due at any now.
        assert_next_fire_matches_due("every 1m", None, 1_000_000);
    }

    #[test]
    fn next_fire_daily_is_next_occurrence_after_last_run() {
        let sched = parse_schedule("daily at 09:00 America/New_York").unwrap();
        // Ran at today's 09:00 ⇒ next fire is tomorrow's 09:00 (DST-aware).
        let today_9 = ms_at("America/New_York", 2026, 3, 10, 9, 0);
        let now = ms_at("America/New_York", 2026, 3, 10, 10, 0);
        let tomorrow_9 = ms_at("America/New_York", 2026, 3, 11, 9, 0);
        assert_eq!(
            schedule_next_fire_ms(&sched, Some(today_9), now),
            Some(tomorrow_9)
        );
        // Never run, now is past today's 09:00 ⇒ next fire is today's 09:00
        // (≤ now ⇒ immediately due, the catch-up case).
        let nf = schedule_next_fire_ms(&sched, None, now).unwrap();
        assert_eq!(nf, today_9);
        assert!(nf <= now);
    }

    #[test]
    fn next_fire_weekly_skips_to_next_selected_day() {
        // Mon/Wed/Fri at 14:00 ET. Ran Monday 14:00 ⇒ next fire Wednesday 14:00.
        let sched = parse_schedule("weekly on mon,wed,fri at 14:00 America/New_York").unwrap();
        let mon_2pm = ms_at("America/New_York", 2026, 6, 15, 14, 0);
        let tue_4pm = ms_at("America/New_York", 2026, 6, 16, 16, 0);
        let wed_2pm = ms_at("America/New_York", 2026, 6, 17, 14, 0);
        assert_eq!(
            schedule_next_fire_ms(&sched, Some(mon_2pm), tue_4pm),
            Some(wed_2pm)
        );
    }

    #[test]
    fn next_fire_matches_due_for_calendar_schedules() {
        // Daily: sweep before/at/after today's slot, never-run and last-run cases.
        let daily = "daily at 09:00 America/New_York";
        let yesterday_9 = ms_at("America/New_York", 2026, 3, 9, 9, 0);
        let today_9 = ms_at("America/New_York", 2026, 3, 10, 9, 0);
        for now in [
            ms_at("America/New_York", 2026, 3, 10, 8, 0),
            today_9,
            ms_at("America/New_York", 2026, 3, 10, 10, 0),
        ] {
            assert_next_fire_matches_due(daily, None, now);
            assert_next_fire_matches_due(daily, Some(yesterday_9), now);
            assert_next_fire_matches_due(daily, Some(today_9), now);
        }
        // Weekly across a non-selected day and a selected day.
        let weekly = "weekly on mon,wed,fri at 14:00 America/New_York";
        let mon_2pm = ms_at("America/New_York", 2026, 6, 15, 14, 0);
        for now in [
            ms_at("America/New_York", 2026, 6, 15, 15, 0), // Mon after slot
            ms_at("America/New_York", 2026, 6, 16, 16, 0), // Tue (not selected)
            ms_at("America/New_York", 2026, 6, 17, 14, 30), // Wed after slot
        ] {
            assert_next_fire_matches_due(weekly, None, now);
            assert_next_fire_matches_due(weekly, Some(mon_2pm), now);
        }
    }

    #[test]
    fn next_fire_daily_is_dst_aware() {
        // 09:00 ET the day after US spring-forward must resolve to 13:00 UTC
        // (EDT), not a naive fixed-offset 14:00 UTC.
        let sched = parse_schedule("daily at 09:00 America/New_York").unwrap();
        // Ran at 09:00 EST on 2026-03-07 ⇒ next fire is 09:00 EDT 2026-03-08
        // (the spring-forward day) at 13:00 UTC.
        let ran = ms_at("America/New_York", 2026, 3, 7, 9, 0);
        let now = ms_at("America/New_York", 2026, 3, 7, 10, 0);
        let next_edt = ms_at("UTC", 2026, 3, 8, 13, 0);
        assert_eq!(
            schedule_next_fire_ms(&sched, Some(ran), now),
            Some(next_edt)
        );
    }

    #[test]
    fn daily_is_dst_aware_spring_forward() {
        // US spring-forward 2026 is 2026-03-08 (clocks jump 02:00→03:00 ET).
        // A 09:00 ET daily slot is well clear of the gap, but the UTC offset
        // changes that day (EST→EDT). Verify the occurrence resolves to the
        // correct EDT wall-clock instant (13:00 UTC, not 14:00).
        let sched = parse_schedule("daily at 09:00 America/New_York").unwrap();
        let now = ms_at("America/New_York", 2026, 3, 9, 10, 0); // day after DST
        // The occurrence used by is_due must equal 09:00 EDT on 2026-03-09,
        // i.e. 13:00 UTC. A naive fixed-offset scheduler would be an hour off.
        let occ_edt = ms_at("UTC", 2026, 3, 9, 13, 0);
        // Last-run exactly at the correct EDT occurrence ⇒ not due; one ms
        // before ⇒ due. This pins the occurrence instant.
        assert!(!schedule_is_due(&sched, Some(occ_edt), now));
        assert!(schedule_is_due(&sched, Some(occ_edt - 1), now));
    }
}
