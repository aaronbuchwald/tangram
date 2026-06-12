//! Upfront preflight — "are we already signed in?"
//! (the substrate extension spec #1).
//!
//! Before ANY login attempt, the runner loads the persisted session
//! ([`crate::session`]) into the browser context, navigates to an auth-gated
//! page (e.g. the Amazon account / cart page), and asks one cheap question:
//! **signed in, not signed in, or expired?**
//!
//! - **Signed in** → proceed straight to the task (cart building). No login,
//!   no CAPTCHA, no credential fetch. This is the whole point of durable
//!   session reuse — the common path on every run after the first.
//! - **Not signed in** (a sign-in form is showing) → surface the decision point
//!   ([`crate::decision`]).
//! - **Expired** (we HAD a persisted session but it no longer authenticates)
//!   → invalidate it and fall back to the decision point.
//!
//! The detection itself is mechanical and offline-testable: it runs against a
//! [`crate::script::Snapshot`] (the a11y snapshot the runner already produces),
//! looking for a **signed-in indicator** (the account name/menu) vs a
//! **sign-in form**. No live browser is needed to test the decision logic.

use crate::script::Snapshot;
use crate::session::{PersistedSession, StorageState};

/// What the cheap upfront check concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightOutcome {
    /// A valid session loaded and the auth-gated page shows the signed-in
    /// indicator → go straight to the task. Carries a redacted note for logs.
    SignedIn,
    /// No persisted session at all (a first run) → decision point.
    NoSession,
    /// A persisted session existed but is expired/invalid → decision point.
    /// The caller should [`PersistedSession::invalidate`] before re-auth.
    Expired,
    /// A session loaded but the page still shows a sign-in form (cookies were
    /// present yet not accepted — soft expiry/invalidation) → decision point.
    NotSignedIn,
}

impl PreflightOutcome {
    /// Does this outcome let us skip login entirely?
    pub fn is_signed_in(&self) -> bool {
        matches!(self, PreflightOutcome::SignedIn)
    }

    /// Should the caller route to the decision point ([`crate::decision`])?
    pub fn needs_decision(&self) -> bool {
        !self.is_signed_in()
    }
}

/// Heuristics for reading "signed in" vs "a sign-in form" off a page snapshot.
/// Configurable so the same preflight serves any site (Amazon is the first
/// consumer, not the design center). Matching is on the a11y snapshot's text
/// and locators — credential field VALUES are masked upstream (§8 T2), so a
/// password field shows as a labelled locator with no value here.
#[derive(Debug, Clone)]
pub struct SignedInHeuristics {
    /// Locator roles+names that, if present, mean "signed in" (e.g. the account
    /// menu). Matched against [`Snapshot::has_locator`] by role+name.
    pub signed_in_locators: Vec<(String, String)>,
    /// Text fragments that indicate a signed-in page (e.g. "Hello, " greeting).
    pub signed_in_text: Vec<String>,
    /// Locator roles+names that mean "a sign-in form is showing" (e.g. the
    /// password field, the Sign-In button).
    pub sign_in_form_locators: Vec<(String, String)>,
    /// Text fragments that indicate the sign-in / re-auth page.
    pub sign_in_text: Vec<String>,
}

impl SignedInHeuristics {
    /// The Amazon profile (the first consumer). Account menu / "Hello," ⇒ in;
    /// the password field / "Sign in" heading ⇒ a form.
    pub fn amazon() -> Self {
        Self {
            signed_in_locators: vec![
                ("button".into(), "Account & Lists".into()),
                ("link".into(), "Account & Lists".into()),
                ("link".into(), "Returns & Orders".into()),
            ],
            signed_in_text: vec!["Hello,".into(), "Your Account".into(), "Sign Out".into()],
            sign_in_form_locators: vec![
                ("textbox".into(), "Password".into()),
                ("textbox".into(), "Email or mobile number".into()),
                ("button".into(), "Sign in".into()),
            ],
            sign_in_text: vec![
                "Sign in".into(),
                "Sign-In".into(),
                "Enter your password".into(),
            ],
        }
    }

