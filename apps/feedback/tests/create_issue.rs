//! `create_issue` egress shaping, against a recorded GitHub fixture server.
//!
//! These tests NEVER touch real GitHub and NEVER file a real issue: the GitHub
//! REST base URL is overridden to a local fixture (`FEEDBACK_GITHUB_API`) that
//! records every request and replies with Contents-API / issue-shaped bodies.
//! They pin the two-call screenshot flow, the markdown image embed, the issue
//! request shape, and the recorded-submission history.

mod support;
use support::{GithubFixture, act, act_err, env_guard, fresh_ctx};

/// A 1x1 transparent PNG as a browser data URL (what a drag-drop produces).
const PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

async fn set_env(fixture: &GithubFixture) {
    // SAFETY: serialized by env_guard.
    unsafe {
        std::env::set_var("FEEDBACK_GITHUB_API", &fixture.url);
        std::env::set_var("FEEDBACK_REPO", "owner/repo");
        std::env::set_var("GH_TOKEN", "fake-test-token");
    }
}

fn clear_env() {
    unsafe {
        std::env::remove_var("FEEDBACK_GITHUB_API");
        std::env::remove_var("FEEDBACK_REPO");
        std::env::remove_var("GH_TOKEN");
    }
}

#[tokio::test]
async fn files_a_text_only_issue_and_records_it() {
    let _guard = env_guard().await;
    let fixture = GithubFixture::start().await;
    set_env(&fixture).await;

    let ctx = fresh_ctx();
    let url = act(
        &ctx,
        "create_issue",
        serde_json::json!({ "title": "Bug: thing broke", "body": "Steps to repro." }),
    )
    .await;
    assert!(
        url.as_str().unwrap().contains("/issues/4242"),
        "returns the created issue URL: {url}"
    );

    let reqs = fixture.requests();
    clear_env();

    // Text-only: exactly one call, and it is POST .../issues.
    assert_eq!(reqs.len(), 1, "one call for a text-only issue: {reqs:?}");
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, "/repos/owner/repo/issues");
    assert_eq!(reqs[0].body["title"], "Bug: thing broke");
    assert_eq!(reqs[0].body["body"], "Steps to repro.");

    // The submission was recorded in the doc.
    let rows = ctx.state_json();
    let submitted = rows["submitted"].as_array().unwrap();
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted[0]["number"], 4242);
    assert_eq!(submitted[0]["had_image"], false);
}

#[tokio::test]
async fn uploads_screenshot_then_embeds_it_in_the_issue() {
    let _guard = env_guard().await;
    let fixture = GithubFixture::start().await;
    set_env(&fixture).await;

    let ctx = fresh_ctx();
    act(
        &ctx,
        "create_issue",
        serde_json::json!({
            "title": "With screenshot",
            "body": "See attached.",
            "image_data_url": PNG_DATA_URL,
        }),
    )
    .await;

    let reqs = fixture.requests();
    clear_env();

    // Two calls in order: upload via Contents API, then create the issue.
    assert_eq!(reqs.len(), 2, "upload + create: {reqs:?}");
    assert_eq!(reqs[0].method, "PUT");
    // The screenshot uploads under a clean `assets/` path — NOT a
    // `feedback-assets/` dir prefix (the branch is already named that), and
    // never anything that implies a write to the default branch.
    assert!(
        reqs[0]
            .path
            .starts_with("/repos/owner/repo/contents/assets/"),
        "screenshot uploads under assets/: {}",
        reqs[0].path
    );
    assert!(
        !reqs[0].path.contains("/contents/feedback-assets/"),
        "no feedback-assets/ dir prefix on the path: {}",
        reqs[0].path
    );
    assert!(
        reqs[0].path.ends_with(".png"),
        "png extension preserved: {}",
        reqs[0].path
    );
    // The upload commit is pinned to the dedicated feedback-assets branch, so
    // the image binary never lands on the default branch (main).
    assert_eq!(
        reqs[0].body["branch"], "feedback-assets",
        "PUT pins the feedback-assets branch: {:?}",
        reqs[0].body
    );
    // The uploaded content is base64 (the Contents API shape), non-empty.
    assert!(reqs[0].body["content"].as_str().unwrap().len() > 10);

    assert_eq!(reqs[1].method, "POST");
    assert_eq!(reqs[1].path, "/repos/owner/repo/issues");
    let body = reqs[1].body["body"].as_str().unwrap();
    assert!(
        body.contains("See attached.")
            && body.contains("![screenshot](")
            && body.contains("/raw/feedback-assets/assets/shot.png"),
        "issue body embeds the uploaded raw URL on the feedback-assets ref: {body:?}"
    );
    // The embedded URL is on the feedback-assets ref, never main.
    assert!(
        body.contains("/feedback-assets/") && !body.contains("/main/"),
        "embedded URL points at the feedback-assets ref, not main: {body:?}"
    );

    let submitted = ctx.state_json()["submitted"].as_array().unwrap().clone();
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted[0]["had_image"], true);
}

#[tokio::test]
async fn only_hits_declared_github_paths() {
    // The component's entire view of the outside world is the two declared
    // GitHub REST calls (the egress-containment property, component side).
    let _guard = env_guard().await;
    let fixture = GithubFixture::start().await;
    set_env(&fixture).await;

    let ctx = fresh_ctx();
    act(
        &ctx,
        "create_issue",
        serde_json::json!({ "title": "t", "body": "b", "image_data_url": PNG_DATA_URL }),
    )
    .await;

    let reqs = fixture.requests();
    clear_env();

    for r in &reqs {
        assert!(
            (r.method == "PUT" && r.path.contains("/repos/owner/repo/contents/"))
                || (r.method == "POST" && r.path == "/repos/owner/repo/issues"),
            "feedback only issues the two declared GitHub calls: {} {}",
            r.method,
            r.path
        );
        // Natively the bearer rides on the request (in the component the host
        // injects it); either way the call is authenticated.
        assert_eq!(
            r.authorization.as_deref().map(str::to_ascii_lowercase),
            Some("bearer fake-test-token".to_string())
        );
    }
}

#[tokio::test]
async fn rejects_an_empty_title() {
    let _guard = env_guard().await;
    // No fixture needed — the title check fires before any network I/O.
    let ctx = fresh_ctx();
    let err = act_err(
        &ctx,
        "create_issue",
        serde_json::json!({ "title": "   ", "body": "b" }),
    )
    .await;
    assert!(err.contains("title"), "empty title is rejected: {err}");
}

#[tokio::test]
async fn rejects_a_non_image_screenshot() {
    let _guard = env_guard().await;
    let fixture = GithubFixture::start().await;
    set_env(&fixture).await;

    let ctx = fresh_ctx();
    let err = act_err(
        &ctx,
        "create_issue",
        serde_json::json!({
            "title": "t",
            "body": "b",
            "image_data_url": "data:text/plain;base64,aGk=",
        }),
    )
    .await;
    let reqs = fixture.requests();
    clear_env();

    assert!(err.contains("image"), "non-image payload rejected: {err}");
    assert!(reqs.is_empty(), "no egress on a bad screenshot: {reqs:?}");
}
