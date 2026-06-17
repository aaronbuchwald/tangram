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
describe; SO2 (reactivity), SO3 (recipe types + the reactive chain), SO4
(recipe URL ingestion), and SO5 (the §8 meal-plan mockup + the cart-fill stub)
are **built** — **Build-1 is COMPLETE**. The LIVE cart-fill action pipeline (the
`tangram-automation` browser + credential tier) is deferred to **Build-3** (§5).

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
  derive? // SO2 (built) — DeriveSpec{kind,deps,params}; a derived object's data is computed (cached inline)
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
| **Reactivity (derived objects recompute)** | **SO2 (built)** (§4) — a topological recompute engine in the component (`reactive.rs`), cached inline in the doc, cycle-detecting. The Handoff-2 push-in-doc/stale-cross-doc split **collapses** to one in-doc graph (single replicated doc; §4). Inert in SO1. |
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
    (SO2 adds a trailing `derive` arg + triggers a reactive recompute.)
  - `update_object(id, obj_type, data, links, render)` — edit in place; errors
    if absent. (SO2 adds a trailing `derive` arg + triggers a reactive recompute.)
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

## 4. SO2 — the reactivity engine (AS BUILT)

**Derived** objects are now live. A derived object carries a `derive` descriptor;
its `data` is *computed* from its dependency objects rather than written, cached
inline, and recomputed on every mutation. **As shipped:**

- **The `derive` descriptor on the model.** `SmartObject` gained
  `derive: Option<DeriveSpec>` + `derive_error: Option<String>` (both
  `#[autosurgeon(missing)]`, deterministic defaults — agents/runs + SO1 plain
  objects are unaffected). `DeriveSpec { kind, deps, params }` names a typed
  derive `kind` + the dependency object ids + optional kind params; the actual
  computation is a pure Rust `fn(deps) -> data` in the derive registry
  (`reactive::compute_derived`), since `derive` is pure.
- **Topological recompute, in the component.** `reactive::recompute`
  (`apps/tangram/src/reactive.rs`) builds the dependency DAG from the derived
  objects' `deps`, topologically sorts it (Kahn's algorithm), and recomputes each
  derived object's `data` in dependency order — so a chain A → B → C settles in
  one pass. It runs in the SAME `Ctx::mutate` as every object mutation
  (`create/update/delete_object` + `reconcile_objects`/the agent-tick prune), so
  the cached results commit atomically with the change.
- **Cycle detection (never loops).** A node Kahn can't drain is in (or downstream
  of) a cycle; the engine marks each such derived object with a `derive_error`
  (cached in the doc) and SKIPS its recompute — it terminates, and the cycle
  surfaces as an error chip + a red banner in the popup (§2). An unrelated cycle
  is isolated: healthy derived objects still recompute.
- **Cached-inline derived.** The computed `data` is written back into the
  object's `data` (in the doc), so a derived chip renders its last-computed value
  inline WITHOUT the runtime (portable/readable). The chip shows the value + a
  "derived / auto" `↻` badge; it updates live when a dependency changes (the
  state frame pushes object updates, and a `refreshObjectChips` editor effect
  rebuilds the chips even when the local doc body is unchanged — e.g. a recompute
  driven by another replica).
- **A real derived type.** `rollup` (registered in `KNOWN_OBJECT_TYPES`)
  aggregates over its dependency objects — `count`, `sum` over a numeric `field`,
  or `concat` over a field. The deterministic `Default` (genesis) seeds a working
  demo: a `smart-objects-demo.md` note with two source `tag` chips
  (`demo-apples` qty 3, `demo-oranges` qty 5) and a derived `rollup` chip
  (`demo-total`) summing them to 8 — edit a source's `qty` and the total
  recomputes live in the doc.
- **Pure + deterministic.** No egress, no I/O, no clock — wasm-clean,
  lock-discipline-safe, and byte-identical across replicas (the engine runs at
  genesis too, so the seeded cache is part of the shared commit).

### The single-doc simplification (Tangram-adapted)

The Handoff-2 handoff split reactivity into a **push within an open document** /
**stale-then-refresh across documents** tier. That split assumes Obsidian's
many-files model. In Tangram **all notes + objects live in ONE replicated
Automerge doc** (the vault), so there is a **single reactive graph**: SO2
recomputes the affected derived subgraph deterministically on each mutation, in
the component, and commits the cached results — every replica converges (CRDT).
**There is no separate cross-document staleness tier**; that tier collapses into
one in-doc eager recompute. (The UI's `refreshObjectChips` nudge is the only
"refresh" left, and it is a render-side concern — the data is already fresh in
the doc.)

