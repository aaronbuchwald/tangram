//! Agent/skill execution support for the vault: the minimal frontmatter parser
//! and the v1 cron/interval schedule grammar.
//!
//! This mirrors the fields `apps/tangram/ui/src/agents.ts` already parses
//! (`kind`, `name`, `model`, `trigger: { type, schedule }`) but lives in the
//! component so the host-driven `tick_agents` action can decide which agent
//! notes are DUE and run them. Everything here is pure (no I/O, no clock), so
//! it is straightforward to unit-test; the action layer in `lib.rs` supplies
//! the wall clock and the LLM egress.

/// The default model when a definition omits `model` (matches
/// `DEFAULT_MODEL` in `ui/src/agents.ts`).
pub const DEFAULT_MODEL: &str = "deepseek-chat";

/// A parsed agent/skill definition from a note's leading frontmatter. Only the
/// fields v1 triggers need are surfaced; richer frontmatter is parsed-and-
/// ignored (forward-compatible with the UI's superset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// `agent` or `skill`.
    pub kind: String,
    /// The agent's name (the `/<name>` handle).
    pub name: String,
    /// The model id to call (defaults to [`DEFAULT_MODEL`]).
    pub model: String,
    /// The trigger, if one is declared. v1 only acts on `cron`.
    pub trigger: Option<Trigger>,
    /// The note body after the closing `---` — the system prompt / task.
    pub instructions: String,
}

/// A parsed `trigger: { type, schedule }`. v1 only executes `type == "cron"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub trigger_type: String,
    pub schedule: Option<String>,
}

impl AgentDef {
    /// Whether this definition is a cron-triggered agent (the only kind v1
    /// runs on a schedule).
    #[must_use]
    pub fn is_cron(&self) -> bool {
        self.trigger
            .as_ref()
            .is_some_and(|t| t.trigger_type == "cron")
    }

    /// The parsed schedule interval, if this is a cron agent with a schedule
    /// v1 understands. `None` ⇒ disabled (unknown schedule, or not cron).
    #[must_use]
    pub fn interval_ms(&self) -> Option<i64> {
        if !self.is_cron() {
            return None;
        }
        self.trigger
            .as_ref()
            .and_then(|t| t.schedule.as_deref())
            .and_then(parse_schedule_ms)
    }
}

// ── frontmatter parsing (a flat-scalar subset + one inline `{…}` map) ─────────

/// Parse one file body as an agent/skill definition, or `None` if it is not one
/// (no leading `---\n…\n---` block, or missing `kind`/`name`). The body after
/// the closing fence becomes `instructions`. Mirrors `parseAgent` in
/// `ui/src/agents.ts` for the fields v1 cares about.
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
    let trigger = fm.get("trigger").and_then(parse_trigger);
    let instructions = lines[close + 1..].join("\n").trim().to_string();

    Some(AgentDef {
        kind,
        name,
        model,
        trigger,
        instructions,
    })
}

/// Parse the frontmatter block (lines between the fences) into a flat
/// key→raw-value map. Only column-0 `key: value` lines are read (indented
/// continuations are skipped), matching the UI parser's intentional
/// minimalism. Values are kept as their raw (trimmed) text — scalars are
/// unquoted lazily by callers, and the one structured value v1 reads
/// (`trigger`) is parsed by [`parse_trigger`].
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

/// Strip one layer of matching quotes from a scalar token.
fn unquote(raw: &str) -> &str {
    let s = raw.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let q = bytes[0];
        if (q == b'"' || q == b'\'') && bytes[bytes.len() - 1] == q {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Parse an inline `{ type: cron, schedule: "@hourly" }` map into a [`Trigger`].
/// Returns `None` if the value is not an inline map. Unknown keys are ignored.
fn parse_trigger(raw: &str) -> Option<Trigger> {
    let s = raw.trim();
    let inner = s.strip_prefix('{').and_then(|s| s.strip_suffix('}'))?;
    let mut trigger_type: Option<String> = None;
    let mut schedule: Option<String> = None;
    for part in split_top_level(inner, ',') {
        let Some(idx) = part.find(':') else { continue };
        let key = unquote(part[..idx].trim()).trim().to_string();
        let val = unquote(part[idx + 1..].trim()).to_string();
        match key.as_str() {
            "type" => trigger_type = Some(val),
            "schedule" => schedule = Some(val),
            _ => {}
        }
    }
    let trigger_type = trigger_type?;
    Some(Trigger {
        trigger_type,
        schedule,
    })
}

/// Split on `sep` at the top level, ignoring separators inside quotes (mirrors
/// the UI's `splitTopLevel` — enough for a one-level inline map).
fn split_top_level(input: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut quote: Option<char> = None;
    for ch in input.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                }
                buf.push(ch);
            }
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                buf.push(ch);
            }
            None if ch == sep => {
                out.push(buf.clone());
                buf.clear();
            }
            None => buf.push(ch),
        }
    }
    out.push(buf);
    out
}

// ── the v1 schedule grammar ───────────────────────────────────────────────────

