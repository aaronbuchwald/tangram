//! Offline/mechanical end-to-end test of the preflight → decision → persist
//! flow (the substrate extension spec). NO live site, NO real CAPTCHA, NO real
//! browser: every browser interaction is represented by a fixture
//! [`Snapshot`], and the LLM-assisted path is a deterministic closure. Anything
//! that would need a live browser self-skips.

use tangram_automation::decision::{
    AssistanceRequest, LlmCaptchaBudget, LlmSolveOutcome, SignInPath, SignInResolution, route,
    run_llm_captcha_loop,
};
use tangram_automation::preflight::{PreflightOutcome, SignedInHeuristics, preflight_with_session};
use tangram_automation::request::{AutomationRequest, OperatorPolicy, authorize};
use tangram_automation::script::{Locator, Snapshot};
use tangram_automation::session::{PersistedSession, StorageState};

const FIXTURE: &str = include_str!("fixtures/amazon_storage_state.json");

fn amazon_authorized() -> tangram_automation::request::AuthorizedAutomation {
    let policy = OperatorPolicy::new()
        .allow_app("grocery")
        .approve_template("amazon-grocery-cart")
        .ceiling(["www.amazon.com"])
        .grant_credentials(
            "grocery",
            [
                "op://Shopper/Amazon/username",
                "op://Shopper/Amazon/password",
            ],
        );
    let request = AutomationRequest {
        app: "grocery".into(),
        template_id: "amazon-grocery-cart".into(),
        params: vec!["milk".into()],
        domains: vec!["www.amazon.com".into()],
        credential_refs: vec![
            "op://Shopper/Amazon/username".into(),
            "op://Shopper/Amazon/password".into(),
        ],
    };
    authorize(&request, &policy).expect("authorized")
}

fn signed_in_snapshot() -> Snapshot {
    Snapshot {
        url_host: Some("www.amazon.com".into()),
        text: "Hello, Aaron  Returns & Orders".into(),
        locators: vec![Locator::new("link", "Account & Lists")],
    }
}

fn sign_in_form_snapshot() -> Snapshot {
    Snapshot {
        url_host: Some("www.amazon.com".into()),
        text: "Sign-In  Enter your password".into(),
        locators: vec![
            Locator::new("textbox", "Password"),
            Locator::new("button", "Sign in"),
        ],
    }
}

#[test]
fn fixture_storage_state_parses_and_is_valid() {
    let state = StorageState::from_json(FIXTURE).expect("fixture parses");
    assert_eq!(state.site, "www.amazon.com");
    assert!(state.is_usable());
    // The fixture's persistent cookies are far in the future ⇒ not expired.
    assert!(!state.is_expired(1_749_600_001));
}

#[test]
fn happy_path_signed_in_skips_login_entirely() {
    // A persisted session loads; the auth-gated page shows the account menu →
    // SignedIn → NO decision, NO credential fetch, straight to the task.
    let dir = tempfile::tempdir().unwrap();
    let sess = PersistedSession::at(dir.path().join("amazon"), "www.amazon.com");
    let state = StorageState::from_json(FIXTURE).unwrap();
    sess.save_storage_state(&state).unwrap();

    let h = SignedInHeuristics::amazon();
    let outcome = preflight_with_session(&sess, 1_749_600_001, &signed_in_snapshot(), &h).unwrap();
    assert_eq!(outcome, PreflightOutcome::SignedIn);

    // No assistance request is surfaced when signed in.
    let req = AssistanceRequest::from_preflight(
        &outcome,
        "www.amazon.com",
        &amazon_authorized(),
        LlmCaptchaBudget::modest(),
    );
    assert!(req.is_none());
}

#[test]
fn expired_session_routes_to_decision_then_persists_after_interactive() {
    // 1. An EXPIRED persisted session + a sign-in form page → Expired.
    let dir = tempfile::tempdir().unwrap();
    let sess = PersistedSession::at(dir.path().join("amazon"), "www.amazon.com");
    let mut expired = StorageState::new("www.amazon.com");
    expired.cookies.push(tangram_automation::session::Cookie {
        name: "at-main".into(),
        value: "old".into(),
        domain: ".amazon.com".into(),
        path: "/".into(),
        expires: 1_000.0,
    });
    sess.save_storage_state(&expired).unwrap();

    let h = SignedInHeuristics::amazon();
    let now = 2_000_000_000;
    let outcome = preflight_with_session(&sess, now, &sign_in_form_snapshot(), &h).unwrap();
    assert_eq!(outcome, PreflightOutcome::Expired);

    // 2. The decision point surfaces, offering both paths.
    let auth = amazon_authorized();
    let req = AssistanceRequest::from_preflight(
        &outcome,
        "www.amazon.com",
        &auth,
        LlmCaptchaBudget::modest(),
    )
    .unwrap();
    assert_eq!(
        req.offered,
        vec![SignInPath::Interactive, SignInPath::LlmAssisted]
    );

    // 3. Owner picks interactive; on this headless box the resolution is to run
    //    interactively (on the owner's machine) and bring back the session.
    assert_eq!(
        route(SignInPath::Interactive, None),
        SignInResolution::RunInteractive
    );

    // 4. The expired artifact is invalidated, then the freshly-solved session
    //    (here: the portable fixture brought back) is persisted.
    sess.invalidate().unwrap();
    let fresh = StorageState::from_json(FIXTURE).unwrap();
    sess.save_storage_state(&fresh).unwrap();

    // 5. The NEXT run hits the preflight and — with a signed-in page — skips
    //    login. One-time cost proven.
    let outcome2 = preflight_with_session(&sess, now, &signed_in_snapshot(), &h).unwrap();
    assert_eq!(outcome2, PreflightOutcome::SignedIn);
}

