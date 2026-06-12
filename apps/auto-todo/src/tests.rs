//! State-machine tests for the safe tier (AC1). The lifecycle is a PURE
//! function of registered actions; these tests assert the invariants
//! (illegal transitions rejected; approval binds to the plan hash; re-plan
//! re-opens approval; revoke/confirm bookkeeping) without any LLM, credential,
//! or browser involvement.

use super::*;

const WHO: &str = "operator@example.com";

/// Drive an item through discover → classify → plan and return its id.
fn to_plan_proposed(app: &mut AutoTodo, text: &str) -> String {
    let id = app.add_item(text.into()).expect("add");
    app.discover(id.clone()).expect("discover");
    app.classify(id.clone()).expect("classify");
    app.plan(id.clone()).expect("plan");
    assert_eq!(app.get_item(id.clone()).unwrap().phase, "PLAN_PROPOSED");
    id
}

fn plan_hash(app: &AutoTodo, id: &str) -> String {
    app.get_item(id.to_string())
        .unwrap()
        .plan
        .expect("plan")
        .plan_hash
}

#[test]
fn add_item_starts_drafted() {
    let mut app = AutoTodo::default();
    let id = app.add_item("renew my domain".into()).unwrap();
    let item = app.get_item(id).unwrap();
    assert_eq!(item.phase, "DRAFTED");
    assert!(item.requirements.is_none());
    assert!(item.plan.is_none());
    assert!(app.add_item("   ".into()).is_err(), "blank rejected");
}

#[test]
fn happy_path_to_approved_and_execute_noop() {
    let mut app = AutoTodo::default();
    let id = to_plan_proposed(&mut app, "what's on my calendar Tuesday");
    let h = plan_hash(&app, &id);
    app.approve(id.clone(), h.clone(), WHO.into(), None)
        .unwrap();
    assert_eq!(app.get_item(id.clone()).unwrap().phase, "APPROVED");

    // execute() is a no-op that records "would execute" and stays APPROVED.
    let result = app.execute(id.clone()).unwrap();
    assert!(result.contains("would execute"), "{result}");
    let item = app.get_item(id).unwrap();
    assert_eq!(item.phase, "APPROVED");
    assert!(item.result.unwrap().contains("no real action taken"));
}

#[test]
fn transitions_require_their_predecessor_phase() {
    let mut app = AutoTodo::default();
    let id = app.add_item("my calendar agenda".into()).unwrap();
    // Out-of-order calls are rejected.
    assert!(
        app.classify(id.clone()).is_err(),
        "classify before discover"
    );
    assert!(app.plan(id.clone()).is_err(), "plan before classify");
    assert!(
        app.approve(id.clone(), "x".into(), WHO.into(), None)
            .is_err(),
        "approve before plan"
    );
    assert!(app.execute(id.clone()).is_err(), "execute before approve");
    assert!(
        app.confirm(id.clone(), 0).is_err(),
        "confirm before approve"
    );
    assert!(app.resume(id).is_err(), "resume when not blocked");
}

#[test]
fn approve_binds_to_plan_hash() {
    let mut app = AutoTodo::default();
    let id = to_plan_proposed(&mut app, "my calendar agenda");
    // A wrong hash is refused (cannot be tricked into approving plan A,
    // executing plan B).
    assert!(
        app.approve(id.clone(), "deadbeef".into(), WHO.into(), None)
            .is_err(),
        "stale/wrong hash refused"
    );
    let h = plan_hash(&app, &id);
    app.approve(id.clone(), h.clone(), WHO.into(), None)
        .unwrap();
    let approval = app.get_item(id).unwrap().approval.unwrap();
    assert_eq!(approval.plan_hash, h);
    assert_eq!(approval.principal, WHO);
}

#[test]
fn approve_requires_principal() {
    let mut app = AutoTodo::default();
    let id = to_plan_proposed(&mut app, "my calendar agenda");
    let h = plan_hash(&app, &id);
    assert!(
        app.approve(id, h, "   ".into(), None).is_err(),
        "empty principal rejected"
    );
}

