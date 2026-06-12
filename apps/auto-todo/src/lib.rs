//! Auto-TODO — a TODO list where each item carries a gated, per-item agent
//! *lifecycle* (design: `docs/design/auto-todo.md`).
//!
//! THIS CRATE IS THE SAFE TIER (AC1–AC3) of that design:
//!
//! - **AC1** — the per-item lifecycle as a PURE state machine over a
//!   replicated document: `DRAFTED → DISCOVERED → CLASSIFIED → PLAN_PROPOSED
//!   → APPROVED`, with `REJECTED`, `request_changes`, and `BLOCKED_HUMAN`
//!   off-ramps. No agency: `execute()` only records "would execute".
//! - **AC2** — discovery + classification that is READ-ONLY and tool-based:
//!   it infers the permissions / connections / human-assistance an item needs
//!   and routes each need to TOOL / BROWSER / NEEDS_HUMAN, as *reviewable
//!   data*. No credentials, no browser, no execution. A deterministic
//!   rule-based classifier is the source of truth; an optional LLM assist
//!   sits behind the same fixture-offline seam nutrition uses.
//! - **AC3** — the approval protocol: a structured plan bound to a
//!   `plan_hash`, `approve` / `reject` / `request_changes` / narrow-grants,
//!   and the per-step `confirm()` checkpoint mechanism.
//!
//! AC4–AC6 (scoped delegation, browser items, 1Password reads) are
//! DELIBERATELY NOT BUILT here — they are a later tier gated on the
//! automation substrate and PR #1 (`docs/design/auto-todo.md` §10).
//!
//! **Gating posture.** The risk-bearing transitions (`approve`, `provision`,
//! `execute`, `confirm`) are mutating actions; the host bearer-gates them via
//! `require_auth = true` in `apps.toml`, exactly as the registry / marketplace
//! mutating routes are gated. The *load-bearing safety* is in the machine
//! invariants (you cannot execute without an approval bound to the CURRENT
//! plan hash; re-planning re-opens approval), not in the model's judgement.

use tangram::prelude::*;

mod discovery;

pub use discovery::{Disposition, classify_text, plan_from};

/// The per-item lifecycle phase (design §3). Stored as a stable string in the
/// document so older/newer binaries round-trip it; [`Phase`] is the typed
/// view the actions reason over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Item text only; nothing inferred yet.
    Drafted,
    /// Inferred requirements recorded (capabilities, connections, credentials,
    /// human-assistance) — reviewable data, no action taken.
    Discovered,
    /// Each need annotated TOOL | BROWSER | NEEDS_HUMAN.
    Classified,
    /// A structured plan + requested grants + human-assist schedule, bound to
    /// a `plan_hash`. Awaiting the human approval gate.
    PlanProposed,
    /// A human granted the plan + grants on the current plan hash.
    Approved,
    /// Terminal: a human rejected the plan; the reason is recorded.
    Rejected,
    /// Parked awaiting a human (2FA / CAPTCHA / ambiguity / a needed
    /// confirm). A first-class state, never an error.
    BlockedHuman,
}

impl Phase {
    /// The stable wire string stored in the document.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Drafted => "DRAFTED",
            Phase::Discovered => "DISCOVERED",
            Phase::Classified => "CLASSIFIED",
            Phase::PlanProposed => "PLAN_PROPOSED",
            Phase::Approved => "APPROVED",
            Phase::Rejected => "REJECTED",
            Phase::BlockedHuman => "BLOCKED_HUMAN",
        }
    }

    /// Parse the stored string. Unknown values hydrate as `Drafted` (the
    /// safe, no-agency floor) rather than erroring the whole document.
    #[must_use]
    pub fn parse(s: &str) -> Phase {
        match s {
            "DISCOVERED" => Phase::Discovered,
            "CLASSIFIED" => Phase::Classified,
            "PLAN_PROPOSED" => Phase::PlanProposed,
            "APPROVED" => Phase::Approved,
            "REJECTED" => Phase::Rejected,
            "BLOCKED_HUMAN" => Phase::BlockedHuman,
            _ => Phase::Drafted,
        }
    }
}

#[model]
#[derive(Default)]
pub struct AutoTodo {
    items: Vec<Item>,
}

