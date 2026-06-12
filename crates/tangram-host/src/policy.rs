//! The DEFERRED imperative policy-engine variant of fine-grained egress
//! (docs/design/fine-grained-egress.md §9.2; ADR-0009). This is the explicit
//! ESCAPE HATCH — NOT the default. The declarative call-level engine
//! ([`crate::egress`]) is the default and the first authoritative gate; the
//! policy engine is an additional, OPT-IN, per-app gate that runs AFTER the
//! host fence and the declarative call match for cases the declarative grammar
//! cannot express (e.g. a relationship between two already-canonicalized
//! fields).
//!
//! ## What this is — and deliberately is NOT
//!
//! Per §9.2 and the §2 "custom glue is where bugs appear" lesson, this is NOT
//! an arbitrary-code or unbounded-regex evaluator. It is a small, bounded,
//! auditable, declarative rule/condition AST over the request fields the
//! [`crate::egress::CanonicalRequest`] seam already produced — method, host,
//! path segments, query/header NAMES, and the JSON-RPC-method body selector.
//! There is:
//!
//! - **no second parser** — every field a condition reads comes from the
//!   SHARED [`CanonicalRequest`]/[`BodyMatch`] seam, so the policy engine and
//!   the declarative matcher can never disagree on what a host/path means (the
//!   SOCKS5 parser-differential lesson, §2/§8);
//! - **no regex, no value-matching on arbitrary fields** — string conditions
//!   are exact equality / membership / prefix-segment containment on the
//!   already-canonicalized values; the only body condition is the same fixed
//!   JSON-pointer + literal-set rung the declarative engine uses;
//! - **a hard latency budget** — a bounded rule count (checked at parse) and a
//!   bounded evaluation step count (checked at eval). A policy that would
//!   exceed the budget FAILS CLOSED (deny) with a clear error. There is no
//!   backtracking and no unbounded loop, so evaluation is O(rules × conditions)
//!   and terminates.
//!
//! ## Composition (it NARROWS, never widens)
//!
//! The policy engine can only ever turn an ALLOW into a DENY. It runs on a
//! request the declarative engine already matched (so the credential is already
//! bound to the matched call); the policy's verdict gates whether that matched
//! request proceeds. A policy can never grant a call the declarative engine
//! denied, and never changes which credential is injected. This keeps the
//! section-1 guarantee intact: an app with no policy is behavior-identical.
//!
//! ## Surfacing (never silent)
//!
//! An app that attaches a policy is clearly marked: `[apps.<app>.policy]` in
//! apps.toml, a `policy` flag in the app's surfaced egress posture, and a
//! startup log line. "This app uses custom policy" is never silent.

// The AST + evaluator land in this commit ahead of their consumers: the config
// parser ([`crate::config`]) and the `http_fetch` gate ([`crate::runtime`])
// wire these in over the following commits. Until then a couple of accessors
// read as dead from this module's view; the allow is removed as wiring lands.
#![allow(dead_code)]

use crate::egress::{BodyMatch, BodyVerdict, CanonicalRequest};

/// The hard cap on the number of rules in a single policy, checked at parse
/// time. A policy with more rules is REJECTED as a config error (it would
/// blow the latency budget). Small on purpose — the §9.2 "bounded, not
/// Turing-complete" constraint and the §2 glue-risk lesson.
pub const MAX_RULES: usize = 64;

/// The hard cap on the number of conditions across all rules in a policy,
/// checked at parse time. Bounds the per-request evaluation cost regardless of
/// how the rules are shaped.
pub const MAX_CONDITIONS: usize = 256;

/// The hard cap on evaluation STEPS per request (one step per condition
/// evaluated). Enforced at runtime as a defense-in-depth backstop to the
/// parse-time caps: if evaluation somehow exceeds it, the engine FAILS CLOSED
/// (deny). With the parse-time caps this is never reached in practice; it is
/// the explicit, auditable latency budget the §9.2 deferral requires.
pub const MAX_EVAL_STEPS: usize = 512;

