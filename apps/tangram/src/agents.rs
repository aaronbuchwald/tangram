//! Agent/skill execution support for the vault: the definition frontmatter
//! parser, the ```agent``` invocation-block parser, and the v2 schedule grammar
//! (interval shorthand `2m`/`2h`/`2d` + calendar daily/weekly-at-a-time with a
//! DST-aware IANA timezone, plus the `@hourly`/`@daily` back-compat aliases).
//!
//! R1 — the trigger belongs to the INVOCATION, not the definition:
//!
//! - A **definition** (`agents/…` note) is a pure capability: `kind`, `name`,
//!   `model`, instructions, labels. It carries NO trigger. This mirrors the
//!   fields `apps/tangram/ui/src/agents.ts` parses (any stray `trigger:` left in
//!   an old definition is parsed-and-ignored).
//! - An **invocation** is a durable instance — a fenced ```` ```agent ```` block
//!   inside any note — that owns the `trigger` + `prompt` and links to a
//!   definition via `use:`. It is derived from the file text (so editing or
//!   removing the block self-cleans, stray-ref-safe), keyed by a stable
//!   `invocation_id`. This mirrors `apps/tangram/ui/src/invocations.ts` EXACTLY.
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

/// A parsed ```` ```agent ```` invocation block: a durable instance inside a
/// note that links to a definition (`use:`) and owns the `trigger` + `prompt`.
/// The source of truth for whether/how an agent runs (R1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The definition this invocation runs (the `use:` field — a definition
    /// `name`).
    pub use_name: String,
    /// The raw `trigger:` text, e.g. `2h`, `daily at 09:00 America/New_York`,
    /// `weekly on mon,wed,fri at 14:00 America/New_York`, `@daily`, or
    /// `one-time`. A legacy `cron ` prefix is accepted (back-compat).
    pub trigger: String,
    /// The `prompt:` text (may span multiple lines until the closing fence).
    pub prompt: String,
    /// A stable hash of `{host_file_id + use + trigger + prompt}` — unchanged
    /// while the block is unedited, new on any edit, gone when removed
    /// (stray-ref-safe). Mirrors `invocationId` in the UI's `invocations.ts`.
    pub invocation_id: String,
    /// The byte offset just past the closing ```` ``` ```` fence in the source
    /// body (where the run output is appended), or the source body length when
    /// the block runs to EOF without a closing fence.
    pub block_end: usize,
}

impl Invocation {
    /// The schedule portion of the trigger, stripped of the optional back-compat
    /// `cron ` prefix. `daily at …`/`weekly on …`/`2m`/`@hourly` (with or
    /// without the legacy `cron ` prefix) all return the bare schedule text;
    /// `one-time` returns `None`.
    ///
    /// v2: the trigger no longer REQUIRES a `cron ` prefix — bare schedules
    /// (`2m`, `daily at 09:00 America/New_York`, …) are first-class. The prefix
    /// is still accepted so existing `cron @hourly` blocks keep parsing.
    #[must_use]
    pub fn schedule_str(&self) -> Option<&str> {
        let t = self.trigger.trim();
        if t.is_empty() || t == "one-time" {
            return None;
        }
        // Strip an optional leading `cron` token (back-compat with v1 blocks).
        if let Some(rest) = t.strip_prefix("cron") {
            if rest.is_empty() || !rest.starts_with(char::is_whitespace) {
                // `cronish …` is NOT a cron prefix; fall through and let the
                // grammar reject it (it won't parse), i.e. disabled.
                return Some(t);
            }
            return Some(rest.trim());
        }
        Some(t)
    }

    /// The parsed [`Schedule`], if this invocation declares one the grammar
    /// understands. `None` ⇒ disabled (`one-time`, or an unknown/unparseable
    /// schedule — the caller skips it).
    #[must_use]
    pub fn schedule(&self) -> Option<Schedule> {
        self.schedule_str().and_then(parse_schedule)
    }

