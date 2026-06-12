//! GL6 — degrade-without-key + capabilities probe.
//!
//! With NO Anthropic credential resolvable: the sync actions still work (the
//! artifact is editable offline); the LLM-backed actions return a clean,
//! actionable error ("configure ANTHROPIC_API_KEY"); and the capabilities
//! probe reports the tutor unavailable. Mirrors the nutrition / egress
//! `configured-iff-resolves` precedent.

mod support;
use support::{act, fresh_ctx, llm_env_guard};

/// Clear every credential env var (and the fixture URL) for the duration of a
/// test, restoring them after. Returns the env guard so the var mutations are
/// serialized against the other LLM tests.
async fn no_credentials() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = llm_env_guard().await;
    for var in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "GUIDED_LEARNING_LLM_URL",
    ] {
        // SAFETY: serialized by the guard; the LLM tests are the only readers.
        unsafe { std::env::remove_var(var) };
    }
    guard
}

#[tokio::test]
async fn capabilities_report_tutor_unavailable_without_key() {
    let _guard = no_credentials().await;
    assert!(
        !guided_learning::tutor::credential_present(),
        "with no credential and no fixture URL, the tutor reports unavailable"
    );
    let caps = guided_learning::capabilities_json(guided_learning::tutor::credential_present());
    assert_eq!(caps["tutor_available"], false);
}

#[tokio::test]
async fn sync_actions_work_offline() {
    let _guard = no_credentials().await;
    let ctx = fresh_ctx();

    // Start a session, edit the artifact, reflect — all without a key.
    let sid = act(
        &ctx,
        "start_session",
        serde_json::json!({ "material": "Offline study" }),
    )
    .await
    .as_str()
    .unwrap()
    .to_string();
    act(
        &ctx,
        "edit_artifact",
        serde_json::json!({ "session_id": sid, "new_md": "# Hand-written" }),
    )
    .await;
    act(
        &ctx,
        "record_reflection",
        serde_json::json!({ "session_id": sid, "text": "wrote this by hand" }),
    )
    .await;

    let state = ctx.state_json();
    assert!(
        state["sessions"][0]["artifact_md"]
            .as_str()
            .unwrap()
            .contains("Hand-written")
    );
}

#[tokio::test]
async fn llm_actions_return_actionable_error_without_key() {
    let _guard = no_credentials().await;
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

    let err = ctx
        .apply(
            "generate_questions",
            serde_json::json!({ "session_id": sid }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("ANTHROPIC_API_KEY") || err.contains("credential"),
        "the LLM action names the missing credential: {err}"
    );
    // The error does not corrupt state — no questions were committed.
    assert_eq!(
        ctx.state_json()["sessions"][0]["questions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}
