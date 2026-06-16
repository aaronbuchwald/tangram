//! GL7 — egress containment (the §7 theorem, component side).
//!
//! The component's entire view of the outside world is ONE call: `POST
//! .../v1/chat/completions`. This pins the component-side half of the containment
//! theorem — the tutor only ever issues the single declared call (method +
//! path), so the host's allowlist/inject (and PR #1's call-level capability)
//! has exactly one egress to bind a credential to.
//!
//! The HOST-side denial (any other host/path is refused and un-credentialed)
//! is exercised through `tangram-host` once the component is built and
//! registered — see `crates/tangram-host/tests/guided_learning_egress.rs`,
//! which reuses the nutrition `egress_injection.rs` harness and self-skips
//! without the wasm component.

mod support;
use support::{FixtureServer, act, deepseek_response, fresh_ctx, llm_env_guard};

#[tokio::test]
async fn the_tutor_only_issues_the_declared_post_completions_call() {
    let _guard = llm_env_guard().await;
    let generate = deepseek_response(serde_json::json!({
        "questions": [
            { "topic": "T", "kind": "factual", "prompt": "Q?", "model_answer": "A." }
        ]
    }));
    let evaluate = deepseek_response(serde_json::json!({
        "grade": 70, "feedback": "ok", "model_answer": "A."
    }));
    let server = FixtureServer::with_sequence(vec![generate, evaluate]).await;
    // SAFETY: serialized by llm_env_guard.
    unsafe { std::env::set_var("GUIDED_LEARNING_LLM_URL", &server.url) };

    let ctx = fresh_ctx();
    let sid = act(
        &ctx,
        "start_session",
        serde_json::json!({ "material": "m" }),
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
        serde_json::json!({ "session_id": sid, "question_id": qid, "answer": "x", "idk": false, "confidence": 50 }),
    )
    .await;
    act(
        &ctx,
        "evaluate_answer",
        serde_json::json!({ "session_id": sid, "question_id": qid }),
    )
    .await;

    unsafe { std::env::remove_var("GUIDED_LEARNING_LLM_URL") };

    // Every request the tutor made is exactly `POST /v1/chat/completions`.
    let lines = server.request_lines();
    assert!(!lines.is_empty(), "the tutor made at least one call");
    for line in &lines {
        assert!(
            line.starts_with("POST /v1/chat/completions "),
            "the tutor only issues POST /v1/chat/completions — never another method/path: {line:?}"
        );
    }
}