/// One TODO item and its replicated agent state. Every field added beyond the
/// genesis shape is `Option`/`Vec` with `#[autosurgeon(missing)]` so documents
/// written by older binaries still hydrate (CLAUDE.md convention).
#[model]
pub struct Item {
    /// Stable id (also the path the UI addresses).
    id: String,
    /// The free-text task ("renew my domain", "what's on my calendar Tuesday").
    text: String,
    /// Lifecycle phase, stored as a stable string (see [`Phase`]).
    phase: String,
    created_at_ms: i64,
    /// Last lifecycle transition time.
    #[autosurgeon(missing = "Option::default")]
    updated_at_ms: Option<i64>,

    /// `discover()` output (AC2): the inferred requirements, written as
    /// reviewable data. `None` until discovered.
    #[autosurgeon(missing = "Option::default")]
    requirements: Option<InferredRequirements>,

    /// `classify()` output (AC2): per-need TOOL/BROWSER/NEEDS_HUMAN
    /// dispositions over the requirements. Empty until classified.
    #[autosurgeon(missing = "Vec::new")]
    dispositions: Vec<NeedDisposition>,

    /// `plan()` output (AC3): the structured execution plan an approval binds
    /// to. `None` until planned.
    #[autosurgeon(missing = "Option::default")]
    plan: Option<Plan>,

    /// The approval record (AC3). `None` until a human approves; cleared when
    /// the plan is re-opened (re-plan / request_changes).
    #[autosurgeon(missing = "Option::default")]
    approval: Option<Approval>,

    /// Per-step `confirm()` checkpoints cleared by a human (AC3). Holds the
    /// indices of plan steps the human has confirmed in real time.
    #[autosurgeon(missing = "Vec::new")]
    confirmed_steps: Vec<i64>,

    /// Free-text note attached on a `request_changes` / `reject` / block, for
    /// the human's audit trail. `None` when there is nothing to say.
    #[autosurgeon(missing = "Option::default")]
    note: Option<String>,

    /// The recorded result of a (non-executing, AC1) `execute()` — in the safe
    /// tier this is only "would execute …", never a real action.
    #[autosurgeon(missing = "Option::default")]
    result: Option<String>,
}

/// The structured requirements `discover()` infers from the item text
/// (design §5.1). It is DATA the human reviews, not an action.
#[model]
pub struct InferredRequirements {
    /// Restated goal.
    summary: String,
    /// e.g. "calendar.read", "calendar.write", "email.send".
    capabilities: Vec<String>,
    /// Named services: "Google Calendar", "the domain registrar", …
    connections: Vec<String>,
    /// What auth each connection needs ("oauth", "api_key", "login").
    credentials: Vec<String>,
    /// Human-assistance points: "2FA", "CAPTCHA", "payment confirmation", …
    human_assistance: Vec<String>,
    /// "none" | "reversible" | "irreversible" — drives gate strictness (§8).
    irreversibility: String,
    /// 0.0–1.0 model/rule confidence. Low confidence parks for clarification.
    confidence: f64,
}

/// One per-need disposition over the inferred requirements (design §5.2): the
/// cheapest safe path for a single capability/connection.
#[model]
pub struct NeedDisposition {
    /// The capability or connection this disposition is about.
    need: String,
    /// "TOOL" | "BROWSER" | "NEEDS_HUMAN" (see [`Disposition`]).
    kind: String,
    /// For TOOL: the matched read-only tool id (e.g.
    /// "Google_Calendar.list_events"). Empty otherwise.
    tool: Option<String>,
    /// Whether this need is read-only (no credential, no irreversible action).
    /// Only read-only TOOL needs are eligible for the frictionless safe tier.
    read_only: bool,
    /// Human-readable rationale shown in the approval card.
    rationale: String,
}

/// The structured plan an approval binds to (design §7.1).
#[model]
pub struct Plan {
    /// Ordered steps: each is a tool call / browser nav / human checkpoint.
    steps: Vec<PlanStep>,
    /// The grants the plan requests (calls/tools + credential mode per step).
    requested_grants: Vec<String>,
    /// Human-assist points with WHEN in the sequence.
    human_assist: Vec<String>,
    /// Worst-case reversibility across steps; drives gate strictness.
    reversibility: String,
    /// Approval binds to THIS hash; any change re-opens approval (design §8).
    plan_hash: String,
}

