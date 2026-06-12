//! The tutor: the app's AI-enabled half. Each tutor action issues ONE
//! Anthropic Messages-API call through `tangram::http` (the same call shape
//! nutrition's `strategy/llm.rs` already ships — `claude-opus-4-8`, structured
//! `json_schema` output, the OAuth-vs-api-key credential split). The component
//! issues a BARE request; the host injects the Anthropic credential at the
//! `http-fetch` egress boundary (ADR-0005), so the key never enters the
//! component's address space.
//!
//! The base URL is overridable via `GUIDED_LEARNING_LLM_URL` so CI can point
//! the call at a local recorded-fixture server (no live LLM needed). The live
//! path self-skips when no credential resolves — sync actions still work, the
//! tutor reports unavailable (the nutrition degrade precedent).

use serde::Deserialize;
use serde_json::json;
use tangram::http;

const DEFAULT_URL: &str = "https://api.anthropic.com/v1/messages";
const MODEL: &str = "claude-opus-4-8";

/// The Messages-API endpoint: the live Anthropic URL, or a test/CI override.
fn endpoint() -> String {
    std::env::var("GUIDED_LEARNING_LLM_URL").unwrap_or_else(|_| DEFAULT_URL.to_string())
}

/// Whether an Anthropic credential is resolvable in this environment. Natively
/// (and in tests) this reads the env directly; inside the component the key is
/// host-injected at egress and the host ANDs its inject-resolution into the
/// reported capability, so the component's own view here is best-effort.
// Used natively by the degrade-without-key capabilities probe / test; the
// component path reports `description_input` intrinsically and lets the host
// gate it (ADR-0005), so this is not called on the plain library build.
#[allow(dead_code)]
#[must_use]
pub fn credential_present() -> bool {
    // A test/fixture URL means "the tutor is reachable" even without a real
    // key (the fixture server needs no auth) — keeps CI's capability gate true.
    if std::env::var("GUIDED_LEARNING_LLM_URL").is_ok() {
        return true;
    }
    std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"))
        .or_else(|_| std::env::var("CLAUDE_CODE_OAUTH_TOKEN"))
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false)
}

/// Resolve the credential for the native/test path. `None` means "not
/// configured" → the tutor degrades cleanly. In the component the request is
/// issued bare (the host attaches the credential), so an absent key here does
/// not block the call — but it lets the action return an actionable error
/// natively rather than firing an unauthenticated request.
#[cfg(not(target_family = "wasm"))]
fn credential() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"))
        .or_else(|_| std::env::var("CLAUDE_CODE_OAUTH_TOKEN"))
        .ok()
        .filter(|k| !k.trim().is_empty())
}

/// A question as emitted by the generation call.
#[derive(Debug, Clone, Deserialize)]
pub struct GeneratedQuestion {
    pub topic: String,
    pub kind: String,
    pub prompt: String,
    pub model_answer: String,
}

#[derive(Deserialize)]
struct GenerateOutput {
    #[serde(default)]
    questions: Vec<GeneratedQuestion>,
}

/// The tutor's evaluation of one attempt.
#[derive(Debug, Clone, Deserialize)]
pub struct Evaluation {
    pub grade: u8,
    pub feedback: String,
    #[serde(default)]
    pub model_answer: Option<String>,
}

const GEN_SYSTEM: &str = "You are a tutor applying the techniques of *Make It Stick*. Given a piece \
     of study material, produce retrieval-practice QUESTIONS that make the learner recall and \
     generate rather than re-read. Cover the material's distinct topics; vary the kind across \
     factual recall, elaboration (\"explain in your own words\"), connection (\"how does this \
     relate to something you know?\"), and application. For each question give a concise, correct \
     model_answer grounded ONLY in the material. Do not restate the material as a question; ask \
     something that requires effortful retrieval.";

const EVAL_SYSTEM: &str = "You are a tutor grading a learner's attempt against a model answer, in \
     the spirit of *Make It Stick*. Grade 0-100 on correctness and completeness. Give brief, \
     Socratic feedback: name what's right, surface the specific misconception, and nudge toward \
     the gap rather than just stating the answer. Be encouraging about the attempt itself — \
     attempting before being told is what builds retention, even when wrong.";

