//! Morning Brief — Tangram's first AI-enabled component.
//!
//! A once-a-day (or on-demand) AI-generated digest of the user's day,
//! assembled from pluggable read-only **sources** (calendar + Gmail), shaped
//! by a user-configurable **prompt** into a user-configurable set of
//! **output sections**, with an in-tangram **feedback/"dreaming" loop**: run,
//! inspect, rate/correct, and fold the correction back into the prompt.
//!
//! This crate is the **offline core** (design checkpoints MB1–MB5): the
//! model, the config/feedback actions, the pluggable source seam with
//! checked-in **fixtures**, the prompt builder, and a fully offline
//! `run_brief` that runs against fixtures with a recorded (no-network) LLM
//! response. The egress + live tier (real Google/Anthropic calls, the
//! `[[calls]]` grants, manifest verification — checkpoints MB6–MB8) is a
//! separate later PR; the source/LLM strategy seam is the slot it drops into.
//!
//! ## The AI-enabled-component pattern (the design's center of gravity)
//!
//! `run_brief` does exactly four things: (1) fetch sources, (2) build the
//! prompt in memory, (3) call the LLM, (4) write the resulting brief to the
//! component's OWN replicated document via `ctx.mutate`. Writing local state
//! is NOT an egress — there is no allowlisted host to send a brief to — so the
//! brief is *contained to the tangram* by construction. In this offline core,
//! steps 1 and 3 are served by the **fixture** strategy (zero network); the
//! live tier swaps the fixture source/LLM for the host's allowlist-gated,
//! credential-injecting `http-fetch` without touching the model or actions.

use tangram::prelude::*;

#[cfg(not(target_family = "wasm"))]
mod api;
mod source;

pub use source::{BriefInput, Sources};

#[model]
pub struct MorningBrief {
    /// The editable global config — the principal axis the user tunes
    /// ("surface what to be aware of" vs "flag what needs action today").
    /// First-class, not hard-coded.
    config: BriefConfig,
    /// Data-driven output sections; `position` is render/run order. NOT
    /// hard-coded — adding a section is an action, not a code change.
    sections: Vec<OutputSection>,
    /// Which sources to pull and how (pluggable — see [`source`]). Each entry
    /// is a source *selection* plus its scope knobs — never a secret.
    sources: Vec<SourceConfig>,
    /// Run history — the spine of the dreaming/feedback loop. Newest last;
    /// capped by `config.max_runs` at record time (oldest unrated evicted
    /// first, so feedback-bearing runs are preserved).
    runs: Vec<BriefRun>,
    /// Few-shot examples distilled from human feedback, folded into the
    /// prompt preamble on the next run. Data, not code.
    learned: Vec<LearnedExample>,
}

#[model]
pub struct BriefConfig {
    /// The master instruction — the surface-vs-act axis the user edits as
    /// plain text.
    system_prompt: String,
    /// Default model tier for a run: `"default"` | `"deep"` (maps to a model
    /// id host-side / in a const table; the component never embeds a key).
    model_tier: String,
    /// Cap on retained runs (oldest evicted at record time — bounded doc).
    max_runs: i64,
}

#[model]
pub struct OutputSection {
    id: String,
    /// User-named, e.g. "Summary", "Highlights", "Action items".
    title: String,
    /// The sub-prompt for THIS section. Data-driven.
    prompt: String,
    /// Render hint: `"prose"` | `"bullets"` | `"checklist"`.
    format: String,
    /// Render/run order.
    position: i64,
    enabled: bool,
}

#[model]
pub struct SourceConfig {
    /// `"calendar"` | `"gmail"` (matches a [`source::Source`] name).
    kind: String,
    enabled: bool,
    /// Scope knobs that are NOT secrets: how far back/forward, max items.
    /// Credentials are NEVER here — they are injected host-side (ADR-0005).
    window_hours_back: i64,
    window_hours_fwd: i64,
    max_items: i64,
    /// Free-form selector (calendar ids / gmail label query) — opaque to the
    /// model, passed through to the source strategy.
    selector: String,
}

