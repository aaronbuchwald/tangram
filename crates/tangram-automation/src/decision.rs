//! The runtime DECISION POINT — "not signed in: how do we authenticate?"
//! (the substrate extension spec #2 / #3).
//!
//! When the preflight ([`crate::preflight`]) reports we are NOT signed in, the
//! runner does not silently start a login. It surfaces a structured choice to
//! the human through the same request/approval channel as the rest of the
//! project ([`crate::request`]) — the human-in-the-loop posture is invariant.
//!
//! Two paths converge on a verified authenticated session that is then
//! persisted ([`crate::session`]) so the NEXT run hits the preflight and skips
//! everything:
//!
//! - **(a) Interactive headed solve** ([`SignInPath::Interactive`]): a *headed*
//!   browser with the persistent profile; credentials filled from `op`
//!   in-process (the [`crate::broker`] discipline — never in LLM context); then
//!   PAUSE for the human to solve the CAPTCHA / 2FA; detect auth success;
//!   persist the session. NOTE: this box is headless, so the interactive solve
//!   realistically runs on the OWNER'S machine (or via a VNC bridge) and the
//!   resulting `storageState` artifact is carried back here — see the doc.
//!
//! - **(b) LLM-assisted CAPTCHA solve** ([`SignInPath::LlmAssisted`]): the
//!   multimodal Anthropic model reads the challenge and proposes a solution,
//!   under a HARD bounded attempt/token budget ([`LlmCaptchaBudget`]) with
//!   automatic fallback to path (a) on exhaustion. This can burn significant
//!   tokens and may fail (Amazon image puzzles are hard) — the budget is a
//!   parameter and the path fails safe.

use serde::{Deserialize, Serialize};

use crate::preflight::PreflightOutcome;
use crate::request::AuthorizedAutomation;

/// Which sign-in path the human chose (or policy defaulted to).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInPath {
    /// (a) One-time human-assisted headed solve. Realistically on the owner's
    /// machine for this headless box; portable session carried back.
    Interactive,
    /// (b) Bounded LLM-assisted CAPTCHA solve, auto-falling back to (a).
    LlmAssisted,
}

/// The structured "not signed in — choose how to authenticate" prompt the
/// runner surfaces through [`crate::request`]. It states the two paths, the
/// credential/CAPTCHA implications, and the LLM-path budget so the human's
/// approval is informed. Carries NO secret — only references and counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistanceRequest {
    /// The app the underlying automation is for (echoed from the authorized
    /// automation, for the operator's per-app gate).
    pub app: String,
    /// The site we need to authenticate to (e.g. `"www.amazon.com"`).
    pub site: String,
    /// Why we're asking now — the preflight outcome in human terms.
    pub reason: PreflightReason,
    /// The credential references the interactive/headed path would use (from
    /// the authorized automation's grant). References, NEVER values.
    pub credential_refs: Vec<String>,
    /// The budget the LLM-assisted path would run under (so the human sees the
    /// token implication before choosing it).
    pub llm_budget: LlmCaptchaBudget,
    /// The paths offered. Always both, unless policy disabled one.
    pub offered: Vec<SignInPath>,
}

/// A serializable, human-readable restatement of why the decision point fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightReason {
    /// First run / no stored session.
    NoSession,
    /// A stored session had expired.
    Expired,
    /// A stored session was rejected by the server (soft invalidation).
    NotSignedIn,
}

impl PreflightReason {
    fn from_outcome(o: &PreflightOutcome) -> Option<Self> {
        match o {
            PreflightOutcome::SignedIn => None,
            PreflightOutcome::NoSession => Some(Self::NoSession),
            PreflightOutcome::Expired => Some(Self::Expired),
            PreflightOutcome::NotSignedIn => Some(Self::NotSignedIn),
        }
    }
}

impl AssistanceRequest {
    /// Build the prompt from a preflight outcome + the authorized automation.
    /// Returns `None` when the outcome is `SignedIn` (no decision needed).
    pub fn from_preflight(
        outcome: &PreflightOutcome,
        site: impl Into<String>,
        auth: &AuthorizedAutomation,
        llm_budget: LlmCaptchaBudget,
    ) -> Option<Self> {
        let reason = PreflightReason::from_outcome(outcome)?;
        Some(Self {
            app: auth.app.clone(),
            site: site.into(),
            reason,
            credential_refs: auth.credential_refs.clone(),
            llm_budget,
            offered: vec![SignInPath::Interactive, SignInPath::LlmAssisted],
        })
    }

