# Design: Embedded Agents & Runs — the two-layer model for the Tangram shell

**Status:** PROPOSED — approved direction (handoff reconciliation). This is the
**embedded-runs redesign of record** for the `tangram` shell's in-note agent
surface. It adapts an Obsidian-flavored design handoff to **our** architecture:
the storage is a replicated **Automerge document**, the inline surface is a
**CodeMirror 6** web UI, and the runtime is a **wasm component** + a host-side
scheduler — **not** an Obsidian plugin. It is the inline/UI companion to the
canonical [Agents & Skills design](agents.md) (which owns the execution model,
the substrate-reuse table, and the host runtime); read that first.

This doc is staged into checkpoints **R1–R4**; R1 is the first shippable slice
(this checkpoint). Each R is independently reviewable.

---

## 1. Terminology (LOCKED)

The handoff reconciliation settled a three-layer vocabulary. Use these terms
**in all user-facing copy** going forward; they replace the older
"Trigger"/"Invocation" labels.

| Term | What it is | Today's internal name (UNCHANGED) |
|---|---|---|
| **Agent** | the **reusable definition** — a vault markdown file (frontmatter + instructions). A pure capability with no trigger. | `AgentDef` / `parse_agent` (`agents.rs`, `agents.ts`) |
| **Run** | a **bound instance** that references an Agent by name and layers context (trigger/schedule + prompt + host note). This is what we currently call a "Trigger"/"invocation". | the `invocations` index — `Invocation { id, agent, trigger, prompt, host_file_id, last_run_ms, status, next_fire_ms }`; `invocationId`; `create_invocation` / `update_invocation` / `delete_invocation` / `list_invocations`; `[data-invocation-id]` |
| **Execution** | **one execution of a Run** — what actually produces output (one LLM↔tool loop, one appended output block). | not yet a first-class record; today only `last_run_ms` + `status` on the Run. R3 introduces an append-only **executions log**. |

**The rename is user-facing only.** Every internal data identifier above stays
exactly as it is — the `invocations` index, `invocationId`, the `*_invocation`
actions, `[data-invocation-id]`, and the CSS hooks the tests assert. This is the
same surgical-relabel approach as the prior **Invocations → Triggers** pass:
labels change, the data model does not.

---

## 2. Platform mapping — Tangram-adapted (LOCKED)

The handoff assumed Obsidian primitives (frontmatter-as-store, `.ndjson` run
logs, `_agents/` paths, an `agt:` token, plugin settings for secrets). None of
those are how Tangram works. Each FIRM product decision maps onto a Tangram
pillar instead:

| FIRM product decision (the redesign's spine) | Tangram-adapted mechanism |
|---|---|
| **Two-layer model** (Agent = definition, Run = bound instance) | Agent = a vault markdown file indexed by `agents.rs`; Run = an entry in the replicated `invocations` index keyed by the inline link's UUID. Storage is the **Automerge document**, NOT frontmatter or a sidecar file. |
| **Inline chip is the only resting state** | a **portable markdown link** `[⚡ <agent>](agent://<id>)` in the note body — degrades gracefully (a plain link if the decoration is absent), NOT a bespoke `agt:`/`agent:` token. The `<id>` is the Run's UUID; the link is only the handle. |
| **Chip is atomic & click-to-edit (never text-edit)** | a CM6 **atomic widget** — the replaced range is registered with `EditorView.atomicRanges` so the cursor *steps over* it and cannot enter to text-edit. A click opens the **modal Run editor** (R2). We deliberately do **NOT** do the common "reveal raw source on cursor entry" — the chip is opaque by design. |
| **Run output lands as a callout below the host block, with a bidirectional backlink** | R3: the component appends a markdown **callout** below the host block and stamps a **block id** on both the chip's host block and the output, so each side links to the other. (R1 keeps the current append-just-after-the-link behavior; the formal callout + block-id backlinks are R3.) |
| **Run history lives out of the note body** | the per-Run history (Executions) lives in the **Automerge doc**, surfaced in the Run editor's History tab (R2/R3) and the Agents view's Runs table — never as text in the note. R3 makes each Execution a record in an append-only executions log. |
| **Inheritance is additive and visibly distinguished** | R2: a Run inherits the Agent's config additively (the Run layers context on top of the Agent's instructions/model/tools); the Run editor renders inherited-vs-overridden fields with a visible distinction. |
| **Versioning** | **DEFERRED** (see §4). A Run references its Agent by **name** — no semver, no pinning, no snapshots this pass. |
| **Secrets** | host-side only (ADR-0005): the LLM key is injected at the `/llm/<provider>` boundary and never reaches the Agent file, the Run record, the client, or the replicated doc. NOT plugin/app settings. |

---

## 3. The R1–R4 staged roadmap

Each checkpoint is independently shippable + reviewable.