    fn has_signed_in_indicator(&self, snap: &Snapshot) -> bool {
        let loc_hit = self.signed_in_locators.iter().any(|(role, name)| {
            snap.locators
                .iter()
                .any(|l| &l.role == role && &l.name == name)
        });
        let text_hit = self.signed_in_text.iter().any(|t| snap.text.contains(t));
        loc_hit || text_hit
    }

    fn has_sign_in_form(&self, snap: &Snapshot) -> bool {
        let loc_hit = self.sign_in_form_locators.iter().any(|(role, name)| {
            snap.locators
                .iter()
                .any(|l| &l.role == role && &l.name == name)
        });
        let text_hit = self.sign_in_text.iter().any(|t| snap.text.contains(t));
        loc_hit || text_hit
    }

    /// Classify a single observed snapshot. Signed-in indicator wins over a
    /// stray "Sign in" link (a signed-in Amazon page can still carry the word).
    pub fn classify(&self, snap: &Snapshot) -> PageAuthState {
        if self.has_signed_in_indicator(snap) {
            PageAuthState::SignedIn
        } else if self.has_sign_in_form(snap) {
            PageAuthState::SignInForm
        } else {
            PageAuthState::Unknown
        }
    }
}

/// What a single page snapshot looks like, auth-wise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAuthState {
    SignedIn,
    SignInForm,
    /// Neither indicator present (an interstitial, a CAPTCHA, a blank load).
    Unknown,
}

/// Run the preflight decision over the *loaded session* and the *observed
/// auth-gated page snapshot*. This is the pure core the runner wraps with the
/// actual "load session → navigate → snapshot" browser I/O.
///
/// - `loaded`: the persisted session the runner loaded (`None` ⇒ first run).
/// - `now`: current unix seconds (for the expiry check).
/// - `page`: the snapshot of the auth-gated page after loading the session.
/// - `h`: the site's signed-in heuristics.
///
/// Precedence:
/// 1. The page shows the signed-in indicator ⇒ `SignedIn` (regardless of what
///    the cookie clock says — the live page is authoritative).
/// 2. No session loaded ⇒ `NoSession`.
/// 3. Session loaded but expired by the cookie clock ⇒ `Expired`.
/// 4. Session loaded, not expired, but the page shows a sign-in form / no
///    indicator ⇒ `NotSignedIn` (soft invalidation — server rejected it).
pub fn preflight(
    loaded: Option<&StorageState>,
    now: u64,
    page: &Snapshot,
    h: &SignedInHeuristics,
) -> PreflightOutcome {
    if h.classify(page) == PageAuthState::SignedIn {
        return PreflightOutcome::SignedIn;
    }
    match loaded {
        None => PreflightOutcome::NoSession,
        Some(state) if state.is_expired(now) => PreflightOutcome::Expired,
        Some(_) => PreflightOutcome::NotSignedIn,
    }
}

