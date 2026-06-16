//! Feedback — file a GitHub issue on this repo straight from a Tangram app.
//!
//! A title, a body, and an optional drag-and-drop screenshot become a real
//! GitHub issue. The user described "a tool that uses the gh CLI", but apps
//! here are sandboxed WASM components and a host shell-out would break the
//! model — so issue creation is an `async` action that does HTTP egress to the
//! GitHub REST API (the guided-learning DeepSeek precedent). The `GH_TOKEN`
//! credential is injected HOST-side at the `http-fetch` egress boundary
//! (ADR-0005); it never enters the component or the replicated document.
//!
//! The egress lives in [`github`]; this module is the model + actions. The
//! app records every issue it files (`number`, `title`, `url`, `created_ms`)
//! in its Automerge doc so the UI shows a synced history of submissions.

use tangram::prelude::*;
use tangram::time::now_ms;

pub mod github;

/// The repo issues are filed against, as `owner/repo`. Determined at build time
/// from this worktree's `origin` remote (`aaronbuchwald/tangram`); overridable
/// host-side via `FEEDBACK_REPO` (e.g. `[apps.feedback.env]`) so the same
/// component can target a fork without a rebuild.
const DEFAULT_REPO: &str = "aaronbuchwald/tangram";

#[model]
#[derive(Default)]
pub struct Feedback {
    /// Issues this app has filed, newest stored last (the UI sorts).
    submitted: Vec<SubmittedIssue>,
}

#[model]
pub struct SubmittedIssue {
    /// The GitHub issue number.
    pub number: i64,
    /// The title that was filed.
    pub title: String,
    /// The `html_url` to the created issue.
    pub url: String,
    /// Whether a screenshot was attached.
    pub had_image: bool,
    pub created_ms: i64,
}

#[actions]
impl Feedback {
    /// File a GitHub issue on this repo. `title` and `body` are the issue's
    /// text; `image_data_url` is an optional drag-and-dropped screenshot as a
    /// browser `data:` URL (e.g. `data:image/png;base64,...`).
    ///
    /// When an image is supplied it is FIRST uploaded to the repo via the
    /// Contents API (a base64 data URI does not render in GitHub markdown, so
    /// the screenshot must be a real uploaded raw URL), then embedded as
    /// `![screenshot](<raw-url>)` in the body. The issue is then created and
    /// its `number`/`url` recorded in the app's doc. Returns the new issue's
    /// `html_url`.
    ///
    /// All network I/O happens OUTSIDE the store lock; only the final record is
    /// committed via [`Ctx::mutate`].
    pub async fn create_issue(
        ctx: Ctx<Self>,
        title: String,
        body: String,
        image_data_url: Option<String>,
    ) -> Result<String, String> {
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err("an issue needs a title".into());
        }
        let (owner, repo) = repo_owner_name();

        // If a screenshot was supplied, upload it first and fold the raw URL
        // into the body. Decoding happens before any network I/O so a bad
        // payload fails fast.
        let had_image = image_data_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let image_url = if had_image {
            let data_url = image_data_url.unwrap_or_default();
            let image = github::decode_data_url(&data_url).map_err(|e| format!("{e:#}"))?;
            let id = uuid::Uuid::new_v4().to_string();
            let path = github::asset_path(&id, &image.mime);
            let commit_message = format!("feedback: screenshot for \"{title}\"");
            let url = github::upload_image(&owner, &repo, &path, &image, &commit_message)
                .await
                .map_err(|e| format!("could not upload the screenshot: {e:#}"))?;
            Some(url)
        } else {
            None
        };

        let full_body = github::compose_body(&body, image_url.as_deref());
        let created = github::create_issue(&owner, &repo, &title, &full_body)
            .await
            .map_err(|e| format!("could not file the issue: {e:#}"))?;

        let url = created.url.clone();
        ctx.mutate("create_issue", move |m| {
            m.record_submission(
                created.number,
                title.clone(),
                created.url.clone(),
                had_image,
            );
            url.clone()
        })
        .map_err(|e| e.to_string())
    }

    /// The issues this app has filed, most recent first. Pure selector.
    #[must_use]
    pub fn list_submissions(&self) -> Vec<SubmittedIssue> {
        let mut rows = self.submitted.clone();
        rows.sort_by_key(|r| std::cmp::Reverse(r.created_ms));
        rows
    }
}

impl Feedback {
    /// Append a filed-issue record (commit step of `create_issue`).
    fn record_submission(&mut self, number: i64, title: String, url: String, had_image: bool) {
        self.submitted.push(SubmittedIssue {
            number,
            title,
            url,
            had_image,
            created_ms: now_ms(),
        });
    }
}

/// The `(owner, repo)` to file against: `FEEDBACK_REPO` if set, else the
/// build-time default. Malformed values fall back to the default.
fn repo_owner_name() -> (String, String) {
    let spec = std::env::var("FEEDBACK_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string());
    parse_repo(&spec).unwrap_or_else(|| parse_repo(DEFAULT_REPO).expect("default repo is valid"))
}

/// Split an `owner/repo` spec into its parts. `None` if either side is empty.
fn parse_repo(spec: &str) -> Option<(String, String)> {
    let (owner, repo) = spec.trim().split_once('/')?;
    let owner = owner.trim();
    let repo = repo.trim().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// MCP instructions, shared between the native app builder and the WASM
/// component's `describe()` export.
const INSTRUCTIONS: &str = "File a GitHub issue on this project's repository. Call create_issue \
     with a title, a body (markdown), and an optional screenshot as a base64 image data URL; the \
     screenshot is uploaded to the repo and embedded so it renders in the issue. list_submissions \
     shows the issues filed so far. The GitHub token is injected by the host at the egress \
     boundary — the component never sees it.";

/// The feedback app, fully configured (native binary / multi-app host).
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub fn app() -> App<Feedback> {
    App::<Feedback>::new("feedback")
        .instructions(INSTRUCTIONS)
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
}

// Compiled for wasm32-wasip2, the same model + actions become a Tangram
// component (`tangram-host` owns the platform around it). The GitHub REST calls
// go through the host's allowlist-enforced `http-fetch` import, which injects
// the GH_TOKEN bearer at the egress boundary (ADR-0005) — the component never
// sees the token.
#[cfg(target_family = "wasm")]
tangram::export_component!(Feedback {
    name: "feedback",
    instructions: INSTRUCTIONS,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_owner_repo() {
        assert_eq!(
            parse_repo("aaronbuchwald/tangram"),
            Some(("aaronbuchwald".into(), "tangram".into()))
        );
        assert_eq!(
            parse_repo("owner/repo.git"),
            Some(("owner".into(), "repo".into()))
        );
        assert!(parse_repo("noslash").is_none());
        assert!(parse_repo("owner/").is_none());
        assert!(parse_repo("/repo").is_none());
        assert!(parse_repo("a/b/c").is_none());
    }
}
