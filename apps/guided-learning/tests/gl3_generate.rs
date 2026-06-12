//! GL3 — the fixture LLM transport + `generate_questions`.
//!
//! Stands up a local recorded-Anthropic fixture server, points the tutor's
//! `http-fetch` at it via `GUIDED_LEARNING_LLM_URL`, and asserts that
//! `generate_questions` parses the structured `json_schema` output into
//! Question/Topic rows and commits them via `ctx.mutate`. No live key needed
//! (the nutrition / rmcp-golden fixture precedent).

mod support;
use support::{FixtureServer, act, anthropic_response, fresh_ctx, llm_env_guard};

#[tokio::test]
async fn generate_questions_parses_and_commits_structured_output() {
    let _guard = llm_env_guard().await;

    // The canned tutor output: two topics, mixed kinds.
    let fixture = anthropic_response(serde_json::json!({
        "questions": [
            { "topic": "Light reactions", "kind": "factual",
              "prompt": "What pigment captures light?", "model_answer": "Chlorophyll." },
            { "topic": "Light reactions", "kind": "elaboration",
              "prompt": "Explain in your own words how light energy is captured.",
              "model_answer": "Chlorophyll absorbs photons, exciting electrons." },
            { "topic": "Calvin cycle", "kind": "connection",
              "prompt": "How does the Calvin cycle relate to respiration you know?",
              "model_answer": "Both move carbon and energy between forms." }
        ]
    }));
    let server = FixtureServer::fixed(fixture).await;
    // SAFETY: serialized by `llm_env_guard`; single-threaded over this var.
    unsafe { std::env::set_var("GUIDED_LEARNING_LLM_URL", &server.url) };

    let ctx = fresh_ctx();
    let sid = act(
        &ctx,
        "start_session",
        serde_json::json!({ "material": "Photosynthesis: light reactions and the Calvin cycle." }),
    )
    .await
    .as_str()
    .unwrap()
    .to_string();

    let added = act(
        &ctx,
        "generate_questions",
        serde_json::json!({ "session_id": sid, "count": 3 }),
    )
    .await;
    assert_eq!(added, 3, "all three structured questions were committed");

    let state = ctx.state_json();
    let session = &state["sessions"][0];
    let questions = session["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 3);

    // Two distinct topics were created (de-duplicated by name).
    let topics = session["topics"].as_array().unwrap();
    assert_eq!(topics.len(), 2, "topics de-duplicated: {topics:?}");

    // Kinds normalized and varied (retrieval is not all one kind).
    let kinds: Vec<&str> = questions
        .iter()
        .map(|q| q["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"factual")
            && kinds.contains(&"elaboration")
            && kinds.contains(&"connection")
    );

    // The generation gate: model answers are WITHHELD on the questions until
    // reveal (they live in pending_answers, not on the question).
    for q in questions {
        assert!(
            q["model_answer"].is_null(),
            "model_answer withheld pre-reveal: {q}"
        );
        assert_eq!(q["revealed"], false);
        assert_eq!(q["schedule"]["due_at_ms"], 0, "new questions are due now");
    }
    assert_eq!(
        session["pending_answers"].as_array().unwrap().len(),
        3,
        "the three reference answers are held back in pending_answers"
    );

    unsafe { std::env::remove_var("GUIDED_LEARNING_LLM_URL") };
}

#[tokio::test]
async fn generate_questions_errors_on_unknown_session() {
    let _guard = llm_env_guard().await;
    let server =
        FixtureServer::fixed(anthropic_response(serde_json::json!({ "questions": [] }))).await;
    unsafe { std::env::set_var("GUIDED_LEARNING_LLM_URL", &server.url) };

    let ctx = fresh_ctx();
    let err = ctx
        .apply(
            "generate_questions",
            serde_json::json!({ "session_id": "nope" }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no session with id"), "got: {err}");

    unsafe { std::env::remove_var("GUIDED_LEARNING_LLM_URL") };
}
