//! The LLM strategy seam (step 3 of the AI-enabled-component pattern).
//!
//! [`Llm::Fixture`] returns a **bundled canned response** with ZERO network —
//! the offline-core / CI path (design §8.2 "fixture-offline LLM"), so the
//! whole pipeline runs deterministically without a key. [`Llm::Live`] issues
//! the real Anthropic Messages call, reusing `apps/nutrition/src/strategy/
//! llm.rs` verbatim in shape (the `/v1/messages` POST, `anthropic-version`,
//! structured `json_schema` output, the OAuth-vs-api-key header split). Under
//! `tangram-host` the component issues the BARE request and the host injects
//! the key at the egress boundary (ADR-0005); the credential header logic here
//! is the native-binary fallback. Wiring the live grant + enabling this path
//! end to end is the separate later PR — the offline core only exercises the
//! fixture variant.

use serde::Deserialize;

use crate::prompt::Prompt;
use crate::source::fixtures;
use crate::{BriefConfig, OutputSection, SectionOutput};

/// Default-tier model: a cheaper class for the routine daily brief.
const MODEL_DEFAULT: &str = "claude-sonnet-4-6";
/// Deep-tier model: highest quality for an occasional "deep" brief.
const MODEL_DEEP: &str = "claude-opus-4-8";

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";

/// Which LLM backing a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Llm {
    /// Bundled canned response — no network (offline core / CI).
    Fixture,
    /// Real Anthropic Messages call. The offline core never constructs this —
    /// `run_brief`'s `"live"` branch is gated off until the egress grants land
    /// (a later PR), which is what will select this variant. The machinery
    /// (request shape, schema, credential split) is kept here so that PR is a
    /// thin enablement rather than a rewrite.
    #[allow(dead_code)]
    Live,
}

impl Llm {
    /// Generate one [`SectionOutput`] per enabled section. The fixture variant
    /// is fully offline; the live variant calls the model. Either way the
    /// result is keyed back onto the requested section ids — every requested
    /// section gets an output (missing ones are filled with a clear
    /// placeholder rather than dropped, so the run shape is stable).
    pub async fn generate(
        self,
        config: &BriefConfig,
        prompt: &Prompt,
        sections: &[OutputSection],
    ) -> Result<Vec<SectionOutput>, String> {
        let raw = match self {
            Llm::Fixture => parse_sections(fixtures::LLM_RESPONSE)
                .map_err(|e| format!("bundled LLM fixture is invalid: {e}"))?,
            Llm::Live => generate_live(config, prompt).await?,
        };
        Ok(map_to_sections(sections, &raw))
    }
}

/// `{ "section_id" -> content }` from a structured model response body.
fn parse_sections(json: &str) -> Result<Vec<(String, String)>, String> {
    #[derive(Deserialize)]
    struct Resp {
        sections: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        section_id: String,
        content: String,
    }
    let resp: Resp = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(resp
        .sections
        .into_iter()
        .map(|i| (i.section_id, i.content))
        .collect())
}

/// Key the model's `(section_id, content)` pairs back onto the requested
/// sections, preserving render order and titles. A requested section the model
/// omitted gets an explicit placeholder so the run always has one output per
/// enabled section (the MB3 invariant).
fn map_to_sections(sections: &[OutputSection], raw: &[(String, String)]) -> Vec<SectionOutput> {
    sections
        .iter()
        .map(|s| {
            let content = raw
                .iter()
                .find(|(id, _)| *id == s.id)
                .map(|(_, c)| c.trim().to_string())
                .unwrap_or_else(|| "(no content produced for this section)".to_string());
            SectionOutput {
                section_id: s.id.clone(),
                title: s.title.clone(),
                content,
            }
        })
        .collect()
}

/// The structured-output schema requested of the live model: a list of
/// `{section_id, content}` matching [`map_to_sections`] and the bundled
/// fixture, so the fixture and live paths return the identical shape.
fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "sections": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "section_id": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["section_id", "content"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["sections"],
        "additionalProperties": false
    })
}

/// Map the configured tier to a model id.
fn model_for(tier: &str) -> &'static str {
    match tier {
        "deep" => MODEL_DEEP,
        _ => MODEL_DEFAULT,
    }
}

/// The live Anthropic Messages call. Reuses the nutrition LLM strategy's
/// request shape; under `tangram-host` the request is BARE and the host
/// injects the credential (ADR-0005). Enabling this end to end (the egress
/// grant + key) is the separate later PR — the offline core never reaches it.
async fn generate_live(
    config: &BriefConfig,
    prompt: &Prompt,
) -> Result<Vec<(String, String)>, String> {
    use serde_json::json;
    use tangram::http;

    let body = json!({
        "model": model_for(&config.model_tier),
        "max_tokens": 2048,
        "output_config": {
            "format": { "type": "json_schema", "schema": output_schema() },
        },
        "system": prompt.system,
        "messages": [{ "role": "user", "content": prompt.user }],
    });

    // Bare request: under the host the credential is injected at the egress
    // boundary. The native fallback reads a key from the environment and sets
    // the header itself (OAuth token vs api key — the nutrition split).
    let mut req = http::Request::post(ANTHROPIC_URL)
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    if let Ok(key) =
        std::env::var("ANTHROPIC_API_KEY").or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"))
    {
        if key.starts_with("sk-ant-oat") {
            req = req
                .header("authorization", format!("Bearer {key}"))
                .header("anthropic-beta", "oauth-2025-04-20");
        } else {
            req = req.header("x-api-key", &key);
        }
    }

    let resp = http::fetch(req)
        .await
        .map_err(|e| format!("Anthropic request failed: {e:#}"))?;
    let payload: serde_json::Value = resp
        .json()
        .map_err(|e| format!("Anthropic response body: {e}"))?;
    if !resp.is_success() {
        return Err(format!(
            "Anthropic call failed ({}): {payload}",
            resp.status
        ));
    }
    let text = payload
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        })
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| "Anthropic response had no text block".to_string())?;
    parse_sections(text).map_err(|e| format!("model output did not match schema: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(id: &str, title: &str) -> OutputSection {
        OutputSection {
            id: id.into(),
            title: title.into(),
            prompt: "p".into(),
            format: "prose".into(),
            position: 0,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn fixture_maps_canned_response_onto_sections() {
        let cfg = BriefConfig {
            system_prompt: "s".into(),
            model_tier: "default".into(),
            max_runs: 30,
        };
        let sections = [
            section("sec_summary", "Summary"),
            section("sec_actions", "Action items"),
        ];
        let prompt = Prompt {
            system: "s".into(),
            user: "u".into(),
        };
        let out = Llm::Fixture
            .generate(&cfg, &prompt, &sections)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].section_id, "sec_summary");
        assert!(!out[0].content.is_empty());
        assert!(out[0].content != "(no content produced for this section)");
    }

    #[tokio::test]
    async fn missing_section_gets_placeholder_not_dropped() {
        let cfg = BriefConfig {
            system_prompt: "s".into(),
            model_tier: "default".into(),
            max_runs: 30,
        };
        // A section id the canned fixture does not cover.
        let sections = [section("sec_novel", "Novel")];
        let prompt = Prompt {
            system: "s".into(),
            user: "u".into(),
        };
        let out = Llm::Fixture
            .generate(&cfg, &prompt, &sections)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "(no content produced for this section)");
    }

    #[test]
    fn tier_maps_to_model() {
        assert_eq!(model_for("deep"), MODEL_DEEP);
        assert_eq!(model_for("default"), MODEL_DEFAULT);
        assert_eq!(model_for("unknown"), MODEL_DEFAULT);
    }
}
