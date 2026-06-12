//! Gmail source (Route A: a Gmail MCP server).
//!
//! Provides the fixture inputs (offline) and the bare JSON-RPC `tools/call`
//! request builder for the read-only `list_messages` tool. The live tier (a
//! later PR) wires the actual send through the host's credential-injecting,
//! method-gated `http-fetch`.

use serde::Deserialize;
use serde_json::json;

use super::{BriefInput, Source};
use crate::SourceConfig;

/// The bundled fixture mailbox (checked-in `fixtures/gmail.json`).
const FIXTURE: &str = include_str!("../../fixtures/gmail.json");

/// The read-only MCP tool this source calls live.
pub const TOOL: &str = "list_messages";

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

    fn live_request(&self, cfg: &SourceConfig, mcp_url: &str) -> tangram::http::Request {
        super::jsonrpc_tools_call(
            mcp_url,
            TOOL,
            json!({
                "window_hours_back": cfg.window_hours_back,
                "window_hours_fwd": cfg.window_hours_fwd,
                "max_items": cfg.max_items,
                "query": cfg.selector,
            }),
        )
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

    #[test]
    fn live_request_is_a_bare_readonly_jsonrpc_tools_call() {
        let req = Gmail.live_request(&cfg(), "https://gmail-mcp.internal/mcp");
        assert_eq!(req.method, "POST");
        assert!(
            !req.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization")
                    || k.eq_ignore_ascii_case("x-api-key"))
        );
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], TOOL);
        assert_eq!(
            body["params"]["arguments"]["query"],
            "in:inbox newer_than:1d"
        );
    }
}