fn generate_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "description": "retrieval-practice questions over the material",
                "items": {
                    "type": "object",
                    "properties": {
                        "topic": { "type": "string", "description": "short topic label this question belongs to" },
                        "kind": { "type": "string", "enum": ["factual", "elaboration", "connection", "application"] },
                        "prompt": { "type": "string", "description": "the question text" },
                        "model_answer": { "type": "string", "description": "concise correct answer grounded in the material" }
                    },
                    "required": ["topic", "kind", "prompt", "model_answer"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["questions"],
        "additionalProperties": false
    })
}

fn evaluate_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "grade": { "type": "integer", "minimum": 0, "maximum": 100, "description": "correctness/completeness 0-100" },
            "feedback": { "type": "string", "description": "brief Socratic feedback on the attempt" },
            "model_answer": { "type": "string", "description": "the reference answer to reveal" }
        },
        "required": ["grade", "feedback"],
        "additionalProperties": false
    })
}

/// Issue one Messages-API call with a structured-output schema and return the
/// model's structured text payload (the JSON string in the first text block).
async fn call(system: &str, user: String, schema: serde_json::Value) -> anyhow::Result<String> {
    // Natively, fail fast with an actionable message if no credential is
    // configured (the component path issues a bare request that the host
    // authenticates, so it does not gate here).
    #[cfg(not(target_family = "wasm"))]
    let cred = credential();
    #[cfg(not(target_family = "wasm"))]
    if cred.is_none() && std::env::var("GUIDED_LEARNING_LLM_URL").is_err() {
        anyhow::bail!(
            "the tutor needs an Anthropic credential: set ANTHROPIC_API_KEY (or \
             ANTHROPIC_AUTH_TOKEN / CLAUDE_CODE_OAUTH_TOKEN). Without it you can still \
             write and edit the study artifact by hand."
        );
    }

    let body = json!({
        "model": MODEL,
        "max_tokens": 2048,
        "output_config": {
            "format": { "type": "json_schema", "schema": schema },
        },
        "system": system,
        "messages": [{ "role": "user", "content": user }],
    });

    let req = http::Request::post(endpoint())
        .header("anthropic-version", "2023-06-01")
        .json(&body);

    // Attach the credential on the native path (inside the component the
    // request stays BARE — the host injects the credential at the http-fetch
    // egress boundary, ADR-0005). An OAuth token (sk-ant-oat… / a bearer)
    // authenticates via Authorization: Bearer + the OAuth beta header; a
    // standard API key uses x-api-key.
    #[cfg(not(target_family = "wasm"))]
    let req = match cred {
        Some(key)
            if key.starts_with("sk-ant-oat")
                || std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_ok() =>
        {
            req.header("authorization", format!("Bearer {key}"))
                .header("anthropic-beta", "oauth-2025-04-20")
        }
        Some(key) => req.header("x-api-key", &key),
        None => req,
    };

    let resp = http::fetch(req).await?;
    let payload: serde_json::Value = resp.json()?;
    if !resp.is_success() {
        anyhow::bail!("Anthropic request failed ({}): {payload}", resp.status);
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
        .ok_or_else(|| anyhow::anyhow!("Anthropic response had no text block: {payload}"))?;
    Ok(text.to_string())
}

/// Ask the tutor for retrieval-practice questions over the material.
pub async fn generate(material: &str, count: usize) -> anyhow::Result<Vec<GeneratedQuestion>> {
    let user = format!(
        "Produce about {count} retrieval-practice questions over this material.\n\nMATERIAL:\n{material}"
    );
    let text = call(GEN_SYSTEM, user, generate_schema()).await?;
    let out: GenerateOutput = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!("tutor question output did not match schema: {e}; got {text}")
    })?;
    Ok(out
        .questions
        .into_iter()
        .filter(|q| !q.prompt.trim().is_empty())
        .collect())
}

/// Ask the tutor to grade an attempt against the question + model answer.
pub async fn evaluate(
    prompt: &str,
    model_answer: &str,
    learner_answer: &str,
    idk: bool,
) -> anyhow::Result<Evaluation> {
    let attempt = if idk && learner_answer.trim().is_empty() {
        "(the learner said \"I don't know\")".to_string()
    } else {
        learner_answer.to_string()
    };
    let user = format!(
        "QUESTION:\n{prompt}\n\nMODEL ANSWER:\n{model_answer}\n\nLEARNER'S ATTEMPT:\n{attempt}\n\n\
         Grade the attempt and give Socratic feedback."
    );
    let text = call(EVAL_SYSTEM, user, evaluate_schema()).await?;
    let mut eval: Evaluation = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!("tutor evaluation output did not match schema: {e}; got {text}")
    })?;
    eval.grade = eval.grade.min(100);
    Ok(eval)
}