#[test]
fn llm_path_exhausts_budget_and_falls_back_to_interactive() {
    let dir = tempfile::tempdir().unwrap();
    let sess = PersistedSession::at(dir.path().join("amazon"), "www.amazon.com");
    let h = SignedInHeuristics::amazon();

    // First run, no session → NoSession → decision.
    let outcome = preflight_with_session(&sess, 1, &sign_in_form_snapshot(), &h).unwrap();
    assert_eq!(outcome, PreflightOutcome::NoSession);

    // Owner picks the LLM path; the puzzle is never solved (Amazon puzzles are
    // hard). The budget bounds the burn and we fall back.
    let budget = LlmCaptchaBudget {
        max_attempts: 3,
        max_tokens: 30_000,
    };
    let llm = run_llm_captcha_loop(budget, 10_000, |_| (false, 10_000));
    match &llm {
        LlmSolveOutcome::ExhaustedFallBackToInteractive { attempts, tokens } => {
            assert_eq!(*attempts, 3);
            assert!(*tokens <= 30_000, "hard token ceiling held");
        }
        other => panic!("expected fallback, got {other:?}"),
    }
    assert_eq!(
        route(SignInPath::LlmAssisted, Some(&llm)),
        SignInResolution::LlmExhaustedFellBack
    );
}

#[test]
fn llm_path_solves_within_budget() {
    let budget = LlmCaptchaBudget::modest();
    // Solve on the first attempt.
    let llm = run_llm_captcha_loop(budget, 10_000, |_| (true, 8_000));
    assert!(matches!(llm, LlmSolveOutcome::Solved { .. }));
    assert_eq!(
        route(SignInPath::LlmAssisted, Some(&llm)),
        SignInResolution::LlmSolved
    );
}

#[test]
fn persisted_artifact_and_assistance_request_carry_no_secret_values() {
    // The persisted session IS a bearer (it holds cookie values) — but it must
    // live ONLY at the owner-only on-disk path, never in the repo. The
    // ASSISTANCE REQUEST (which is surfaced to the operator / could be logged)
    // must carry only references, never values.
    let auth = amazon_authorized();
    let req = AssistanceRequest::from_preflight(
        &PreflightOutcome::NoSession,
        "www.amazon.com",
        &auth,
        LlmCaptchaBudget::modest(),
    )
    .unwrap();
    let serialized = serde_json::to_string(&req).unwrap();
    // References are present; no resolved value ever is (there are none here).
    assert!(serialized.contains("op://Shopper/Amazon/password"));
    // The human-facing prompt names counts + budget, not the listed refs.
    let prompt = req.prompt();
    assert!(!prompt.contains("op://Shopper/Amazon/password"));

    // The session summary (the loggable view) never leaks a cookie value.
    let state = StorageState::from_json(FIXTURE).unwrap();
    let summary = state.summary();
    assert!(!summary.contains("FIXTURE-at-main-bearer-not-a-real-token"));
    assert!(!summary.contains("FIXTURE-session-id-bearer-not-a-real-token"));
}

#[test]
fn recorded_script_never_carries_a_session_or_credential_value() {
    // The companion property to the session-as-credential rule: the REVIEWABLE
    // recorded script holds only references, never a value (the existing
    // assert_no_secret_values), and a session never belongs in it at all.
    let demo = include_str!("fixtures/amazon_cart_demo.script.json");
    let script =
        tangram_automation::script::AutomationScript::from_json(demo).expect("demo script parses");
    script
        .assert_no_secret_values()
        .expect("no literal secrets in the script");
    // And no cookie bearer accidentally pasted in.
    assert!(!demo.contains("at-main"));
    assert!(!demo.contains("session-id="));
}