#[model]
pub struct BriefRun {
    id: String,
    created_at_ms: i64,
    /// `"fixture"` | `"live"` — a fixture run needs no Google/LLM egress,
    /// which is what makes CI and "dreaming" cheap and offline.
    input_mode: String,
    /// Human-readable snapshot of the inputs the run actually saw (item count
    /// plus a bounded preview), so a run is reproducible and the human can see
    /// what the model saw. NOT the raw mailbox.
    input_summary: String,
    /// The produced sections, parallel to `sections` by id at run time.
    outputs: Vec<SectionOutput>,
    /// The exact prompt sent (post learned-preamble fold) — auditable.
    effective_prompt: String,
    /// `"ok"` | `"error: ..."` — a failed run is recorded, not dropped.
    status: String,
    /// Human feedback on this run (the dreaming loop). `Option` because a run
    /// exists before feedback does.
    feedback: Option<RunFeedback>,
}

#[model]
pub struct SectionOutput {
    section_id: String,
    title: String,
    /// The model's text for this section.
    content: String,
}

#[model]
pub struct RunFeedback {
    /// 1..=5, or 0 for "unrated".
    rating: i64,
    /// Free-text annotation/correction from the human.
    note: String,
    /// Per-section corrections the human typed; these become candidate
    /// few-shot examples.
    corrections: Vec<SectionCorrection>,
    created_at_ms: i64,
}

#[model]
pub struct SectionCorrection {
    section_id: String,
    corrected: String,
}

#[model]
pub struct LearnedExample {
    /// Distilled from a rated run + its corrections: a compact "prefer Y for
    /// inputs like X" note folded into the preamble. Curated by an action,
    /// capped, and editable.
    id: String,
    note: String,
    weight: i64,
    created_at_ms: i64,
}

#[actions]
impl MorningBrief {
    // ── Configuration (sync, pure mutations) ─────────────────────────────

    /// Edit the master prompt — the surface-vs-act axis.
    pub fn set_system_prompt(&mut self, prompt: String) {
        self.config.system_prompt = prompt;
    }

    /// Set the default model tier for a run (`"default"` | `"deep"`). Unknown
    /// values are rejected so a run never references a tier with no model.
    pub fn set_model_tier(&mut self, tier: String) -> Result<(), String> {
        match tier.as_str() {
            "default" | "deep" => {
                self.config.model_tier = tier;
                Ok(())
            }
            other => Err(format!(
                "unknown model tier {other:?} (use \"default\" or \"deep\")"
            )),
        }
    }

    /// Cap on retained runs (minimum 1). Applied on the next `run_brief`.
    pub fn set_max_runs(&mut self, max_runs: i64) -> Result<(), String> {
        if max_runs < 1 {
            return Err("max_runs must be at least 1".into());
        }
        self.config.max_runs = max_runs;
        Ok(())
    }

    /// Add an output section (appended at the end). `format` is a render hint
    /// (`"prose"` | `"bullets"` | `"checklist"`). Returns the new section id.
    pub fn add_section(&mut self, title: String, prompt: String, format: String) -> String {
        let id = format!("sec_{}", uuid::Uuid::new_v4());
        let position = self.sections.iter().map(|s| s.position).max().unwrap_or(-1) + 1;
        self.sections.push(OutputSection {
            id: id.clone(),
            title,
            prompt,
            format: normalize_format(&format),
            position,
            enabled: true,
        });
        id
    }