/// One leaf condition over the ALREADY-CANONICALIZED request. Every variant
/// reads a field the [`CanonicalRequest`] seam produced — there is NO second
/// parser and NO regex. Conditions are exact/membership/segment-prefix checks
/// on canonical values plus the one fixed JSON-RPC-method body rung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// The canonical (upper-cased) method is one of these.
    MethodIn(Vec<String>),
    /// The canonical (lowercased, trailing-dot-stripped) host equals this.
    HostEq(String),
    /// The canonical path equals this exact (canonicalized) path.
    PathEq(String),
    /// The canonical path segments START WITH this segment list (a prefix
    /// match on PARSED segments — never a string-suffix/`endsWith` check, which
    /// is exactly the SOCKS5 differential class). Empty list matches any path.
    PathPrefix(Vec<String>),
    /// This query parameter NAME is present (never a value match).
    QueryNamePresent(String),
    /// This header NAME (lowercased) is present (never a value match).
    HeaderNamePresent(String),
    /// The JSON-RPC-method body rung: the value at the fixed pointer is in the
    /// literal set. Reuses the declarative engine's [`BodyMatch`] verbatim —
    /// the SAME body seam, parsed only when this condition is evaluated and
    /// only up to `max_body`. A malformed/oversized/absent body is NOT a match.
    BodyJsonMethodIn(BodyMatch),
}

impl Condition {
    /// Evaluate this leaf against the canonical request (and, for the body
    /// rung, the raw body bytes bounded by `max_body`). Pure and total — never
    /// panics, never does I/O, reads only the seam-produced fields.
    fn eval(&self, req: &CanonicalRequest, body: &[u8], max_body: Option<usize>) -> bool {
        match self {
            Self::MethodIn(methods) => methods.iter().any(|m| m == &req.method),
            Self::HostEq(host) => host == &req.host,
            Self::PathEq(path) => path == &req.path,
            Self::PathPrefix(prefix) => {
                prefix.len() <= req.segments.len()
                    && prefix
                        .iter()
                        .zip(&req.segments)
                        .all(|(want, got)| want == got)
            }
            Self::QueryNamePresent(name) => req.query_names.contains(name),
            Self::HeaderNamePresent(name) => req.header_names.contains(name),
            Self::BodyJsonMethodIn(bm) => bm.evaluate(body, max_body) == BodyVerdict::Match,
        }
    }
}

/// What a matching rule decides. A policy NARROWS only: `Deny` blocks a request
/// the declarative engine allowed; `Allow` lets evaluation stop early with the
/// request permitted (it does NOT grant anything the declarative engine
/// denied — by the time the policy runs the request already matched a declared
/// call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Deny,
}

/// One rule: ALL of its conditions must hold for the rule to fire (logical AND
/// over the leaves — deliberately the only combinator; there is no nesting, no
/// OR/NOT tree to reason about, keeping the grammar small and auditable, the
/// §3(b)/§8 lesson). A rule with no conditions always fires (a catch-all,
/// useful as a trailing `Deny` default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub effect: Effect,
    pub conditions: Vec<Condition>,
}

impl Rule {
    /// Whether every condition holds. Charges one evaluation step per condition
    /// to the budget; short-circuits on the first false condition.
    fn matches(
        &self,
        req: &CanonicalRequest,
        body: &[u8],
        max_body: Option<usize>,
        steps: &mut usize,
    ) -> Result<bool, BudgetExceeded> {
        for cond in &self.conditions {
            *steps += 1;
            if *steps > MAX_EVAL_STEPS {
                return Err(BudgetExceeded);
            }
            if !cond.eval(req, body, max_body) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The evaluation budget was exceeded — the engine FAILS CLOSED (deny). This
/// is the §9.2 latency-budget guarantee surfaced as a typed error so the caller
/// cannot accidentally treat it as "allow".
#[derive(Debug, PartialEq, Eq)]
pub struct BudgetExceeded;

/// The verdict the policy engine returns for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// No rule denied — the request proceeds (the declarative engine already
    /// allowed it; the policy added no further restriction here).
    Allow,
    /// A `Deny` rule fired, or the default effect denied. Carries a short,
    /// operator-facing reason (the rule index that fired, or "default deny").
    Deny(String),
    /// The evaluation budget was exceeded — FAIL CLOSED (treated as a deny by
    /// the caller). Surfaced distinctly so the log can name the budget.
    FailClosed(String),
}

/// A bounded, auditable egress policy: an ordered list of rules evaluated
/// first-match-wins, plus a `default` effect applied when no rule fires. This
/// is the §9.2 escape hatch — the OPT-IN gate that runs AFTER the declarative
/// call match. It is constructed by [`Policy::new`] (the budget is enforced
/// THERE), so a `Policy` value is always within budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    rules: Vec<Rule>,
    default: Effect,
    /// Bound on the body bytes any [`Condition::BodyJsonMethodIn`] will parse,
    /// mirrored from the matched call's `max_body_bytes` (set by the caller so
    /// the policy never parses more than the declarative engine would).
    max_body: Option<usize>,
}

impl Policy {
    /// Construct a policy, enforcing the latency budget at construction time:
    /// the rule count and the total condition count must be within
    /// [`MAX_RULES`]/[`MAX_CONDITIONS`], and an empty policy (no rules) is
    /// rejected (an empty policy is a no-op that should be expressed by simply
    /// not attaching a policy — refusing it keeps "this app uses custom policy"
    /// meaningful). Returns a config error string otherwise.
    pub fn new(rules: Vec<Rule>, default: Effect) -> Result<Self, String> {
        if rules.is_empty() {
            return Err(
                "egress policy has no rules — attach a policy only when it expresses at least \
                 one rule (an empty policy is a no-op; omit it instead)"
                    .to_string(),
            );
        }
        if rules.len() > MAX_RULES {
            return Err(format!(
                "egress policy has {} rules, over the budget of {MAX_RULES} (the policy engine \
                 is deliberately bounded — §9.2); split or simplify the policy",
                rules.len()
            ));
        }
        let conditions: usize = rules.iter().map(|r| r.conditions.len()).sum();
        if conditions > MAX_CONDITIONS {
            return Err(format!(
                "egress policy has {conditions} conditions across its rules, over the budget of \
                 {MAX_CONDITIONS} (the policy engine is deliberately bounded — §9.2)"
            ));
        }
        Ok(Self {
            rules,
            default,
            max_body: None,
        })
    }