const MINUTE_MS: i64 = 60 * 1000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Parse a v1 schedule string into an interval in milliseconds, or `None` for
/// an unknown/disabled schedule (the caller logs/skips). Supported forms:
///
/// - `@hourly` → 1 hour
/// - `@daily`  → 24 hours
/// - `every <N>m` → N minutes
/// - `every <N>h` → N hours
///
/// A full 5-field cron is intentionally NOT supported in v1.
#[must_use]
pub fn parse_schedule_ms(schedule: &str) -> Option<i64> {
    let s = schedule.trim();
    match s {
        "@hourly" => return Some(HOUR_MS),
        "@daily" => return Some(DAY_MS),
        _ => {}
    }
    // `every <N>m` / `every <N>h`
    if let Some(rest) = s.strip_prefix("every ") {
        let rest = rest.trim();
        let (num, unit) = rest.split_at(rest.len().checked_sub(1)?);
        let n: i64 = num.trim().parse().ok()?;
        if n <= 0 {
            return None;
        }
        return match unit {
            "m" => n.checked_mul(MINUTE_MS),
            "h" => n.checked_mul(HOUR_MS),
            _ => None,
        };
    }
    None
}

/// Whether a cron agent is DUE: it has never run (`last_run_ms` is `None`), or
/// at least its interval has elapsed since the last run. A schedule v1 does not
/// understand returns `false` (disabled).
#[must_use]
pub fn is_due(def: &AgentDef, last_run_ms: Option<i64>, now_ms: i64) -> bool {
    let Some(interval) = def.interval_ms() else {
        return false;
    };
    match last_run_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= interval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_schedule_forms() {
        assert_eq!(parse_schedule_ms("@hourly"), Some(HOUR_MS));
        assert_eq!(parse_schedule_ms("@daily"), Some(DAY_MS));
        assert_eq!(parse_schedule_ms("every 1m"), Some(MINUTE_MS));
        assert_eq!(parse_schedule_ms("every 15m"), Some(15 * MINUTE_MS));
        assert_eq!(parse_schedule_ms("every 2h"), Some(2 * HOUR_MS));
        assert_eq!(parse_schedule_ms("  @hourly  "), Some(HOUR_MS));
        // Unknown / unsupported → disabled.
        assert_eq!(parse_schedule_ms("* * * * *"), None);
        assert_eq!(parse_schedule_ms("@weekly"), None);
        assert_eq!(parse_schedule_ms("every 0m"), None);
        assert_eq!(parse_schedule_ms("every -3m"), None);
        assert_eq!(parse_schedule_ms("every 5s"), None);
        assert_eq!(parse_schedule_ms("everything"), None);
    }

    #[test]
    fn parse_cron_agent_frontmatter() {
        let body = "---\nkind: skill\nname: standup\nmodel: deepseek-chat\n\
                    trigger: { type: cron, schedule: \"@hourly\" }\n---\n\
                    Write a one-line status note for the team.";
        let def = parse_agent(body).expect("parses");
        assert_eq!(def.kind, "skill");
        assert_eq!(def.name, "standup");
        assert_eq!(def.model, "deepseek-chat");
        assert!(def.is_cron());
        assert_eq!(def.interval_ms(), Some(HOUR_MS));
        assert_eq!(
            def.instructions,
            "Write a one-line status note for the team."
        );
    }

    #[test]
    fn model_defaults_when_absent() {
        let body = "---\nkind: agent\nname: foo\ntrigger: { type: cron, schedule: \"every 1m\" }\n---\nDo it.";
        let def = parse_agent(body).unwrap();
        assert_eq!(def.model, DEFAULT_MODEL);
        assert_eq!(def.interval_ms(), Some(MINUTE_MS));
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
    fn non_cron_trigger_is_not_scheduled() {
        let body =
            "---\nkind: skill\nname: x\ntrigger: { type: event, schedule: \"@hourly\" }\n---\nbody";
        let def = parse_agent(body).unwrap();
        assert!(!def.is_cron());
        assert_eq!(def.interval_ms(), None);
    }

    #[test]
    fn unknown_schedule_is_disabled() {
        let body =
            "---\nkind: skill\nname: x\ntrigger: { type: cron, schedule: \"@weekly\" }\n---\nbody";
        let def = parse_agent(body).unwrap();
        assert!(def.is_cron());
        assert_eq!(def.interval_ms(), None, "unknown schedule ⇒ disabled");
        assert!(!is_due(&def, None, 1_000_000));
    }

    #[test]
    fn due_logic() {
        let body =
            "---\nkind: skill\nname: x\ntrigger: { type: cron, schedule: \"every 1m\" }\n---\nbody";
        let def = parse_agent(body).unwrap();
        // Never run → due.
        assert!(is_due(&def, None, 0));
        // Run just now → not due.
        assert!(!is_due(&def, Some(1_000_000), 1_000_000));
        // Run a minute ago → due.
        assert!(is_due(&def, Some(1_000_000), 1_000_000 + MINUTE_MS));
        // Run 59s ago → not yet.
        assert!(!is_due(&def, Some(1_000_000), 1_000_000 + MINUTE_MS - 1));
    }
}
