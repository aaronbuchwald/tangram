//! Bundled fixtures for the offline core.
//!
//! The per-source input fixtures live next to their source modules
//! (`calendar.rs`/`gmail.rs` `include_str!` their JSON). This module holds the
//! **canned LLM response** the fixture-offline `run_brief` returns, so the
//! whole pipeline runs with ZERO network in CI (design §8.2 "fixture-offline
//! LLM"). It is parsed by the [`crate::llm`] fixture path into per-section
//! outputs keyed by `section_id`.

/// The canned model response (checked-in `fixtures/llm_response.json`): a
/// `{ "sections": [{ "section_id", "content" }, ...] }` object matching the
/// structured-output schema the live LLM call requests.
pub const LLM_RESPONSE: &str = include_str!("../../fixtures/llm_response.json");
