//! The Smart Objects **SO2 reactivity engine** (`docs/design/smart-objects.md`
//! §4). Makes DERIVED smart objects live: a derived object carries a
//! [`crate::DeriveSpec`] (`kind` + dependency object `deps`), and its `data` is
//! *computed* from its dependency objects rather than written. On any object
//! mutation the engine recomputes the affected derived subgraph in **topological
//! order**, **caches each result inline** in the object's `data` (in the doc, so
//! it stays readable/portable without the runtime), and **detects cycles** (a
//! cycle is surfaced as an error state on the involved objects, never an infinite
//! loop).
//!
//! ## The single-doc simplification (Tangram-adapted)
//!
//! The Handoff-2 spec split reactivity into "push an eager recompute within an
//! open document / mark cross-document dependents STALE and recompute lazily".
//! That split assumes Obsidian's many-files model. In Tangram ALL notes + objects
//! live in ONE replicated Automerge doc (the vault), so there is a **single
//! reactive graph**: we recompute the affected derived subgraph deterministically
//! on each mutation, in the component, and commit the cached results — every
//! replica converges (CRDT). There is **no separate cross-document staleness
//! tier**; that tier collapses into one in-doc eager recompute.
//!
//! ## Purity / determinism
//!
//! The whole engine is a pure, in-component state transition over the object
//! store — no egress, no I/O, no clock — so it stays wasm-clean and
//! lock-discipline-safe (it runs INSIDE the `Ctx::mutate` of the triggering
//! action, never across an await), and every replica computes byte-identical
//! cached results from the same graph.

use std::collections::BTreeMap;

use crate::{DeriveSpec, SmartObject};

