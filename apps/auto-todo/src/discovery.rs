//! Discovery + classification — the READ-ONLY, tool-based reasoning that
//! turns a free-text item into REVIEWABLE DATA (design §5). No credentials,
//! no browser, no execution.
//!
//! The source of truth is a DETERMINISTIC, rule-based classifier so the
//! lifecycle is testable offline (CI never makes a live call). AC2 layers an
//! optional LLM assist on top, behind the same fixture-offline seam nutrition
//! uses (`AUTO_TODO_DISCOVERY=offline|llm`, defaulting to offline when no key):
//! the LLM only ever PROPOSES requirements that are then re-classified by the
//! same deterministic rules, so the gates never depend on the model's
//! judgement.

use crate::{InferredRequirements, NeedDisposition, Plan, PlanStep};

/// The classification of a single inferred need (design §5.2), in
/// risk-ascending preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// An existing read-only MCP/API tool covers it with an already-connected
    /// credential — lowest risk, the only kind the safe tier can "execute".
    Tool,
    /// Only reachable via a website with no API — DEFERRED to AC5 (substrate);
    /// in the safe tier this routes to a plan step the human must drive.
    Browser,
    /// A human is intrinsically required (2FA, CAPTCHA, payment, judgement) or
    /// nothing automatable matches.
    NeedsHuman,
}

impl Disposition {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Tool => "TOOL",
            Disposition::Browser => "BROWSER",
            Disposition::NeedsHuman => "NEEDS_HUMAN",
        }
    }
}

/// One entry in the READ-ONLY tool catalog the classifier matches against
/// (design §5.2, AC2). DELIBERATELY read-only: every tool here needs no new
/// credential and takes no irreversible action, so a matched item is
/// executable in the safe tier with only the plan approval (no per-step
/// confirm). Credentialed / write tools are AC4+.
struct CatalogTool {
    /// The tool id surfaced in the plan and the UI badge.
    id: &'static str,
    /// Capability keywords this read-only tool satisfies.
    capabilities: &'static [&'static str],
}

/// The read-only, already-connected tool catalog. Kept small and explicit:
/// the safe tier only auto-routes to tools that cannot spend, send, or delete.
const READ_ONLY_CATALOG: &[CatalogTool] = &[
    CatalogTool {
        id: "Google_Calendar.list_events",
        capabilities: &["calendar.read"],
    },
    CatalogTool {
        id: "Google_Calendar.get_event",
        capabilities: &["calendar.read"],
    },
    CatalogTool {
        id: "Notion.query",
        capabilities: &["notes.read", "docs.read"],
    },
    CatalogTool {
        id: "Gmail.list_messages",
        capabilities: &["email.read"],
    },
];

/// Keyword → capability inference rules (deterministic). Ordered so the most
/// specific writes/irreversible verbs are detected before generic reads.
struct Rule {
    /// Lowercased substrings any of which trigger this rule.
    triggers: &'static [&'static str],
    /// The capability this implies.
    capability: &'static str,
    /// A named connection/service this implies (empty = none).
    connection: &'static str,
    /// The credential kind the connection needs (empty = none).
    credential: &'static str,
    /// A human-assistance point this implies (empty = none).
    human_assist: &'static str,
    /// Reversibility this verb implies: "none" | "reversible" | "irreversible".
    irreversibility: &'static str,
}

const RULES: &[Rule] = &[
    // --- irreversible / spending / sending verbs (highest concern) ---
    Rule {
        triggers: &[
            "renew",
            "buy",
            "purchase",
            "pay",
            "order",
            "checkout",
            "subscribe",
        ],
        capability: "web.purchase",
        connection: "a billing/checkout site",
        credential: "login",
        human_assist: "payment confirmation",
        irreversibility: "irreversible",
    },
    Rule {
        triggers: &[
            "send email",
            "email ",
            "reply to",
            "send a message",
            "send mail",
        ],
        capability: "email.send",
        connection: "Gmail",
        credential: "oauth",
        human_assist: "",
        irreversibility: "irreversible",
    },
    Rule {
        triggers: &["delete", "remove", "cancel", "unsubscribe"],
        capability: "data.delete",
        connection: "",
        credential: "",
        human_assist: "",
        irreversibility: "irreversible",
    },
    // --- reversible writes ---
    Rule {
        triggers: &[
            "rsvp",
            "add to my calendar",
            "schedule",
            "create event",
            "book",
        ],
        capability: "calendar.write",
        connection: "Google Calendar",
        credential: "oauth",
        human_assist: "",
        irreversibility: "reversible",
    },
    Rule {
        triggers: &[
            "download",
            "invoice",
            "portal",
            "log in to",
            "login to",
            "sign in",
        ],
        capability: "web.download",
        connection: "a web portal",
        credential: "login",
        human_assist: "2FA",
        irreversibility: "reversible",
    },
    // --- read-only (the safe-tier sweet spot) ---
    Rule {
        triggers: &[
            "what's on my calendar",
            "whats on my calendar",
            "my calendar",
            "agenda",
            "meetings",
        ],
        capability: "calendar.read",
        connection: "Google Calendar",
        credential: "oauth",
        human_assist: "",
        irreversibility: "none",
    },
    Rule {
        triggers: &["check my email", "read email", "unread", "inbox"],
        capability: "email.read",
        connection: "Gmail",
        credential: "oauth",
        human_assist: "",
        irreversibility: "none",
    },
    Rule {
        triggers: &[
            "look up",
            "find my notes",
            "search notes",
            "in notion",
            "my notes",
        ],
        capability: "notes.read",
        connection: "Notion",
        credential: "oauth",
        human_assist: "",
        irreversibility: "none",
    },
];

