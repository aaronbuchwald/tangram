//! GL8 — UI contract smoke test.
//!
//! The single-file UI must obey the app contract: one self-contained
//! `ui/index.html`, vendored marked + DOMPurify as single files (no CDN, no
//! build step), relative fetch paths only, and it must subscribe to the live
//! `api/events` SSE stream and read the capabilities probe. These are static,
//! deterministic invariants — no server needed in CI.

use std::path::Path;

fn ui_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ui")
}

#[test]
fn ui_is_single_file_with_vendored_libs() {
    let html = std::fs::read_to_string(ui_dir().join("index.html")).expect("index.html");

    // Vendored single files exist and are referenced (no CDN).
    assert!(
        ui_dir().join("vendor/marked.min.js").exists(),
        "marked vendored"
    );
    assert!(
        ui_dir().join("vendor/purify.min.js").exists(),
        "DOMPurify vendored"
    );
    assert!(
        html.contains("vendor/marked.min.js"),
        "references vendored marked"
    );
    assert!(
        html.contains("vendor/purify.min.js"),
        "references vendored DOMPurify"
    );
    assert!(
        !html.contains("cdn.") && !html.contains("https://unpkg") && !html.contains("//cdn"),
        "no CDN / external script sources (buildless, self-contained)"
    );

    // Defense-in-depth: markdown is sanitized through DOMPurify before innerHTML.
    assert!(
        html.contains("DOMPurify.sanitize"),
        "sanitizes rendered markdown"
    );
}

#[test]
fn ui_uses_relative_paths_and_the_live_surfaces() {
    let html = std::fs::read_to_string(ui_dir().join("index.html")).expect("index.html");

    // Relative fetch paths only (prefix-mounted under the shell).
    assert!(
        html.contains("`api/actions/${name}`"),
        "actions via relative api/actions/"
    );
    assert!(!html.contains("fetch(\"/api"), "no absolute /api paths");
    assert!(!html.contains("fetch(`/"), "no absolute fetch roots");

    // Subscribes to the live state stream and the capabilities probe.
    assert!(
        html.contains("new EventSource(\"api/events\")"),
        "subscribes to api/events SSE"
    );
    assert!(
        html.contains("fetch(\"api/capabilities\")"),
        "reads the capabilities probe"
    );
    assert!(
        html.contains("description_input"),
        "gates the tutor UI on the host-gated flag"
    );

    // The Make-It-Stick affordances are present in the UI.
    for affordance in [
        "Reveal",
        "Confidence",
        "I don't know",
        "Due for review",
        "Calibration",
        "Study note",
        "show source",
        "Reflection",
    ] {
        assert!(
            html.contains(affordance),
            "UI surfaces the affordance: {affordance:?}"
        );
    }

    // The reveal is gated on an attempt (the button starts disabled).
    assert!(
        html.contains("id=\"reveal-btn\" disabled"),
        "reveal is gated (starts disabled)"
    );
}
