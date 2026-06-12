//! Gmail source (Route A: a Gmail MCP server).
//!
//! The offline core provides the fixture inputs; MB2 adds the bare JSON-RPC
//! request *builder* and the live tier (a later PR) wires the actual send
//! through the host's credential-injecting `http-fetch`.

use serde::Deserialize;

use super::{BriefInput, Source};
use crate::SourceConfig;

/// The bundled fixture mailbox (checked-in `fixtures/gmail.json`).
const FIXTURE: &str = include_str!("../../fixtures/gmail.json");

#[derive(Deserialize)]
struct FixtureFile {
    messages: Vec<FixtureMessage>,
}

#[derive(Deserialize)]
struct FixtureMessage {
    from: String,
    subject: String,
    received_ms: i64,
    #[serde(default)]
    snippet: String,
}

pub struct Gmail;

impl Source for Gmail {
    fn fixtures(&self, cfg: &SourceConfig) -> Vec<BriefInput> {
        let file: FixtureFile = serde_json::from_str(FIXTURE).expect("gmail fixture is valid");
        file.messages
            .into_iter()
            .take(cfg.max_items.max(0) as usize)
            .map(|m| {
                let detail = if m.snippet.is_empty() {
                    format!("from {}", m.from)
                } else {
                    format!("from {} — {}", m.from, m.snippet)
                };
                BriefInput {
                    kind: "gmail".into(),
                    when_ms: m.received_ms,
                    title: m.subject,
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
            kind: "gmail".into(),
            enabled: true,
            window_hours_back: 24,
            window_hours_fwd: 0,
            max_items: 25,
            selector: "in:inbox newer_than:1d".into(),
        }
    }

    #[test]
    fn fixtures_normalize_messages() {
        let inputs = Gmail.fixtures(&cfg());
        assert!(!inputs.is_empty());
        assert!(inputs.iter().all(|i| i.kind == "gmail"));
        let invoice = inputs.iter().find(|i| i.title.contains("Invoice")).unwrap();
        assert!(invoice.detail.contains("billing@acme-cloud.example"));
    }
}