#[test]
fn replan_reopens_approval() {
    let mut app = AutoTodo::default();
    let id = to_plan_proposed(&mut app, "my calendar agenda");
    let h = plan_hash(&app, &id);
    app.approve(id.clone(), h, WHO.into(), None).unwrap();
    assert_eq!(app.get_item(id.clone()).unwrap().phase, "APPROVED");

    // request_changes sends it back to DISCOVERED and drops the approval.
    app.request_changes(id.clone(), "narrow this".into())
        .unwrap();
    let item = app.get_item(id.clone()).unwrap();
    assert_eq!(item.phase, "DISCOVERED");
    assert!(item.approval.is_none());
    assert!(item.plan.is_none());

    // A fresh classify+plan must be re-approved before execute.
    app.classify(id.clone()).unwrap();
    app.plan(id.clone()).unwrap();
    assert!(
        app.execute(id.clone()).is_err(),
        "cannot execute a re-planned item without re-approval"
    );
}

#[test]
fn rediscover_invalidates_downstream_state() {
    let mut app = AutoTodo::default();
    let id = to_plan_proposed(&mut app, "my calendar agenda");
    let h = plan_hash(&app, &id);
    app.approve(id.clone(), h, WHO.into(), None).unwrap();

    // Re-running discover() from a later phase resets to DISCOVERED and clears
    // dispositions/plan/approval.
    app.discover(id.clone()).unwrap();
    let item = app.get_item(id).unwrap();
    assert_eq!(item.phase, "DISCOVERED");
    assert!(item.dispositions.is_empty());
    assert!(item.plan.is_none());
    assert!(item.approval.is_none());
}

#[test]
fn narrowing_grants_can_only_remove() {
    let mut app = AutoTodo::default();
    let id = to_plan_proposed(&mut app, "my calendar agenda");
    let h = plan_hash(&app, &id);
    // Granting something not requested is rejected (narrow, never widen).
    assert!(
        app.approve(
            id.clone(),
            h.clone(),
            WHO.into(),
            Some(vec!["tool:Evil.delete (write)".into()])
        )
        .is_err(),
        "cannot widen grants"
    );
    // Narrowing to a subset (here: empty) is allowed.
    app.approve(id.clone(), h, WHO.into(), Some(vec![]))
        .unwrap();
    assert!(
        app.get_item(id)
            .unwrap()
            .approval
            .unwrap()
            .granted
            .is_empty()
    );
}

#[test]
fn reject_is_terminal_and_records_reason() {
    let mut app = AutoTodo::default();
    let id = to_plan_proposed(&mut app, "my calendar agenda");
    app.reject(id.clone(), "not now".into()).unwrap();
    let item = app.get_item(id.clone()).unwrap();
    assert_eq!(item.phase, "REJECTED");
    assert_eq!(item.note.as_deref(), Some("not now"));
    assert!(item.approval.is_none());
    // No advancing out of REJECTED.
    assert!(app.classify(id.clone()).is_err());
    assert!(app.execute(id).is_err());
}

#[test]
fn confirm_gates_irreversible_steps_before_execute() {
    let mut app = AutoTodo::default();
    // "renew my domain" → irreversible/purchase → a NEEDS_HUMAN step that
    // requires_confirm. Drive to APPROVED.
    let id = to_plan_proposed(&mut app, "renew my domain");
    let plan = app.get_item(id.clone()).unwrap().plan.unwrap();
    let confirm_idx: Vec<i64> = plan
        .steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.requires_confirm)
        .map(|(i, _)| i as i64)
        .collect();
    assert!(
        !confirm_idx.is_empty(),
        "an irreversible item must produce at least one confirm-gated step"
    );
    let h = plan.plan_hash.clone();
    app.approve(id.clone(), h, WHO.into(), None).unwrap();

    // execute() refuses while a requires_confirm step is uncleared.
    assert!(
        app.execute(id.clone()).is_err(),
        "execute refused before per-step confirm"
    );
    // A read-only/reversible step cannot be confirmed (nothing to confirm).
    if let Some((reversible_idx, _)) = plan
        .steps
        .iter()
        .enumerate()
        .find(|(_, s)| !s.requires_confirm)
    {
        assert!(
            app.confirm(id.clone(), reversible_idx as i64).is_err(),
            "no confirm needed for a read-only step"
        );
    }
    // Clear every confirm-gated step, then execute (the no-op) succeeds.
    for i in confirm_idx {
        app.confirm(id.clone(), i).unwrap();
    }
    assert!(app.execute(id).is_ok(), "execute after all confirms");
}