/// Recompute every DERIVED object's cached `data` from its dependency objects, in
/// topological order, detecting cycles. The single entry point the [`crate::Vault`]
/// calls after every object mutation (create / update / delete / prune).
///
/// - **Plain objects** (no `derive`) are untouched; their `data` is whatever was
///   written (the SO1 inert model) and any stale `derive_error` on them is cleared.
/// - **Derived objects** in a dependency CYCLE are marked with a `derive_error`
///   and their recompute is SKIPPED (never loops); their cached `data` is left as
///   the last good value so the inline chip still shows something.
/// - **Healthy derived objects** are recomputed in dependency order (a dependency
///   that is itself derived is computed BEFORE its dependents), so a chain
///   A → B → C settles in one pass.
pub fn recompute(objs: &mut [SmartObject]) {
    // Index ids → position for O(1) dependency lookup; ids are unique (the store
    // keys by id). A BTreeMap keeps the engine deterministic regardless of the
    // store's vec order.
    let index: BTreeMap<String, usize> = objs
        .iter()
        .enumerate()
        .map(|(i, o)| (o.id.clone(), i))
        .collect();

    // The derived nodes (only these participate in the recompute graph). Each is
    // a (position, derive-spec snapshot) pair so we can borrow `objs` mutably
    // later without re-borrowing the spec.
    let derived: Vec<(usize, DeriveSpec)> = objs
        .iter()
        .enumerate()
        .filter_map(|(i, o)| o.derive.clone().map(|d| (i, d)))
        .collect();

    // Clear any stale derive_error on PLAIN objects (a node that used to be
    // derived-and-broken but is now plain should not keep an error chip).
    for o in objs.iter_mut() {
        if o.derive.is_none() {
            o.derive_error = None;
        }
    }

    if derived.is_empty() {
        return;
    }

    // ── topological sort over the derived subgraph (Kahn's algorithm) ─────────
    //
    // An edge goes from a dependency to the derived object that depends on it.
    // For ordering + cycle detection only the edges BETWEEN derived nodes matter
    // (a plain dependency has no outgoing derive edge, so it can never be part of
    // a cycle and is always "ready"). We compute, per derived node, how many of
    // its deps are THEMSELVES derived (the in-degree within the derived subgraph).
    let derived_pos: BTreeMap<usize, usize> = derived
        .iter()
        .enumerate()
        .map(|(slot, (pos, _))| (*pos, slot))
        .collect();

    // For each derived node (by slot), the slots of its derived dependencies.
    let mut dep_slots: Vec<Vec<usize>> = Vec::with_capacity(derived.len());
    let mut in_degree: Vec<usize> = vec![0; derived.len()];
    for (_, spec) in &derived {
        let mut slots = Vec::new();
        for dep_id in &spec.deps {
            if let Some(dep_pos) = index.get(dep_id)
                && let Some(&dep_slot) = derived_pos.get(dep_pos)
            {
                slots.push(dep_slot);
            }
        }
        slots.sort_unstable();
        slots.dedup();
        dep_slots.push(slots);
    }
    // in_degree[slot] = number of derived nodes that DEPEND ON this slot's node,
    // i.e. count of edges pointing OUT of it toward a dependent. Kahn processes a
    // node once all of its DEPENDENCIES are done, so the in-degree we decrement is
    // the count of unresolved dependencies. Compute that directly:
    for (slot, slots) in dep_slots.iter().enumerate() {
        in_degree[slot] = slots.len();
    }

    // Build the reverse adjacency: dependency-slot → [dependent slots] so that
    // resolving a node can decrement its dependents' in-degree.
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); derived.len()];
    for (slot, slots) in dep_slots.iter().enumerate() {
        for &dep_slot in slots {
            dependents[dep_slot].push(slot);
        }
    }

    // Kahn: seed the queue with every derived node that has no derived deps
    // (in-degree 0). Process in ascending slot order for determinism.
    let mut ready: Vec<usize> = (0..derived.len()).filter(|&s| in_degree[s] == 0).collect();
    ready.sort_unstable();
    let mut order: Vec<usize> = Vec::with_capacity(derived.len());
    while let Some(slot) = pop_min(&mut ready) {
        order.push(slot);
        for &dependent in &dependents[slot] {
            in_degree[dependent] -= 1;
            if in_degree[dependent] == 0 {
                ready.push(dependent);
            }
        }
    }

    // Any derived node NOT in `order` is part of (or downstream of) a cycle —
    // Kahn never drains a cycle. Mark those with a derive error and skip them.
    let in_order: std::collections::BTreeSet<usize> = order.iter().copied().collect();
    for (slot, (pos, _)) in derived.iter().enumerate() {
        if !in_order.contains(&slot) {
            objs[*pos].derive_error = Some(
                "dependency cycle: this derived object depends (directly or \
                 transitively) on itself; recompute skipped"
                    .to_string(),
            );
        }
    }

    // ── recompute the acyclic derived nodes in topological order ──────────────
    //
    // Each derived node's deps are read from the CURRENT store (so an upstream
    // derived dep computed earlier in this pass is already fresh). We resolve the
    // dependency snapshots, compute the new data, then write it back inline.
    for slot in order {
        let (pos, spec) = &derived[slot];
        let pos = *pos;

        // Resolve the dependency objects' snapshots (id, type, data) in the
        // spec's declared order; a dep id that no longer resolves is skipped
        // (a deleted dependency simply drops out of the aggregate).
        let deps: Vec<DepSnapshot> = spec
            .deps
            .iter()
            .filter_map(|dep_id| index.get(dep_id).map(|&i| &objs[i]))
            .map(|o| DepSnapshot {
                id: o.id.clone(),
                data: o.data.clone(),
            })
            .collect();

        match compute_derived(&spec.kind, &deps, spec.params.as_deref()) {
            Ok(data) => {
                objs[pos].data = data;
                objs[pos].derive_error = None;
            }
            Err(e) => {
                objs[pos].derive_error = Some(e);
            }
        }
    }
}

/// Pop the smallest element from `ready` (a tiny priority pop that keeps the
/// topological order deterministic — ties resolve by ascending slot).
fn pop_min(ready: &mut Vec<usize>) -> Option<usize> {
    let min_idx = (0..ready.len()).min_by_key(|&i| ready[i])?;
    Some(ready.swap_remove(min_idx))
}

/// A read-only snapshot of one dependency object passed to a derive computation
/// (the engine never lets a derive fn mutate the store — `derive` is a pure
/// `fn(deps) -> data`). SO2's `rollup` aggregates over `data`; the `id` is carried
/// for clear per-dependency error messages.
pub struct DepSnapshot {
    /// The dependency object's id (used in derive error messages).
    pub id: String,
    /// The dependency object's `data` payload (JSON or plain text).
    pub data: String,
}

