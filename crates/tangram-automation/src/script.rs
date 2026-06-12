//! Primitive D — record → script → LLM-fallback replay
//! (`task-automation-browser.md` §7).
//!
//! A run produces an [`AutomationScript`]: an ordered list of small,
//! declarative [`Step`]s with **semantic** locators (a11y role + accessible
//! name + the stable `ref`), each carrying an `expect` post-condition the
//! recording observed. Replay executes the steps WITHOUT the LLM and checks
//! each `expect`; a **divergence** consults the LLM, whose chosen
//! [`Disposition`] the runner *validates* — the LLM may never override a
//! [`Step::StopGate`] nor steer off the session's domain allowlist (§7.4,
//! §8). The credential never enters the script: a login is an
//! [`Step::InjectCredential`] holding the `op://` *reference*, never a value.

use serde::{Deserialize, Serialize};

/// A semantic locator: prefer role+name (resilient to a moved DOM node), keep
/// the recorded `ref` as a hint. CSS/XPath are deliberately absent — the
/// parser-differential / brittleness discipline applies to locators too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locator {
    /// Accessibility role (e.g. `"button"`, `"textbox"`, `"link"`).
    pub role: String,
    /// Accessible name (the visible/announced label).
    pub name: String,
    /// The recording's `ref` handle (a hint; role+name is authoritative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
}

impl Locator {
    pub fn new(role: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            name: name.into(),
            r#ref: None,
        }
    }

    pub fn with_ref(mut self, r: impl Into<String>) -> Self {
        self.r#ref = Some(r.into());
        self
    }
}

/// A post-condition the recording observed *after* a step; replay checks it
/// against the live a11y snapshot to detect divergence. Deliberately small.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Expect {
    /// The (canonicalized) host the page should be on after this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_host: Option<String>,
    /// A substring that should be present in the page's accessible text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_present: Option<String>,
    /// A locator that should be present after the step (e.g. the cart count
    /// badge after add-to-cart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator_present: Option<Locator>,
}

impl Expect {
    /// Does this post-condition hold against an observed snapshot?
    pub fn holds(&self, snap: &Snapshot) -> bool {
        if let Some(host) = &self.url_host
            && snap.url_host.as_deref() != Some(host.as_str())
        {
            return false;
        }
        if let Some(text) = &self.text_present
            && !snap.text.contains(text.as_str())
        {
            return false;
        }
        if let Some(loc) = &self.locator_present
            && !snap.has_locator(loc)
        {
            return false;
        }
        true
    }
}

/// One step in a script. `inject_credential` carries only the `op://`
/// reference; `stop_gate` is a hard barrier replay never auto-passes (§8 T4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    Navigate {
        url: String,
        #[serde(default)]
        expect: Expect,
    },
    Click {
        target: Locator,
        #[serde(default)]
        expect: Expect,
    },
    /// Type literal/parameterized text into a field (NEVER a secret — secrets
    /// go through `InjectCredential`).
    Type {
        target: Locator,
        text: String,
        #[serde(default)]
        expect: Expect,
    },
    /// Resolve `secret_ref` host-side and `fill` it into `target` — the value
    /// never appears here, in a log, or in an LLM snapshot (§6.2).
    InjectCredential {
        /// An `op://…` reference, NEVER a value.
        secret_ref: String,
        target: Locator,
    },
    WaitFor {
        #[serde(default)]
        expect: Expect,
    },
    Assert {
        #[serde(default)]
        expect: Expect,
    },
    /// A hard stop before an irreversible action; replay halts and requires
    /// explicit human approval to proceed past it. The LLM can NEVER skip it.
    StopGate { reason: String },
}

impl Step {
    /// The post-condition for this step (the empty `Expect` for steps that
    /// don't carry one). Replay checks this after executing the step.
    pub fn expect(&self) -> Expect {
        match self {
            Step::Navigate { expect, .. }
            | Step::Click { expect, .. }
            | Step::Type { expect, .. }
            | Step::WaitFor { expect }
            | Step::Assert { expect } => expect.clone(),
            Step::InjectCredential { .. } | Step::StopGate { .. } => Expect::default(),
        }
    }

    pub fn is_stop_gate(&self) -> bool {
        matches!(self, Step::StopGate { .. })
    }
}