/// One ordered step of a [`Plan`].
#[model]
pub struct PlanStep {
    /// "TOOL" | "BROWSER" | "HUMAN" — the badge shown in the UI.
    kind: String,
    /// Plain-language summary of the step ("list events on Tuesday").
    summary: String,
    /// Whether clearing this step requires a real-time per-step `confirm()`
    /// (irreversible or credential-using steps; design §7.2). Read-only tool
    /// steps with already-connected creds are exempt → the safe tier is
    /// frictionless.
    requires_confirm: bool,
}

/// The human approval record bound to a plan hash (design §7.2).
#[model]
pub struct Approval {
    /// The plan hash this approval is bound to. If the item's current plan
    /// hash differs, the approval is STALE and execution is refused.
    plan_hash: String,
    /// The authed principal who approved (recorded for the audit trail; the
    /// host bearer-gates the action that writes this).
    principal: String,
    approved_at_ms: i64,
    /// Grants the human actually granted — may be NARROWER than the plan's
    /// `requested_grants` if the human struck some before approving (§7.2).
    granted: Vec<String>,
}

impl AutoTodo {
    fn find(&self, id: &str) -> Result<&Item, String> {
        self.items
            .iter()
            .find(|i| i.id == id)
            .ok_or_else(|| format!("no item with id {id}"))
    }

    fn find_mut(&mut self, id: &str) -> Result<&mut Item, String> {
        self.items
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| format!("no item with id {id}"))
    }

    /// Store discovered requirements on an item and move it to DISCOVERED,
    /// invalidating any downstream dispositions/plan/approval (re-discovery
    /// re-opens the cycle, design §8). Shared by the sync `discover` and the
    /// async `discover_assisted` commit step.
    fn apply_requirements(&mut self, id: &str, req: InferredRequirements) -> Result<(), String> {
        let item = self.find_mut(id)?;
        item.requirements = Some(req);
        item.dispositions.clear();
        item.plan = None;
        item.approval = None;
        item.confirmed_steps.clear();
        item.set_phase(Phase::Discovered);
        Ok(())
    }
}

impl Item {
    fn phase(&self) -> Phase {
        Phase::parse(&self.phase)
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase.as_str().to_string();
        self.updated_at_ms = Some(now_ms());
    }

    /// Whether `self.plan`'s hash still matches `self.approval`'s bound hash —
    /// i.e. the approval is for the CURRENT plan, not a superseded one.
    fn approval_is_current(&self) -> bool {
        match (&self.approval, &self.plan) {
            (Some(a), Some(p)) => a.plan_hash == p.plan_hash,
            _ => false,
        }
    }
}

#[actions]
impl AutoTodo {
    /// Add a free-text TODO item. It starts in `DRAFTED` — nothing is
    /// inferred or acted on until `discover()` is called. Returns the id.
    pub fn add_item(&mut self, text: String) -> Result<String, String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err("item text must be non-empty".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        self.items.push(Item {
            id: id.clone(),
            text,
            phase: Phase::Drafted.as_str().to_string(),
            created_at_ms: now,
            updated_at_ms: Some(now),
            requirements: None,
            dispositions: Vec::new(),
            plan: None,
            approval: None,
            confirmed_steps: Vec::new(),
            note: None,
            result: None,
        });
        Ok(id)
    }

    /// Delete an item entirely. Errors if no item has the given id.
    pub fn delete_item(&mut self, id: String) -> Result<(), String> {
        let before = self.items.len();
        self.items.retain(|i| i.id != id);
        if self.items.len() == before {
            return Err(format!("no item with id {id}"));
        }
        Ok(())
    }

    /// `discover()` — DRAFTED → DISCOVERED, DETERMINISTIC (rule-based, no
    /// network). Records a reviewable requirements inference over the item
    /// text. This is the keyless/offline path and the floor every other path
    /// builds on; the LLM-assisted variant is [`discover_assisted`]. Re-running
    /// from a later phase resets the item to DISCOVERED and clears the
    /// downstream plan/approval (re-planning re-opens approval, design §8).
    pub fn discover(&mut self, id: String) -> Result<(), String> {
        let req = discovery::infer_requirements(&self.find(&id)?.text);
        self.apply_requirements(&id, req)
    }