    /// A redacted, human-facing prompt string for logs / the approval UI. Names
    /// references and the budget, never a value.
    pub fn prompt(&self) -> String {
        format!(
            "Not signed in to {site} ({reason:?}). Choose how to authenticate:\n\
             (a) interactive — one-time headed solve (human solves CAPTCHA/2FA); uses {n} \
             credential reference(s); on this headless box, run on your machine and bring back \
             the session artifact.\n\
             (b) llm-assisted — bounded multimodal CAPTCHA solve (~{attempts} attempts, \
             ≤{tokens} tokens; may burn significant tokens and may FAIL — falls back to (a)).",
            site = self.site,
            reason = self.reason,
            n = self.credential_refs.len(),
            attempts = self.llm_budget.max_attempts,
            tokens = self.llm_budget.max_tokens,
        )
    }
}

/// The HARD bound on the LLM-assisted CAPTCHA path. Both limits are upper
/// bounds; the path fails over to interactive the instant EITHER is hit. Make
/// it a parameter so the operator/owner sets the token appetite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmCaptchaBudget {
    /// Maximum solve attempts before falling back to interactive.
    pub max_attempts: u32,
    /// Maximum total tokens (prompt + completion across all attempts) before
    /// falling back. A hard ceiling — never exceeded.
    pub max_tokens: u64,
}

impl LlmCaptchaBudget {
    /// A deliberately small default — Amazon image puzzles are hard and the
    /// path is expected to fail more often than not, so the default appetite is
    /// modest. Operators can raise it.
    pub fn modest() -> Self {
        Self {
            max_attempts: 3,
            max_tokens: 40_000,
        }
    }

    /// A disabled budget — the LLM path is not offered / always falls back.
    pub fn disabled() -> Self {
        Self {
            max_attempts: 0,
            max_tokens: 0,
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.max_attempts == 0 || self.max_tokens == 0
    }
}

impl Default for LlmCaptchaBudget {
    fn default() -> Self {
        Self::modest()
    }
}

/// Tracks consumption against an [`LlmCaptchaBudget`] across the attempt loop.
/// The runner calls [`Self::charge`] after each attempt; once the budget is
/// exhausted it [`Self::is_exhausted`] and the runner falls back to interactive.
#[derive(Debug, Clone)]
pub struct LlmBudgetTracker {
    budget: LlmCaptchaBudget,
    attempts_used: u32,
    tokens_used: u64,
}

impl LlmBudgetTracker {
    pub fn new(budget: LlmCaptchaBudget) -> Self {
        Self {
            budget,
            attempts_used: 0,
            tokens_used: 0,
        }
    }

    pub fn attempts_used(&self) -> u32 {
        self.attempts_used
    }
    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    /// True if the budget has no remaining attempts OR no remaining token
    /// headroom — i.e. we must fall back to interactive now.
    pub fn is_exhausted(&self) -> bool {
        self.budget.is_disabled()
            || self.attempts_used >= self.budget.max_attempts
            || self.tokens_used >= self.budget.max_tokens
    }

    /// May we START another attempt costing roughly `est_tokens`? False if it
    /// would exceed either bound — the runner then stops BEFORE spending, so
    /// the token ceiling is never overshot.
    pub fn can_attempt(&self, est_tokens: u64) -> bool {
        !self.is_disabled_or_done()
            && self.tokens_used.saturating_add(est_tokens) <= self.budget.max_tokens
    }

    fn is_disabled_or_done(&self) -> bool {
        self.budget.is_disabled() || self.attempts_used >= self.budget.max_attempts
    }

    /// Record one finished attempt and the tokens it actually consumed.
    pub fn charge(&mut self, tokens: u64) {
        self.attempts_used = self.attempts_used.saturating_add(1);
        self.tokens_used = self.tokens_used.saturating_add(tokens);
    }
}

/// The outcome of running the LLM-assisted path's bounded loop. Pure routing
/// type so the loop is offline-testable without a live model or a real puzzle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmSolveOutcome {
    /// The model solved the challenge within budget — proceed to verify auth.
    Solved { attempts: u32, tokens: u64 },
    /// Budget exhausted (attempts or tokens) without a solve → fall back to the
    /// interactive path. Fail safe, never autonomous retry forever.
    ExhaustedFallBackToInteractive { attempts: u32, tokens: u64 },
}

