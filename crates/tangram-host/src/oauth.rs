//! Authorization-code OAuth/OIDC sign-in (docs/design/auth.md §7 C6).
//!
//! A hand-rolled authorization-code CLIENT (not an OAuth *server* — ADR-0003
//! rejected that), GitHub-first, pointed at any external IdP. The endpoint URLs
//! are env-overridable so the e2e swaps a stub IdP for GitHub — the exact seam
//! `scripts/e2e-cloudflare-identity.sh` exercises on the Cloudflare side, mirrored
//! here for the native host (`scripts/e2e-host-oauth.sh`).
//!
//! Flow (mounted at the host root in `routes::root_router`, multi-tenant only):
//!
//! 1. `GET /api/auth/oauth/start` — generate a CSPRNG `state`, set it in a
//!    short-lived HttpOnly cookie, and 302 to the IdP's authorize URL with
//!    `client_id` + `redirect_uri` + `state` + `scope`.
//! 2. `GET /api/auth/oauth/callback?code=…&state=…` — validate `state` against
//!    the cookie (CSRF defense), exchange the `code` at the token URL (sending
//!    the client secret), fetch userinfo, map the IdP identity
//!    `ident:<provider>:<sub>` → an EXISTING account (re-login) or a NEW one
//!    (first sign-in), mint a session, set the session cookie, and 302 to `/`.
//!
//! Config (`[auth]` in apps.toml): `oauth_issuer` selects the provider family
//! (GitHub default endpoints), `oauth_client_id`, `oauth_client_secret`
//! (`env://…`, resolved through the secret seam — never inline). The three URLs
//! default to GitHub's and are individually overridable by
//! `OAUTH_{AUTHORIZE,TOKEN,USER}_URL`. Config validation (`OauthConfig::resolve`)
//! rejects a PARTIAL config (some but not all of client_id/secret) but treats a
//! FULLY-ABSENT one as "PAT-only bootstrap" — still fully functional (§7).

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use rand::RngCore as _;

use crate::accounts::AccountStore;
use crate::auth::ScopeSet;
use crate::multitenant::{SESSION_COOKIE, now_ms};

/// 30 days in ms — the session TTL (matches `authapi`).
const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// The CSRF state cookie name (HttpOnly, short-lived, SameSite=Lax).
const STATE_COOKIE: &str = "tangram_oauth_state";

/// The resolved OAuth client config (auth.md §7 C6). Present only when the
/// operator configured a client id + secret in multi-tenant mode.
#[derive(Debug, Clone)]
pub struct OauthConfig {
    pub authorize_url: String,
    pub token_url: String,
    pub user_url: String,
    pub client_id: String,
    pub client_secret: String,
    /// A stable provider tag for the identity key (`ident:<provider>:<sub>`).
    pub provider: String,
    /// The OAuth scope(s) requested at authorize time.
    pub scope: String,
}

impl OauthConfig {
    /// Resolve the OAuth config from `[auth]` + env overrides, with the secret
    /// taken through the secret seam (`env://…`). Returns:
    ///
    /// - `Ok(None)` — no OAuth configured: PAT-only bootstrap (still fully
    ///   functional, §7). This is the common no-IdP fleet.
    /// - `Ok(Some(cfg))` — a complete, usable client config.
    /// - `Err(_)` — a PARTIAL / unresolvable config (e.g. a client id but no
    ///   secret, or an `env://` secret that didn't resolve). Multi-tenant must
    ///   reject this rather than silently fall back to an open/half-configured
    ///   mode (auth.md §2 config-validation rule, §12 checklist).
    pub async fn resolve(
        auth: &crate::config::AuthConfig,
        secrets: &crate::secrets::SecretRegistry,
    ) -> anyhow::Result<Option<Self>> {
        let client_id = auth
            .oauth_client_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let secret_ref = auth
            .oauth_client_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        match (client_id, secret_ref) {
            (None, None) => Ok(None), // PAT-only bootstrap.
            (Some(_), None) | (None, Some(_)) => anyhow::bail!(
                "incomplete [auth] OAuth config: set BOTH oauth_client_id AND \
                 oauth_client_secret, or neither (PAT-only). A partial OAuth config must not \
                 silently fall back to an open mode (auth.md §2)."
            ),
            (Some(client_id), Some(secret_ref)) => {
                let client_secret =
                    crate::config::expand_value(secrets, "auth: oauth_client_secret", secret_ref)
                        .await;
                anyhow::ensure!(
                    !client_secret.trim().is_empty(),
                    "[auth] oauth_client_secret ({secret_ref:?}) did not resolve to a value \
                     (set the env var it references) — multi-tenant OAuth must not run with an \
                     unresolvable secret"
                );
                let issuer = auth.oauth_issuer.as_deref().unwrap_or("github");
                let provider = provider_tag(issuer);
                let (def_auth, def_token, def_user) = default_endpoints(issuer);
                Ok(Some(Self {
                    authorize_url: env_or("OAUTH_AUTHORIZE_URL", def_auth),
                    token_url: env_or("OAUTH_TOKEN_URL", def_token),
                    user_url: env_or("OAUTH_USER_URL", def_user),
                    client_id: client_id.to_string(),
                    client_secret,
                    provider,
                    scope: std::env::var("OAUTH_SCOPE").unwrap_or_else(|_| "read:user".into()),
                }))
            }
        }
    }
}

