//! The pluggable source seam, mirroring `apps/nutrition/src/strategy.rs`.
//!
//! A [`Source`] is *how a kind of input is fetched and normalized* into a
//! common [`BriefInput`] shape. Selection is **data-driven** from the model's
//! `SourceConfig.kind` (not env, since which sources to pull is a user choice,
//! not a deployment secret). Adding a source = add a module + a `kind` string;
//! no model change.
//!
//! ## Live vs fixture
//!
//! Each source has two ways to produce inputs:
//! - **fixture** ([`Sources::fixtures`]) — bundled, checked-in canned inputs
//!   (see [`fixtures`]). Makes NO network call; this is the offline core's
//!   path and CI's flagship (zero egress).
//! - **live** ([`Source::fetch`]) — issues a bare `http-fetch` the host gates
//!   and credentials (Route A: one JSON-RPC `POST` to a Google MCP server).
//!   The request *builders* live here so they are unit-testable without
//!   sending; the live wiring (real egress + grants) is the separate later PR.

use crate::SourceConfig;

pub mod calendar;
pub mod gmail;

/// One normalized input item the prompt is built from (a calendar event, an
/// email summary, …) — source-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefInput {
    /// `"calendar"` | `"gmail"` — which source produced it.
    pub kind: String,
    /// Event start / message time, ms since epoch (0 if undated).
    pub when_ms: i64,
    /// One-line title (event title, email subject).
    pub title: String,
    /// A short supporting detail (location/attendees, sender/snippet).
    pub detail: String,
}

/// A pluggable way to fetch and normalize a kind of input.
///
/// The offline core pins the **fixture** path here; MB2 adds the live request
/// *builder* (`live_request`) so the exact JSON-RPC body the grants pin is
/// unit-testable, and the live tier (a later PR) wires the actual send through
/// the host's credential-injecting `http-fetch`.
pub trait Source {
    /// Bundled fixture inputs for this source, scoped by `cfg` (offline; no
    /// network).
    fn fixtures(&self, cfg: &SourceConfig) -> Vec<BriefInput>;
}

/// Resolve a `SourceConfig.kind` to its [`Source`] implementation.
fn source_for(kind: &str) -> Option<Box<dyn Source>> {
    match kind {
        "calendar" => Some(Box::new(calendar::Calendar)),
        "gmail" => Some(Box::new(gmail::Gmail)),
        _ => None,
    }
}

/// The known source kinds (also what the capabilities probe reports).
pub fn known_kinds() -> Vec<String> {
    vec!["calendar".into(), "gmail".into()]
}

/// Whether `kind` names a known source.
pub fn is_known_kind(kind: &str) -> bool {
    source_for(kind).is_some()
}

/// Source resolution over a set of configured sources.
pub struct Sources;

impl Sources {
    /// Resolve all ENABLED sources against their **fixtures** (offline; no
    /// network). Inputs are returned sorted by time, then kind, then title so
    /// a fixture run is deterministic. This is the offline core's source path.
    pub fn fixtures(configs: &[SourceConfig]) -> Vec<BriefInput> {
        let mut inputs: Vec<BriefInput> = configs
            .iter()
            .filter(|c| c.enabled)
            .filter_map(|c| source_for(&c.kind).map(|s| s.fixtures(c)))
            .flatten()
            .collect();
        inputs.sort_by(|a, b| {
            a.when_ms
                .cmp(&b.when_ms)
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.title.cmp(&b.title))
        });
        inputs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: &str, enabled: bool) -> SourceConfig {
        SourceConfig {
            kind: kind.into(),
            enabled,
            window_hours_back: 24,
            window_hours_fwd: 24,
            max_items: 25,
            selector: String::new(),
        }
    }

    #[test]
    fn known_kinds_round_trip() {
        assert!(is_known_kind("calendar"));
        assert!(is_known_kind("gmail"));
        assert!(!is_known_kind("slack"));
        assert_eq!(known_kinds(), vec!["calendar", "gmail"]);
    }

    #[test]
    fn fixtures_only_include_enabled_sources_sorted() {
        let only_calendar = Sources::fixtures(&[cfg("calendar", true), cfg("gmail", false)]);
        assert!(!only_calendar.is_empty());
        assert!(only_calendar.iter().all(|i| i.kind == "calendar"));

        let both = Sources::fixtures(&[cfg("calendar", true), cfg("gmail", true)]);
        assert!(both.iter().any(|i| i.kind == "calendar"));
        assert!(both.iter().any(|i| i.kind == "gmail"));
        // sorted by time ascending
        assert!(both.windows(2).all(|w| w[0].when_ms <= w[1].when_ms));

        let none = Sources::fixtures(&[cfg("calendar", false), cfg("gmail", false)]);
        assert!(none.is_empty());
    }

    #[test]
    fn max_items_is_respected_by_fixtures() {
        let mut c = cfg("calendar", true);
        c.max_items = 1;
        assert_eq!(Sources::fixtures(&[c]).len(), 1);
    }

    #[test]
    fn fixtures_are_deterministic() {
        let configs = [cfg("calendar", true), cfg("gmail", true)];
        assert_eq!(Sources::fixtures(&configs), Sources::fixtures(&configs));
    }
}