    /// Edit an existing section's title/prompt/format in place.
    pub fn update_section(
        &mut self,
        id: String,
        title: String,
        prompt: String,
        format: String,
    ) -> Result<(), String> {
        let section = self
            .sections
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("no section with id {id}"))?;
        section.title = title;
        section.prompt = prompt;
        section.format = normalize_format(&format);
        Ok(())
    }

    /// Remove a section by id.
    pub fn remove_section(&mut self, id: String) -> Result<(), String> {
        let before = self.sections.len();
        self.sections.retain(|s| s.id != id);
        if self.sections.len() == before {
            return Err(format!("no section with id {id}"));
        }
        Ok(())
    }

    /// Move a section to a new render/run position.
    pub fn reorder_section(&mut self, id: String, position: i64) -> Result<(), String> {
        let section = self
            .sections
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("no section with id {id}"))?;
        section.position = position;
        Ok(())
    }

    /// Enable/disable a section without deleting it (a disabled section is
    /// not run and not rendered).
    pub fn set_section_enabled(&mut self, id: String, enabled: bool) -> Result<(), String> {
        let section = self
            .sections
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("no section with id {id}"))?;
        section.enabled = enabled;
        Ok(())
    }

    /// Configure a source (no secrets — only scope knobs). Creates the source
    /// entry if `kind` is not yet configured, else updates it. `kind` must be
    /// a known source ("calendar" | "gmail").
    pub fn set_source(
        &mut self,
        kind: String,
        enabled: bool,
        window_hours_back: i64,
        window_hours_fwd: i64,
        max_items: i64,
        selector: String,
    ) -> Result<(), String> {
        if !source::is_known_kind(&kind) {
            return Err(format!(
                "unknown source kind {kind:?} (known: {})",
                source::known_kinds().join(", ")
            ));
        }
        let cfg = SourceConfig {
            kind: kind.clone(),
            enabled,
            window_hours_back: window_hours_back.max(0),
            window_hours_fwd: window_hours_fwd.max(0),
            max_items: max_items.max(0),
            selector,
        };
        match self.sources.iter_mut().find(|s| s.kind == kind) {
            Some(existing) => *existing = cfg,
            None => self.sources.push(cfg),
        }
        Ok(())
    }

    // ── Read views (the auto-UI and MCP read these) ──────────────────────

    /// The current config.
    pub fn get_config(&self) -> BriefConfig {
        self.config.clone()
    }

    /// All output sections in render order.
    pub fn list_sections(&self) -> Vec<OutputSection> {
        let mut sections = self.sections.clone();
        sections.sort_by_key(|s| s.position);
        sections
    }

    /// All configured sources.
    pub fn list_sources(&self) -> Vec<SourceConfig> {
        self.sources.clone()
    }

    /// Run history, newest first.
    pub fn list_runs(&self) -> Vec<BriefRun> {
        let mut runs = self.runs.clone();
        runs.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        runs
    }

    /// A single run by id.
    pub fn get_run(&self, run_id: String) -> Result<BriefRun, String> {
        self.runs
            .iter()
            .find(|r| r.id == run_id)
            .cloned()
            .ok_or_else(|| format!("no run with id {run_id}"))
    }

    /// The learned few-shot examples (highest weight first).
    pub fn list_learned(&self) -> Vec<LearnedExample> {
        let mut learned = self.learned.clone();
        learned.sort_by_key(|l| std::cmp::Reverse(l.weight));
        learned
    }
}

/// Normalize a free-text render-hint into one of the known formats, defaulting
/// to "prose" for anything unrecognized (formats are data, so an unknown value
/// degrades rather than erroring).
fn normalize_format(format: &str) -> String {
    match format.trim().to_lowercase().as_str() {
        "bullets" => "bullets",
        "checklist" => "checklist",
        _ => "prose",
    }
    .to_string()
}