/// `OAUTH_<KEY>` env override, else the GitHub-family default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// A short, stable provider tag for the identity key. GitHub for the default
/// issuer; otherwise the issuer's host (so two providers never collide).
fn provider_tag(issuer: &str) -> String {
    if issuer.eq_ignore_ascii_case("github") || issuer.contains("github.com") {
        return "github".to_string();
    }
    // Best-effort host extraction; fall back to the raw issuer.
    issuer
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(issuer)
        .to_string()
}

/// The GitHub-family default endpoints (the e2e overrides each via env).
fn default_endpoints(_issuer: &str) -> (&'static str, &'static str, &'static str) {
    (
        "https://github.com/login/oauth/authorize",
        "https://github.com/login/oauth/access_token",
        "https://api.github.com/user",
    )
}

/// Mint a CSPRNG `state` value (URL-safe, no padding) for the CSRF round-trip.
fn mint_state() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The redirect_uri the IdP calls back. Derived from the incoming request's
/// Host header so it works behind any bind / proxy without extra config.
fn redirect_uri(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:8080");
    // Loopback is plain http; anything else assumes https (a real deploy is
    // TLS-terminated). The scheme only affects the URL we hand the IdP.
    let scheme = if host.starts_with("127.0.0.1")
        || host.starts_with("localhost")
        || host.starts_with("[::1]")
    {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}/api/auth/oauth/callback")
}

/// `GET /api/auth/oauth/start` — set a state cookie and 302 to the IdP.
pub fn start(cfg: &OauthConfig, headers: &HeaderMap) -> Response {
    let state = mint_state();
    let redirect = redirect_uri(headers);
    let authorize = format!(
        "{}?client_id={}&redirect_uri={}&scope={}&state={}&response_type=code",
        cfg.authorize_url,
        urlencode(&cfg.client_id),
        urlencode(&redirect),
        urlencode(&cfg.scope),
        urlencode(&state),
    );
    // Short-lived (10 min) HttpOnly state cookie — the CSRF defense.
    let cookie = format!("{STATE_COOKIE}={state}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600");
    (
        StatusCode::FOUND,
        [(header::SET_COOKIE, cookie), (header::LOCATION, authorize)],
    )
        .into_response()
}

/// `GET /api/auth/oauth/callback?code=…&state=…` — validate state, exchange the
/// code, map identity → account, mint a session, redirect home.
pub async fn callback(
    cfg: &OauthConfig,
    store: &Arc<AccountStore>,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Response {
    let params = parse_query(query);
    let code = params
        .iter()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.clone());
    let state = params
        .iter()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.clone());
    let (Some(code), Some(state)) = (code, state) else {
        return oauth_error("missing code or state in the OAuth callback");
    };
    // CSRF: the returned state must match the cookie we set at /start.
    let cookie_state = cookie_value(headers, STATE_COOKIE);
    if cookie_state.as_deref() != Some(state.as_str()) {
        return oauth_error("OAuth state mismatch (possible CSRF) — start sign-in again");
    }

    let redirect = redirect_uri(headers);
    let access_token = match exchange_code(cfg, &code, &redirect).await {
        Ok(token) => token,
        Err(e) => {
            tracing::warn!("oauth: code exchange failed: {e:#}");
            return oauth_error("OAuth code exchange failed");
        }
    };
    let (sub, login_name, email) = match fetch_userinfo(cfg, &access_token).await {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!("oauth: userinfo fetch failed: {e:#}");
            return oauth_error("OAuth userinfo fetch failed");
        }
    };

    let ident = format!("ident:{}:{}", cfg.provider, sub);
    let user_id = match map_identity_to_account(store, &ident, &login_name, &email) {
        Ok(user_id) => user_id,
        Err(e) => {
            tracing::error!("oauth: account mapping failed for {ident}: {e:#}");
            return oauth_error("OAuth account mapping failed");
        }
    };

    let now = now_ms();
    let session = match store.create_session(&user_id, now, Some(now + SESSION_TTL_MS)) {
        Ok(token) => token,
        Err(e) => {
            tracing::error!("oauth: session mint failed for {user_id}: {e:#}");
            return oauth_error("OAuth session mint failed");
        }
    };
    let session_cookie = format!(
        "{SESSION_COOKIE}={session}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        SESSION_TTL_MS / 1000
    );
    // Clear the state cookie and set the session cookie, then land on the shell.
    // Two `Set-Cookie` headers must be APPENDED (not inserted) — an array of
    // same-named headers in an axum response overwrites, dropping one.
    let clear_state = format!("{STATE_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    let mut response = (StatusCode::FOUND, [(header::LOCATION, "/")]).into_response();
    let headers = response.headers_mut();
    if let (Ok(s), Ok(c)) = (
        header::HeaderValue::from_str(&session_cookie),
        header::HeaderValue::from_str(&clear_state),
    ) {
        headers.append(header::SET_COOKIE, s);
        headers.append(header::SET_COOKIE, c);
    }
    response
}