/// The reviewable, replayable artifact (§7.1). Content-addressable via
/// [`AutomationScript::digest`] so a trusted script can't be silently swapped
/// (§8 T7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationScript {
    pub template_id: String,
    pub version: u32,
    /// The allowlist the recording used — the replay gate is built from this
    /// (intersected with the operator ceiling at request time, §4.3).
    pub domains: Vec<String>,
    pub steps: Vec<Step>,
}

impl AutomationScript {
    pub fn new(template_id: impl Into<String>, domains: Vec<String>) -> Self {
        Self {
            template_id: template_id.into(),
            version: 1,
            domains,
            steps: Vec::new(),
        }
    }

    pub fn push(&mut self, step: Step) -> &mut Self {
        self.steps.push(step);
        self
    }

    /// Round-trip through JSON (the on-disk artifact form).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("script serializes")
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// A stable content digest (the script's identity for the swap-resistance
    /// property, §8 T7). Hash of the canonical JSON.
    pub fn digest(&self) -> String {
        // A tiny FNV-1a over the canonical JSON keeps the crate dep-free of a
        // hash lib; this is an integrity *identifier*, not a security MAC.
        let json = serde_json::to_vec(self).expect("script serializes");
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in json {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("fnv1a:{hash:016x}")
    }

    /// Assert no step leaks a secret value: the only credential-bearing step
    /// is `InjectCredential`, and it must carry an `op://` (or other
    /// `scheme://`) *reference*, never a bare value. Used by the record-time
    /// review gate.
    pub fn assert_no_secret_values(&self) -> Result<(), String> {
        for (i, step) in self.steps.iter().enumerate() {
            if let Step::InjectCredential { secret_ref, .. } = step
                && !secret_ref.contains("://")
            {
                return Err(format!(
                    "step {i}: inject_credential secret_ref {secret_ref:?} is not a \
                     scheme://reference — it must never be a literal value"
                ));
            }
        }
        Ok(())
    }
}

/// An accessibility snapshot the runner observes (and replay checks `expect`
/// against). Credential fields are MASKED here — a snapshot never carries a
/// secret into LLM context (§8 T2). In production this is built from the
/// browser's a11y tree; tests construct it directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub url_host: Option<String>,
    /// Concatenated accessible text of the page (secret field values masked).
    #[serde(default)]
    pub text: String,
    /// The locators present on the page (role+name; masked-value fields keep
    /// their label, never their value).
    #[serde(default)]
    pub locators: Vec<Locator>,
}

impl Snapshot {
    pub fn has_locator(&self, want: &Locator) -> bool {
        self.locators
            .iter()
            .any(|l| l.role == want.role && l.name == want.name)
    }
}

// ── replay + divergence detection (AC4) ──────────────────────────────────────

/// What replay reports for one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step ran and its `expect` post-condition held.
    Ok,
    /// The step's `expect` did not hold, or the target wasn't found — a
    /// divergence the runner must resolve (LLM-fallback or abort).
    Diverged(String),
    /// A `stop_gate` — replay halts here and requires human approval.
    StoppedAtGate(String),
}

/// The disposition the LLM returns on a divergence; the runner VALIDATES it
/// before acting (§7.4). Note: the LLM never produces `StoppedAtGate` — only
/// the runner does, and it is never overridable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
pub enum Disposition {
    /// The post-condition was a false negative; proceed.
    Continue,
    /// Patch this step (the control moved) and resume. The patch is a
    /// reviewable amendment.
    Recover { patched: Step },
    /// Finish the remainder free-form under LLM guidance (re-enter record).
    HandOff,
    /// The page is in an unexpected/dangerous state; stop and report.
    Abort { reason: String },
}

/// The result of the runner validating an LLM [`Disposition`] against the
/// hard rules (§7.4 "Hard rule"): a stop-gate is never skippable, and a
/// recovery patch may not steer off the domain allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedDisposition {
    Continue,
    Recover {
        patched: Step,
    },
    HandOff,
    Abort {
        reason: String,
    },
    /// The LLM's choice was rejected by a hard rule; the runner overrides it
    /// (always toward the safe side). Carries why, for the security log.
    RejectedToAbort {
        reason: String,
    },
}