impl LlmSolveOutcome {
    pub fn should_fall_back(&self) -> bool {
        matches!(self, LlmSolveOutcome::ExhaustedFallBackToInteractive { .. })
    }
}

/// Drive the bounded LLM-assisted CAPTCHA loop. `attempt` is the per-iteration
/// solve: given the attempt index it returns `(solved, tokens_spent_estimate)`.
/// In production it calls the multimodal model with the challenge screenshot;
/// in tests it's a closure. The loop NEVER exceeds the budget — it checks
/// [`LlmBudgetTracker::can_attempt`] before each call and falls back the moment
/// the budget is spent.
///
/// `est_tokens` is the per-attempt token estimate used to refuse an attempt
/// that would overshoot the ceiling.
pub fn run_llm_captcha_loop<F>(
    budget: LlmCaptchaBudget,
    est_tokens: u64,
    mut attempt: F,
) -> LlmSolveOutcome
where
    F: FnMut(u32) -> (bool, u64),
{
    let mut tracker = LlmBudgetTracker::new(budget);
    while tracker.can_attempt(est_tokens) {
        let idx = tracker.attempts_used();
        let (solved, spent) = attempt(idx);
        tracker.charge(spent);
        if solved {
            return LlmSolveOutcome::Solved {
                attempts: tracker.attempts_used(),
                tokens: tracker.tokens_used(),
            };
        }
    }
    LlmSolveOutcome::ExhaustedFallBackToInteractive {
        attempts: tracker.attempts_used(),
        tokens: tracker.tokens_used(),
    }
}

/// The full decision routing given the human's choice and the LLM-path result.
/// This is the converge logic of spec #3: both paths must end at a verified,
/// persisted session before the task proceeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInResolution {
    /// Run the interactive headed solve (path a). For this headless box, the
    /// runner emits the handoff instruction (do it on the owner's machine,
    /// bring back the `storageState`).
    RunInteractive,
    /// The LLM path solved it within budget → verify + persist.
    LlmSolved,
    /// The LLM path was chosen but exhausted its budget → fall back to (a).
    LlmExhaustedFellBack,
}