/// Deterministic, rule-based requirements inference over the item text
/// (design §5.1). Over-discloses needs (false positives are cheap — a human
/// reviews) and flags irreversibility explicitly (it drives gate strictness).
#[must_use]
pub fn infer_requirements(text: &str) -> InferredRequirements {
    let hay = text.to_lowercase();
    let mut capabilities = Vec::new();
    let mut connections = Vec::new();
    let mut credentials = Vec::new();
    let mut human_assistance = Vec::new();
    let mut irreversibility = "none";

    for rule in RULES {
        if rule.triggers.iter().any(|t| hay.contains(t)) {
            push_unique(&mut capabilities, rule.capability);
            if !rule.connection.is_empty() {
                push_unique(&mut connections, rule.connection);
            }
            if !rule.credential.is_empty() {
                push_unique(&mut credentials, rule.credential);
            }
            if !rule.human_assist.is_empty() {
                push_unique(&mut human_assistance, rule.human_assist);
            }
            irreversibility = worse(irreversibility, rule.irreversibility);
        }
    }

    // Confidence: high when we matched something concrete, low when the text
    // matched nothing (→ lands in DISCOVERED with a "needs clarification"
    // posture rather than auto-advancing with empty requirements).
    let confidence = if capabilities.is_empty() { 0.2 } else { 0.85 };

    InferredRequirements {
        summary: text.trim().to_string(),
        capabilities,
        connections,
        credentials,
        human_assistance,
        irreversibility: irreversibility.to_string(),
        confidence,
    }
}

/// Classify each inferred capability into TOOL | BROWSER | NEEDS_HUMAN
/// (design §5.2). Decision order: prefer a read-only tool match; else if the
/// need is reversible-but-web route to BROWSER (deferred to AC5); else
/// NEEDS_HUMAN.
#[must_use]
pub fn classify(req: &InferredRequirements) -> Vec<NeedDisposition> {
    if req.capabilities.is_empty() {
        return vec![NeedDisposition {
            need: "(unclassified)".into(),
            kind: Disposition::NeedsHuman.as_str().into(),
            tool: None,
            read_only: false,
            rationale: "could not infer any concrete capability from the item text; \
                        a human needs to clarify the task"
                .into(),
        }];
    }

    req.capabilities
        .iter()
        .map(|cap| classify_capability(cap, req))
        .collect()
}

fn classify_capability(cap: &str, req: &InferredRequirements) -> NeedDisposition {
    // 1. TOOL — a read-only tool match (preferred, lowest risk).
    if let Some(tool) = READ_ONLY_CATALOG
        .iter()
        .find(|t| t.capabilities.contains(&cap))
    {
        return NeedDisposition {
            need: cap.to_string(),
            kind: Disposition::Tool.as_str().into(),
            tool: Some(tool.id.to_string()),
            read_only: true,
            rationale: format!(
                "the read-only tool {} covers {cap:?} on an already-connected \
                 connection — no new credential, no irreversible action",
                tool.id
            ),
        };
    }

    // 2/3. No read-only tool. 2FA / payment / explicit human need, or an
    // irreversible action, is NEEDS_HUMAN in the safe tier; an otherwise
    // web-reachable reversible need is BROWSER (DEFERRED to AC5).
    let needs_human = req.irreversibility == "irreversible"
        || req
            .human_assistance
            .iter()
            .any(|h| h == "2FA" || h == "CAPTCHA" || h == "payment confirmation");

    if needs_human {
        NeedDisposition {
            need: cap.to_string(),
            kind: Disposition::NeedsHuman.as_str().into(),
            tool: None,
            read_only: false,
            rationale: format!(
                "{cap:?} is irreversible or needs human assistance (2FA / payment); \
                 in the safe tier it stops at a human — credentialed/browser execution \
                 is a later, separately-reviewed tier"
            ),
        }
    } else {
        NeedDisposition {
            need: cap.to_string(),
            kind: Disposition::Browser.as_str().into(),
            tool: None,
            read_only: false,
            rationale: format!(
                "{cap:?} has no read-only tool; it would need browser automation, \
                 which is DEFERRED (AC5, gated on the automation substrate)"
            ),
        }
    }
}