/// Convenience: run the preflight directly against a [`PersistedSession`],
/// loading the stored state and applying [`preflight`]. The caller still
/// supplies the observed page snapshot (the browser I/O lives in the runner).
/// On `Expired`, the caller is expected to call [`PersistedSession::invalidate`].
pub fn preflight_with_session(
    session: &PersistedSession,
    now: u64,
    page: &Snapshot,
    h: &SignedInHeuristics,
) -> anyhow::Result<PreflightOutcome> {
    let loaded = session.load_storage_state()?;
    Ok(preflight(loaded.as_ref(), now, page, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::Locator;
    use crate::session::Cookie;

    fn signed_in_page() -> Snapshot {
        Snapshot {
            url_host: Some("www.amazon.com".into()),
            text: "Hello, Aaron  Returns & Orders".into(),
            locators: vec![Locator::new("link", "Account & Lists")],
        }
    }

    fn sign_in_form_page() -> Snapshot {
        Snapshot {
            url_host: Some("www.amazon.com".into()),
            text: "Sign in  Enter your password".into(),
            locators: vec![
                Locator::new("textbox", "Password"),
                Locator::new("button", "Sign in"),
            ],
        }
    }

    fn fresh_state() -> StorageState {
        let mut s = StorageState::new("www.amazon.com");
        s.cookies.push(Cookie {
            name: "at-main".into(),
            value: "v".into(),
            domain: "www.amazon.com".into(),
            path: "/".into(),
            expires: 9_999_999_999.0,
        });
        s
    }

    fn expired_state(now: u64) -> StorageState {
        let mut s = StorageState::new("www.amazon.com");
        s.cookies.push(Cookie {
            name: "at-main".into(),
            value: "v".into(),
            domain: "www.amazon.com".into(),
            path: "/".into(),
            expires: (now - 10) as f64,
        });
        s
    }

    #[test]
    fn classify_signed_in_vs_form() {
        let h = SignedInHeuristics::amazon();
        assert_eq!(h.classify(&signed_in_page()), PageAuthState::SignedIn);
        assert_eq!(h.classify(&sign_in_form_page()), PageAuthState::SignInForm);
        assert_eq!(h.classify(&Snapshot::default()), PageAuthState::Unknown);
    }

    #[test]
    fn signed_in_indicator_wins_over_stray_signin_text() {
        // A signed-in Amazon page still carries "Sign Out" / maybe "Sign in"
        // somewhere; the account menu must win.
        let h = SignedInHeuristics::amazon();
        let mut page = signed_in_page();
        page.text.push_str("  Sign in to your other account");
        assert_eq!(h.classify(&page), PageAuthState::SignedIn);
    }

    #[test]
    fn preflight_signed_in_skips_everything() {
        let h = SignedInHeuristics::amazon();
        // Even with NO session loaded, a live signed-in page (e.g. profile
        // already carried it) means skip login.
        let out = preflight(Some(&fresh_state()), 1_000, &signed_in_page(), &h);
        assert_eq!(out, PreflightOutcome::SignedIn);
        assert!(out.is_signed_in());
        assert!(!out.needs_decision());
    }

    #[test]
    fn preflight_no_session_routes_to_decision() {
        let h = SignedInHeuristics::amazon();
        let out = preflight(None, 1_000, &sign_in_form_page(), &h);
        assert_eq!(out, PreflightOutcome::NoSession);
        assert!(out.needs_decision());
    }

    #[test]
    fn preflight_expired_session_routes_to_decision() {
        let h = SignedInHeuristics::amazon();
        let now = 2_000_000_000;
        // Cookies expired AND the page now shows the form → Expired.
        let out = preflight(Some(&expired_state(now)), now, &sign_in_form_page(), &h);
        assert_eq!(out, PreflightOutcome::Expired);
        assert!(out.needs_decision());
    }

    #[test]
    fn preflight_soft_invalidation_not_signed_in() {
        let h = SignedInHeuristics::amazon();
        // Cookies look unexpired by the clock, but the server rejected them and
        // the page shows the form → NotSignedIn (server is authoritative).
        let out = preflight(Some(&fresh_state()), 1_000, &sign_in_form_page(), &h);
        assert_eq!(out, PreflightOutcome::NotSignedIn);
        assert!(out.needs_decision());
    }

    #[test]
    fn preflight_with_session_loads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let sess = PersistedSession::at(dir.path().join("site"), "www.amazon.com");
        sess.save_storage_state(&fresh_state()).unwrap();
        let h = SignedInHeuristics::amazon();

        // Signed-in page ⇒ SignedIn even though a session is on disk.
        let out = preflight_with_session(&sess, 1_000, &signed_in_page(), &h).unwrap();
        assert_eq!(out, PreflightOutcome::SignedIn);

        // Form page + fresh cookies ⇒ NotSignedIn.
        let out = preflight_with_session(&sess, 1_000, &sign_in_form_page(), &h).unwrap();
        assert_eq!(out, PreflightOutcome::NotSignedIn);
    }

    #[test]
    fn preflight_with_no_stored_session_is_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let sess = PersistedSession::at(dir.path().join("none"), "www.amazon.com");
        let h = SignedInHeuristics::amazon();
        let out = preflight_with_session(&sess, 1_000, &sign_in_form_page(), &h).unwrap();
        assert_eq!(out, PreflightOutcome::NoSession);
    }
}