    /// Whether this invocation declares a schedule the grammar understands
    /// (i.e. the host scheduler should consider it).
    #[must_use]
    pub fn is_scheduled(&self) -> bool {
        self.schedule().is_some()
    }
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

// ── invocation-block parsing (the ```agent fenced block) ─────────────────────

/// Parse every ```` ```agent ```` invocation block in `body`, in document
/// order. Each block has the shape:
///
/// ```text
/// ```agent
/// use: <definition-name>
/// trigger: 1h                     # or "daily at 09:00 America/New_York", "one-time"
/// prompt: <prompt text, may span
/// multiple lines until the fence>
/// ```
/// ```
///
/// `use`/`trigger`/`prompt` are flat `key: value` lines at the top of the
/// block; everything after the `prompt:` line (until the closing fence) is part
/// of the prompt (so it may span multiple lines). A block missing `use` is
/// skipped (it cannot resolve a definition). Mirrors `parseInvocations` in
/// `ui/src/invocations.ts` EXACTLY so the UI and component agree on the format.
///
/// `host_file_id` is folded into each block's `invocation_id` so the same block
/// text in two different notes gets distinct ids.
#[must_use]
pub fn parse_invocations(host_file_id: &str, body: &str) -> Vec<Invocation> {
    let mut out = Vec::new();
    let lines: Vec<&str> = body.split('\n').collect();
    // Byte offset of the start of each line, so we can report `block_end`.
    let mut line_start = Vec::with_capacity(lines.len() + 1);
    {
        let mut acc = 0usize;
        for l in &lines {
            line_start.push(acc);
            acc += l.len() + 1; // +1 for the '\n' that split removed
        }
        line_start.push(acc); // sentinel = body.len() + 1
    }

    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].trim() != "```agent" {
            i += 1;
            continue;
        }
        // Collect block lines until the closing fence.
        let mut j = i + 1;
        let mut block: Vec<&str> = Vec::new();
        let mut closed = false;
        while j < lines.len() {
            if lines[j].trim() == "```" {
                closed = true;
                break;
            }
            block.push(lines[j]);
            j += 1;
        }
        // `block_end`: the offset just past the closing fence line's newline,
        // or the body length when the block ran to EOF without a fence.
        let block_end = if closed {
            // line `j` is the closing fence; the next line starts after it.
            line_start
                .get(j + 1)
                .copied()
                .map_or(body.len(), |s| s.min(body.len()))
        } else {
            body.len()
        };

        if let Some(inv) = parse_invocation_block(host_file_id, &block, block_end) {
            out.push(inv);
        }
        // Resume scanning after the closing fence (or at EOF).
        i = if closed { j + 1 } else { j };
    }
    out
}

/// Parse the inner lines of one ```` ```agent ```` block into an [`Invocation`].
/// Returns `None` when `use` is missing/empty (an unresolvable block).
fn parse_invocation_block(
    host_file_id: &str,
    block: &[&str],
    block_end: usize,
) -> Option<Invocation> {
    let mut use_name: Option<String> = None;
    let mut trigger: Option<String> = None;
    let mut prompt: Option<String> = None;

    let mut k = 0usize;
    while k < block.len() {
        let line = block[k];
        let Some(idx) = line.find(':') else {
            k += 1;
            continue;
        };
        let key = line[..idx].trim().to_ascii_lowercase();
        let val = line[idx + 1..].trim();
        match key.as_str() {
            "use" if use_name.is_none() => use_name = Some(val.to_string()),
            "trigger" if trigger.is_none() => trigger = Some(val.to_string()),
            "prompt" if prompt.is_none() => {
                // The prompt runs from this line's value to the end of the
                // block (multi-line). The first line is the inline value; any
                // following block lines are appended verbatim.
                let mut parts: Vec<String> = vec![val.to_string()];
                for rest in &block[k + 1..] {
                    parts.push((*rest).to_string());
                }
                let joined = parts.join("\n");
                prompt = Some(joined.trim().to_string());
                break;
            }
            _ => {}
        }
        k += 1;
    }

    let use_name = use_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let trigger = trigger.unwrap_or_else(|| "one-time".to_string());
    let prompt = prompt.unwrap_or_default();
    let invocation_id = invocation_id(host_file_id, &use_name, &trigger, &prompt);

    Some(Invocation {
        use_name,
        trigger,
        prompt,
        invocation_id,
        block_end,
    })
}

