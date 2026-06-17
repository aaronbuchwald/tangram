# Design: Smart Objects — one typed-graph primitive for the Tangram shell

**Status:** PROPOSED — approved direction (Handoff-2 reconciliation). This is the
**smart-objects design of record** for the `tangram` shell. It adapts the
Handoff-2 "smart objects" product handoff to **our** architecture: the storage
is a replicated **Automerge document** (the Vault's object store), the inline
surface is a **CodeMirror 6** atomic chip, and the runtime is a **wasm
component** + the existing host substrates — **not** an Obsidian plugin,
frontmatter store, or NDJSON sidecar.

Smart objects **generalize** the just-shipped embedded-runs Agent/Run inline-chip
system ([embedded-runs.md](embedded-runs.md)) into one typed graph primitive. We
build the primitive **ALONGSIDE** agents/runs — agents/runs keep working
unchanged, and converge onto this primitive in a **later** checkpoint (§7). This
doc owns the primitive, the locked Handoff-2 decisions, and the SO checkpoint
roadmap. SO1 (the foundation: the object store + the `@` chip + the basic popup +
typed links) is the first shippable slice and what this doc's "as built" notes
describe; SO2–SO4 are specified here as plans.

---

## 1. The primitive (LOCKED)

A **smart object** is a typed node in a replicated graph:

```
SmartObject {
  id:     string            // global, stable (a UUID); the graph key
  type:   string            // names a registered type (the type registry, §3)
  data:   string            // a JSON / opaque payload, shape owned by the type
  links:  ObjLink[]         // first-class, typed edges (may cross documents)
  render: string            // the inline/detail presentation hint (chip|panel|table|…)
  derive? // SO2 — a recompute rule (a derived object's data is computed)
  act?    // SOx — an action binding (maps to tangram-automation, §5)
}

ObjLink { rel: string, target: string /* an object id */, url?: string }
```

The **document holds only a reference** — a portable markdown link
`[<label>](obj://<id>)` in a note body (mirroring the embedded-runs
`[⚡ <agent>](agent://<id>)` handle). The **object store is the source of
truth**: the markdown carries only the id; everything else (`type`, `data`,
`links`, `render`) lives in the replicated store, keyed by `id`. The link
degrades gracefully — a renderer that doesn't know the `obj://` chip shows a
plain markdown link.

### The five roles

The single primitive expresses five roles by *how its fields are populated*. The
roles are a usage convention over one struct, not five structs:

| Role | Shape | Example |
|---|---|---|
| **definition** | `data` only (no `derive`/`act`); the canonical record of a thing | a recipe, a tag, a contact |
| **reference** | `data = {ref: <id>}` — points at another object | "this meal-plan slot references recipe X" |
| **derived** | carries `derive` (SO2) — `data` is *computed* from its `links`' targets | a grocery-list derived from a recipe's ingredients |
| **action** | carries `act` (SOx) — invoking it runs a pipeline (§5) | "add this grocery list to a cart" |
| **run** | an **immutable** object with `links: [{rel: "produced-by", target: <action-id>}]` — the record of one action execution | "cart-preview produced by the add-to-cart action at T" |

The **run role is the bridge to embedded-runs**: an embedded-runs Execution is
exactly a run-role smart object (`produced-by → action`), which is why the
convergence in §7 is a mapping, not a rewrite.

---

## 2. Platform mapping — Tangram-adapted (LOCKED)

The Handoff-2 handoff assumed Obsidian primitives. Each product decision maps
onto a Tangram pillar instead — the same discipline as embedded-runs §2:

| Handoff-2 product decision | Tangram-adapted mechanism |
|---|---|
| **Typed graph of objects** | a replicated `objects: Vec<SmartObject>` on the Vault (Automerge), NOT frontmatter / a `.ndjson` sidecar / a plugin store. A deterministic `Vec` (model `Default` must stay deterministic), `Option<T>` + `#[autosurgeon(missing)]` so older docs hydrate. |
| **Inline surface is the only resting state** | a **portable markdown link** `[<label>](obj://<id>)` in the note body — degrades to a plain link, NOT a bespoke token. The `<id>` is the object's UUID; the link is only the handle. |
| **Chip is atomic & click-to-edit** | a CM6 **atomic widget** — the replaced range is in `EditorView.atomicRanges`, so the cursor *steps over* it; a click opens the **object popup** (view/edit `type`/`data`/`links`). The chip is opaque by design — we do NOT reveal raw source on cursor entry. A per-type glyph distinguishes it from the agent `⚡` chip. |
| **Object store is the source of truth; the document holds references** | the markdown carries `{id}`; `type`/`data`/`links`/`render` live in the replicated store, keyed by id. |
| **Links are first-class, typed, may cross documents** | `links: Vec<ObjLink{rel, target, url?}>` — an edge in the graph, independent of where the chip sits. Cross-document is free: the target is an object id, not a doc offset. |
| **Reactivity (derived objects recompute)** | **SO2** (§4) — a topological recompute engine. Inert in SO1: an object's `data` is whatever was written. |
| **Action pipeline (explore→compile→run→verify→repair)** | **Build-3/SOx** (§5) — mapped onto the existing `tangram-automation` crate (browser/credential substrate), NOT a new runtime. |
| **Versioning** | **DEFERRED** — same posture as embedded-runs §4. SO1 objects reference types by name; no semver/pinning/snapshots. |
| **Secrets** | host-side only (ADR-0005) — any action-role egress injects credentials at the boundary; never in the object `data` or the replicated doc. |

---

## 3. SO1 — the foundation (this checkpoint, as built)

SO1 ships the primitive + store + the `@` inline surface + the basic popup +
typed links + the design doc. **No reactivity, no action pipeline, no rich
types.** Objects are **inert** (their `data` is whatever was written).

### Object store + model (component)

- `objects: Option<Vec<SmartObject>>` on the Vault (`apps/tangram/src/lib.rs`),
  `#[autosurgeon(missing = "Option::default")]`, seeded `Some(Vec::new())` in
  the deterministic `Default`. Mirrors the `invocations` index exactly.
- `SmartObject { id, obj_type, data, links: Vec<ObjLink>, render }`. The Rust
  field is `obj_type` (`type` is a keyword) serialized as `type` for the wire,
  matching the design's field name. `data` is an opaque string payload (JSON or
  plain text — the type owns the shape; SO1 does not parse it).