/// Route a chosen [`SignInPath`] (with the LLM result, if that path was taken)
/// to a [`SignInResolution`]. Choosing the LLM path with a disabled/exhausted
/// budget always falls back to interactive — fail safe.
pub fn route(path: SignInPath, llm: Option<&LlmSolveOutcome>) -> SignInResolution {
    match path {
        SignInPath::Interactive => SignInResolution::RunInteractive,
        SignInPath::LlmAssisted => match llm {
            Some(LlmSolveOutcome::Solved { .. }) => SignInResolution::LlmSolved,
            // None (LLM path chosen but not run / disabled) or exhausted ⇒ fall back.
            _ => SignInResolution::LlmExhaustedFellBack,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> AuthorizedAutomation {
        AuthorizedAutomation {
            app: "grocery".into(),
            template_id: "amazon-grocery-cart".into(),
            params: vec![],
            domains: vec!["www.amazon.com".into()],
            credential_refs: vec![
                "op://Shopper/Amazon/username".into(),
                "op://Shopper/Amazon/password".into(),
            ],
        }
    }

    #[test]
    fn no_assistance_request_when_signed_in() {
        let req = AssistanceRequest::from_preflight(
            &PreflightOutcome::SignedIn,
            "www.amazon.com",
            &auth(),
            LlmCaptchaBudget::modest(),
        );
        assert!(req.is_none(), "signed in ⇒ no decision surfaced");
    }

    #[test]
    fn assistance_request_offers_both_paths_and_names_refs() {
        let req = AssistanceRequest::from_preflight(
            &PreflightOutcome::NoSession,
            "www.amazon.com",
            &auth(),
            LlmCaptchaBudget::modest(),
        )
        .unwrap();
        assert_eq!(
            req.offered,
            vec![SignInPath::Interactive, SignInPath::LlmAssisted]
        );
        assert_eq!(req.credential_refs.len(), 2);
        assert_eq!(req.reason, PreflightReason::NoSession);
        // The prompt names references + the budget but NO secret value.
        let p = req.prompt();
        assert!(p.contains("interactive"));
        assert!(p.contains("llm-assisted"));
        assert!(p.contains("falls back"));
        assert!(
            !p.contains("op://Shopper/Amazon/password"),
            "prompt counts refs, not lists values"
        );
    }

    #[test]
    fn expired_maps_to_expired_reason() {
        let req = AssistanceRequest::from_preflight(
            &PreflightOutcome::Expired,
            "www.amazon.com",
            &auth(),
            LlmCaptchaBudget::modest(),
        )
        .unwrap();
        assert_eq!(req.reason, PreflightReason::Expired);
    }

    // ── budget bound + fallback ──

    #[test]
    fn llm_loop_solves_within_budget() {
        // Solve on the 2nd attempt; budget allows 3.
        let out = run_llm_captcha_loop(LlmCaptchaBudget::modest(), 10_000, |i| (i == 1, 10_000));
        assert_eq!(
            out,
            LlmSolveOutcome::Solved {
                attempts: 2,
                tokens: 20_000
            }
        );
        assert!(!out.should_fall_back());
    }

    #[test]
    fn llm_loop_exhausts_attempts_then_falls_back() {
        // Never solves; the attempt bound (3) is hit → fall back.
        let out = run_llm_captcha_loop(LlmCaptchaBudget::modest(), 10_000, |_| (false, 10_000));
        match out {
            LlmSolveOutcome::ExhaustedFallBackToInteractive { attempts, tokens } => {
                assert_eq!(attempts, 3);
                assert_eq!(tokens, 30_000);
            }
            other => panic!("expected fallback, got {other:?}"),
        }
        assert!(out.should_fall_back());
    }

    #[test]
    fn llm_loop_never_overshoots_the_token_ceiling() {
        // Budget 40k tokens; each attempt estimated at 25k → only one attempt
        // can START (2nd would project to 50k > 40k), so it stops BEFORE
        // overspending and falls back.
        let mut starts = 0;
        let out = run_llm_captcha_loop(
            LlmCaptchaBudget {
                max_attempts: 10,
                max_tokens: 40_000,
            },
            25_000,
            |_| {
                starts += 1;
                (false, 25_000)
            },
        );
        assert_eq!(starts, 1, "second attempt refused before spending");
        assert!(out.should_fall_back());
        if let LlmSolveOutcome::ExhaustedFallBackToInteractive { tokens, .. } = out {
            assert!(tokens <= 40_000, "ceiling never exceeded");
        }
    }

    #[test]
    fn disabled_budget_runs_no_attempts_and_falls_back() {
        let mut starts = 0;
        let out = run_llm_captcha_loop(LlmCaptchaBudget::disabled(), 1, |_| {
            starts += 1;
            (true, 1)
        });
        assert_eq!(starts, 0, "disabled ⇒ never calls the model");
        assert!(out.should_fall_back());
    }

    // ── routing ──

    #[test]
    fn route_interactive_runs_interactive() {
        assert_eq!(
            route(SignInPath::Interactive, None),
            SignInResolution::RunInteractive
        );
    }

    #[test]
    fn route_llm_solved_proceeds() {
        let llm = LlmSolveOutcome::Solved {
            attempts: 1,
            tokens: 10,
        };
        assert_eq!(
            route(SignInPath::LlmAssisted, Some(&llm)),
            SignInResolution::LlmSolved
        );
    }

    #[test]
    fn route_llm_exhausted_falls_back_to_interactive() {
        let llm = LlmSolveOutcome::ExhaustedFallBackToInteractive {
            attempts: 3,
            tokens: 30_000,
        };
        assert_eq!(
            route(SignInPath::LlmAssisted, Some(&llm)),
            SignInResolution::LlmExhaustedFellBack
        );
    }

    #[test]
    fn route_llm_chosen_but_not_run_fails_safe_to_fallback() {
        // Choosing LLM with no result (disabled / not attempted) must not
        // silently proceed — it falls back to interactive.
        assert_eq!(
            route(SignInPath::LlmAssisted, None),
            SignInResolution::LlmExhaustedFellBack
        );
    }
}