/// Map the IdP identity to a local account: an existing link → that account
/// (re-login); otherwise create a NEW account under a collision-safe user_id
/// derived from the login name, link the identity, and mint a full-scope
/// authority (a signed-in human is the interactive authority — narrowing is the
/// PAT's job). Returns the resolved `user_id`.
fn map_identity_to_account(
    store: &AccountStore,
    ident: &str,
    login_name: &str,
    email: &str,
) -> anyhow::Result<String> {
    if let Some(existing) = store.account_for_ident(ident)? {
        return Ok(existing);
    }
    // First sign-in: a fresh, collision-safe account.
    let base = slugify(login_name);
    let mut user_id = base.clone();
    let mut n = 2;
    while store.account(&user_id)?.is_some() {
        user_id = format!("{base}-{n}");
        n += 1;
    }
    let email = if email.is_empty() {
        format!("{login_name}@oauth.local")
    } else {
        email.to_string()
    };
    store.create_account(&user_id, &email, &[], now_ms())?;
    store.link_ident(ident, &user_id)?;
    // A signed-in human gets the full interactive scope set (a session always
    // carries `all()`; the account's PATs are minted narrower in Devices & Keys).
    let _ = ScopeSet::all();
    tracing::info!("oauth: created account {user_id} for {ident}");
    Ok(user_id)
}

/// A URL/path-trivial slug for a login name (lowercase alnum + dash). Empty →
/// "user" so a degenerate login still produces a valid id.
fn slugify(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = s.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "user".to_string()
    } else {
        trimmed
    }
}

/// Exchange the authorization code for an access token at the token URL. Sends
/// the client secret form-encoded; accepts a JSON or form-encoded response
/// (GitHub returns form-encoded by default, JSON with an Accept header — we ask
/// for JSON).
async fn exchange_code(
    cfg: &OauthConfig,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(&cfg.token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "token endpoint {}",
        resp.status()
    );
    let body = resp.text().await?;
    // Try JSON first, then form-encoded (`access_token=…&token_type=…`).
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(tok) = json.get("access_token").and_then(serde_json::Value::as_str) {
            return Ok(tok.to_string());
        }
        anyhow::bail!("token response had no access_token: {json}");
    }
    parse_query(Some(&body))
        .into_iter()
        .find(|(k, _)| k == "access_token")
        .map(|(_, v)| v)
        .ok_or_else(|| anyhow::anyhow!("token response had no access_token"))
}

/// Fetch userinfo with the access token. Returns `(sub, login, email)`. Handles
/// both GitHub (`id` numeric + `login`) and generic OIDC (`sub` + `email` /
/// `preferred_username`).
async fn fetch_userinfo(
    cfg: &OauthConfig,
    access_token: &str,
) -> anyhow::Result<(String, String, String)> {
    let client = reqwest::Client::new();
    let resp = client
        .get(&cfg.user_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {access_token}"),
        )
        // GitHub requires a User-Agent on its API.
        .header(reqwest::header::USER_AGENT, "tangram-host")
        .send()
        .await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "userinfo endpoint {}",
        resp.status()
    );
    let json: serde_json::Value = resp.json().await?;
    // `sub` (OIDC) or `id` (GitHub) — both stable per-user identifiers.
    let sub = json
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| json.get("id").map(|v| v.to_string()))
        .ok_or_else(|| anyhow::anyhow!("userinfo had no sub/id"))?;
    let login = json
        .get("login")
        .or_else(|| json.get("preferred_username"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| sub.clone());
    let email = json
        .get("email")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((sub, login, email))
}