/// Assemble the structured plan from the classified requirements (design
/// §7.1) and bind it to a content hash. A step is `requires_confirm` when it
/// is browser/human or otherwise not a read-only tool step — the per-step
/// real-time gate (design §7.2). Read-only tool steps ride the plan approval.
#[must_use]
pub fn build_plan(req: &InferredRequirements, dispositions: &[NeedDisposition]) -> Plan {
    let mut steps = Vec::new();
    let mut requested_grants = Vec::new();
    let mut human_assist = req.human_assistance.clone();

    for d in dispositions {
        match d.kind.as_str() {
            "TOOL" => {
                steps.push(PlanStep {
                    kind: "TOOL".into(),
                    summary: format!("{}: {}", d.tool.as_deref().unwrap_or("(tool)"), d.need),
                    // Read-only, already-connected → no per-step confirm.
                    requires_confirm: false,
                });
                if let Some(tool) = &d.tool {
                    push_unique_string(&mut requested_grants, format!("tool:{tool} (read-only)"));
                }
            }
            "BROWSER" => {
                steps.push(PlanStep {
                    kind: "BROWSER".into(),
                    summary: format!("browser automation for {} (DEFERRED, AC5)", d.need),
                    requires_confirm: true,
                });
                push_unique_string(&mut requested_grants, format!("browser:{} (AC5)", d.need));
            }
            _ => {
                steps.push(PlanStep {
                    kind: "HUMAN".into(),
                    summary: format!("human assistance for {}", d.need),
                    requires_confirm: true,
                });
                push_unique_string(&mut human_assist, format!("human step: {}", d.need));
            }
        }
    }

    let reversibility = req.irreversibility.clone();
    let plan_hash = hash_plan(&steps, &requested_grants, &human_assist, &reversibility);

    Plan {
        steps,
        requested_grants,
        human_assist,
        reversibility,
        plan_hash,
    }
}

/// Convenience: classify a raw item text in one shot (used by tests and the
/// re-classify-the-LLM-proposal seam). Returns the dispositions.
#[must_use]
pub fn classify_text(text: &str) -> Vec<NeedDisposition> {
    classify(&infer_requirements(text))
}

/// Convenience: text → full plan, deterministically (tests / preview).
#[must_use]
pub fn plan_from(text: &str) -> Plan {
    let req = infer_requirements(text);
    let disp = classify(&req);
    build_plan(&req, &disp)
}

/// Content hash the approval binds to. Any change to the steps / grants /
/// assist / reversibility changes the hash, which invalidates a prior
/// approval (design §8).
fn hash_plan(
    steps: &[PlanStep],
    grants: &[String],
    human_assist: &[String],
    reversibility: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for s in steps {
        hasher.update(s.kind.as_bytes());
        hasher.update([0]);
        hasher.update(s.summary.as_bytes());
        hasher.update([u8::from(s.requires_confirm)]);
        hasher.update([0xff]);
    }
    hasher.update(b"|grants|");
    for g in grants {
        hasher.update(g.as_bytes());
        hasher.update([0]);
    }
    hasher.update(b"|assist|");
    for h in human_assist {
        hasher.update(h.as_bytes());
        hasher.update([0]);
    }
    hasher.update(b"|rev|");
    hasher.update(reversibility.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// "none" < "reversible" < "irreversible".
fn worse<'a>(a: &'a str, b: &'a str) -> &'a str {
    fn rank(s: &str) -> u8 {
        match s {
            "irreversible" => 2,
            "reversible" => 1,
            _ => 0,
        }
    }
    if rank(b) > rank(a) { b } else { a }
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

fn push_unique_string(v: &mut Vec<String>, s: String) {
    if !v.contains(&s) {
        v.push(s);
    }
}
