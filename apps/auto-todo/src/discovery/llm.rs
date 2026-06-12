//! Optional LLM assist for discovery (AC2), modeled on nutrition's
//! `strategy/llm.rs`. It asks Anthropic to PROPOSE the permissions /
//! connections / human-assistance a free-text item implies — over-disclosing
//! by design (design §5.1). The proposal is advisory DATA: the caller merges
//! it into the deterministic rule output and re-classifies deterministically,
//! so the gates never depend on the model.
//!
//! Fixture-offline for CI (the project pattern): the call only runs in
//! `AUTO_TODO_DISCOVERY=llm` mode with a key present; tests run offline and
//! never reach the network. The request goes through the `tangram::http`
//! facade — reqwest natively, the host's allowlist-enforced `http-fetch`
//! import inside the WASM component — and the host injects the credential at
//! the egress boundary (ADR-0005), so the plaintext key never enters the
//! component.

use anyhow::Context;
use serde::Deserialize;
use serde_json::json;
use tangram::http;

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const MODEL: &str = "claude-opus-4-8";

const SYSTEM_PROMPT: &str = "You analyze a free-text TODO item and infer, as REVIEWABLE DATA, what an \
     agent would need to complete it. OVER-DISCLOSE: list every capability, \
     named connection/service, credential kind, and human-assistance point \
     (2FA, CAPTCHA, payment confirmation, a judgement call) it might require — \
     false positives are cheap because a human reviews this. Flag \
     irreversibility explicitly: \"none\" for read-only, \"reversible\" for an \
     undoable write, \"irreversible\" if it spends money, sends a message, or \
     deletes. You are NOT taking any action; you are only describing needs. \
     Use lowercase dotted capability names where natural (e.g. \
     \"calendar.read\", \"email.send\", \"web.purchase\").";

/// The additive proposal the model returns — merged into (never replacing)
/// the deterministic rule output.
pub struct Proposal {
    pub capabilities: Vec<String>,
    pub connections: Vec<String>,
    pub credentials: Vec<String>,
    pub human_assistance: Vec<String>,
    /// "none" | "reversible" | "irreversible" — only ever RAISES the base.
    pub irreversibility: String,
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "capabilities": { "type": "array", "items": { "type": "string" } },
            "connections": { "type": "array", "items": { "type": "string" } },
            "credentials": { "type": "array", "items": { "type": "string" } },
            "human_assistance": { "type": "array", "items": { "type": "string" } },
            "irreversibility": { "type": "string", "enum": ["none", "reversible", "irreversible"] }
        },
        "required": ["capabilities", "connections", "credentials", "human_assistance", "irreversibility"],
        "additionalProperties": false
    })
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    connections: Vec<String>,
    #[serde(default)]
    credentials: Vec<String>,
    #[serde(default)]
    human_assistance: Vec<String>,
    #[serde(default)]
    irreversibility: String,
}

/// Ask the model to propose requirements for a free-text item. Returns
/// `Ok(None)` when the model declines/returns nothing; `Err` on a transport
/// or auth failure (the caller falls back to the deterministic rules).
pub async fn propose(text: &str) -> anyhow::Result<Option<Proposal>> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"))
        .map_err(|_| anyhow::anyhow!("AUTO_TODO_DISCOVERY=llm requires ANTHROPIC_API_KEY"))?;

    let body = json!({
        "model": MODEL,
        "max_tokens": 1024,
        "thinking": { "type": "disabled" },
        "output_config": {
            "effort": "low",
            "format": { "type": "json_schema", "schema": schema() },
        },
        "system": SYSTEM_PROMPT,
        "messages": [{ "role": "user", "content": format!("TODO item: {text:?}") }],
    });

    let mut req = http::Request::post(ANTHROPIC_URL)
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    // OAuth token (sk-ant-oat…) vs API key (sk-ant-api…), same dual path as
    // nutrition's strategy.
    if key.starts_with("sk-ant-oat") {
        req = req
            .header("authorization", format!("Bearer {key}"))
            .header("anthropic-beta", "oauth-2025-04-20");
    } else {
        req = req.header("x-api-key", &key);
    }

    let resp = http::fetch(req)
        .await
        .with_context(|| format!("Anthropic discovery request failed for {text:?}"))?;
    let payload: serde_json::Value = resp.json().context("Anthropic response body")?;
    if !resp.is_success() {
        anyhow::bail!("Anthropic discovery failed ({}): {payload}", resp.status);
    }

    let Some(out) = payload
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        })
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
    else {
        return Ok(None);
    };

    let raw: Raw =
        serde_json::from_str(out).context("Anthropic structured output did not match schema")?;
    let clean = |v: Vec<String>| -> Vec<String> {
        v.into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let irreversibility = match raw.irreversibility.as_str() {
        "irreversible" | "reversible" => raw.irreversibility,
        _ => "none".to_string(),
    };
    Ok(Some(Proposal {
        capabilities: clean(raw.capabilities),
        connections: clean(raw.connections),
        credentials: clean(raw.credentials),
        human_assistance: clean(raw.human_assistance),
        irreversibility,
    }))
}
