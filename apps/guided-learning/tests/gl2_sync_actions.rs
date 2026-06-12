//! GL2 — sync actions + the generation/calibration invariants (no LLM).
//!
//! Dispatches the pure sync actions against an in-memory doc and asserts the
//! invariants the *Make It Stick* techniques require:
//!   - generation gate: a reveal needs a prior committed attempt;
//!   - calibration ordering: confidence is captured BEFORE reveal, grade stays
//!     unset until evaluation;
//!   - peeking is recorded; the artifact edits last-writer-wins; reflection
//!     appends to the artifact.

mod support;
use support::{act, act_err, fresh_ctx};

/// Seed a session and inject one question directly into the doc (no LLM), so
/// the pure sync gates can be exercised. Returns (session_id, question_id).
async fn session_with_question(
    ctx: &tangram_core::Ctx<guided_learning::GuidedLearning>,
) -> (String, String) {
    let sid = act(
        ctx,
        "start_session",
        serde_json::json!({ "material": "Photosynthesis basics." }),
    )
    .await
    .as_str()
    .unwrap()
    .to_string();
    // Inject a question via a direct mutation (stands in for generate_questions).
    let qid = "q-test".to_string();
    ctx.mutate("seed_question", |m| {
        m.test_push_question(&sid, &qid, "What captures light?", "Chlorophyll");
    })
    .unwrap();
    (sid, qid)
}

#[tokio::test]
async fn start_session_seeds_artifact_and_title() {
    let ctx = fresh_ctx();
    let sid = act(
        &ctx,
        "start_session",
        serde_json::json!({ "material": "The Krebs cycle\noccurs in the mitochondria." }),
    )
    .await;
    let sid = sid.as_str().unwrap();

    let sessions = act(&ctx, "list_sessions", serde_json::json!({})).await;
    let row = &sessions.as_array().unwrap()[0];
    assert_eq!(row["id"], sid);
    assert_eq!(row["title"], "The Krebs cycle", "title is the first line");

    let state = ctx.state_json();
    let artifact = state["sessions"][0]["artifact_md"].as_str().unwrap();
    assert!(
        artifact.contains("# The Krebs cycle"),
        "artifact seeded with heading"
    );
    assert!(
        artifact.contains("## Source"),
        "artifact records provenance"
    );
}

#[tokio::test]
async fn submit_answer_records_confidence_before_grade() {
    let ctx = fresh_ctx();
    let (sid, qid) = session_with_question(&ctx).await;

    act(
        &ctx,
        "submit_answer",
        serde_json::json!({ "session_id": sid, "question_id": qid, "answer": "chlorophyll", "idk": false, "confidence": 80 }),
    )
    .await;

    let state = ctx.state_json();
    let attempt = &state["sessions"][0]["questions"][0]["attempts"][0];
    assert_eq!(
        attempt["confidence"], 80,
        "confidence captured at submit time"
    );
    assert!(
        attempt["grade"].is_null(),
        "grade stays None until evaluate_answer (calibration ordering)"
    );
    assert_eq!(
        state["sessions"][0]["questions"][0]["revealed"], false,
        "submitting an answer does not reveal"
    );
}

#[tokio::test]
async fn reveal_requires_a_prior_attempt_generation_gate() {
    let ctx = fresh_ctx();
    let (sid, qid) = session_with_question(&ctx).await;

    // evaluate_answer with NO prior attempt must fail (the generation gate:
    // you cannot reveal/grade before committing an attempt). This exercises
    // the gate without an LLM — it fails before any network call.
    let err = act_err(
        &ctx,
        "evaluate_answer",
        serde_json::json!({ "session_id": sid, "question_id": qid }),
    )
    .await;
    assert!(
        err.contains("no unscored attempt") || err.contains("submit_answer"),
        "reveal must be gated on an attempt; got: {err}"
    );

    let state = ctx.state_json();
    assert!(
        state["sessions"][0]["questions"][0]["model_answer"].is_null(),
        "model_answer stays withheld until a gated reveal"
    );
}

#[tokio::test]
async fn idk_counts_as_an_attempt_but_empty_text_alone_does_not() {
    let ctx = fresh_ctx();
    let (sid, qid) = session_with_question(&ctx).await;

    // Empty answer, idk=false -> rejected.
    let err = act_err(
        &ctx,
        "submit_answer",
        serde_json::json!({ "session_id": sid, "question_id": qid, "answer": "", "idk": false, "confidence": 10 }),
    )
    .await;
    assert!(err.contains("provide an answer"), "got: {err}");

    // Empty answer, idk=true -> a valid attempt.
    act(
        &ctx,
        "submit_answer",
        serde_json::json!({ "session_id": sid, "question_id": qid, "answer": "", "idk": true, "confidence": 5 }),
    )
    .await;
    let state = ctx.state_json();
    assert_eq!(
        state["sessions"][0]["questions"][0]["attempts"][0]["idk"],
        true
    );
}

#[tokio::test]
async fn mark_peeked_records_the_peek() {
    let ctx = fresh_ctx();
    let (sid, qid) = session_with_question(&ctx).await;
    act(
        &ctx,
        "mark_peeked",
        serde_json::json!({ "session_id": sid, "question_id": qid }),
    )
    .await;
    let state = ctx.state_json();
    assert_eq!(state["sessions"][0]["questions"][0]["peeked"], true);
}

#[tokio::test]
async fn edit_artifact_is_last_writer_wins() {
    let ctx = fresh_ctx();
    let sid = act(
        &ctx,
        "start_session",
        serde_json::json!({ "material": "x" }),
    )
    .await
    .as_str()
    .unwrap()
    .to_string();
    act(
        &ctx,
        "edit_artifact",
        serde_json::json!({ "session_id": sid, "new_md": "# Rewritten\nbody" }),
    )
    .await;
    let state = ctx.state_json();
    assert_eq!(state["sessions"][0]["artifact_md"], "# Rewritten\nbody");
}

#[tokio::test]
async fn record_reflection_appends_to_artifact() {
    let ctx = fresh_ctx();
    let sid = act(
        &ctx,
        "start_session",
        serde_json::json!({ "material": "x" }),
    )
    .await
    .as_str()
    .unwrap()
    .to_string();
    act(
        &ctx,
        "record_reflection",
        serde_json::json!({ "session_id": sid, "text": "I confused light and dark reactions." }),
    )
    .await;
    let state = ctx.state_json();
    assert_eq!(
        state["sessions"][0]["reflection"],
        "I confused light and dark reactions."
    );
    let artifact = state["sessions"][0]["artifact_md"].as_str().unwrap();
    assert!(
        artifact.contains("## Reflection"),
        "reflection section appended"
    );
    assert!(
        artifact.contains("confused light and dark"),
        "reflection text appended verbatim"
    );
}