The recipe → grocery-list reactive demo (§6) remains the SO3/SO4 acceptance
vehicle; SO2's `rollup` demo proves the engine end to end now.

### Seeding the demos onto existing vaults — the `seed_demos` backfill (#111)

The demos (the SO2 `smart-objects-demo.md` rollup note + the SO3
`meal-plan-demo.md` chain, with their backing objects) are seeded by the
deterministic `Default` — but `Default` is the **genesis commit only**. A vault
whose Automerge doc was created *before* a demo existed never receives it: a new
`Default` seed does not retro-apply to an already-created replicated doc. So the
live vault had no `smart-objects-demo.md` / `meal-plan-demo.md`.

The fix is an **idempotent backfill**, component-only:

- **Marker.** `Vault.demos_seeded: Option<bool>` (`#[autosurgeon(missing =
  "Option::default")]`). A fresh `Default` already carries the demos, so it is
  born `Some(true)` (sealed — it must NOT re-seed). An older doc hydrates `None`
  → eligible for backfill.
- **One source of truth.** `Default` and the backfill both build the demo
  objects/files from the shared `demo_seed_objects()` / `demo_seed_files()`
  builders (extracted from the old inline `Default`), so a fresh vault and a
  backfilled one are **byte-identical** (same ids, paths, derive specs, and —
  after the post-seed `reactive::recompute` — the same cached derived values).
- **`seed_demos` (idempotent).** When `demos_seeded != Some(true)`, it adds only
  the demo files/objects whose stable id is **absent** (never clobbering a
  user-edited demo), runs the reactivity recompute so the derived demo objects
  carry their caches, then **seals** `demos_seeded = Some(true)`. Once sealed it
  is a no-op — so **deleting a demo afterwards does NOT bring it back** (the #111
  requirement). Exposed as an action (manual trigger + tests).
- **Auto-invoke.** The first `tick_agents` (already driven host-side on a ~60s
  cadence by `tangram-host/src/scheduler.rs`) runs the backfill in its own
  lock-safe `Ctx::mutate` before the orphan-prune — so an existing vault
  backfills within one scheduler tick of the next restart, with no host change.
  After the first run the marker is sealed and the per-tick cost is one flag
  check.

Redeploy: rebuild the `tangram` wasm component + restart `tangram-host`; the
backfill then runs on the first tick.

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
SO3 types existing. SO5 shipped the **review-only STUB** of this action
(`fill_cart`): the §8 "Fill Whole Foods cart" button streams the phase names
(Explore → Compile → Run → Verify) and ends on a lock-icon "nothing purchased"
terminus, but the action is pure/read-only — it touches none of the substrates
above and **never purchases**. Build-3 replaces the stub with the live pipeline
behind the same §8 surface.

---

## 6. The recipe golden-path (SO3 AS BUILT — SO4 PLANNED)

The Handoff-2 flagship demo, staged:

- **SO3 — recipe types + the reactive chain (AS BUILT).** Registered the three
  rich types (`recipe`, `grocery-list`, `cart-preview` in `KNOWN_OBJECT_TYPES`)
  and the two new **derive kinds** in `reactive::grocery` (pure, deterministic,
  wasm-clean `fn(deps) -> data`, driven by SO2's topological engine):
  - **`recipe`** is a DEFINITION type (manual JSON entry for SO3; URL ingestion
    is SO4). `data = { name, servings, ingredients: [{canonicalName, quantity,
    unit, category}], source? }`.
  - **`grocery-list`** is DERIVED over its **included** `recipe` deps: it groups
    ingredient lines by `canonicalName + reconciled-unit`, **sums quantity**, and
    collects the source recipe names. **Unit reconciliation** (the core risk):
    units of the same dimension convert to a canonical unit and merge (volume
    tsp→tbsp→cup, mass mg→g→kg, count → ct); incompatible/unknown units for the
    same ingredient stay as **separate rows** (no guessing). Output
    `{ rows: [{ name, quantity, unit, category, sources }] }`.
  - **`cart-preview`** is DERIVED over its `grocery-list` dep: it groups the rows
    by `category` (aisle) → `{ aisles: [{ category, items }] }`. The terminus of
    Build-1.

  The reactive demo ships as the seeded **`meal-plan-demo.md`** note (genesis,
  deterministic): three recipes whose ingredients deliberately overlap (olive oil
  2tbsp + 2tbsp + 1tbsp = 5tbsp; onion across two; tomato across two), a derived
  grocery-list over all three, and a cart-preview over the grocery-list. A
  recipe's chip popup carries an **Include-in-plan toggle** (`toggle_recipe_in_plan`
  adds/removes the recipe from the grocery-list's `derive.deps`); toggling
  recomputes the grocery-list AND the cart-preview **live in the doc** (the
  "document recalculates itself" moment). Functional §8-styled rendering: the
  recipe popup is an expandable card (ingredient list + the include toggle), the
  grocery-list an Item | Qty | From "N recipes" table, the cart-preview grouped
  by aisle — all in the §8 smart-object purple accent family (fill `#EEEDFE`,
  border `#AFA9EC`, text `#3C3489`, dot `#534AB7`), the derived views flagged
  "auto-synced". Per-type chip glyphs (🍳 / 🛒 / 🧺 / 🔗 / 🏷 / ∑) resolve from
  the store (#109 fix 2); deleting a chip strips its whole inline span (#109
  fix 1).
- **SO4 — recipe URL ingestion (AS BUILT).** Paste a recipe URL → a normalized
  `recipe` smart object that flows into the SO3 reactive grocery→cart chain. The
  **§8 meal-plan mockup and the "add to cart" action-role stub moved to SO5**
  (now **built** — the in-document recipe/grocery/cart cards, the review-only
  cart-fill stub + phase stream, and the light "Add via chat" demo) — SO4 lands
  the ingestion pipeline (the handoff's flagged core technical risk).

  **The architecture split (LOCKED): host fetches, the component
  extracts + normalizes + dispatches.** A WASM component cannot fetch arbitrary
  recipe URLs (its egress is a closed, operator-declared allow-list), so
  ingestion is **host-mediated**, but normalization + the object write stay
  in-component (the AI-enabled-component pattern — write the normalized object to
  the store, not an egress of user data):
  - **Fetch (host).** A new loopback route `GET /recipe/fetch?url=…`
    (`tangram-host/src/recipe.rs`) performs the read-only, user-initiated,
    bounded page GET, **gated by the `tangram-automation` browser egress gate**
    (`BrowserEgressGate`) built from the operator policy ceiling
    `[automation].browser_domains_ceiling` over the shared `tangram-egress`
    canonicalizer — the SAME fence the component egress + the manifest verifier
    use, never bypassed. Default-deny: no `[automation]` ceiling ⇒ the route
    403s. The component reaches it over loopback (already in `allow_hosts`) via
    a declared `[[apps.tangram.calls]]` `GET 127.0.0.1 /recipe/fetch` grant.
    - **Any-host `*` ceiling (operator decision).** To let a user import a
      recipe from ANY site by URL, the operator may set
      `browser_domains_ceiling = ["*"]`. The `*` is a deliberate **widening of
      the allowlist only**: the `BrowserEgressGate` still canonicalizes and
      fails closed on an unparseable URL, still honors its denylist, and still
      fires call-level path-denies — `*` bypasses ONLY the
      `NotAllowlisted` check (one tested behavior, `tangram-automation`
      `egress.rs`). This is safe for THIS surface because the fetch is a
      read-only, user-initiated GET, size/time-capped in `recipe.rs`
      (`MAX_BODY_BYTES` / `FETCH_TIMEOUT`).
    - **Process-once / opaque-to-LLM stance.** The fetched page is
      **LLM-normalized EXACTLY ONCE** (Normalize, below) into the fixed,
      structured ingredient model; the SO2/SO3 reactive chain over that model is
      **pure** — it never re-feeds the raw page (or the normalized fields) back
      into an LLM. So even under `*`, an untrusted recipe page cannot steer a
      later LLM step. Making the ingested page data **fully opaque to downstream
      LLMs** (a stronger guarantee than process-once) is a further hardening,
      tracked as **future work** — NOT built in this pass.
  - **Extract (component, pure).** `ingest::extract_recipe_jsonld` pulls the
    schema.org/Recipe JSON-LD from the returned HTML — handles `@graph`, top-
    level arrays, multiple `<script type="application/ld+json">` blocks, and
    `@type` as string or array.
  - **Normalize (component, LLM — the core risk).** `ingest::Llm` turns each
    free-text `recipeIngredient` ("2 tbsp olive oil", "½ medium onion, diced")
    into `{canonicalName, quantity, unit, category, raw}` via DeepSeek (the same
    host-injected-key egress the agent run uses; `[[apps.tangram.calls]] POST
    api.deepseek.com`). A small canonical dictionary (`ingest::canonicalize`)
    collapses `tomato`/`tomatoes`, `scallion`/`green onion`, `garbanzo`/
    `chickpea`, … so the SO3 grocery-list does not fragment. **Fixture-testable**
    (`Llm::Fixture` / `Llm::Live` — the morning-brief precedent): CI is offline
    + deterministic, NO live LLM/network.
  - **Create + cache (component).** `ingest_recipe(url, object_id)` creates the
    `recipe` object (the SO3 `data` shape, so it flows into the chain when
    toggled into a meal plan) and records a `recipe_cache` entry keyed by
    **URL + JSON-LD hash** — a re-import of an unchanged page is a CACHE HIT (no
    re-fetch, no re-LLM-call). The cache row is invalidated if its object is
    deleted.
  - **UI.** Picking `@recipe` prompts "Import a recipe — paste a URL, or Cancel
    to add one manually": a URL imports (a placeholder chip relabels when the
    object lands); Cancel falls through to the SO3 manual path (never lost).
  - **DEFERRED (the seam is wired):** the no-JSON-LD **LLM page-parse fallback**
    is a clearly-marked stub (`ingest::Fallback` — a clear, actionable error
    today); `tangram-host/src/recipe.rs::fetch_recipe_html` is the single choke
    point a later change routes through the `tangram-automation` **browser-driver
    runner** for JS-rendered pages. Neither needs new policy — the egress gate,
    the request shape, and the operator ceiling are already in place. The live
    `tangram-automation` browser SUPERVISION (`runner.rs`) is not wired into the
    host for SO4; the static-page GET is the smallest correct end-to-end slice.

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
| **SO2** | **Reactivity engine** | the `derive`/`derive_error` model + a topological recompute engine (`reactive.rs`), cached-inline derived, cycle detection, the single-doc simplification, a real derived type (`rollup`) + a seeded live demo, and the chip/popup derived rendering (§4). | **DONE** |
| **SO3** | **Recipe types + the reactive chain** | the `recipe`/`grocery-list`/`cart-preview` types + the two derive kinds (`reactive::grocery`, with unit reconciliation); the seeded reactive `meal-plan-demo.md` (3 overlapping recipes → derived grocery-list → derived cart-preview); the Include-in-plan toggle (`toggle_recipe_in_plan`) driving the live recompute; functional §8-styled rendering (recipe card / grocery table / cart-by-aisle, purple accent); the two #109 fix-forwards (whole-span delete-strip + per-type chip glyph) (§6). | **DONE** |
| **SO4** | **Recipe URL ingestion** | host-mediated fetch (`GET /recipe/fetch`, gated by the `tangram-automation` egress ceiling) + in-component schema.org/Recipe JSON-LD extraction + LLM ingredient normalization (DeepSeek, fixture-testable) + the canonical dictionary + the `recipe` object & URL+JSON-LD-hash cache + the `@recipe` import affordance (§6). Deferred (seam wired): the no-JSON-LD LLM page-parse fallback + the JS-render browser-driver fetch. | **DONE** |
| **SO5** | **The §8 mockup + cart-fill stub** | the §8 meal-plan mockup over the wired primitive — in-document BLOCK cards (the recipe card: an include-in-plan checkbox + the purple chip pill + a chevron over an expandable ingredient list; the grocery Item \| Qty \| From table; the cart-preview grouped by aisle), the §8 **Action** affordance (a full-width "Fill Whole Foods cart · N items" button that streams the §3 phases — Explore → Compile → Run → Verify, each a green check — and ends on a lock-icon **review-only "nothing purchased"** terminus), and a light in-document chat affordance (two seeded messages + an "Add 'Tacos' via chat" button that injects a 4th recipe and re-syncs the chain). The cart-fill is a **STUB** — `fill_cart` is a pure, read-only action returning the phase sequence + the review message; **no live browser, no `tangram-automation`, NEVER purchases**. | **DONE** |
| — | **Agent/Run convergence + versioning** | re-home agents/runs onto the primitive behind the unchanged surface; versioning (§7). | LATER / PARALLEL |

The **LIVE action pipeline** (§5 — the `tangram-automation` browser-driver +
`op://` credential broker, producing a cart-preview run-role object) lands with
the action role at **Build-3/SOx**. SO5 ships only the review-only STUB
(`fill_cart`): it reproduces the §8 Action affordance + phase stream over the
wired primitive but performs NO egress and never purchases — the safety posture
holds until Build-3 wires the live pipeline behind the same surface.

---

*This doc is the source of truth for the smart-objects (typed-graph) primitive in
the shell. The primitive + 5 roles, the Tangram-adapted platform mapping, the
locked Handoff-2 decisions, and the SO1→SO4 staging are approved; **SO1 is the
shipped foundation** (the object store + `@` chip + basic popup + typed links).
Smart objects are built ALONGSIDE the embedded-runs Agent/Run system
([embedded-runs.md](embedded-runs.md)), which converges onto this primitive in a
later checkpoint (§7).*