    /// Set the body-byte bound the body rung will parse (mirrored from the
    /// matched declarative call so the policy never parses more than the
    /// declarative engine already bounded). Builder-style.
    pub fn with_max_body(mut self, max_body: Option<usize>) -> Self {
        self.max_body = max_body;
        self
    }

    /// The number of rules (surfaced for diagnostics / the "uses custom policy"
    /// marker).
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Evaluate the policy against an ALREADY-CANONICALIZED request (the shared
    /// seam) and the raw body bytes. First-match-wins over the ordered rules;
    /// if no rule fires, the `default` effect decides. The evaluation is
    /// bounded by [`MAX_EVAL_STEPS`]; exceeding it returns
    /// [`PolicyVerdict::FailClosed`] (a deny). Pure, total, no I/O — and reads
    /// ONLY the seam-produced fields, so it can never disagree with the
    /// declarative matcher on what a host/path means.
    pub fn evaluate(&self, req: &CanonicalRequest, body: &[u8]) -> PolicyVerdict {
        let mut steps = 0usize;
        for (i, rule) in self.rules.iter().enumerate() {
            match rule.matches(req, body, self.max_body, &mut steps) {
                Ok(true) => {
                    return match rule.effect {
                        Effect::Allow => PolicyVerdict::Allow,
                        Effect::Deny => PolicyVerdict::Deny(format!(
                            "egress policy rule #{} denied this request",
                            i + 1
                        )),
                    };
                }
                Ok(false) => continue,
                Err(BudgetExceeded) => {
                    return PolicyVerdict::FailClosed(format!(
                        "egress policy evaluation exceeded the step budget of {MAX_EVAL_STEPS} \
                         — failing CLOSED (deny). This is a policy-engine guardrail (§9.2)."
                    ));
                }
            }
        }
        match self.default {
            Effect::Allow => PolicyVerdict::Allow,
            Effect::Deny => {
                PolicyVerdict::Deny("egress policy default-denied this request".to_string())
            }
        }
    }
}

#[cfg(test)]
mod policy {
    use super::*;
    use crate::egress::CanonicalRequest;

    fn req(method: &str, url: &str) -> CanonicalRequest {
        let parsed = reqwest::Url::parse(url).expect("parse url");
        CanonicalRequest::from_request(method, &parsed, std::iter::empty()).expect("canonicalize")
    }

    fn req_headers(method: &str, url: &str, headers: &[&str]) -> CanonicalRequest {
        let parsed = reqwest::Url::parse(url).expect("parse url");
        CanonicalRequest::from_request(method, &parsed, headers.iter().copied())
            .expect("canonicalize")
    }

    // ── Construction enforces the latency budget. ────────────────────────────

    #[test]
    fn empty_policy_is_rejected() {
        assert!(Policy::new(vec![], Effect::Deny).is_err());
    }

    #[test]
    fn over_rule_budget_is_rejected_fail_closed_at_parse() {
        let rules: Vec<Rule> = (0..MAX_RULES + 1)
            .map(|_| Rule {
                effect: Effect::Deny,
                conditions: vec![],
            })
            .collect();
        let err = Policy::new(rules, Effect::Deny).unwrap_err();
        assert!(err.contains("over the budget"), "{err}");
    }