/// The per-`kind` derive **registry** (Smart Objects SO2): a pure
/// `fn(deps, params) -> Result<data, error>`. Selecting on `kind` keeps the
/// computation in Rust (not in the doc) while the `DeriveSpec` in the doc names
/// only the kind + deps + params. An unknown kind is a derive error (surfaced on
/// the object, like a cycle). Add a new derived type by registering its kind here.
///
/// SO2 ships exactly one genuinely-derived kind, `rollup`, which aggregates a
/// numeric field (or a count, or a text concat) over its dependency objects.
pub fn compute_derived(
    kind: &str,
    deps: &[DepSnapshot],
    params: Option<&str>,
) -> Result<String, String> {
    match kind {
        "rollup" => rollup(deps, params),
        other => Err(format!("unknown derive kind {other:?}")),
    }
}

/// The `rollup` derive (Smart Objects SO2 — the proof-of-end-to-end derived
/// type): aggregate over the dependency objects and emit a JSON summary cached
/// inline. The optional `params` is a JSON object:
///
/// ```json
/// { "op": "sum" | "count" | "concat", "field": "<dep data field>" }
/// ```
///
/// - `count` (the default when `params` is absent/blank) → `{ "count": N }`.
/// - `sum` over `field`: parse each dep's `data` as a JSON object, read `field`
///   as a number, and sum → `{ "count": N, "sum": S, "field": "<field>" }`.
/// - `concat` over `field`: collect each dep's `field` (stringified) →
///   `{ "count": N, "values": [...], "field": "<field>" }`.
///
/// The output is deterministic JSON (stable key order via `serde_json` object
/// construction in a fixed sequence) so replicas converge byte-for-byte.
fn rollup(deps: &[DepSnapshot], params: Option<&str>) -> Result<String, String> {
    let cfg: RollupParams = match params {
        Some(p) if !p.trim().is_empty() => {
            serde_json::from_str(p).map_err(|e| format!("invalid rollup params: {e}"))?
        }
        _ => RollupParams::default(),
    };
    let count = deps.len();

    let out = match cfg.op.as_str() {
        "count" => serde_json::json!({ "op": "count", "count": count }),
        "sum" => {
            let field = require_field(&cfg, "sum")?;
            let mut sum = 0.0_f64;
            for d in deps {
                sum += dep_number(d, &field)?;
            }
            // Render the sum without a trailing `.0` for whole numbers so the
            // cached JSON is clean and stable.
            serde_json::json!({
                "op": "sum",
                "field": field,
                "count": count,
                "sum": number_value(sum),
            })
        }
        "concat" => {
            let field = require_field(&cfg, "concat")?;
            let values: Vec<String> = deps.iter().map(|d| dep_string(d, &field)).collect();
            serde_json::json!({
                "op": "concat",
                "field": field,
                "count": count,
                "values": values,
            })
        }
        other => return Err(format!("unknown rollup op {other:?} (sum|count|concat)")),
    };
    Ok(out.to_string())
}

/// The parsed `rollup` params. `op` defaults to `count`; `field` is required by
/// `sum`/`concat` and ignored by `count`.
#[derive(serde::Deserialize)]
struct RollupParams {
    #[serde(default = "default_op")]
    op: String,
    #[serde(default)]
    field: Option<String>,
}

impl Default for RollupParams {
    fn default() -> Self {
        Self {
            op: default_op(),
            field: None,
        }
    }
}

fn default_op() -> String {
    "count".to_string()
}

/// The required `field` for a `sum`/`concat` rollup, or a clear error.
fn require_field(cfg: &RollupParams, op: &str) -> Result<String, String> {
    cfg.field
        .as_ref()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .ok_or_else(|| format!("rollup op {op:?} requires a \"field\""))
}

/// Read a numeric `field` out of a dependency object's JSON `data`. A dep whose
/// data is not a JSON object, or whose field is missing/non-numeric, is an error
/// (surfaced on the derived object so the user fixes the source) — except an
/// EMPTY-data dep counts as 0 (a freshly-created, not-yet-filled source object
/// should not break the whole rollup).
fn dep_number(d: &DepSnapshot, field: &str) -> Result<f64, String> {
    if d.data.trim().is_empty() {
        return Ok(0.0);
    }
    let value: serde_json::Value = serde_json::from_str(&d.data).map_err(|_| {
        format!(
            "dependency {} data is not JSON (need a number in {field:?})",
            d.id
        )
    })?;
    match value.get(field) {
        Some(serde_json::Value::Number(n)) => n
            .as_f64()
            .ok_or_else(|| format!("dependency {} field {field:?} is not a finite number", d.id)),
        Some(serde_json::Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("dependency {} field {field:?} is not numeric", d.id)),
        _ => Err(format!(
            "dependency {} has no numeric field {field:?}",
            d.id
        )),
    }
}