/// A stable id for an invocation: a hex FNV-1a hash of
/// `host_file_id\0use\0trigger\0prompt`. Mirrors `invocationId` in the UI's
/// `invocations.ts` byte-for-byte (same fields, same separator, same FNV-1a)
/// so the UI and the component derive identical ids for the same block.
#[must_use]
pub fn invocation_id(host_file_id: &str, use_name: &str, trigger: &str, prompt: &str) -> String {
    let key = format!("{host_file_id}\0{use_name}\0{trigger}\0{prompt}");
    fnv1a_hex(&key)
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

/// Whether a scheduled invocation is DUE at `now_ms` given its last run.
///
/// - **Interval** (`2m`/`2h`/`2d`/`@hourly`/`@daily`): due iff never run, or at
///   least the interval has elapsed since the last run.
/// - **Daily / Weekly**: due iff the most-recent scheduled occurrence `<= now`
///   (computed in the schedule's timezone, DST-aware) is strictly AFTER the
///   last run. This fires exactly once per occurrence (a second tick within the
///   same slot sees the same occurrence ≤ last_run and is NOT due), and a missed
///   occurrence fires on the next tick (the occurrence is still > last_run).
///   Never run ⇒ due as soon as an occurrence exists at or before now.
///
/// A trigger the grammar does not understand (or `one-time`) returns `false`.
#[must_use]
pub fn is_due(inv: &Invocation, last_run_ms: Option<i64>, now_ms: i64) -> bool {
    let Some(schedule) = inv.schedule() else {
        return false;
    };
    schedule_is_due(&schedule, last_run_ms, now_ms)
}

/// [`is_due`] over an already-parsed [`Schedule`] (the testable core).
#[must_use]
pub fn schedule_is_due(schedule: &Schedule, last_run_ms: Option<i64>, now_ms: i64) -> bool {
    match schedule {
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
    fn parse_one_cron_invocation() {
        let body = "Some note.\n\n```agent\nuse: standup\ntrigger: cron @hourly\n\
                    prompt: Summarize today.\n```\n\nMore text.";
        let invs = parse_invocations("file-1", body);
        assert_eq!(invs.len(), 1);
        let inv = &invs[0];
        assert_eq!(inv.use_name, "standup");
        assert_eq!(inv.trigger, "cron @hourly");
        assert_eq!(inv.prompt, "Summarize today.");
        assert!(inv.is_scheduled());
        assert_eq!(inv.schedule_str(), Some("@hourly"));
        assert_eq!(inv.schedule(), Some(Schedule::Interval(HOUR_MS)));
        // block_end points right after the closing fence line.
        assert_eq!(&body[inv.block_end..], "\nMore text.");
    }

    #[test]
    fn one_time_invocation_is_not_cron() {
        let body = "```agent\nuse: foo\ntrigger: one-time\nprompt: Hi.\n```";
        let invs = parse_invocations("f", body);
        assert_eq!(invs.len(), 1);
        assert!(!invs[0].is_scheduled());
        assert_eq!(invs[0].schedule_str(), None);
        assert_eq!(invs[0].schedule(), None);
    }

    #[test]
    fn trigger_defaults_to_one_time() {
        let body = "```agent\nuse: foo\nprompt: Hi.\n```";
        let invs = parse_invocations("f", body);
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].trigger, "one-time");
        assert!(!invs[0].is_scheduled());
    }

    #[test]
    fn multi_line_prompt_runs_to_fence() {
        let body = "```agent\nuse: foo\ntrigger: cron every 1m\nprompt: line one\nline two\nline three\n```";
        let invs = parse_invocations("f", body);
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].prompt, "line one\nline two\nline three");
        assert_eq!(invs[0].schedule(), Some(Schedule::Interval(MINUTE_MS)));
    }

    #[test]
    fn block_without_use_is_skipped() {
        let body = "```agent\ntrigger: cron @hourly\nprompt: orphan\n```";
        assert!(parse_invocations("f", body).is_empty());
    }

    #[test]
    fn multiple_blocks_parse_in_order() {
        let body = "```agent\nuse: a\ntrigger: one-time\nprompt: first\n```\n\n\
                    ```agent\nuse: b\ntrigger: cron @daily\nprompt: second\n```";
        let invs = parse_invocations("f", body);
        assert_eq!(invs.len(), 2);
        assert_eq!(invs[0].use_name, "a");
        assert_eq!(invs[1].use_name, "b");
        assert_eq!(invs[1].schedule(), Some(Schedule::Interval(DAY_MS)));
    }

    #[test]
    fn invocation_id_is_stable_and_field_sensitive() {
        let a = invocation_id("f", "foo", "cron @hourly", "p");
        let b = invocation_id("f", "foo", "cron @hourly", "p");
        assert_eq!(a, b, "same fields ⇒ same id");
        // Any field change ⇒ a different id.
        assert_ne!(a, invocation_id("g", "foo", "cron @hourly", "p"));
        assert_ne!(a, invocation_id("f", "bar", "cron @hourly", "p"));
        assert_ne!(a, invocation_id("f", "foo", "cron @daily", "p"));
        assert_ne!(a, invocation_id("f", "foo", "cron @hourly", "q"));
    }

    #[test]
    fn cronish_prefix_is_not_a_schedule() {
        // `cronish whatever` must NOT be read as a schedule (the `cron` prefix
        // requires a whitespace separator; the whole string fails the grammar).
        let inv = Invocation {
            use_name: "x".into(),
            trigger: "cronish whatever".into(),
            prompt: String::new(),
            invocation_id: "id".into(),
            block_end: 0,
        };
        assert!(!inv.is_scheduled());
        assert_eq!(inv.schedule(), None);
    }

    #[test]
    fn bare_interval_trigger_without_cron_prefix() {
        // v2: a bare `2d` trigger (no `cron ` prefix) is a first-class schedule.
        let body = "```agent\nuse: x\ntrigger: 2d\nprompt: p\n```";
        let inv = &parse_invocations("f", body)[0];
        assert!(inv.is_scheduled());
        assert_eq!(inv.schedule(), Some(Schedule::Interval(2 * DAY_MS)));
    }

    #[test]
    fn unknown_schedule_is_disabled() {
        let body = "```agent\nuse: x\ntrigger: cron @weekly\nprompt: p\n```";
        let inv = &parse_invocations("f", body)[0];
        // @weekly is not in the grammar → not scheduled.
        assert!(!inv.is_scheduled(), "@weekly is an unknown schedule");
        assert_eq!(inv.schedule(), None);
        assert!(!is_due(inv, None, 1_000_000));
    }

    #[test]
    fn interval_due_logic() {
        // Back-compat block (legacy `cron ` prefix, but new `1m` is fine too).
        let body = "```agent\nuse: x\ntrigger: cron every 1m\nprompt: p\n```";
        let inv = &parse_invocations("f", body)[0];
        // Never run → due.
        assert!(is_due(inv, None, 0));
        // Run just now → not due.
        assert!(!is_due(inv, Some(1_000_000), 1_000_000));
        // Run a minute ago → due.
        assert!(is_due(inv, Some(1_000_000), 1_000_000 + MINUTE_MS));
        // Run 59s ago → not yet.
        assert!(!is_due(inv, Some(1_000_000), 1_000_000 + MINUTE_MS - 1));
    }

    #[test]
    fn back_compat_hourly_block_still_fires() {
        // An old `cron @hourly` block keeps working under v2.
        let body = "```agent\nuse: x\ntrigger: cron @hourly\nprompt: p\n```";
        let inv = &parse_invocations("f", body)[0];
        assert_eq!(inv.schedule(), Some(Schedule::Interval(HOUR_MS)));
        assert!(is_due(inv, None, 0));
        assert!(!is_due(inv, Some(1_000_000), 1_000_000 + HOUR_MS - 1));
        assert!(is_due(inv, Some(1_000_000), 1_000_000 + HOUR_MS));
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