- `ObjLink { rel, target, url: Option<String> }` — `target` is an object id;
  `url` is the optional external href for an `obj://`-degraded link.
- Actions, mirroring the invocations index:
  - `create_object(id, obj_type, data, links, render)` — idempotent on `id`
    (a re-create overwrites). Defaults `render` to the type's default when blank.
  - `update_object(id, obj_type, data, links, render)` — edit in place; errors
    if absent.
  - `delete_object(id)` — remove by id; errors if absent.
  - `list_objects()` — the store, sorted by id (deterministic).
  - `reconcile_objects()` — prune objects whose `obj://<id>` reference no longer
    appears in any note body (stray-ref reconcile, exactly mirroring
    `reconcile_invocations` / `prune_orphan_invocations`). Runs on the agent tick
    too (folded into `tick_agents` so the existing host cadence drives it).
- A tiny **type registry** (`object_types()` read action + a `KNOWN_OBJECT_TYPES`
  table) with 1–2 trivial seed types so the end-to-end loop is demonstrable:
  - `note-ref` — a reference to another vault note (render `chip`).
  - `tag` — a generic label (render `chip`).
  Rich types (recipe, grocery-list, cart-preview) are **SO3**.

### The `@` type-picker + atomic chip + popup (UI)

- Typing `@` opens a **type-picker completion** listing the known types
  (`objectComplete.ts`, a completion source). It is added to the **EXISTING
  single `autocompletion({override: [...]})` array** in `editor.ts` — NEVER a
  second `autocompletion()` (a duplicate throws "Config merge conflict" and broke
  the editor before; see `editor.ts`'s comment + `editor.smoke.test.ts`).
- On select: mint a UUID, insert the atomic chip `[<label>](obj://<id>)`, and
  call `create_object` (`objectLink.ts` `buildObjectLink`).
- The chip renders as an **atomic widget** (`objectChip` in `objectLink.ts`,
  cloned from `agentChip`): `EditorView.atomicRanges` so the cursor steps over
  it; a per-type **glyph** (e.g. `◆`) distinct from the agent `⚡`; EOF-safe
  click hit-test (reusing `posOnToken` / `clickWithinRange` from `wikiLink.ts`).
- Click → a **basic object popup** (`objectPopup.ts`, mirroring `modal.ts` /
  `triggerPopup.ts`'s overlay): view/edit `type`, `data`, and `links`; **Save**
  → `update_object`; **Delete** → `delete_object` + strip the inline link.

### Typed links + reconcile

`links[]` are stored on the object (the graph source of truth); the `obj://<id>`
references in notes are the inline handles. Orphan prune runs on tick/edit like
the invocations index. SO1 links are **inert** — no reactivity reads them yet.

---

## 4. SO2 — the reactivity engine (PLANNED)

Make **derived** objects live. A derived object carries a `derive` rule; its
`data` is *computed* from its `links`' targets rather than written.

- **Topological recompute.** Build the dependency DAG from `links` (a derived
  object depends on each `target`). On a source change, recompute downstream in
  topological order. **Cycle detection** rejects a `derive` graph that would
  loop (fail closed, surface the cycle in the popup).
- **Push-in-doc / stale-cross-doc.** A change pushes an eager recompute to
  derived objects in the **same document**; cross-document dependents are marked
  **stale** and recompute lazily on next render/open (bounded blast radius — a
  vault-wide eager cascade is the failure mode we avoid).
- **Cached-inline derived.** A derived chip renders its last-computed value
  inline (cached), with a freshness marker; recompute updates the cache + chip.
- This is a **pure, in-component** engine (a state transition over the object
  store) — no egress, no I/O — so it stays wasm-clean and lock-discipline-safe.

The recipe→grocery-list reactive demo (§6) is the SO2 acceptance vehicle.

---

## 5. SOx / Build-3 — the action pipeline (PLANNED, maps to `tangram-automation`)

An **action**-role object, when invoked, runs the Handoff-2 pipeline
**explore → compile → run-supervised → verify → repair**, producing a **run**-role
object (`produced-by → action`). This maps onto **existing** substrates — it is
NOT a new runtime:

- **run-supervised** reuses `crates/tangram-automation` (ADR-0010): the
  supervised browser-driver runner (`runner.rs`), the browser egress gate
  (`egress.rs` over the shared `tangram-egress` canonicalizer), the `op://`
  credential broker (`broker.rs`), and the record→replay→validated-LLM-fallback
  engine (`script.rs`) — the same record/replay/validate spine the pipeline's
  explore/compile/repair stages need.
- The **request-not-grant** posture (`tangram-automation/src/request.rs`): the
  action declares an `AutomationRequest`; the operator policy intersects it. An
  action-role object never widens its own authority.
- Credentials are host-injected at the egress boundary (ADR-0005); the object
  `data` and the replicated doc never see them.

The cart-preview action (§6) is the pipeline's acceptance vehicle, gated on the
SO3 types existing.

---

## 6. The recipe golden-path (PLANNED — SO3 + SO4)

The Handoff-2 flagship demo, staged:

- **SO3 — recipe types + ingestion.** Register rich types: `recipe`,
  `grocery-list`, `cart-preview`. **Ingestion**: a `recipe` is created from a URL
  via a `tangram-automation` **browser fetch** + an **LLM normalization** to
  schema.org/Recipe (the AI-enabled-component pattern — fetch → prompt → write
  the normalized object to the store, not an egress of user data). **Ingredient
  canonicalization is the core risk** — mapping free-text ingredient lines to a
  canonical pantry vocabulary so a derived grocery-list can de-dupe/aggregate;
  this is where the design effort concentrates.
- **SO4 — the meal-plan mockup.** The reactive demo: a `recipe` →
  (derived, SO2) `grocery-list` → (action, §5) `cart-preview`, where editing the
  recipe's servings/ingredients **reactively** updates the grocery-list, and
  "add to cart" runs the action pipeline to a cart-preview run object. A UI
  mockup over the wired primitive.

---

## 7. Agent/Run convergence + versioning (LATER / PARALLEL)

- **Convergence.** Agents/Runs are the *first instances* of the smart-object
  roles: an **Agent definition** is a definition-role object, a **Run** is an
  action-role object (its mounted set + prompt = its `data`/`links`), and an
  **Execution** is a run-role object (`produced-by → Run`). A later checkpoint
  migrates the `invocations`/`executions` indexes onto the object store behind
  the unchanged user-facing surface — a **surgical re-home** (the same posture as
  embedded-runs' Trigger→Run relabel), NOT a rewrite. Until then both systems run
  side by side; SO1 does **not** touch agents/runs.
- **Versioning.** DEFERRED, exactly as embedded-runs §4. Objects reference types
  by name; no semver/pinning/snapshots this pass. The object id is the stable
  seam a later versioning pass builds on.

---

## 8. The SO checkpoint roadmap

| # | Checkpoint | Deliverables | Status |
|---|---|---|---|
| **SO1** | **Primitive foundation + reconciled doc** | this doc; the `objects` store + `SmartObject`/`ObjLink` model; `create/update/delete/list/reconcile_objects` + the type registry (seed `note-ref`/`tag`); the `@` type-picker (in the single autocompletion override); the generalized atomic `obj://` chip; the basic object popup; typed links + orphan reconcile. Objects are **inert**. | **THIS CHECKPOINT** |
| **SO2** | **Reactivity engine** | topological recompute, push-in-doc/stale-cross-doc, cached-inline derived, cycle detection (§4). | PLANNED |
| **SO3** | **Recipe types + ingestion** | `recipe`/`grocery-list`/`cart-preview` types; recipe ingestion via `tangram-automation` browser fetch + LLM schema.org/Recipe normalization; **ingredient canonicalization** (the core risk) (§6). | PLANNED |
| **SO4** | **Meal-plan mockup** | the reactive `recipe → grocery-list → cart-preview` demo over the wired primitive (§6). | PLANNED |
| — | **Agent/Run convergence + versioning** | re-home agents/runs onto the primitive behind the unchanged surface; versioning (§7). | LATER / PARALLEL |

The **action pipeline** (§5) lands with the action role — Build-3/SOx — alongside
SO3/SO4 (the cart-preview action needs the recipe types to act on).

---

*This doc is the source of truth for the smart-objects (typed-graph) primitive in
the shell. The primitive + 5 roles, the Tangram-adapted platform mapping, the
locked Handoff-2 decisions, and the SO1→SO4 staging are approved; **SO1 is the
shipped foundation** (the object store + `@` chip + basic popup + typed links).
Smart objects are built ALONGSIDE the embedded-runs Agent/Run system
([embedded-runs.md](embedded-runs.md)), which converges onto this primitive in a
later checkpoint (§7).*