/// Read a `field` from a dependency's JSON `data` as a display string (best
/// effort): a JSON string yields its contents, any other JSON value is
/// stringified, a missing field / non-JSON data yields the raw data trimmed.
fn dep_string(d: &DepSnapshot, field: &str) -> String {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&d.data)
        && let Some(v) = map.get(field)
    {
        return match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    d.data.trim().to_string()
}

/// Render an `f64` sum as a JSON number that drops a redundant `.0` for whole
/// values (so `3.0` caches as `3`), keeping the cached payload clean + stable.
fn number_value(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        serde_json::json!(n as i64)
    } else {
        serde_json::json!(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeriveSpec, ObjLink, SmartObject};

    /// A plain (definition) source object carrying JSON `data`.
    fn source(id: &str, data: &str) -> SmartObject {
        SmartObject {
            id: id.to_string(),
            obj_type: "tag".to_string(),
            data: data.to_string(),
            links: Vec::new(),
            render: "chip".to_string(),
            derive: None,
            derive_error: None,
        }
    }

    /// A derived object of `kind` over `deps` with optional `params`.
    fn derived(id: &str, kind: &str, deps: &[&str], params: Option<&str>) -> SmartObject {
        SmartObject {
            id: id.to_string(),
            obj_type: "rollup".to_string(),
            data: String::new(),
            links: deps
                .iter()
                .map(|t| ObjLink {
                    rel: "depends-on".to_string(),
                    target: (*t).to_string(),
                    url: None,
                })
                .collect(),
            render: "chip".to_string(),
            derive: Some(DeriveSpec {
                kind: kind.to_string(),
                deps: deps.iter().map(|s| (*s).to_string()).collect(),
                params: params.map(str::to_string),
            }),
            derive_error: None,
        }
    }

    fn data_of<'a>(objs: &'a [SmartObject], id: &str) -> &'a str {
        &objs.iter().find(|o| o.id == id).unwrap().data
    }
    fn error_of<'a>(objs: &'a [SmartObject], id: &str) -> Option<&'a str> {
        objs.iter()
            .find(|o| o.id == id)
            .unwrap()
            .derive_error
            .as_deref()
    }

    #[test]
    fn rollup_count_over_sources() {
        let mut objs = vec![
            source("a", "{}"),
            source("b", "{}"),
            derived("roll", "rollup", &["a", "b"], None),
        ];
        recompute(&mut objs);
        let v: serde_json::Value = serde_json::from_str(data_of(&objs, "roll")).unwrap();
        assert_eq!(v["op"], "count");
        assert_eq!(v["count"], 2);
        assert_eq!(error_of(&objs, "roll"), None);
    }

    #[test]
    fn rollup_sum_aggregates_a_numeric_field() {
        let mut objs = vec![
            source("a", "{\"qty\": 2}"),
            source("b", "{\"qty\": 3}"),
            source("c", "{\"qty\": 5}"),
            derived(
                "roll",
                "rollup",
                &["a", "b", "c"],
                Some("{\"op\":\"sum\",\"field\":\"qty\"}"),
            ),
        ];
        recompute(&mut objs);
        let v: serde_json::Value = serde_json::from_str(data_of(&objs, "roll")).unwrap();
        assert_eq!(v["sum"], 10);
        assert_eq!(v["count"], 3);
    }

    #[test]
    fn recompute_runs_on_dependency_mutation() {
        let mut objs = vec![
            source("a", "{\"qty\": 2}"),
            derived(
                "roll",
                "rollup",
                &["a"],
                Some("{\"op\":\"sum\",\"field\":\"qty\"}"),
            ),
        ];
        recompute(&mut objs);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data_of(&objs, "roll")).unwrap()["sum"],
            2
        );
        // Mutate the source; a fresh recompute reflects it (the live cache update).
        objs[0].data = "{\"qty\": 9}".to_string();
        recompute(&mut objs);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data_of(&objs, "roll")).unwrap()["sum"],
            9
        );
    }

    #[test]
    fn topological_chain_a_b_c_settles_in_one_pass() {
        // a (source qty=4) → B (sum over a) → C (sum over B's `sum`).
        // B caches {"sum": 4, ...}; C sums B's `sum` field ⇒ 4.
        let mut objs = vec![
            source("a", "{\"qty\": 4}"),
            derived(
                "B",
                "rollup",
                &["a"],
                Some("{\"op\":\"sum\",\"field\":\"qty\"}"),
            ),
            derived(
                "C",
                "rollup",
                &["B"],
                Some("{\"op\":\"sum\",\"field\":\"sum\"}"),
            ),
        ];
        recompute(&mut objs);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data_of(&objs, "B")).unwrap()["sum"],
            4
        );
        // C must see B's FRESH value within the same pass (topological order).
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data_of(&objs, "C")).unwrap()["sum"],
            4
        );
        assert_eq!(error_of(&objs, "C"), None);
    }

    #[test]
    fn cycle_detection_flags_error_and_terminates() {
        // A two-node cycle: X depends on Y, Y depends on X. The engine must flag
        // both with a derive_error and NOT loop.
        let mut objs = vec![
            derived("X", "rollup", &["Y"], None),
            derived("Y", "rollup", &["X"], None),
        ];
        recompute(&mut objs); // must terminate
        assert!(error_of(&objs, "X").unwrap().contains("cycle"));
        assert!(error_of(&objs, "Y").unwrap().contains("cycle"));
    }

    #[test]
    fn self_cycle_is_flagged() {
        let mut objs = vec![derived("S", "rollup", &["S"], None)];
        recompute(&mut objs);
        assert!(error_of(&objs, "S").unwrap().contains("cycle"));
    }

    #[test]
    fn acyclic_nodes_recompute_even_when_a_cycle_exists() {
        // A healthy rollup over a plain source must still compute even though an
        // unrelated cycle is present (the cycle is isolated, not contagious).
        let mut objs = vec![
            source("a", "{}"),
            derived("ok", "rollup", &["a"], None),
            derived("X", "rollup", &["Y"], None),
            derived("Y", "rollup", &["X"], None),
        ];
        recompute(&mut objs);
        assert_eq!(error_of(&objs, "ok"), None);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data_of(&objs, "ok")).unwrap()["count"],
            1
        );
        assert!(error_of(&objs, "X").is_some());
    }

    #[test]
    fn unknown_kind_is_a_derive_error() {
        let mut objs = vec![derived("d", "nonsense", &[], None)];
        recompute(&mut objs);
        assert!(
            error_of(&objs, "d")
                .unwrap()
                .contains("unknown derive kind")
        );
    }

    #[test]
    fn deleted_dependency_drops_out_and_recomputes() {
        let mut objs = vec![
            source("a", "{\"qty\": 2}"),
            source("b", "{\"qty\": 3}"),
            derived(
                "roll",
                "rollup",
                &["a", "b"],
                Some("{\"op\":\"sum\",\"field\":\"qty\"}"),
            ),
        ];
        recompute(&mut objs);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(data_of(&objs, "roll")).unwrap()["sum"],
            5
        );
        // Remove dependency `b`; the rollup recomputes over the survivor only.
        objs.retain(|o| o.id != "b");
        recompute(&mut objs);
        let v: serde_json::Value = serde_json::from_str(data_of(&objs, "roll")).unwrap();
        assert_eq!(v["sum"], 2);
        assert_eq!(v["count"], 1);
    }

    #[test]
    fn plain_objects_are_untouched() {
        let mut objs = vec![source("a", "hello"), source("b", "{\"x\":1}")];
        recompute(&mut objs);
        assert_eq!(data_of(&objs, "a"), "hello");
        assert_eq!(data_of(&objs, "b"), "{\"x\":1}");
        assert_eq!(error_of(&objs, "a"), None);
    }

    #[test]
    fn becoming_plain_clears_a_prior_error() {
        // An object that was a broken derived (carrying an error) but is now plain
        // must have its derive_error cleared.
        let mut obj = source("p", "x");
        obj.derive_error = Some("old cycle".to_string());
        let mut objs = vec![obj];
        recompute(&mut objs);
        assert_eq!(error_of(&objs, "p"), None);
    }

    #[test]
    fn sum_renders_whole_numbers_without_trailing_decimal() {
        let mut objs = vec![
            source("a", "{\"qty\": 1.5}"),
            source("b", "{\"qty\": 1.5}"),
            derived(
                "roll",
                "rollup",
                &["a", "b"],
                Some("{\"op\":\"sum\",\"field\":\"qty\"}"),
            ),
        ];
        recompute(&mut objs);
        // 1.5 + 1.5 = 3 ⇒ rendered as the integer 3, not 3.0.
        assert!(data_of(&objs, "roll").contains("\"sum\":3"));
    }
}
