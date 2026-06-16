//! The GitHub egress half of the feedback app.
//!
//! The user described "a tool that uses the gh CLI", but a sandboxed WASM
//! component cannot shell out — so issue creation is implemented the way every
//! other egress works in this repo (the guided-learning DeepSeek precedent,
//! ADR-0005): the component issues BARE requests to the GitHub REST API through
//! [`tangram::http`], and the HOST injects the `GH_TOKEN` bearer at the
//! `http-fetch` egress boundary. The plaintext token never enters the
//! component's address space or the replicated document.
//!
//! Two REST calls back the optional-screenshot flow:
//!   1. (optional) `PUT /repos/{owner}/{repo}/contents/{path}` — upload the
//!      image bytes via the Contents API to get a *raw* URL, because a
//!      base64 data URI embedded in markdown does NOT render on GitHub. The
//!      returned `content.download_url` is the raw link we embed. The commit
//!      is pinned to the dedicated `feedback-assets` branch (NEVER the default
//!      branch) — a stopgap asset store; see tracking issue #37.
//!   2. `POST /repos/{owner}/{repo}/issues` — create the issue with the title
//!      and the (possibly image-augmented) body.
//!
//! The base URL is overridable via `FEEDBACK_GITHUB_API` so tests can point the
//! calls at a local recorded-fixture server (no live GitHub call, no real
//! issue). Natively the credential is read from `GH_TOKEN`/`GITHUB_TOKEN` and
//! attached directly; inside the component the request stays bare and the host
//! attaches it.

use anyhow::Context as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::json;
use tangram::http;

/// Default GitHub REST API root (no trailing slash).
const DEFAULT_API: &str = "https://api.github.com";