    /// `discover_assisted()` — DRAFTED → DISCOVERED with the optional LLM
    /// assist (AC2, design §5.1). The deterministic rules are always run and
    /// stay authoritative; in `AUTO_TODO_DISCOVERY=llm` mode an Anthropic call
    /// (resolved OUTSIDE the store lock, through the host's egress facade) may
    /// PROPOSE additional needs that are merged (union; never lowers
    /// irreversibility). With no key / offline mode this is exactly
    /// [`discover`]. The merge result is reviewable DATA — no action is taken.
    pub async fn discover_assisted(ctx: Ctx<Self>, id: String) -> Result<(), String> {
        // Read the item text outside the lock (the LLM call must not hold it).
        let text = ctx
            .state()
            .map_err(|e| e.to_string())?
            .find(&id)?
            .text
            .clone();
        let req = discovery::discover(&text).await;
        // Commit the resolved requirements as one attributed change.
        ctx.mutate("discover_assisted", |m| m.apply_requirements(&id, req))
            .map_err(|e| e.to_string())?
    }

    /// `classify()` — DISCOVERED → CLASSIFIED. Annotate each inferred need
    /// with a TOOL | BROWSER | NEEDS_HUMAN disposition (design §5.2). Requires
    /// the item to have been discovered first.
    pub fn classify(&mut self, id: String) -> Result<(), String> {
        let item = self.find_mut(&id)?;
        if item.phase() != Phase::Discovered {
            return Err(format!(
                "classify() requires phase DISCOVERED, item is {}",
                item.phase().as_str()
            ));
        }
        let req = item
            .requirements
            .as_ref()
            .ok_or("item has no inferred requirements; call discover() first")?;
        item.dispositions = discovery::classify(req);
        item.plan = None;
        item.approval = None;
        item.set_phase(Phase::Classified);
        Ok(())
    }

    /// `plan()` — CLASSIFIED → PLAN_PROPOSED. Assemble the structured plan
    /// (ordered steps + requested grants + human-assist schedule) and bind it
    /// to a content `plan_hash`. Any later re-plan produces a new hash, which
    /// invalidates a prior approval (design §8). Requires CLASSIFIED.
    pub fn plan(&mut self, id: String) -> Result<(), String> {
        let item = self.find_mut(&id)?;
        if item.phase() != Phase::Classified {
            return Err(format!(
                "plan() requires phase CLASSIFIED, item is {}",
                item.phase().as_str()
            ));
        }
        let req = item
            .requirements
            .as_ref()
            .ok_or("item has no inferred requirements")?;
        item.plan = Some(discovery::build_plan(req, &item.dispositions));
        // A fresh plan supersedes any prior approval.
        item.approval = None;
        item.confirmed_steps.clear();
        item.set_phase(Phase::PlanProposed);
        Ok(())
    }

    /// `approve()` — PLAN_PROPOSED → APPROVED (THE HUMAN GATE, bearer-gated by
    /// the host). Binds to the EXACT `plan_hash` the human saw; a mismatch is
    /// refused so the human cannot be tricked into approving plan A and
    /// executing plan B. `granted` may be a NARROWED subset of the plan's
    /// requested grants (strike a grant before approving, design §7.2); an
    /// omitted `granted` means "all requested grants".
    pub fn approve(
        &mut self,
        id: String,
        plan_hash: String,
        principal: String,
        granted: Option<Vec<String>>,
    ) -> Result<(), String> {
        let item = self.find_mut(&id)?;
        if item.phase() != Phase::PlanProposed {
            return Err(format!(
                "approve() requires phase PLAN_PROPOSED, item is {}",
                item.phase().as_str()
            ));
        }
        let plan = item.plan.as_ref().ok_or("item has no plan to approve")?;
        if plan.plan_hash != plan_hash {
            return Err(format!(
                "stale approval: plan changed (approving {plan_hash}, current is {}); \
                 re-review the plan",
                plan.plan_hash
            ));
        }
        let principal = principal.trim().to_string();
        if principal.is_empty() {
            return Err("approve() requires a non-empty principal".into());
        }
        // Narrowing can only REMOVE requested grants, never add new ones.
        let granted = match granted {
            None => plan.requested_grants.clone(),
            Some(narrowed) => {
                for g in &narrowed {
                    if !plan.requested_grants.contains(g) {
                        return Err(format!(
                            "cannot grant {g:?}: it is not among the plan's requested grants \
                             (approval may only narrow, never widen)"
                        ));
                    }
                }
                narrowed
            }
        };
        item.approval = Some(Approval {
            plan_hash,
            principal,
            approved_at_ms: now_ms(),
            granted,
        });
        item.note = None;
        item.set_phase(Phase::Approved);
        Ok(())
    }