/// Validate an LLM disposition against the deterministic guardrails. This is
/// the wrapper the prompt requires: "the runner validates the choice — never
/// override a stop-gate or go off-allowlist."
///
/// - `at_stop_gate`: the current step is a `StopGate`. NOTHING the LLM says
///   may pass it — even `Continue`/`Recover` is rejected to abort.
/// - `allowed_hosts`: the session allowlist. A `Recover` whose patched step is
///   a `Navigate` to an off-list host is rejected to abort.
pub fn validate_disposition(
    disp: Disposition,
    at_stop_gate: bool,
    allowed_hosts: &[String],
) -> ValidatedDisposition {
    if at_stop_gate {
        // A stop-gate is a hard barrier enforced by host code AROUND the
        // LLM's choice — never by asking the model nicely.
        return ValidatedDisposition::RejectedToAbort {
            reason: "LLM may not pass a stop_gate; human approval required".into(),
        };
    }
    match disp {
        Disposition::Continue => ValidatedDisposition::Continue,
        Disposition::HandOff => ValidatedDisposition::HandOff,
        Disposition::Abort { reason } => ValidatedDisposition::Abort { reason },
        Disposition::Recover { patched } => {
            // A recovery patch may never introduce a stop-gate skip or a
            // navigation off the allowlist.
            if patched.is_stop_gate() {
                return ValidatedDisposition::RejectedToAbort {
                    reason: "recovery patch may not be a stop_gate".into(),
                };
            }
            if let Step::Navigate { url, .. } = &patched {
                let off_list = match crate::egress::canonicalize_url(url) {
                    Some((host, _)) => !allowed_hosts.iter().any(|h| h == &host),
                    None => true,
                };
                if off_list {
                    return ValidatedDisposition::RejectedToAbort {
                        reason: format!("recovery navigate {url:?} is off the domain allowlist"),
                    };
                }
            }
            ValidatedDisposition::Recover { patched }
        }
    }
}