    #[test]
    fn over_condition_budget_is_rejected() {
        // One rule carrying more than MAX_CONDITIONS leaves.
        let conditions: Vec<Condition> = (0..MAX_CONDITIONS + 1)
            .map(|_| Condition::HostEq("h.com".into()))
            .collect();
        let err = Policy::new(
            vec![Rule {
                effect: Effect::Allow,
                conditions,
            }],
            Effect::Deny,
        )
        .unwrap_err();
        assert!(err.contains("conditions"), "{err}");
    }

    // ── First-match-wins + default. ──────────────────────────────────────────

    #[test]
    fn first_matching_rule_wins() {
        // Allow GET on /v1/me/*; deny everything else.
        let policy = Policy::new(
            vec![
                Rule {
                    effect: Effect::Allow,
                    conditions: vec![
                        Condition::MethodIn(vec!["GET".into()]),
                        Condition::PathPrefix(vec!["v1".into(), "me".into()]),
                    ],
                },
                Rule {
                    effect: Effect::Deny,
                    conditions: vec![],
                },
            ],
            Effect::Deny,
        )
        .unwrap();

        assert_eq!(
            policy.evaluate(&req("GET", "https://api.vendor.com/v1/me/contacts"), b""),
            PolicyVerdict::Allow
        );
        // POST is not allowed by the first rule, the catch-all denies.
        assert!(matches!(
            policy.evaluate(&req("POST", "https://api.vendor.com/v1/me/contacts"), b""),
            PolicyVerdict::Deny(_)
        ));
        // A sibling path the first rule does not cover is denied.
        assert!(matches!(
            policy.evaluate(
                &req("GET", "https://api.vendor.com/v1/accounts/9/import"),
                b""
            ),
            PolicyVerdict::Deny(_)
        ));
    }

    #[test]
    fn default_applies_when_no_rule_fires() {
        let policy = Policy::new(
            vec![Rule {
                effect: Effect::Allow,
                conditions: vec![Condition::HostEq("only.com".into())],
            }],
            Effect::Deny,
        )
        .unwrap();
        // Host matches → allow.
        assert_eq!(
            policy.evaluate(&req("GET", "https://only.com/x"), b""),
            PolicyVerdict::Allow
        );
        // Host does not match the single rule → default deny.
        assert!(matches!(
            policy.evaluate(&req("GET", "https://other.com/x"), b""),
            PolicyVerdict::Deny(_)
        ));
    }

    // ── Parser-differential discipline: the policy reads the SAME canonical
    //    fields as the declarative engine, so a host/path that the seam
    //    normalized is seen normalized HERE too (no second parser). ───────────

    #[test]
    fn policy_sees_the_canonicalized_host_and_path() {
        // The policy denies an undeclared subtree; an attacker encoding `..` or
        // mixing case must be judged on the CANONICAL value, identically to the
        // declarative matcher.
        let policy = Policy::new(
            vec![Rule {
                effect: Effect::Deny,
                conditions: vec![
                    Condition::HostEq("api.vendor.com".into()),
                    Condition::PathPrefix(vec!["v1".into(), "accounts".into()]),
                ],
            }],
            Effect::Allow,
        )
        .unwrap();

        // Mixed-case host + percent-encoded traversal that canonicalizes INTO
        // /v1/accounts/... must be denied (the seam already normalized it).
        let r = req(
            "GET",
            "https://API.Vendor.COM/v1/me/%2e%2e/accounts/9/import",
        );
        assert_eq!(r.host, "api.vendor.com");
        assert_eq!(r.segments, ["v1", "accounts", "9", "import"]);
        assert!(matches!(policy.evaluate(&r, b""), PolicyVerdict::Deny(_)));
    }

    #[test]
    fn path_prefix_is_segment_wise_never_string_suffix() {
        // PathPrefix matches PARSED segments, so `v1-accounts` (one segment) is
        // NOT a prefix of `["v1","accounts"]` — no endsWith/startsWith string
        // confusion (the SOCKS5 differential class).
        let policy = Policy::new(
            vec![Rule {
                effect: Effect::Deny,
                conditions: vec![Condition::PathPrefix(vec!["v1".into(), "accounts".into()])],
            }],
            Effect::Allow,
        )
        .unwrap();
        // A single segment that *string-contains* the prefix is NOT a match.
        assert_eq!(
            policy.evaluate(&req("GET", "https://h.com/v1-accounts-evil"), b""),
            PolicyVerdict::Allow
        );
        // The genuine segment path IS a match (deny).
        assert!(matches!(
            policy.evaluate(&req("GET", "https://h.com/v1/accounts/9"), b""),
            PolicyVerdict::Deny(_)
        ));
    }