/// Deterministic genesis (CLAUDE.md): every replica derives a byte-identical
/// genesis change, which is what lets host-managed docs merge. No `now_ms()` /
/// `uuid` here — genesis ids are fixed literals; runtime-created ids use
/// `uuid`/`now_ms()` inside actions.
///
/// Seeds a sensible, immediately-useful config: the surface-vs-act master
/// prompt, three default sections (Summary/Highlights/Action items), and
/// calendar + gmail sources **disabled** by default (a fresh instance does no
/// egress until the user opts in and the operator grants the calls).
impl Default for MorningBrief {
    fn default() -> Self {
        let section =
            |id: &str, title: &str, prompt: &str, format: &str, position: i64| OutputSection {
                id: id.into(),
                title: title.into(),
                prompt: prompt.into(),
                format: format.into(),
                position,
                enabled: true,
            };
        let source = |kind: &str, back: i64, fwd: i64| SourceConfig {
            kind: kind.into(),
            enabled: false,
            window_hours_back: back,
            window_hours_fwd: fwd,
            max_items: 25,
            selector: String::new(),
        };
        Self {
            config: BriefConfig {
                system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
                model_tier: "default".into(),
                max_runs: 30,
            },
            sections: vec![
                section(
                    "sec_summary",
                    "Summary",
                    "Write a tight 2-4 sentence overview of the day ahead: the \
                     shape of the schedule and anything notable to be aware of.",
                    "prose",
                    0,
                ),
                section(
                    "sec_highlights",
                    "Highlights",
                    "List the few things most worth knowing today — important \
                     meetings, people, or threads. One short bullet each.",
                    "bullets",
                    1,
                ),
                section(
                    "sec_actions",
                    "Action items",
                    "List ONLY what needs my action today, each with a one-line \
                     reason why it needs action now. Omit anything merely \
                     informational.",
                    "checklist",
                    2,
                ),
            ],
            sources: vec![source("calendar", 0, 24), source("gmail", 24, 0)],
            runs: Vec::new(),
            learned: Vec::new(),
        }
    }
}

/// The default master prompt: states the surface-vs-act axis the owner named
/// explicitly, so a fresh instance produces a meaningfully-shaped brief.
const DEFAULT_SYSTEM_PROMPT: &str = "You are my morning chief of staff. From my calendar and email, \
     produce a brief for the day ahead. Surface what I should be aware of; \
     separately, flag ONLY what needs my action today and why. Be concise, \
     concrete, and specific — prefer names, times, and the single most \
     important thing over generic advice. Never invent events or messages \
     that are not in the inputs.";

/// MCP instructions, shared between the native app builder and the WASM
/// component's `describe()` export.
const INSTRUCTIONS: &str = "A configurable, replicated morning brief over your calendar and email. \
     Configure the master prompt and the output sections (set_system_prompt, \
     add_section), choose sources (set_source), then generate a digest with \
     run_brief (input_mode \"fixture\" runs offline against bundled fixtures \
     and makes no network calls; \"live\" fetches sources and calls the LLM). \
     Read the result with list_runs/get_run, then rate_run / correct_section \
     and promote_to_learned to fold feedback back into the next run's prompt. \
     The brief is written to this document and never sent anywhere else.";

/// The morning-brief app, fully configured. Serve it with
/// `app().serve_with(with_api)` (standalone) or mount
/// `with_api(...app().build_parts()?)` in a multi-app host — `with_api` adds
/// the `GET /api/capabilities` probe on top of the derived surface.
#[cfg(not(target_family = "wasm"))]
pub fn app() -> App<MorningBrief> {
    App::<MorningBrief>::new("morning-brief")
        .instructions(INSTRUCTIONS)
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
}

/// Merge the capabilities probe into the derived router (the only
/// non-operation custom route — it reports whether the LLM/source are
/// configured so the UI can show a "not configured" hint). Shaped to pass
/// straight to [`App::serve_with`].
#[cfg(not(target_family = "wasm"))]
pub fn with_api(router: axum::Router, _ctx: Ctx<MorningBrief>) -> axum::Router {
    router.merge(api::routes())
}

/// The capabilities object reported by `GET /api/capabilities` and the WASM
/// component's `describe()` — ONE constructor so both surfaces are identical.
/// In the offline core the brief always has a working `"fixture"` path; the
/// `live` flag (whether real egress credentials resolve) is decided host-side
/// in the live tier and ANDed in there.
pub(crate) fn capabilities_json() -> serde_json::Value {
    serde_json::json!({
        "fixture": true,
        "sources": source::known_kinds(),
    })
}