| # | Checkpoint | Deliverables | Review gate |
|---|---|---|---|
| **R1** | **Doc + rename + atomic chip + live status** (this checkpoint) | this design doc; user-facing **Triggers → Runs** rename across the shell UI (Agents view sub-tab, the click-to-edit popup, empty states/counts/filters/aria), Executions wording where executions are surfaced; the inline chip made a **CM6 atomic widget** (`EditorView.atomicRanges`) so the cursor steps over it (click still opens the editor, EOF-safe hit test kept); **live status** rendered on the chip from the Run's index record (Idle / Running / Done / Error) with a best-effort `↓` scroll-to-latest-output; a minimal **running→done/error** transition in the component run flow so the chip can show "running". | typing creates a chip; the caret cannot enter the chip (steps over it); clicking opens the editor; the chip reflects the Run's status and a running tick shows "running…"; the UI says "Runs"/"Run"/"Execution", never "Trigger"/"Invocation". |
| **R2** | **The Run editor surface** | the modal **Run editor** opened by a chip click, with tabs: **Config** (trigger + prompt + the layered context), **Runs** (sibling Runs of the same Agent), **History = Executions** (this Run's executions), **Observability** (per-execution timing/tokens/cost stubs). **Visible additive inheritance**: inherited-from-Agent vs Run-override fields are visually distinguished. | the editor opens with all four tabs; editing Config round-trips through `update_invocation`; inherited vs overridden config is visibly distinct; the Runs/History tabs read the live index. |
| **R3** | **Inline output callout + bidirectional backlinks + executions log** | the component appends each Execution's output as a markdown **callout below the host block** (not just after the link); a **block id** stamped on the host block and the output gives a **bidirectional backlink** (chip ⇄ output); an append-only **executions log** in the Automerge doc, each Execution snapshotting a **resolved-config hash** (the effective Agent+Run config that produced it). | a run appends a callout below the host block; the chip's `↓` jumps to it and the callout links back to the chip; the executions log accrues one record per run with a config hash; history reads the log. |
| **R4** | **Run-scoped mounted files** | files mounted onto a Run and exposed to the Agent at run time (the Run's layered context can include vault files/attachments the Agent reads). | a Run with a mounted file makes that file available to the Agent's loop at run time; the mount is recorded on the Run and shown in the editor. |

R1–R2 are the inline + editor core (no change to the append/output model beyond
the running state). R3 formalizes output + history. R4 adds mounted context.
Versioning is out of scope for all four (§4).

---

## 4. Deferred: versioning

A Run references its Agent **by name** for the entirety of R1–R4. No semver
pinning, no immutable published versions, no per-Run Agent snapshots this pass.
If the named Agent is edited, future Executions of the Run use the new
definition. (The canonical [Agents design §10](agents.md) carries the eventual
versioning/publish/share story; this redesign does not pull it forward.) R3's
per-Execution **resolved-config hash** is the seam a later versioning pass would
build on — it already records *which* effective config produced each Execution.

---

## 5. R1 implementation notes (as built)

- **Rename.** `apps/tangram/ui/src/agentsView.ts` (the "Triggers" sub-tab →
  "Runs", section title, empty state, count noun, filter placeholder, column
  header) and `triggerPopup.ts` (the editor title/labels → "Run", "Open in
  Agents" deep-link copy). Where an execution is surfaced, the copy says
  **Execution**. Internal identifiers (`invocations`, `invocationId`,
  `[data-invocation-id]`, the `*_invocation` actions, CSS class hooks) are
  unchanged.
- **Atomic chip.** `apps/tangram/ui/src/agentLink.ts` adds an
  `EditorView.atomicRanges` facet provider over the same `[⚡ …](agent://…)`
  ranges it already decorates, so the cursor steps over each chip. The source
  stays a plain markdown link; the click handler (EOF-safe hit test) is kept.
- **Live status.** `apps/tangram/ui/src/agentLink.ts` renders the chip label
  from the Run's index record via a status lookup (`runStatusChip`): Idle
  (`⚡ <name> 🔒`), Running (`◌ <name> · running…`), Done
  (`✓ <name> · <relative time> · ↓`), Error (`! <name> · failed`). The `↓`
  affordance scrolls to the Run's latest appended output (best-effort
  scroll-to-latest; the formal callout + block-id backlink is R3).
- **Running transition.** `apps/tangram/src/lib.rs` marks a due Run
  `STATUS_RUNNING` in a commit *before* the (lock-free) LLM call, then the
  existing append commit sets `ran`/`error`. This keeps the lock-never-held-
  across-await invariant (CLAUDE.md) and lets the replicated chip show "running"
  between the two commits.

---

*This doc is the source of truth for the embedded-runs (Agent / Run / Execution)
redesign of the shell's inline agent surface. The terminology, the
Tangram-adapted platform mapping, and the R1–R4 staging are approved;
implementation proceeds as the independently-reviewable checkpoints R1–R4. The
execution model and host runtime remain owned by [agents.md](agents.md).*
