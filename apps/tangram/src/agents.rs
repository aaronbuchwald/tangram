//! Agent/skill execution support for the vault: the definition frontmatter
//! parser, the ```agent``` invocation-block parser, and the v1 cron/interval
//! schedule grammar.
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
}

/// A parsed ```` ```agent ```` invocation block: a durable instance inside a
/// note that links to a definition (`use:`) and owns the `trigger` + `prompt`.
/// The source of truth for whether/how an agent runs (R1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The definition this invocation runs (the `use:` field — a definition
    /// `name`).
    pub use_name: String,
    /// The raw `trigger:` text, e.g. `cron every 1h`, `cron @daily`, or
    /// `one-time`.
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
    /// The cron schedule this invocation declares, if its trigger is
    /// `cron <schedule>`. `None` for `one-time` (or any non-cron trigger).
    #[must_use]
    pub fn cron_schedule(&self) -> Option<&str> {
        let t = self.trigger.trim();
        let rest = t.strip_prefix("cron")?;
        // Require a separator after `cron` (so `cronish` is not matched).
        if rest.is_empty() || !rest.starts_with(char::is_whitespace) {
            return None;
        }
        Some(rest.trim())
    }

    /// Whether this invocation is cron-triggered (the only kind v1 schedules).
    #[must_use]
    pub fn is_cron(&self) -> bool {
        self.cron_schedule().is_some()
    }

    /// The parsed schedule interval, if this is a cron invocation with a
    /// schedule v1 understands. `None` ⇒ disabled (unknown schedule, or not
    /// cron).
    #[must_use]
    pub fn interval_ms(&self) -> Option<i64> {
        self.cron_schedule().and_then(parse_schedule_ms)
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

    Some(AgentDef {
        kind,
        name,
        model,
        instructions,
    })
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
/// trigger: cron every 1h          # or "one-time"
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

/// Whether a cron invocation is DUE: it has never run (`last_run_ms` is `None`),
/// or at least its interval has elapsed since the last run. A schedule v1 does
/// not understand returns `false` (disabled).
#[must_use]
pub fn is_due(inv: &Invocation, last_run_ms: Option<i64>, now_ms: i64) -> bool {
    let Some(interval) = inv.interval_ms() else {
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
        assert!(inv.is_cron());
        assert_eq!(inv.cron_schedule(), Some("@hourly"));
        assert_eq!(inv.interval_ms(), Some(HOUR_MS));
        // block_end points right after the closing fence line.
        assert_eq!(&body[inv.block_end..], "\nMore text.");
    }

    #[test]
    fn one_time_invocation_is_not_cron() {
        let body = "```agent\nuse: foo\ntrigger: one-time\nprompt: Hi.\n```";
        let invs = parse_invocations("f", body);
        assert_eq!(invs.len(), 1);
        assert!(!invs[0].is_cron());
        assert_eq!(invs[0].cron_schedule(), None);
        assert_eq!(invs[0].interval_ms(), None);
    }

    #[test]
    fn trigger_defaults_to_one_time() {
        let body = "```agent\nuse: foo\nprompt: Hi.\n```";
        let invs = parse_invocations("f", body);
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].trigger, "one-time");
        assert!(!invs[0].is_cron());
    }

    #[test]
    fn multi_line_prompt_runs_to_fence() {
        let body = "```agent\nuse: foo\ntrigger: cron every 1m\nprompt: line one\nline two\nline three\n```";
        let invs = parse_invocations("f", body);
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].prompt, "line one\nline two\nline three");
        assert_eq!(invs[0].interval_ms(), Some(MINUTE_MS));
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
        assert_eq!(invs[1].interval_ms(), Some(DAY_MS));
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
    fn cron_schedule_requires_separator() {
        // `cronish` must NOT be read as a cron trigger.
        let inv = Invocation {
            use_name: "x".into(),
            trigger: "cronish whatever".into(),
            prompt: String::new(),
            invocation_id: "id".into(),
            block_end: 0,
        };
        assert!(!inv.is_cron());
    }

    #[test]
    fn unknown_schedule_is_disabled() {
        let body = "```agent\nuse: x\ntrigger: cron @weekly\nprompt: p\n```";
        let inv = &parse_invocations("f", body)[0];
        assert!(inv.is_cron(), "trigger is cron…");
        assert_eq!(
            inv.interval_ms(),
            None,
            "…but @weekly is an unknown schedule"
        );
        assert!(!is_due(inv, None, 1_000_000));
    }

    #[test]
    fn due_logic() {
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
}
