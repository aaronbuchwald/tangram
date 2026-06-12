//! Calendar source (Route A: a Google Calendar MCP server).
//!
//! Provides the fixture inputs (offline) and the bare JSON-RPC `tools/call`
//! request builder for the read-only `list_events` tool. The live tier (a
//! later PR) wires the actual send through the host's credential-injecting,
//! method-gated `http-fetch`.

use serde::Deserialize;
use serde_json::json;

use super::{BriefInput, Source};
use crate::SourceConfig;

/// The bundled fixture calendar (checked-in `fixtures/calendar.json`).
const FIXTURE: &str = include_str!("../../fixtures/calendar.json");

/// The read-only MCP tool this source calls live.
pub const TOOL: &str = "list_events";

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

    fn live_request(&self, cfg: &SourceConfig, mcp_url: &str) -> tangram::http::Request {
        super::jsonrpc_tools_call(
            mcp_url,
            TOOL,
            json!({
                "window_hours_back": cfg.window_hours_back,
                "window_hours_fwd": cfg.window_hours_fwd,
                "max_items": cfg.max_items,
                "calendars": cfg.selector,
            }),
        )
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

    #[test]
    fn live_request_is_a_bare_readonly_jsonrpc_tools_call() {
        let req = Calendar.live_request(&cfg(), "https://calendar-mcp.internal/mcp");
        // method + path the grant pins.
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://calendar-mcp.internal/mcp");
        // BARE: no credential rides in the request (the host injects it).
        assert!(
            !req.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization")
                    || k.eq_ignore_ascii_case("x-api-key"))
        );
        // The exact JSON-RPC body the EC4 method rung admits.
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], TOOL);
        assert_eq!(body["params"]["arguments"]["calendars"], "primary");
    }
}
