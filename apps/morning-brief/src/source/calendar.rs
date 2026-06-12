//! Calendar source (Route A: a Google Calendar MCP server).
//!
//! The offline core provides the fixture inputs; MB2 adds the bare JSON-RPC
//! request *builder* and the live tier (a later PR) wires the actual send
//! through the host's credential-injecting `http-fetch`.

use serde::Deserialize;

use super::{BriefInput, Source};
use crate::SourceConfig;

/// The bundled fixture calendar (checked-in `fixtures/calendar.json`).
const FIXTURE: &str = include_str!("../../fixtures/calendar.json");

#[derive(Deserialize)]
struct FixtureFile {
    events: Vec<FixtureEvent>,
}

#[derive(Deserialize)]
struct FixtureEvent {
    summary: String,
    start_ms: i64,
    #[serde(default)]
    location: String,
    #[serde(default)]
    attendees: Vec<String>,
}

pub struct Calendar;

impl Source for Calendar {
    fn fixtures(&self, cfg: &SourceConfig) -> Vec<BriefInput> {
        let file: FixtureFile = serde_json::from_str(FIXTURE).expect("calendar fixture is valid");
        file.events
            .into_iter()
            .take(cfg.max_items.max(0) as usize)
            .map(|e| {
                let detail = match (e.location.is_empty(), e.attendees.is_empty()) {
                    (true, true) => String::new(),
                    (false, true) => format!("at {}", e.location),
                    (true, false) => format!("with {}", e.attendees.join(", ")),
                    (false, false) => {
                        format!("at {} with {}", e.location, e.attendees.join(", "))
                    }
                };
                BriefInput {
                    kind: "calendar".into(),
                    when_ms: e.start_ms,
                    title: e.summary,
                    detail,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SourceConfig {
        SourceConfig {
            kind: "calendar".into(),
            enabled: true,
            window_hours_back: 0,
            window_hours_fwd: 24,
            max_items: 25,
            selector: "primary".into(),
        }
    }

    #[test]
    fn fixtures_normalize_events() {
        let inputs = Calendar.fixtures(&cfg());
        assert!(!inputs.is_empty());
        assert!(inputs.iter().all(|i| i.kind == "calendar"));
        let standup = inputs.iter().find(|i| i.title == "Team standup").unwrap();
        assert!(standup.detail.contains("Zoom"));
        assert!(standup.detail.contains("Priya"));
    }
}