    // ── The body rung reuses the declarative BodyMatch seam. ─────────────────

    #[test]
    fn body_json_method_condition_reuses_the_shared_seam() {
        let policy = Policy::new(
            vec![
                Rule {
                    effect: Effect::Allow,
                    conditions: vec![Condition::BodyJsonMethodIn(BodyMatch {
                        pointer: "/method".into(),
                        allowed: vec!["tools/list".into(), "tools/call".into()],
                    })],
                },
                Rule {
                    effect: Effect::Deny,
                    conditions: vec![],
                },
            ],
            Effect::Deny,
        )
        .unwrap();
        let r = req("POST", "https://api.vendor.com/rpc");
        assert_eq!(
            policy.evaluate(&r, br#"{"method":"tools/list"}"#),
            PolicyVerdict::Allow
        );
        // Undeclared JSON-RPC method → the allow rule does not fire → deny.
        assert!(matches!(
            policy.evaluate(&r, br#"{"method":"resources/read"}"#),
            PolicyVerdict::Deny(_)
        ));
        // Non-JSON body → the body condition is not a match → deny (never a
        // panic).
        assert!(matches!(
            policy.evaluate(&r, b"not json"),
            PolicyVerdict::Deny(_)
        ));
    }

    #[test]
    fn body_rung_respects_the_max_body_bound() {
        // An oversized body is Unusable BEFORE parse, so the allow rule does not
        // fire and the default denies — the policy never parses more than the
        // declarative call bounded.
        let policy = Policy::new(
            vec![Rule {
                effect: Effect::Allow,
                conditions: vec![Condition::BodyJsonMethodIn(BodyMatch {
                    pointer: "/method".into(),
                    allowed: vec!["tools/list".into()],
                })],
            }],
            Effect::Deny,
        )
        .unwrap()
        .with_max_body(Some(8));
        let r = req("POST", "https://api.vendor.com/rpc");
        // 23 bytes > 8 → unusable → not allowed → default deny.
        assert!(matches!(
            policy.evaluate(&r, br#"{"method":"tools/list"}"#),
            PolicyVerdict::Deny(_)
        ));
    }

    #[test]
    fn query_and_header_name_presence() {
        let policy = Policy::new(
            vec![
                Rule {
                    effect: Effect::Deny,
                    conditions: vec![Condition::QueryNamePresent("callback".into())],
                },
                Rule {
                    effect: Effect::Deny,
                    conditions: vec![Condition::HeaderNamePresent("x-evil".into())],
                },
                Rule {
                    effect: Effect::Allow,
                    conditions: vec![],
                },
            ],
            Effect::Deny,
        )
        .unwrap();
        // forbidden query name present → deny.
        assert!(matches!(
            policy.evaluate(&req("GET", "https://h.com/x?callback=evil"), b""),
            PolicyVerdict::Deny(_)
        ));
        // forbidden header present → deny.
        assert!(matches!(
            policy.evaluate(&req_headers("GET", "https://h.com/x", &["X-Evil"]), b""),
            PolicyVerdict::Deny(_)
        ));
        // neither present → allow (third rule).
        assert_eq!(
            policy.evaluate(&req("GET", "https://h.com/x"), b""),
            PolicyVerdict::Allow
        );
    }

    // ── The latency budget fails CLOSED at evaluation. ───────────────────────

    #[test]
    fn eval_step_budget_fails_closed() {
        // A policy WITHIN the parse budget can be forced to exceed the eval-step
        // budget by constructing it past MAX_EVAL_STEPS conditions — which the
        // parse budget normally prevents, so we build the Policy struct directly
        // (bypassing Policy::new) to exercise the runtime backstop in isolation.
        let conditions: Vec<Condition> = (0..MAX_EVAL_STEPS + 1)
            // Each condition is TRUE so the rule keeps charging steps until the
            // backstop trips (it must fail closed, not fall through to allow).
            .map(|_| Condition::MethodIn(vec!["GET".into()]))
            .collect();
        let policy = Policy {
            rules: vec![Rule {
                effect: Effect::Allow,
                conditions,
            }],
            default: Effect::Allow,
            max_body: None,
        };
        match policy.evaluate(&req("GET", "https://h.com/x"), b"") {
            PolicyVerdict::FailClosed(msg) => assert!(msg.contains("step budget"), "{msg}"),
            other => panic!("expected fail-closed, got {other:?}"),
        }
    }
}