// Compiled for wasm32-wasip2, the same model + actions become a Tangram
// component (`tangram-host` owns the platform around it). In the offline core,
// `run_brief` resolves the fixture source/LLM strategy with no `http-fetch`;
// the live tier (a later PR) routes the source + LLM calls through the host's
// allowlist-enforced, credential-injecting `http-fetch` import.
#[cfg(target_family = "wasm")]
tangram::export_component!(MorningBrief {
    name: "morning-brief",
    instructions: INSTRUCTIONS,
    capabilities: || Some(capabilities_json()),
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_is_deterministic() {
        // Default is the shared genesis commit, so two independently-built
        // instances must reconcile byte-identically (the property that lets
        // host-managed docs merge).
        let a = tangram_core::genesis_bytes::<MorningBrief>().expect("genesis a");
        let b = tangram_core::genesis_bytes::<MorningBrief>().expect("genesis b");
        assert_eq!(a, b, "MorningBrief genesis must be deterministic");
    }

    #[test]
    fn default_seeds_three_sections_and_two_disabled_sources() {
        let m = MorningBrief::default();
        let sections = m.list_sections();
        assert_eq!(sections.len(), 3);
        // Render order follows `position`.
        assert_eq!(
            sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Summary", "Highlights", "Action items"]
        );
        // Fail-safe: a fresh instance does NO egress until the user opts in.
        let sources = m.list_sources();
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().all(|s| !s.enabled));
        assert_eq!(m.get_config().model_tier, "default");
        assert_eq!(m.get_config().max_runs, 30);
        assert!(
            m.get_config()
                .system_prompt
                .to_lowercase()
                .contains("action")
        );
    }

    #[test]
    fn add_update_reorder_remove_section() {
        let mut m = MorningBrief::default();
        let id = m.add_section(
            "Threads".into(),
            "Unanswered threads.".into(),
            "BULLETS".into(),
        );
        // format normalized to a known value.
        let added = m.list_sections().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(added.format, "bullets");
        assert!(added.enabled);
        // Appended after the seeded three (positions 0..=2 → new at 3).
        assert_eq!(added.position, 3);

        m.update_section(id.clone(), "Threads".into(), "p".into(), "weird".into())
            .unwrap();
        let updated = m.list_sections().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(updated.format, "prose"); // unknown format → prose

        m.reorder_section(id.clone(), -1).unwrap();
        assert_eq!(m.list_sections()[0].id, id); // now first

        m.set_section_enabled(id.clone(), false).unwrap();
        assert!(
            !m.list_sections()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap()
                .enabled
        );

        m.remove_section(id.clone()).unwrap();
        assert!(m.list_sections().iter().all(|s| s.id != id));
        assert!(m.remove_section(id).is_err()); // idempotent error
    }

    #[test]
    fn set_source_creates_then_updates_and_rejects_unknown() {
        let mut m = MorningBrief::default();
        // calendar exists from genesis — set_source updates it in place.
        m.set_source("calendar".into(), true, 1, 2, 10, "primary".into())
            .unwrap();
        let cals: Vec<_> = m
            .list_sources()
            .into_iter()
            .filter(|s| s.kind == "calendar")
            .collect();
        assert_eq!(cals.len(), 1, "no duplicate source entry");
        assert!(cals[0].enabled);
        assert_eq!(cals[0].max_items, 10);

        assert!(
            m.set_source("slack".into(), true, 0, 0, 0, String::new())
                .is_err()
        );
    }

    #[test]
    fn model_tier_and_max_runs_validate() {
        let mut m = MorningBrief::default();
        assert!(m.set_model_tier("deep".into()).is_ok());
        assert_eq!(m.get_config().model_tier, "deep");
        assert!(m.set_model_tier("turbo".into()).is_err());
        assert!(m.set_max_runs(0).is_err());
        assert!(m.set_max_runs(5).is_ok());
        assert_eq!(m.get_config().max_runs, 5);
    }
}