    /// `reject()` — terminal. From PLAN_PROPOSED (or APPROVED, to revoke a
    /// prior approval), record a reason and move to REJECTED. Bearer-gated.
    pub fn reject(&mut self, id: String, reason: String) -> Result<(), String> {
        let item = self.find_mut(&id)?;
        match item.phase() {
            Phase::PlanProposed | Phase::Approved | Phase::BlockedHuman => {}
            other => {
                return Err(format!(
                    "reject() requires phase PLAN_PROPOSED/APPROVED/BLOCKED_HUMAN, item is {}",
                    other.as_str()
                ));
            }
        }
        item.approval = None;
        item.note = Some(reason);
        item.set_phase(Phase::Rejected);
        Ok(())
    }

    /// `request_changes()` — send a proposed (or approved) plan back with a
    /// note. Re-opens the approval cycle: returns to DISCOVERED so the item is
    /// re-classified and re-planned (design §3). Bearer-gated.
    pub fn request_changes(&mut self, id: String, note: String) -> Result<(), String> {
        let item = self.find_mut(&id)?;
        match item.phase() {
            Phase::PlanProposed | Phase::Approved | Phase::Classified => {}
            other => {
                return Err(format!(
                    "request_changes() requires phase CLASSIFIED/PLAN_PROPOSED/APPROVED, item is {}",
                    other.as_str()
                ));
            }
        }
        item.plan = None;
        item.approval = None;
        item.confirmed_steps.clear();
        item.note = Some(note);
        item.set_phase(Phase::Discovered);
        Ok(())
    }

    /// `confirm()` — clear a per-step real-time checkpoint (design §7.2). A
    /// step marked `requires_confirm` (irreversible / credential-using) is a
    /// hard barrier; the human clears it by index in real time. Only meaningful
    /// on an APPROVED item whose approval is bound to the current plan.
    /// Bearer-gated. In the safe tier this is the *mechanism* — there is no
    /// real execution behind it (AC4+).
    pub fn confirm(&mut self, id: String, step_index: i64) -> Result<(), String> {
        let item = self.find_mut(&id)?;
        if item.phase() != Phase::Approved {
            return Err(format!(
                "confirm() requires phase APPROVED, item is {}",
                item.phase().as_str()
            ));
        }
        if !item.approval_is_current() {
            return Err("approval is stale (plan changed); re-approve before confirming".into());
        }
        let plan = item.plan.as_ref().ok_or("item has no plan")?;
        let idx = usize::try_from(step_index)
            .map_err(|_| format!("step index {step_index} out of range"))?;
        let step = plan
            .steps
            .get(idx)
            .ok_or_else(|| format!("no step at index {step_index}"))?;
        if !step.requires_confirm {
            return Err(format!(
                "step {step_index} does not require a confirm (read-only/reversible)"
            ));
        }
        if !item.confirmed_steps.contains(&step_index) {
            item.confirmed_steps.push(step_index);
        }
        Ok(())
    }

    /// `block()` — park an APPROVED/EXECUTING item awaiting a human (2FA /
    /// CAPTCHA / ambiguity). A first-class state, not an error (design §3).
    /// In the safe tier this is reachable for testing the BLOCKED_HUMAN
    /// off-ramp; the real triggers arrive with the substrate (AC5).
    pub fn block(&mut self, id: String, reason: String) -> Result<(), String> {
        let item = self.find_mut(&id)?;
        match item.phase() {
            Phase::Approved | Phase::BlockedHuman => {}
            other => {
                return Err(format!(
                    "block() requires phase APPROVED, item is {}",
                    other.as_str()
                ));
            }
        }
        item.note = Some(reason);
        item.set_phase(Phase::BlockedHuman);
        Ok(())
    }

