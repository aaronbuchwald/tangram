//! Durable session reuse — make login/CAPTCHA a ONE-TIME cost
//! (`task-automation-browser.md` §6/§8; the substrate extension spec #4).
//!
//! A persisted authenticated session is an **auth bearer** — possessing it is
//! equivalent to being signed in. So it is handled exactly like a credential:
//!
//! - stored OUTSIDE the repo (default `~/.tangram-automation/profiles/<site>/`),
//! - gitignored by location (never under the worktree),
//! - file perms restricted to the owner (`0o600` files, `0o700` dirs),
//! - never logged and never embedded in a recorded [`crate::script`] (the
//!   script stays a reviewable artifact carrying only references),
//! - optionally **sealed back into 1Password** via an `op://` reference and
//!   restored on demand through the existing [`crate::broker`] discipline.
//!
//! Two persistence shapes (both implemented; the recommendation is in the doc):
//!
//! - **Persistent `userDataDir`** (`launchPersistentContext`) — the full
//!   browser profile, including a stable device fingerprint, which reduces
//!   Amazon re-challenges. Site-local, not portable across machines.
//! - **`storageState` export** (`context.storageState({path})`) — portable
//!   cookies + localStorage JSON. This is the **handoff format** from an
//!   interactive solve performed elsewhere (the owner's machine / a VNC
//!   bridge) carried back to this headless box.
//!
//! The preflight ([`crate::preflight`]) loads one of these before any login
//! attempt; expiry/invalidation is detected there and falls back to the
//! decision point ([`crate::decision`]).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One cookie from a Playwright `storageState` export. Only the fields the
/// preflight + expiry logic need are modeled; unknown keys are ignored on the
/// way in and dropped on the way out (we never round-trip a secret we don't
/// understand). `value` IS the bearer — see [`StorageState::is_usable`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default)]
    pub path: String,
    /// Unix seconds; `-1` (or absent) ⇒ a session cookie (no persistent
    /// expiry). Playwright emits a float; we keep it as `f64` for fidelity.
    #[serde(default = "session_cookie")]
    pub expires: f64,
}

fn session_cookie() -> f64 {
    -1.0
}

/// The portable session artifact: Playwright's `storageState` shape (cookies +
/// per-origin localStorage), plus our own small envelope (which site, when
/// captured). This is the JSON that travels from an interactive solve on the
/// owner's machine back to this box.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
pub struct StorageState {
    /// The site this session authenticates (e.g. `"www.amazon.com"`). Our
    /// envelope field, not part of Playwright's native shape.
    #[serde(default)]
    pub site: String,
    /// When this artifact was captured (unix seconds). Our envelope field;
    /// used for a coarse age check independent of cookie expiry.
    #[serde(default)]
    pub captured_at: u64,
    #[serde(default)]
    pub cookies: Vec<Cookie>,
    /// Per-origin localStorage, Playwright's native shape. Opaque to us.
    #[serde(default)]
    pub origins: Vec<serde_json::Value>,
}