/// A plain-text OAuth error page (303 back to the login would loop; show why).
fn oauth_error(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        format!("OAuth sign-in failed: {msg}"),
    )
        .into_response()
}

// ── small helpers (no extra deps) ───────────────────────────────────────────

/// Minimal percent-encoding for query/redirect components (RFC 3986 unreserved
/// set passes through; everything else is `%XX`). Avoids pulling in a URL crate.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Percent-decode a single query value (`%XX` and `+` → space).
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a `a=b&c=d` query string into decoded `(key, value)` pairs.
fn parse_query(query: Option<&str>) -> Vec<(String, String)> {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((urldecode(k), urldecode(v)))
        })
        .collect()
}

/// Read one cookie value out of the `Cookie` header.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_is_url_trivial_and_collision_base() {
        assert_eq!(slugify("Alice"), "alice");
        assert_eq!(slugify("Bob Smith"), "bob-smith");
        assert_eq!(slugify("@@@"), "user");
        assert_eq!(slugify("a.b_c"), "a-b-c");
    }

    #[test]
    fn urlencode_roundtrips_through_decode() {
        for s in ["abc", "a b&c=d", "https://x/y?z=1", "state+/=", "café"] {
            assert_eq!(urldecode(&urlencode(s)), s);
        }
    }

    #[test]
    fn parse_query_decodes_pairs() {
        let q = parse_query(Some("code=abc%2F123&state=xy%2Bz"));
        assert_eq!(q[0], ("code".into(), "abc/123".into()));
        assert_eq!(q[1], ("state".into(), "xy+z".into()));
    }

    #[test]
    fn provider_tag_recognizes_github() {
        assert_eq!(provider_tag("github"), "github");
        assert_eq!(provider_tag("https://github.com"), "github");
        assert_eq!(
            provider_tag("https://accounts.google.com"),
            "accounts.google.com"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_partial_and_allows_absent() {
        use crate::config::AuthConfig;
        let secrets = crate::secrets::SecretRegistry::default();

        // Fully absent → None (PAT-only bootstrap).
        let cfg = AuthConfig::default();
        assert!(
            OauthConfig::resolve(&cfg, &secrets)
                .await
                .unwrap()
                .is_none()
        );

        // client id without secret → error (no silent open-mode fallback).
        let cfg = AuthConfig {
            oauth_client_id: Some("id".into()),
            ..Default::default()
        };
        assert!(OauthConfig::resolve(&cfg, &secrets).await.is_err());

        // secret without client id → error.
        let cfg = AuthConfig {
            oauth_client_secret: Some("env://NOPE".into()),
            ..Default::default()
        };
        assert!(OauthConfig::resolve(&cfg, &secrets).await.is_err());
    }

    #[tokio::test]
    async fn resolve_builds_a_usable_config_with_env_overrides() {
        use crate::config::AuthConfig;
        let secrets = crate::secrets::SecretRegistry::default();
        // The secret resolves from an inline literal (the secret seam passes a
        // non-reference value through unchanged).
        let cfg = AuthConfig {
            oauth_issuer: Some("github".into()),
            oauth_client_id: Some("the-client".into()),
            oauth_client_secret: Some("the-secret".into()),
            ..Default::default()
        };
        // SAFETY: tests are single-threaded per #[tokio::test]; we set + clear.
        unsafe {
            std::env::set_var("OAUTH_AUTHORIZE_URL", "http://idp/authorize");
            std::env::set_var("OAUTH_TOKEN_URL", "http://idp/token");
            std::env::set_var("OAUTH_USER_URL", "http://idp/user");
        }
        let resolved = OauthConfig::resolve(&cfg, &secrets).await.unwrap().unwrap();
        assert_eq!(resolved.authorize_url, "http://idp/authorize");
        assert_eq!(resolved.token_url, "http://idp/token");
        assert_eq!(resolved.user_url, "http://idp/user");
        assert_eq!(resolved.client_id, "the-client");
        assert_eq!(resolved.client_secret, "the-secret");
        assert_eq!(resolved.provider, "github");
        unsafe {
            std::env::remove_var("OAUTH_AUTHORIZE_URL");
            std::env::remove_var("OAUTH_TOKEN_URL");
            std::env::remove_var("OAUTH_USER_URL");
        }
    }
}
