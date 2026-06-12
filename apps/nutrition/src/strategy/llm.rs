//! LLM-backed strategy (Anthropic claude-opus-4-8).
//!
//! `resolve(component)` asks the model for the comprehensive per-100g
//! nutrient panel of an arbitrary free-text food via a structured-output
//! (json_schema) Messages API call and maps it into [`ResolvedNutrient`]s.
//! The (nondeterministic) model is consulted at most once per novel
//! component; the resolved rows are cached into the replicated reference
//! data ("resolve once, replay forever"). Requires ANTHROPIC_API_KEY (or
//! ANTHROPIC_AUTH_TOKEN); select via NUTRITION_STRATEGY=llm.

use anyhow::Context;
use serde::Deserialize;
use serde_json::json;
use tangram::http;

use super::ResolvedNutrient;

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";

/// Uses the current most-capable Opus model id.
const MODEL: &str = "claude-opus-4-8";

const SYSTEM_PROMPT: &str = "You are a nutrition reference. Given a food or dish name, return the typical \
     comprehensive nutrition panel per 100 grams of the edible portion as cooked/served. \
     Include macros (protein, carbs, fat, fiber, sugars, saturated fat) and micros (vitamins \
     and minerals like iron, calcium, potassium, sodium, magnesium, zinc, vitamin C) wherever \
     you can give a realistic estimate — omit a nutrient only when it's truly negligible or \
     unknown. Use the canonical names \"Protein\", \"Carbs\", \"Fat\" for the core macros, with the \
     correct kind (macro/micro) and unit (g/mg/mcg). Set found=false only if the input is not a food.";

/// Structured-output schema: an OPEN list of nutrients per 100g (not a fixed
/// five), so the model can return the full comprehensive panel, each carrying
/// a display name + kind + unit that map straight onto a ResolvedNutrient.
fn nutrition_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "found": { "type": "boolean", "description": "true if this is a recognizable food" },
            "nutrients": {
                "type": "array",
                "description": "the comprehensive nutrient panel for this food, per 100g of edible portion",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "display name, e.g. \"Protein\", \"Vitamin B2\"" },
                        "kind": { "type": "string", "enum": ["macro", "micro"], "description": "macro or micro" },
                        "unit": { "type": "string", "description": "unit: \"g\", \"mg\", or \"mcg\"" },
                        "amount_per_100g": { "type": "number", "description": "amount per 100g of edible portion" }
                    },
                    "required": ["name", "kind", "unit", "amount_per_100g"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["found", "nutrients"],
        "additionalProperties": false
    })
}

#[derive(Deserialize)]
struct LlmNutrition {
    found: bool,
    #[serde(default)]
    nutrients: Vec<LlmNutrientItem>,
}

#[derive(Deserialize)]
struct LlmNutrientItem {
    name: String,
    kind: String,
    unit: String,
    amount_per_100g: f64,
}

/// Ask the model for a per-100g nutrient panel for a free-text food.
pub async fn resolve(component: &str) -> anyhow::Result<Option<Vec<ResolvedNutrient>>> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"))
        .map_err(|_| {
            anyhow::anyhow!("NUTRITION_STRATEGY=llm requires ANTHROPIC_API_KEY to be set")
        })?;

    let body = json!({
        "model": MODEL,
        "max_tokens": 1024,
        "thinking": { "type": "disabled" }, // fast structured lookup, no reasoning needed
        "output_config": {
            "effort": "low",
            "format": { "type": "json_schema", "schema": nutrition_schema() },
        },
        "system": SYSTEM_PROMPT,
        "messages": [{ "role": "user", "content": format!("Food: {component:?}") }],
    });

    // Through the tangram::http facade: reqwest natively, the host's
    // allowlist-enforced `http-fetch` import inside a WASM component.
    let mut req = http::Request::post(ANTHROPIC_URL)
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    // An OAuth token (sk-ant-oat…) authenticates via `Authorization: Bearer`
    // + the OAuth beta header, NOT via x-api-key. A standard API key
    // (sk-ant-api…) uses the x-api-key path. This lets the strategy work
    // with either credential the environment supplies.
    if key.starts_with("sk-ant-oat") {
        req = req
            .header("authorization", format!("Bearer {key}"))
            .header("anthropic-beta", "oauth-2025-04-20");
    } else {
        req = req.header("x-api-key", &key);
    }

    let resp = http::fetch(req)
        .await
        .with_context(|| format!("Anthropic request failed for {component:?}"))?;
    let payload: serde_json::Value = resp.json().context("Anthropic response body")?;
    if !resp.is_success() {
        anyhow::bail!(
            "Anthropic lookup failed ({}) for {component:?}: {payload}",
            resp.status
        );
    }

    let Some(text) = payload
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
    let data: LlmNutrition =
        serde_json::from_str(text).context("Anthropic structured output did not match schema")?;
    if !data.found {
        return Ok(None);
    }

    let rows: Vec<ResolvedNutrient> = data
        .nutrients
        .into_iter()
        .filter(|n| !n.name.trim().is_empty() && n.amount_per_100g.is_finite())
        .map(|n| ResolvedNutrient {
            name: n.name.trim().to_string(),
            kind: if n.kind == "micro" {
                "micro".to_string()
            } else {
                "macro".to_string()
            },
            unit: if n.unit.is_empty() {
                "g".to_string()
            } else {
                n.unit
            },
            amount_per_100g: n.amount_per_100g,
        })
        .collect();
    Ok(if rows.is_empty() { None } else { Some(rows) })
}