    /// `resume()` — a human cleared the block; return to APPROVED so execution
    /// (a no-op in the safe tier) can proceed (design §3). Bearer-gated.
    pub fn resume(&mut self, id: String) -> Result<(), String> {
        let item = self.find_mut(&id)?;
        if item.phase() != Phase::BlockedHuman {
            return Err(format!(
                "resume() requires phase BLOCKED_HUMAN, item is {}",
                item.phase().as_str()
            ));
        }
        if !item.approval_is_current() {
            return Err("cannot resume: no current approval; re-approve the plan".into());
        }
        item.note = None;
        item.set_phase(Phase::Approved);
        Ok(())
    }

    /// `execute()` — THE NO-OP for the safe tier. Real execution (driving MCP
    /// tools / the browser through the substrate, with brokered credentials)
    /// is AC4–AC6 and DELIBERATELY NOT BUILT here. The invariant it enforces
    /// is the load-bearing one: NO execution without a current approval and
    /// every `requires_confirm` step cleared. It records "would execute …"
    /// and leaves the item APPROVED.
    pub fn execute(&mut self, id: String) -> Result<String, String> {
        let item = self.find_mut(&id)?;
        if item.phase() != Phase::Approved {
            return Err(format!(
                "execute() requires phase APPROVED, item is {}",
                item.phase().as_str()
            ));
        }
        if !item.approval_is_current() {
            return Err(
                "refusing to execute: approval does not bind the current plan hash \
                 (re-plan re-opens approval)"
                    .into(),
            );
        }
        let plan = item.plan.as_ref().ok_or("item has no plan")?;
        // Every step that requires a real-time confirm must already be cleared.
        let unconfirmed: Vec<usize> = plan
            .steps
            .iter()
            .enumerate()
            .filter(|(i, s)| s.requires_confirm && !item.confirmed_steps.contains(&(*i as i64)))
            .map(|(i, _)| i)
            .collect();
        if !unconfirmed.is_empty() {
            return Err(format!(
                "refusing to execute: steps {unconfirmed:?} require a per-step confirm() first"
            ));
        }
        let summary = format!(
            "would execute {} step(s) for {:?} (safe tier: no real action taken)",
            plan.steps.len(),
            item.text
        );
        item.result = Some(summary.clone());
        Ok(summary)
    }

    /// List every item (most-recently-updated first) as JSON the UI renders.
    /// Read-only.
    #[must_use]
    pub fn list_items(&self) -> Vec<Item> {
        let mut items = self.items.clone();
        items.sort_by_key(|i| std::cmp::Reverse(i.updated_at_ms.unwrap_or(i.created_at_ms)));
        items
    }

    /// Fetch a single item by id (for the approval card). Errors if absent.
    pub fn get_item(&self, id: String) -> Result<Item, String> {
        self.find(&id).cloned()
    }
}

fn now_ms() -> i64 {
    tangram::time::now_ms()
}

/// MCP instructions, shared between the native app builder and the WASM
/// component's `describe()` export.
const INSTRUCTIONS: &str = "An auto-completing TODO list (SAFE TIER): each item carries a gated \
     per-item agent lifecycle — DRAFTED → DISCOVERED → CLASSIFIED → \
     PLAN_PROPOSED → APPROVED (+ REJECTED / BLOCKED_HUMAN). discover/classify \
     produce REVIEWABLE DATA (inferred permissions, connections, \
     human-assistance, and a TOOL/BROWSER/NEEDS_HUMAN routing) — no \
     credentials, no browser, no execution. approve/reject/request_changes \
     and the per-step confirm gate are the human-in-the-loop controls; they \
     require Authorization: Bearer <TANGRAM_AUTH_TOKEN> when the host has a \
     token configured. execute() is a no-op that only records what it WOULD \
     do — real credentialed/browser execution is a later, separately-reviewed \
     tier.";

/// The auto-todo app, fully configured. Call `.serve()` to run it standalone
/// or `.build()` to mount it in a multi-app host.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub fn app() -> App<AutoTodo> {
    App::<AutoTodo>::new("auto-todo")
        .instructions(INSTRUCTIONS)
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
}

// Compiled for wasm32-wasip2, the same model + actions become a Tangram
// component (`tangram-host` owns the platform around it).
#[cfg(target_family = "wasm")]
tangram::export_component!(AutoTodo {
    name: "auto-todo",
    instructions: INSTRUCTIONS,
});

#[cfg(test)]
mod tests;
