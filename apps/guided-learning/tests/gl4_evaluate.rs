//! GL4 — `evaluate_answer`: grade, calibration, schedule advance, artifact.
//!
//! A submitted attempt + reveal sets grade/feedback, flips `revealed`,
//! advances the spaced schedule (a wrong answer is due sooner), reveals the
//! withheld model answer, and appends the exchange to `artifact_md`. The
//! calibration delta (confidence − grade) is computable and a confident-wrong
//! attempt is flagged.

mod support;
use support::{FixtureServer, act, deepseek_response, fresh_ctx, llm_env_guard};

type Ctx = tangram_core::Ctx<guided_learning::GuidedLearning>;

/// Drive generate → submit → evaluate against a scripted fixture, returning
/// the ctx and session id so callers can read state or run further actions.
async fn run(confidence: u8, grade: u8) -> (Ctx, String) {
    let generate = deepseek_response(serde_json::json!({
        "questions": [
            { "topic": "Light reactions", "kind": "factual",
              "prompt": "What pigment captures light?", "model_answer": "Chlorophyll." }
        ]
    }));
    let evaluate = deepseek_response(serde_json::json!({
        "grade": grade,
        "feedback": "You're close on energy capture, but name the pigment precisely.",
        "model_answer": "Chlorophyll is the light-capturing pigment."
    }));
    let server = FixtureServer::with_sequence(vec![generate, evaluate]).await;
    // SAFETY: serialized by `llm_env_guard`.
    unsafe { std::env::set_var("GUIDED_LEARNING_LLM_URL", &server.url) };

    let ctx = fresh_ctx();
    let sid = act(
        &ctx,
        "start_session",
        serde_json::json!({ "material": "Photosynthesis." }),
    )
    .await
    .as_str()
    .unwrap()
    .to_string();
    act(
        &ctx,
        "generate_questions",
        serde_json::json!({ "session_id": sid, "count": 1 }),
    )
    .await;

    let state = ctx.state_json();
    let qid = state["sessions"][0]["questions"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    act(
        &ctx,
        "submit_answer",
        serde_json::json!({ "session_id": sid, "question_id": qid, "answer": "a green thing", "idk": false, "confidence": confidence }),
    )
    .await;

    let returned_grade = act(
        &ctx,
        "evaluate_answer",
        serde_json::json!({ "session_id": sid, "question_id": qid }),
    )
    .await;
    assert_eq!(returned_grade, grade, "evaluate_answer returns the grade");

    unsafe { std::env::remove_var("GUIDED_LEARNING_LLM_URL") };
    (ctx, sid)
}

#[tokio::test]
async fn evaluate_grades_reveals_schedules_and_appends() {
    let _guard = llm_env_guard().await;
    // Confident but wrong: confidence 90, graded 30.
    let (ctx, _sid) = run(90, 30).await;
    let state = ctx.state_json();
    let q = &state["sessions"][0]["questions"][0];

    let attempt = &q["attempts"][0];
    assert_eq!(attempt["grade"], 30, "grade recorded after reveal");
    assert!(
        attempt["feedback"].as_str().unwrap().contains("pigment"),
        "Socratic feedback recorded"
    );

    assert_eq!(q["revealed"], true, "reveal flips after evaluation");
    assert_eq!(
        q["model_answer"].as_str().unwrap(),
        "Chlorophyll is the light-capturing pigment.",
        "the withheld model answer is revealed"
    );
    assert!(
        state["sessions"][0]["pending_answers"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the pending answer is consumed on reveal"
    );

    // Wrong answer (grade 30 < pass) -> Leitner box stays at 0, due sooner.
    assert_eq!(
        q["schedule"]["interval_index"], 0,
        "a wrong answer does not promote the box"
    );

    // Artifact accreted the exchange with the calibration note.
    let artifact = state["sessions"][0]["artifact_md"].as_str().unwrap();
    assert!(
        artifact.contains("What pigment captures light?"),
        "question appended"
    );
    assert!(
        artifact.contains("a green thing"),
        "learner's own attempt appended verbatim"
    );
    assert!(
        artifact.contains("Over-confident"),
        "calibration note flags over-confidence"
    );
}

#[tokio::test]
async fn calibration_action_flags_confident_wrong() {
    let _guard = llm_env_guard().await;
    let (ctx, sid) = run(95, 20).await;

    // The calibration action computes confidence-vs-grade per graded attempt.
    let points = act(
        &ctx,
        "calibration",
        serde_json::json!({ "session_id": sid }),
    )
    .await;
    let p = &points.as_array().unwrap()[0];
    assert_eq!(p["confidence"], 95);
    assert_eq!(p["grade"], 20);
    assert_eq!(p["delta"], 75, "delta = confidence - grade");
    assert_eq!(
        p["overconfident"], true,
        "a confident-wrong attempt is flagged"
    );
}

#[tokio::test]
async fn passing_answer_promotes_the_box() {
    let _guard = llm_env_guard().await;
    let (ctx, _sid) = run(70, 85).await;
    let state = ctx.state_json();
    let q = &state["sessions"][0]["questions"][0];
    assert_eq!(
        q["schedule"]["interval_index"], 1,
        "a passing grade promotes one Leitner box"
    );
    assert!(
        q["schedule"]["due_at_ms"].as_i64().unwrap() > 0,
        "the next review is scheduled into the future"
    );
    let artifact = state["sessions"][0]["artifact_md"].as_str().unwrap();
    assert!(artifact.contains("well calibrated") || artifact.contains("Confidence"));
}