impl StorageState {
    pub fn new(site: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            captured_at: now_secs(),
            ..Default::default()
        }
    }

    /// Parse a `storageState` JSON (from `context.storageState({path})` or a
    /// handoff file). Tolerates Playwright's native shape (no `site`/
    /// `captured_at` envelope) — those default.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("storage state serializes")
    }

    /// True if EVERY persistent (non-session) cookie has expired as of `now`
    /// (unix seconds) — i.e. the artifact can no longer authenticate and the
    /// preflight must fall back to the decision point. Session cookies
    /// (`expires < 0`) don't count toward expiry (the browser would have them
    /// for the lifetime of a context); an artifact of ONLY session cookies is
    /// treated as expired once reloaded, since they don't survive.
    pub fn is_expired(&self, now: u64) -> bool {
        let now = now as f64;
        let persistent: Vec<&Cookie> = self.cookies.iter().filter(|c| c.expires >= 0.0).collect();
        if persistent.is_empty() {
            // No durable cookie ⇒ nothing carries the session across a reload.
            return true;
        }
        persistent.iter().all(|c| c.expires <= now)
    }

    /// The soonest persistent-cookie expiry (unix seconds), if any. Useful for
    /// logging "session valid until …" without exposing values.
    pub fn earliest_expiry(&self) -> Option<u64> {
        self.cookies
            .iter()
            .filter(|c| c.expires >= 0.0)
            .map(|c| c.expires as u64)
            .min()
    }

    /// A redacted summary safe to log: counts + earliest expiry, NEVER values.
    pub fn summary(&self) -> String {
        format!(
            "session<site={} cookies={} origins={} expiry={:?}>",
            self.site,
            self.cookies.len(),
            self.origins.len(),
            self.earliest_expiry()
        )
    }

    /// A session artifact IS a bearer of `value`s by design — there is no
    /// "no secret values" property to assert (unlike a recorded script). What
    /// we CAN assert is that the artifact never leaks where it must not: this
    /// checks the artifact is non-empty and carries at least one cookie, the
    /// minimum to be a usable, portable handoff. (The repo-safety property —
    /// "this never lives in the repo" — is enforced by path, not content; see
    /// [`PersistedSession`].)
    pub fn is_usable(&self) -> bool {
        !self.cookies.is_empty()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the durable session for one site lives on disk. Both shapes live under
/// the same per-site directory, OUTSIDE the repo by default
/// (`~/.tangram-automation/profiles/<site>/`):
///
/// - `user_data_dir()` — the persistent profile dir for `launchPersistentContext`.
/// - `storage_state_path()` — the portable `storageState` JSON.
///
/// The session is treated as a credential: [`PersistedSession::save_storage_state`]
/// writes with `0o600`, [`PersistedSession::ensure_dir`] makes the tree `0o700`,
/// and [`PersistedSession::assert_outside_repo`] refuses any root under a repo
/// working tree.
#[derive(Debug, Clone)]
pub struct PersistedSession {
    /// The per-site root (e.g. `~/.tangram-automation/profiles/www.amazon.com`).
    root: PathBuf,
    site: String,
}

impl PersistedSession {
    /// A session rooted at an explicit per-site directory (tests use a tempdir).
    pub fn at(root: impl Into<PathBuf>, site: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            site: site.into(),
        }
    }

    /// The default location: `~/.tangram-automation/profiles/<site>/`. Falls
    /// back to `./.tangram-automation/...` only if `$HOME` is unset (and that
    /// is then caught by [`Self::assert_outside_repo`] if inside a repo).
    pub fn default_for(site: &str) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let safe_site = sanitize_site(site);
        let root = home
            .join(".tangram-automation")
            .join("profiles")
            .join(safe_site);
        Self {
            root,
            site: site.to_string(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn site(&self) -> &str {
        &self.site
    }

    /// The persistent-profile dir (`launchPersistentContext(userDataDir)`).
    pub fn user_data_dir(&self) -> PathBuf {
        self.root.join("user-data")
    }

    /// The portable `storageState` JSON (`context.storageState({path})`).
    pub fn storage_state_path(&self) -> PathBuf {
        self.root.join("storage-state.json")
    }

    /// Refuse to root a session inside a git working tree — a persisted session
    /// is a credential and must NEVER be committable. Walks up from `root`
    /// looking for a `.git`; returns an error if found. (Belt to the
    /// suspenders of `~/.tangram-automation` being outside any repo by default.)
    pub fn assert_outside_repo(&self) -> anyhow::Result<()> {
        let mut dir: Option<&Path> = Some(&self.root);
        while let Some(d) = dir {
            if d.join(".git").exists() {
                anyhow::bail!(
                    "refusing to persist a session under a git working tree ({}): a session \
                     is an auth bearer and must live outside the repo",
                    d.display()
                );
            }
            dir = d.parent();
        }
        Ok(())
    }

    /// Create the per-site tree with owner-only perms (`0o700`).
    pub fn ensure_dir(&self) -> anyhow::Result<()> {
        self.assert_outside_repo()?;
        std::fs::create_dir_all(&self.root)?;
        restrict_dir(&self.root)?;
        Ok(())
    }

    /// Persist a [`StorageState`] to `storage_state_path()` with `0o600`. This
    /// is the save side of the save→load round-trip and the export side of an
    /// interactive solve's handoff.
    pub fn save_storage_state(&self, state: &StorageState) -> anyhow::Result<()> {
        self.ensure_dir()?;
        let path = self.storage_state_path();
        std::fs::write(&path, state.to_json())?;
        restrict_file(&path)?;
        Ok(())
    }

    /// Load a previously-persisted [`StorageState`]. `Ok(None)` when none is
    /// stored yet (a first run → straight to the decision point). The preflight
    /// loads this, then checks [`StorageState::is_expired`].
    pub fn load_storage_state(&self) -> anyhow::Result<Option<StorageState>> {
        let path = self.storage_state_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(StorageState::from_json(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// True if a persistent userDataDir profile exists for this site (the
    /// `launchPersistentContext` shape). The richer, less-portable option.
    pub fn has_user_data(&self) -> bool {
        let d = self.user_data_dir();
        d.is_dir()
            && std::fs::read_dir(&d)
                .map(|mut r| r.next().is_some())
                .unwrap_or(false)
    }

    /// Remove the stored session (invalidation: expired / signed-out / rotate).
    /// The next preflight then falls through to the decision point.
    pub fn invalidate(&self) -> anyhow::Result<()> {
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root)?;
        }
        Ok(())
    }
}

/// How a session may be sealed into 1Password and restored, so the durable
/// bearer can live in the vault rather than only on the box's disk. The seal
/// stores the `storageState` JSON as an `op` item field; restore resolves it
/// back through the broker discipline (never logged, never in LLM context).
///
/// This type names the *reference*; the actual `op item create/edit` and
/// `op read` are wired by the host (it owns the `op` invocation, like
/// [`crate::broker::OpCliResolver`]). Keeping it a reference here means the
/// crate carries no secret value and stays test-pure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedSessionRef {
    /// The `op://vault/item/field` the storageState JSON is sealed under.
    pub op_ref: String,
    /// The site this sealed session authenticates.
    pub site: String,
}

impl SealedSessionRef {
    pub fn new(op_ref: impl Into<String>, site: impl Into<String>) -> Self {
        Self {
            op_ref: op_ref.into(),
            site: site.into(),
        }
    }

    /// A sealed reference must be an `op://` (or other `scheme://`) *reference*,
    /// never a literal JSON blob — the same discipline as a script's
    /// `inject_credential`. Guards against accidentally storing the value here.
    pub fn assert_is_reference(&self) -> Result<(), String> {
        if !self.op_ref.contains("://") {
            return Err(format!(
                "sealed session op_ref {:?} is not a scheme://reference — it must never be a \
                 literal session value",
                self.op_ref
            ));
        }
        Ok(())
    }
}

/// Sanitize a site string into a single safe path component (no `/`, `..`,
/// control bytes) so `default_for` can't be steered to write outside the
/// profiles dir.
fn sanitize_site(site: &str) -> String {
    let mut cleaned = String::with_capacity(site.len());
    let mut prev_dot = false;
    for c in site.chars() {
        let ch = if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
            c
        } else {
            '_'
        };
        // Collapse any run of dots to a single dot so no `..` traversal token
        // can survive in the path component.
        if ch == '.' {
            if prev_dot {
                continue;
            }
            prev_dot = true;
        } else {
            prev_dot = false;
        }
        cleaned.push(ch);
    }
    // Never allow a component that is empty, `.`, or leading/trailing dots.
    match cleaned.trim_matches('.') {
        "" => "site".to_string(),
        trimmed => trimmed.to_string(),
    }
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_cookie(name: &str, expires: f64) -> Cookie {
        Cookie {
            name: name.into(),
            value: "BEARER-VALUE-do-not-leak".into(),
            domain: "www.amazon.com".into(),
            path: "/".into(),
            expires,
        }
    }

    fn signed_in_state() -> StorageState {
        let mut s = StorageState::new("www.amazon.com");
        s.cookies
            .push(fresh_cookie("session-id", (now_secs() + 86_400) as f64));
        s.cookies
            .push(fresh_cookie("at-main", (now_secs() + 7 * 86_400) as f64));
        s
    }

    #[test]
    fn storage_state_round_trips_through_json() {
        let s = signed_in_state();
        let json = s.to_json();
        let back = StorageState::from_json(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn parses_native_playwright_shape_without_envelope() {
        // Playwright's own export has no `site`/`captured_at`; they default.
        let native = r#"{
          "cookies": [
            {"name":"session-id","value":"x","domain":"www.amazon.com","path":"/","expires":9999999999}
          ],
          "origins": []
        }"#;
        let s = StorageState::from_json(native).unwrap();
        assert_eq!(s.cookies.len(), 1);
        assert_eq!(s.site, "");
        assert!(!s.is_expired(now_secs()));
    }

    #[test]
    fn expiry_detection_fresh_vs_expired() {
        let now = now_secs();
        let fresh = signed_in_state();
        assert!(!fresh.is_expired(now), "all cookies in the future ⇒ valid");

        let mut expired = StorageState::new("www.amazon.com");
        expired
            .cookies
            .push(fresh_cookie("at-main", (now - 10) as f64));
        assert!(expired.is_expired(now), "all cookies past ⇒ expired");

        // Mixed: one still valid ⇒ not yet expired (the live cookie carries it).
        let mut mixed = StorageState::new("www.amazon.com");
        mixed.cookies.push(fresh_cookie("old", (now - 10) as f64));
        mixed
            .cookies
            .push(fresh_cookie("live", (now + 1000) as f64));
        assert!(!mixed.is_expired(now));
    }

    #[test]
    fn only_session_cookies_count_as_expired_on_reload() {
        // A session-only artifact (`expires < 0`) can't survive a reload.
        let mut s = StorageState::new("www.amazon.com");
        s.cookies.push(fresh_cookie("ses", -1.0));
        assert!(s.is_expired(now_secs()));
    }

    #[test]
    fn earliest_expiry_ignores_session_cookies() {
        let now = now_secs();
        let mut s = StorageState::new("www.amazon.com");
        s.cookies.push(fresh_cookie("ses", -1.0));
        s.cookies.push(fresh_cookie("a", (now + 500) as f64));
        s.cookies.push(fresh_cookie("b", (now + 1500) as f64));
        assert_eq!(s.earliest_expiry(), Some(now + 500));
    }

    #[test]
    fn save_load_round_trip_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let sess = PersistedSession::at(dir.path().join("www.amazon.com"), "www.amazon.com");
        assert!(
            sess.load_storage_state().unwrap().is_none(),
            "none before save"
        );

        let state = signed_in_state();
        sess.save_storage_state(&state).unwrap();
        let loaded = sess
            .load_storage_state()
            .unwrap()
            .expect("loads after save");
        assert_eq!(loaded, state);
    }

    #[cfg(unix)]
    #[test]
    fn persisted_session_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sess = PersistedSession::at(dir.path().join("site"), "www.amazon.com");
        sess.save_storage_state(&signed_in_state()).unwrap();

        let fmode = std::fs::metadata(sess.storage_state_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(fmode, 0o600, "session file is owner read/write only");

        let dmode = std::fs::metadata(sess.root()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "session dir is owner-only");
    }

    #[test]
    fn refuses_to_persist_inside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate a repo working tree: a `.git` above the chosen root.
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let root = dir.path().join("crates").join("x").join("profiles");
        let sess = PersistedSession::at(&root, "www.amazon.com");
        let err = sess.assert_outside_repo().unwrap_err();
        assert!(format!("{err:#}").contains("outside the repo"));
        // And save() refuses too (never writes a bearer into a repo).
        assert!(sess.save_storage_state(&signed_in_state()).is_err());
    }

    #[test]
    fn default_location_is_under_home_dot_tangram() {
        let sess = PersistedSession::default_for("www.amazon.com");
        let s = sess.root().to_string_lossy();
        assert!(s.contains(".tangram-automation"));
        assert!(s.contains("profiles"));
        assert!(s.ends_with("www.amazon.com"));
    }

    #[test]
    fn default_location_sanitizes_a_path_traversal_site() {
        let sess = PersistedSession::default_for("../../etc/passwd");
        // The site collapses to a single safe component — no traversal.
        let last = sess
            .root()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(!last.contains('/'));
        assert!(!last.contains(".."));
    }

    #[test]
    fn invalidate_removes_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let sess = PersistedSession::at(dir.path().join("site"), "www.amazon.com");
        sess.save_storage_state(&signed_in_state()).unwrap();
        assert!(sess.load_storage_state().unwrap().is_some());
        sess.invalidate().unwrap();
        assert!(
            sess.load_storage_state().unwrap().is_none(),
            "gone after invalidate"
        );
    }

    #[test]
    fn sealed_ref_must_be_a_reference_not_a_value() {
        SealedSessionRef::new("op://Shopper/AmazonSession/storageState", "www.amazon.com")
            .assert_is_reference()
            .unwrap();
        let literal = SealedSessionRef::new("{\"cookies\":[...]}", "www.amazon.com");
        assert!(literal.assert_is_reference().is_err());
    }

    #[test]
    fn summary_never_contains_a_cookie_value() {
        let s = signed_in_state();
        let summary = s.summary();
        assert!(!summary.contains("BEARER-VALUE-do-not-leak"));
        assert!(summary.contains("cookies=2"));
    }
}