#[test]
fn blocked_human_round_trip() {
    let mut app = AutoTodo::default();
    let id = to_plan_proposed(&mut app, "my calendar agenda");
    let h = plan_hash(&app, &id);
    app.approve(id.clone(), h, WHO.into(), None).unwrap();
    app.block(id.clone(), "waiting on 2FA".into()).unwrap();
    assert_eq!(app.get_item(id.clone()).unwrap().phase, "BLOCKED_HUMAN");
    // Cannot execute while blocked; resume returns to APPROVED.
    assert!(app.execute(id.clone()).is_err());
    app.resume(id.clone()).unwrap();
    assert_eq!(app.get_item(id).unwrap().phase, "APPROVED");
}

#[test]
fn delete_and_missing_id_errors() {
    let mut app = AutoTodo::default();
    let id = app.add_item("x my calendar".into()).unwrap();
    app.delete_item(id.clone()).unwrap();
    assert!(app.delete_item(id.clone()).is_err());
    assert!(app.discover(id.clone()).is_err());
    assert!(app.get_item(id).is_err());
}

#[test]
fn list_items_sorts_recent_first() {
    let mut app = AutoTodo::default();
    let a = app.add_item("my calendar one".into()).unwrap();
    let b = app.add_item("my calendar two".into()).unwrap();
    // Touch `a` so it sorts ahead of `b`.
    app.discover(a.clone()).unwrap();
    let listed = app.list_items();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, a);
    assert_eq!(listed[1].id, b);
}

// --- AC2: discovery + classification (read-only, rule-based) ---

#[test]
fn discover_infers_reviewable_requirements() {
    let mut app = AutoTodo::default();
    let id = app
        .add_item("what's on my calendar Tuesday".into())
        .unwrap();
    app.discover(id.clone()).unwrap();
    let req = app.get_item(id).unwrap().requirements.unwrap();
    assert!(req.capabilities.iter().any(|c| c == "calendar.read"));
    assert_eq!(req.irreversibility, "none");
    assert!(req.confidence > 0.5);
}

#[test]
fn classify_routes_readonly_to_tool() {
    let disp = classify_text("what's on my calendar Tuesday");
    assert!(
        disp.iter()
            .any(|d| d.kind == "TOOL" && d.read_only && d.tool.is_some()),
        "read-only calendar query routes to a read-only TOOL: {disp:?}"
    );
}

#[test]
fn classify_routes_irreversible_to_needs_human() {
    let disp = classify_text("renew my domain");
    assert!(
        disp.iter().all(|d| d.kind != "TOOL"),
        "renewing a domain must NOT be a safe-tier tool: {disp:?}"
    );
    assert!(
        disp.iter().any(|d| d.kind == "NEEDS_HUMAN"),
        "irreversible purchase routes to NEEDS_HUMAN: {disp:?}"
    );
}

#[test]
fn unrecognized_item_lands_low_confidence_and_needs_human() {
    let mut app = AutoTodo::default();
    let id = app.add_item("ponder the nature of being".into()).unwrap();
    app.discover(id.clone()).unwrap();
    let req = app.get_item(id.clone()).unwrap().requirements.unwrap();
    assert!(req.capabilities.is_empty());
    assert!(
        req.confidence < 0.5,
        "no concrete capability → low confidence"
    );
    app.classify(id.clone()).unwrap();
    let disp = app.get_item(id).unwrap().dispositions;
    assert_eq!(disp.len(), 1);
    assert_eq!(disp[0].kind, "NEEDS_HUMAN");
}

// --- AC3: plan hash stability / re-plan ---

#[test]
fn plan_hash_is_stable_for_same_input() {
    let p1 = plan_from("what's on my calendar Tuesday");
    let p2 = plan_from("what's on my calendar Tuesday");
    assert_eq!(p1.plan_hash, p2.plan_hash, "deterministic hash");
    let p3 = plan_from("renew my domain");
    assert_ne!(p1.plan_hash, p3.plan_hash, "different plans differ");
}

#[test]
fn readonly_tool_steps_need_no_confirm() {
    let plan = plan_from("what's on my calendar Tuesday");
    assert!(
        plan.steps.iter().any(|s| s.kind == "TOOL"),
        "expected a tool step"
    );
    assert!(
        plan.steps
            .iter()
            .filter(|s| s.kind == "TOOL")
            .all(|s| !s.requires_confirm),
        "read-only tool steps are frictionless (no per-step confirm)"
    );
}