/// The REST API root: the live GitHub URL, or a test/CI override.
fn api_base() -> String {
    std::env::var("FEEDBACK_GITHUB_API")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// Resolve the GitHub token for the native/test path. `None` means "not
/// configured" → the action returns an actionable error rather than firing an
/// unauthenticated request. Inside the component the request is issued bare
/// (the host attaches the credential at egress, ADR-0005), so an absent value
/// here does not block the component path.
#[cfg(not(target_family = "wasm"))]
fn credential() -> Option<String> {
    std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
        .filter(|k| !k.trim().is_empty())
}

/// The decoded parts of a `data:` URL (mime + raw bytes).
pub struct DecodedImage {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Decode a browser `data:` URL (e.g. `data:image/png;base64,iVBOR...`) into
/// its mime type and raw bytes. Only base64-encoded image data URLs are
/// accepted — anything else is rejected so a non-image payload never rides the
/// egress.
pub fn decode_data_url(data_url: &str) -> anyhow::Result<DecodedImage> {
    let rest = data_url
        .strip_prefix("data:")
        .context("screenshot is not a data: URL")?;
    let (meta, b64) = rest
        .split_once(',')
        .context("malformed data: URL (no comma)")?;
    // meta looks like "image/png;base64".
    let mime = meta.split(';').next().unwrap_or_default().to_string();
    if !mime.starts_with("image/") {
        anyhow::bail!("screenshot must be an image data URL, got mime {mime:?}");
    }
    if !meta.contains("base64") {
        anyhow::bail!("screenshot data URL must be base64-encoded");
    }
    // Browsers may wrap the payload; strip whitespace before decoding.
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = B64
        .decode(cleaned.as_bytes())
        .context("screenshot data URL payload is not valid base64")?;
    if bytes.is_empty() {
        anyhow::bail!("screenshot is empty");
    }
    Ok(DecodedImage { mime, bytes })
}

/// The file extension for an image mime type (defaults to `png`).
fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

/// The dedicated branch screenshots are committed to — NEVER the repo's
/// default branch. The branch is already named `feedback-assets`, so the asset
/// path under it does NOT repeat that prefix (see [`asset_path`]).
pub const ASSETS_BRANCH: &str = "feedback-assets";

/// The path the screenshot is uploaded to ON the [`ASSETS_BRANCH`], under a
/// clean `assets/` directory. The id keeps uploads unique so concurrent
/// submissions don't collide. No `feedback-assets/` prefix — the branch is
/// already named that.
pub fn asset_path(id: &str, mime: &str) -> String {
    format!("assets/{id}.{}", ext_for_mime(mime))
}

/// Build the issue body, appending a GitHub-rendered image reference when an
/// uploaded screenshot URL is supplied. A base64 data URI does NOT render in
/// GitHub markdown, so `image_url` must be a real uploaded raw URL.
#[must_use]
pub fn compose_body(body: &str, image_url: Option<&str>) -> String {
    let body = body.trim_end();
    match image_url {
        Some(url) if !url.trim().is_empty() => {
            if body.is_empty() {
                format!("![screenshot]({url})")
            } else {
                format!("{body}\n\n![screenshot]({url})")
            }
        }
        _ => body.to_string(),
    }
}

/// The JSON body for `POST /repos/{owner}/{repo}/issues`.
#[must_use]
pub fn issue_request_body(title: &str, body: &str) -> serde_json::Value {
    json!({ "title": title, "body": body })
}

/// The JSON body for `PUT /repos/{owner}/{repo}/contents/{path}`.
///
/// `branch` pins the commit to the dedicated [`ASSETS_BRANCH`] so the image
/// binary NEVER lands on the repo's default branch (`main`).
#[must_use]
pub fn contents_request_body(message: &str, content_b64: &str, branch: &str) -> serde_json::Value {
    json!({ "message": message, "content": content_b64, "branch": branch })
}

/// Attach the standard GitHub REST headers (and, on the native path, the bearer
/// credential). Inside the component the request stays bare — the host injects
/// the `GH_TOKEN` bearer at the `http-fetch` egress boundary (ADR-0005).
fn with_github_headers(req: http::Request) -> http::Request {
    let req = req
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        // GitHub rejects requests with no User-Agent.
        .header("user-agent", "tangram-feedback");
    #[cfg(not(target_family = "wasm"))]
    let req = match credential() {
        Some(token) => req.header("authorization", format!("Bearer {token}")),
        None => req,
    };
    req
}

#[derive(Deserialize)]
struct ContentsResponse {
    content: ContentsContent,
}

#[derive(Deserialize)]
struct ContentsContent {
    download_url: Option<String>,
}

/// Construct the raw URL for `path` on the [`ASSETS_BRANCH`] as a fallback when
/// the Contents-API response omits `content.download_url`.
fn raw_url(owner: &str, repo: &str, path: &str) -> String {
    format!("https://raw.githubusercontent.com/{owner}/{repo}/{ASSETS_BRANCH}/{path}")
}

/// Upload an image to the repo via the Contents API and return its raw link,
/// suitable for embedding in issue markdown.
///
/// STOPGAP (tracking issue #37): the binary is committed to the dedicated
/// `feedback-assets` branch (`branch` in the request body), NEVER the default
/// branch. GitHub has no public API for issue user-attachments, so this
/// side-branch is the asset store; revisit (release assets / external store)
/// per #37. The returned URL is the response `content.download_url` (already on
/// the `feedback-assets` ref), or a constructed `raw.githubusercontent.com`
/// URL on that ref as a fallback.
pub async fn upload_image(
    owner: &str,
    repo: &str,
    path: &str,
    image: &DecodedImage,
    commit_message: &str,
) -> anyhow::Result<String> {
    let content_b64 = B64.encode(&image.bytes);
    let url = format!(
        "{}/repos/{owner}/{repo}/contents/{path}",
        api_base(),
        owner = owner,
        repo = repo,
        path = path,
    );
    let req = with_github_headers(http::Request::new("PUT", url).json(&contents_request_body(
        commit_message,
        &content_b64,
        ASSETS_BRANCH,
    )));
    let resp = http::fetch(req).await?;
    let parsed: serde_json::Value = resp.json()?;
    if !resp.is_success() {
        anyhow::bail!("GitHub asset upload failed ({}): {parsed}", resp.status);
    }
    let contents: ContentsResponse = serde_json::from_value(parsed)
        .context("GitHub contents response was not the expected shape")?;
    // Prefer the response download_url (already points at feedback-assets); fall
    // back to constructing the raw URL on that ref.
    Ok(contents
        .content
        .download_url
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| raw_url(owner, repo, path)))
}

/// The result of creating an issue.
pub struct CreatedIssue {
    pub number: i64,
    pub url: String,
}

#[derive(Deserialize)]
struct IssueResponse {
    number: i64,
    html_url: String,
}

/// Create an issue on `owner/repo` with the given title and body.
pub async fn create_issue(
    owner: &str,
    repo: &str,
    title: &str,
    body: &str,
) -> anyhow::Result<CreatedIssue> {
    // Fail fast on the native path with an actionable message when no token is
    // configured (the component path issues a bare request the host
    // authenticates, so it does not gate here).
    #[cfg(not(target_family = "wasm"))]
    if credential().is_none() {
        anyhow::bail!(
            "filing an issue needs a GitHub token: set GH_TOKEN (or GITHUB_TOKEN). \
             Inside the Tangram host the token is injected automatically."
        );
    }

    let url = format!(
        "{}/repos/{owner}/{repo}/issues",
        api_base(),
        owner = owner,
        repo = repo,
    );
    let req = with_github_headers(http::Request::post(url).json(&issue_request_body(title, body)));
    let resp = http::fetch(req).await?;
    let parsed: serde_json::Value = resp.json()?;
    if !resp.is_success() {
        anyhow::bail!("GitHub issue creation failed ({}): {parsed}", resp.status);
    }
    let issue: IssueResponse = serde_json::from_value(parsed)
        .context("GitHub issue response was not the expected shape")?;
    Ok(CreatedIssue {
        number: issue.number,
        url: issue.html_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_body_appends_image_reference() {
        let out = compose_body("It broke here.", Some("https://raw.example/x.png"));
        assert_eq!(
            out,
            "It broke here.\n\n![screenshot](https://raw.example/x.png)"
        );
    }

    #[test]
    fn compose_body_without_image_is_unchanged() {
        assert_eq!(compose_body("plain", None), "plain");
        assert_eq!(compose_body("plain", Some("   ")), "plain");
    }

    #[test]
    fn compose_body_image_only_when_body_empty() {
        assert_eq!(
            compose_body("", Some("https://r/x.png")),
            "![screenshot](https://r/x.png)"
        );
    }

    #[test]
    fn issue_body_has_title_and_body() {
        let v = issue_request_body("T", "B");
        assert_eq!(v["title"], "T");
        assert_eq!(v["body"], "B");
    }

    #[test]
    fn decodes_a_png_data_url() {
        // "hi" base64-encoded.
        let img = decode_data_url("data:image/png;base64,aGk=").expect("decode");
        assert_eq!(img.mime, "image/png");
        assert_eq!(img.bytes, b"hi");
    }

    #[test]
    fn rejects_non_image_data_url() {
        assert!(decode_data_url("data:text/plain;base64,aGk=").is_err());
        assert!(decode_data_url("not a data url").is_err());
        assert!(decode_data_url("data:image/png,raw").is_err());
    }

    #[test]
    fn asset_path_uses_extension_for_mime() {
        assert_eq!(asset_path("abc", "image/png"), "assets/abc.png");
        assert_eq!(asset_path("abc", "image/jpeg"), "assets/abc.jpg");
        assert_eq!(asset_path("abc", "image/svg+xml"), "assets/abc.svg");
    }

    #[test]
    fn asset_path_has_no_feedback_assets_prefix() {
        // The branch is already named feedback-assets; the path under it must
        // not repeat that, and must never imply a write to main.
        let path = asset_path("abc", "image/png");
        assert!(!path.starts_with("feedback-assets/"), "path: {path}");
        assert!(path.starts_with("assets/"), "path: {path}");
    }

    #[test]
    fn contents_body_pins_the_assets_branch() {
        let v = contents_request_body("msg", "Yg==", ASSETS_BRANCH);
        assert_eq!(v["message"], "msg");
        assert_eq!(v["content"], "Yg==");
        assert_eq!(v["branch"], "feedback-assets");
    }

    #[test]
    fn raw_url_points_at_the_assets_branch() {
        assert_eq!(
            raw_url("owner", "repo", "assets/abc.png"),
            "https://raw.githubusercontent.com/owner/repo/feedback-assets/assets/abc.png"
        );
    }
}
