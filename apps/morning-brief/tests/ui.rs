//! MB5: the single-file UI honors the app contract.
//!
//! The brief's UI is prefix-mounted under the shell (`/morning-brief/`), so
//! every fetch MUST be relative — an absolute `/api/...` path would break out
//! of the prefix and hit the wrong app. This test reads the committed
//! `ui/index.html` and asserts that invariant statically (no browser, no
//! server, no network — it runs in CI), plus that the UI only drives
//! registered actions and the capabilities probe. The live UX is verified
//! manually (the `verify` skill).

const UI: &str = include_str!("../ui/index.html");

/// The actions the offline core registers (the UI must not call anything else).
const KNOWN_ACTIONS: &[&str] = &[
    "set_system_prompt",
    "set_model_tier",
    "set_max_runs",
    "add_section",
    "update_section",
    "remove_section",
    "reorder_section",
    "set_section_enabled",
    "set_source",
    "rate_run",
    "correct_section",
    "promote_to_learned",
    "set_learned_weight",
    "remove_learned",
    "delete_run",
    "run_brief",
];

#[test]
fn all_fetches_are_relative() {
    // Catch absolute fetch/EventSource targets like fetch("/api/…") or
    // new EventSource("/api/…") — the prefix-mount footgun the contract forbids.
    for marker in [
        "fetch(\"/",
        "fetch('/",
        "fetch(`/",
        "EventSource(\"/",
        "EventSource('/",
        "EventSource(`/",
    ] {
        assert!(
            !UI.contains(marker),
            "UI must use relative fetches only; found absolute target via {marker:?}"
        );
    }
    // The relative endpoints it should use.
    assert!(UI.contains("api/actions/"), "posts actions relatively");
    assert!(UI.contains("api/events"), "subscribes to SSE relatively");
    assert!(
        UI.contains("api/capabilities"),
        "reads the capabilities probe"
    );
}

#[test]
fn ui_only_calls_registered_actions() {
    // Every `act("name"` / `api/actions/${name}` literal must be a known action.
    // The UI builds the URL as `api/actions/${name}` and calls `act("name", …)`,
    // so scan for the `act("…"` call sites.
    let mut found = 0;
    for (i, _) in UI.match_indices("act(\"") {
        let rest = &UI[i + 5..];
        let name: String = rest.chars().take_while(|&c| c != '"').collect();
        // skip the helper definition `async function act(` — its call sites
        // always pass a string literal first.
        assert!(
            KNOWN_ACTIONS.contains(&name.as_str()),
            "UI calls unknown action {name:?}"
        );
        found += 1;
    }
    assert!(
        found >= 10,
        "expected the UI to drive most actions, saw {found}"
    );
}

#[test]
fn ui_has_the_three_panes_and_run_controls() {
    for pane in [
        "data-pane=\"brief\"",
        "data-pane=\"config\"",
        "data-pane=\"history\"",
    ] {
        assert!(UI.contains(pane), "missing pane {pane}");
    }
    // The dry-run (fixture, offline) entry point is the dreaming flagship.
    assert!(UI.contains("input_mode\": \"fixture\"") || UI.contains("input_mode: \"fixture\""));
    assert!(UI.contains("Dry-run on fixtures"));
}