/// Replay a script against a sequence of observed snapshots (one per step),
/// without an LLM. Returns the per-step outcomes, halting at the first
/// `stop_gate` or unrecovered divergence. This is the deterministic core AC4
/// tests; AC5 layers the LLM-fallback over a `Diverged` outcome.
///
/// `observe` is the runner's snapshot source; in tests it's a fixture.
pub fn replay<F>(script: &AutomationScript, mut observe: F) -> Vec<StepOutcome>
where
    F: FnMut(usize, &Step) -> Snapshot,
{
    let mut outcomes = Vec::with_capacity(script.steps.len());
    for (i, step) in script.steps.iter().enumerate() {
        if let Step::StopGate { reason } = step {
            outcomes.push(StepOutcome::StoppedAtGate(reason.clone()));
            break;
        }
        let snap = observe(i, step);
        let expect = step.expect();
        if expect.holds(&snap) {
            outcomes.push(StepOutcome::Ok);
        } else {
            outcomes.push(StepOutcome::Diverged(format!(
                "step {i} expect not satisfied"
            )));
            break;
        }
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cart_script() -> AutomationScript {
        let mut s = AutomationScript::new("amazon-grocery-cart", vec!["www.amazon.com".into()]);
        s.push(Step::Navigate {
            url: "https://www.amazon.com/".into(),
            expect: Expect {
                url_host: Some("www.amazon.com".into()),
                ..Default::default()
            },
        })
        .push(Step::InjectCredential {
            secret_ref: "op://Private/Amazon/password".into(),
            target: Locator::new("textbox", "Password"),
        })
        .push(Step::Type {
            target: Locator::new("searchbox", "Search Amazon"),
            text: "milk".into(),
            expect: Expect::default(),
        })
        .push(Step::Click {
            target: Locator::new("button", "Add to Cart"),
            expect: Expect {
                text_present: Some("Added to Cart".into()),
                ..Default::default()
            },
        })
        .push(Step::StopGate {
            reason: "cart built — placing the order requires explicit owner approval".into(),
        });
        s
    }

    #[test]
    fn script_round_trips_through_json() {
        let s = cart_script();
        let json = s.to_json();
        let back = AutomationScript::from_json(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn no_secret_value_in_artifact() {
        let s = cart_script();
        // The credential step holds only the op:// reference.
        let json = s.to_json();
        assert!(json.contains("op://Private/Amazon/password"));
        assert!(!json.contains("hunter2"));
        s.assert_no_secret_values().unwrap();
    }

    #[test]
    fn assert_no_secret_values_catches_a_literal() {
        let mut s = AutomationScript::new("x", vec![]);
        s.push(Step::InjectCredential {
            secret_ref: "hunter2".into(), // a literal — the bug we guard against
            target: Locator::new("textbox", "Password"),
        });
        assert!(s.assert_no_secret_values().is_err());
    }

    #[test]
    fn digest_is_stable_and_swap_sensitive() {
        let a = cart_script();
        let b = cart_script();
        assert_eq!(a.digest(), b.digest());
        let mut c = cart_script();
        c.steps.pop();
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn replay_ok_until_stop_gate() {
        let s = cart_script();
        // Snapshots that satisfy every expect, in order.
        let outcomes = replay(&s, |i, _step| match i {
            0 => Snapshot {
                url_host: Some("www.amazon.com".into()),
                ..Default::default()
            },
            3 => Snapshot {
                text: "Added to Cart".into(),
                ..Default::default()
            },
            _ => Snapshot::default(),
        });
        // 4 steps run OK (navigate, inject, type, click) then the stop-gate.
        assert_eq!(outcomes.len(), 5);
        assert_eq!(outcomes[0], StepOutcome::Ok);
        assert_eq!(outcomes[3], StepOutcome::Ok);
        assert!(matches!(outcomes[4], StepOutcome::StoppedAtGate(_)));
    }

    #[test]
    fn replay_detects_divergence_and_halts() {
        let s = cart_script();
        // The add-to-cart expect ("Added to Cart") never shows up → diverge
        // at step 3, not blindly continued.
        let outcomes = replay(&s, |i, _step| match i {
            0 => Snapshot {
                url_host: Some("www.amazon.com".into()),
                ..Default::default()
            },
            _ => Snapshot::default(),
        });
        assert!(matches!(outcomes.last().unwrap(), StepOutcome::Diverged(_)));
        // Replay halted BEFORE the stop-gate (didn't reach it).
        assert!(
            !outcomes
                .iter()
                .any(|o| matches!(o, StepOutcome::StoppedAtGate(_)))
        );
    }

    // ── AC5: the validated LLM-fallback wrapper ──

    #[test]
    fn recover_with_inlist_navigate_is_accepted() {
        let hosts = vec!["www.amazon.com".to_string()];
        let disp = Disposition::Recover {
            patched: Step::Navigate {
                url: "https://www.amazon.com/gp/cart".into(),
                expect: Expect::default(),
            },
        };
        assert!(matches!(
            validate_disposition(disp, false, &hosts),
            ValidatedDisposition::Recover { .. }
        ));
    }

    #[test]
    fn recover_with_offlist_navigate_is_rejected_to_abort() {
        let hosts = vec!["www.amazon.com".to_string()];
        let disp = Disposition::Recover {
            patched: Step::Navigate {
                url: "https://attacker.com/phish".into(),
                expect: Expect::default(),
            },
        };
        assert!(matches!(
            validate_disposition(disp, false, &hosts),
            ValidatedDisposition::RejectedToAbort { .. }
        ));
    }

    #[test]
    fn llm_can_never_pass_a_stop_gate() {
        let hosts = vec!["www.amazon.com".to_string()];
        // Even a `Continue` at a stop-gate is rejected to abort.
        for disp in [
            Disposition::Continue,
            Disposition::HandOff,
            Disposition::Recover {
                patched: Step::Click {
                    target: Locator::new("button", "Place order"),
                    expect: Expect::default(),
                },
            },
        ] {
            assert!(matches!(
                validate_disposition(disp, true, &hosts),
                ValidatedDisposition::RejectedToAbort { .. }
            ));
        }
    }

    #[test]
    fn recover_patch_that_is_a_stop_gate_is_rejected() {
        let hosts = vec!["www.amazon.com".to_string()];
        let disp = Disposition::Recover {
            patched: Step::StopGate {
                reason: "sneaky".into(),
            },
        };
        assert!(matches!(
            validate_disposition(disp, false, &hosts),
            ValidatedDisposition::RejectedToAbort { .. }
        ));
    }

    #[test]
    fn abort_passes_through() {
        let hosts = vec![];
        assert_eq!(
            validate_disposition(
                Disposition::Abort {
                    reason: "weird page".into()
                },
                false,
                &hosts
            ),
            ValidatedDisposition::Abort {
                reason: "weird page".into()
            }
        );
    }
}
