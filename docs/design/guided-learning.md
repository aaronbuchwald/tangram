# Guided Learning — design & decisions for approval

**Status:** IMPLEMENTED — shipped as `apps/guided-learning`: a *Make It
Stick*-driven tutor (quiz-don't-re-present, gated reveal on an attempt,
confidence-vs-grade calibration, spaced reviews, and a collaboratively-editable
`.md` study artifact in its Automerge doc). The only egress is the Anthropic
Messages call with a host-injected credential (ADR-0005, `[apps.guided-learning]`
inject); CI is fixture-LLM (no live key). This document is retained as the
design record; where it says "no production code" or "morning-brief not in the
tree", read it as the original plan — both apps have since landed.

**Date:** 2026-06-12
**Author:** Aaron (owner), with research + design by Claude

**Concept (owner):** a learning integration that applies the techniques of
*Make It Stick* (Brown, Roediger, McDaniel) to walk a learner through a specific
piece of material **by asking them questions**, inside an interactive,
well-designed UI, and **produces a written artifact / note that persists as an
`.md` file** out of the collaborative conversation — and that artifact stays
**collaboratively editable** (the CRDT gives that for free).

**Read alongside:**
- [docs/RUNTIME_PLAN.md](../RUNTIME_PLAN.md) — "The app contract" (the WASM door)
  and the AI-component capability model; the UI-authoring exception (ADR-0007).
- `docs/design/morning-brief.md` **if present** — it canonically defines the
  **AI-enabled-component pattern** (LLM call = a scoped `http-fetch` with a
  host-injected key; output confined to the component's own Automerge state;
  containment via the capability/egress model). *Reuse that pattern; do not
  redesign it.* **As of this writing morning-brief.md is not yet in the tree**
  (no branch carries it), so this doc designs against the same model directly
  and **defers ownership of the canonical pattern statement to
  morning-brief.md** when it lands. The pattern, restated for grounding, is:
  closed WIT world (`http-fetch` / `log` / `now-ms`), ADR-0005 egress credential
  injection, and the in-flight fine-grained-egress work
  ([docs/design/fine-grained-egress.md](fine-grained-egress.md), PR #1) scoping
  the LLM call.
- [docs/design/tangram-shell-redesign.md](tangram-shell-redesign.md) — the
  `tangram` shell app: its markdown **vault** (`Vec<MdFile>` in its replicated
  Automerge doc, CodeMirror/textarea editing, marked+DOMPurify rendering), the
  ADR-0007 build exception, and apps-as-iframes. The guided-learning artifact is
  a `.md` note; §6 below designs its relationship to that vault.
- `apps/nutrition/src/strategy/llm.rs` — the **already-working** Anthropic
  Messages-API call from inside a component (`claude-opus-4-8`, structured
  `json_schema` output, the OAuth-vs-api-key credential split). Guided learning's
  tutor calls reuse this verbatim as the template.
- `apps/notes/src/lib.rs` — the canonical `#[model]` / `#[actions]` /
  additive-field (`Option<T>` + `#[autosurgeon(missing)]`) conventions.

---

## 1. Problem & goals

People "learn" by re-reading and highlighting — which *feels* productive and is
nearly worthless (the **illusion of knowing**). *Make It Stick*'s thesis is that
durable learning comes from **effortful retrieval**, not fluent re-exposure. The
techniques that work are well established and individually testable; what's
missing is a tool that *embodies* them in the moment of studying rather than
lecturing about them.

**Goal:** a Tangram app that takes a specific piece of material (a pasted text, a
note from the vault, a topic) and runs the learner through a *Make It
Stick*-shaped session — quizzing rather than re-presenting, demanding the
learner generate and explain before being told, calibrating confidence against
correctness, interleaving and spacing return visits — and **co-authors a durable
`.md` study artifact** from that session that the learner keeps and can edit
collaboratively (and that syncs across their devices and the fleet, because it's
a CRDT document).

**Concrete goals**

1. **Quiz, don't re-present.** The default interaction is a *question*, not a
   restatement of the material. The material is consumed by the tutor to
   generate questions; the learner mostly *answers*.
2. **The seven techniques are features, not copy** (§2). Each maps to a concrete
   model field, action, and UI affordance.
3. **A persistent `.md` artifact** grows out of the session — a study note with
   the learner's own-words explanations, the questions they struggled with, the
   corrections, and a spaced-review schedule. It lives as markdown and is
   **collaboratively editable** via the CRDT (§6).
4. **AI-driven tutor logic** (generate questions, evaluate free-text answers,
   Socratic follow-ups, decide spacing) as an **AI-enabled component**: the only
   egress is the scoped LLM call; the material, conversation, and artifact never
   leave the component's Automerge state (§4, §7).
5. **A well-styled, interactive UI** that makes effortful retrieval feel good —
   a focused question card, a reveal-after-attempt flow, a confidence slider, a
   calibration readout, a session timeline, and a live preview of the growing
   artifact (§5).

**Non-goals (this round)**

- A spaced-repetition *scheduler daemon* / push notifications. Spacing is
  computed and surfaced as **due reviews** the learner returns to; a background
  reminder transport is a later, host-side concern (§2 "spaced repetition",
  §8 phasing).
- A general courseware/LMS. One material → one (resumable) learning session →
  one artifact. Multiple sessions coexist as multiple artifacts.
- Multi-learner shared sessions in v1 (the CRDT *allows* it; the question flow
  is designed single-learner-at-a-time — see Open Decision 5).
- Importing arbitrary file formats (PDF/EPUB). v1 takes **text**: pasted, typed,
  or a `.md` picked from the shell vault (§6).

---

## 2. The *Make It Stick* techniques → concrete features & UI flows

This is the heart of the design: each technique becomes a model field, an
action, and a UI element. The rule is **the UI defaults to the harder, more
effortful path** (retrieval over recognition, generate-then-reveal over
show-then-ask) because that is the whole point of the book.

| Technique | What the book says | Feature / model | UI flow |
|---|---|---|---|
| **Retrieval practice** | Recalling beats re-reading; testing *is* studying. | The session's primary unit is a `Question`, never a re-display of the source. The tutor LLM call *consumes* the material and *emits questions*; the material text is shown only on explicit "peek" (which is logged as a peek). | The main card shows **a question with an empty answer box**, not the material. The material is collapsed behind a "show source" toggle that records a `peeked` event (and gently notes that peeking weakens retrieval). |
| **Generation** | Attempting an answer *before* being shown the solution produces better retention even when the attempt is wrong. | A `Question` has `learner_answer`, then `revealed: bool`. The reveal/model-answer is **gated behind a submitted attempt** — you cannot see the answer until you've committed one (or explicitly clicked "I don't know", which still counts as an attempt). | The "Reveal / Check" button is disabled until the answer box is non-empty or "I don't know" is pressed. No peeking at the answer first. |
| **Elaboration** | Explaining in your own words and connecting to prior knowledge deepens encoding. | Question kinds include `Elaboration` ("explain X in your own words") and `Connection` ("how does this relate to something you already know?"). The learner's elaboration text is captured and **becomes a paragraph in the artifact** (§6). | After a factual question, the tutor sometimes follows with "now explain that back in your own words" — a free-text box whose content flows straight into the artifact. |
| **Calibration** | Surface the *illusion of knowing*: compare predicted confidence vs actual correctness. | Each attempt carries `confidence: u8` (0–100) recorded **before** reveal, and the tutor's `grade` (0–100 or a small enum) recorded **after**. A `Calibration` view aggregates `confidence − grade` per question. | A **confidence slider** appears with the answer box ("how sure are you?"). After reveal, the card shows *you said 90% sure; that was wrong* — the explicit illusion-of-knowing moment. A session-level calibration chart shows over/under-confidence. |
| **Reflection** | Reviewing what happened — what worked, what was hard — consolidates learning. | A `reflection: Option<String>` per session (and optional per-question "why did I miss this?"). | A **reflection prompt at session close** ("what surprised you? what was hardest?") whose answer is appended to the artifact's reflection section. |
| **Spaced repetition** | Spacing study over time beats massing; let some forgetting happen. | Each `Question` (or topic) carries a `ReviewSchedule { due_at_ms, interval_index, ease }` updated by a Leitner/SM-2-lite step on each grade. Due items computed by a pure `&self` method; **no background daemon** in v1 — due reviews are surfaced when the learner returns. | A "**Due for review**" section lists items whose `due_at_ms ≤ now`; opening one re-quizzes (retrieval again) and re-schedules. A small "next review: in 3 days" stamp per item. |
| **Interleaving** | Mixing problem types/topics beats blocking one type. | The session can hold multiple `topics` (derived from the material or added). The question selector **interleaves** across topics/question-kinds rather than exhausting one before the next — a pure ordering function over the `Question` set + schedule. | The tutor visibly mixes kinds ("a factual one, now a connection, now a due review from last time") rather than ten of the same. The timeline shows the mix. |

Two cross-cutting UI principles drawn from the book:

- **Effort is the feature, not a bug.** The UI never offers the easy path by
  default (no "show me the answer" before an attempt; no re-reading loop). When
  the learner takes an easy path (peek, skip) it's *allowed* but *logged and
  gently surfaced* — because hiding the easy path is paternalistic, but making
  its cost visible is exactly calibration.
- **The artifact is the reward.** Every effortful act (an own-words elaboration,
  a corrected misconception, a reflection) visibly **adds to the growing study
  note** on screen — so the learner sees durable output accrue from effort,
  which is the motivational loop the book argues massed re-reading lacks.

---

## 3. The model (Automerge shape)

A normal Tangram app: `#[model]` structs in the app's replicated Automerge
document, deterministic `Default` (genesis), additive fields as `Option<T>` +
`#[autosurgeon(missing = …)]`, **`Vec` not `HashMap`** (the §"Conventions" rule —
`HashMap` iteration order is nondeterministic and breaks genesis parity). The
material, the full question/answer history, the evolving artifact, and the
schedule **all live here** — which is precisely what makes the containment
argument (§7) hold: nothing about the session exists outside this document
except the in-flight bytes of one LLM request.

```rust
#[model]
#[derive(Default)]
pub struct GuidedLearning {
    sessions: Vec<Session>,
}

#[model]
pub struct Session {
    id: String,
    title: String,            // first line of the material, or learner-set
    /// The source material the session is teaching. Stored in the doc so the
    /// tutor's questions are reproducible and nothing leaves the component.
    material: String,
    /// Derived/added topics for interleaving (Vec, ordered, deterministic).
    topics: Vec<Topic>,
    questions: Vec<Question>,
    /// The growing markdown artifact (the "note"). Raw markdown text; rendered
    /// client-side. Editable directly (collaborative via the CRDT, §6).
    artifact_md: String,
    /// Session-close reflection (Make It Stick: reflection).
    #[autosurgeon(missing = "Option::default")]
    reflection: Option<String>,
    created_at_ms: i64,
    #[autosurgeon(missing = "Option::default")]
    updated_at_ms: Option<i64>,
    /// Where this artifact lives relative to the shell vault (Open Decision 1):
    /// e.g. "learning/photosynthesis.md". None until the learner names/saves it.
    #[autosurgeon(missing = "Option::default")]
    vault_path: Option<String>,
}

#[model]
pub struct Topic {
    id: String,
    name: String,
}

#[model]
pub struct Question {
    id: String,
    topic_id: String,
    kind: String,             // "factual" | "elaboration" | "connection" | "application"
    prompt: String,           // the question text (LLM-generated)
    /// The reference/model answer — only populated when revealed, so the doc
    /// itself can't be used to "peek" before an attempt is committed by the UI.
    #[autosurgeon(missing = "Option::default")]
    model_answer: Option<String>,
    attempts: Vec<Attempt>,   // generation: at least one before reveal
    revealed: bool,
    peeked: bool,             // did the learner show the source for this one?
    schedule: ReviewSchedule, // spaced repetition state
    created_at_ms: i64,
}

#[model]
pub struct Attempt {
    answer: String,           // free text; "" with `idk=true` is a valid attempt
    idk: bool,
    confidence: u8,           // 0..=100, recorded BEFORE reveal (calibration)
    /// Tutor's grade, recorded AFTER reveal. None for an unscored attempt.
    #[autosurgeon(missing = "Option::default")]
    grade: Option<u8>,        // 0..=100
    /// The tutor's Socratic follow-up / correction for this attempt.
    #[autosurgeon(missing = "Option::default")]
    feedback: Option<String>,
    at_ms: i64,
}

#[model]
pub struct ReviewSchedule {
    due_at_ms: i64,           // when this item is next due (genesis: 0 = due now)
    interval_index: u8,       // Leitner box / SM-2-lite step
    ease: u8,                 // coarse ease factor (e.g. 130..=300, /100)
}
```

Notes:

- **Deterministic `Default`.** `GuidedLearning::default()` is an empty
  `sessions` vec — a stable genesis commit identical guest↔native (the
  `genesis_bytes()` parity the host relies on). No timestamps, no UUIDs in
  `Default`.
- **Additive discipline.** Every field added after v1 ships must be `Option<T>`
  with `#[autosurgeon(missing = …)]` (the `updated_at_ms`-on-notes lesson). The
  fields above marked `missing` are the ones plausibly added post-v1; the rest
  are v1 genesis fields.
- **The artifact is `artifact_md: String`** — one markdown blob per session.
  Storing it as text (not structured) is what makes it (a) renderable by the
  same marked+DOMPurify path the shell uses and (b) **directly, collaboratively
  editable as a CRDT string** (§6). Structured fields (`attempts`, `reflection`)
  are the *source* the tutor uses to *regenerate/extend* the artifact; the
  `artifact_md` is the *editable surface*.
- **Schedule is data, computed by a pure method.** "Due now" is
  `due_at_ms ≤ now_ms()`, evaluated by a `&self` selector — no daemon, no I/O.

---

## 4. Actions (LLM calls via `Ctx`-async, outside the store lock)

Every user-facing operation is a registered action — a **sync method** (pure
state transition, no I/O, `&self`/`&mut self`) or an **`async fn` taking
`Ctx<Self>`** for the LLM call. The hard rule (`CLAUDE.md` Conventions): the
store lock is **never held across an await** — resolve the LLM call *outside* the
lock, then commit via `Ctx::mutate`. This is exactly how `nutrition::log_meal`
works (§ read: `apps/nutrition/src/lib.rs:101`), and the guided-learning actions
mirror it one-for-one.

**Sync actions** (pure state transitions, no LLM):

- `start_session(material, title?) -> id` — push an empty `Session`; topics
  empty until `generate_questions` runs. (The material is the only input.)
- `submit_answer(session_id, question_id, answer, idk, confidence)` — append an
  `Attempt` (with confidence, **before** any grade). Pure; enables the
  generation gate (no reveal without this).
- `record_reflection(session_id, text)` — set `reflection`; append to the
  artifact's reflection section.
- `edit_artifact(session_id, new_md)` — last-writer-wins replace of
  `artifact_md` (the textarea/CodeMirror editing path, identical to notes'
  `update_note`). Collaborative concurrent edits merge via Automerge (§6).
- `mark_peeked(session_id, question_id)` — record that the source was shown for
  a question (calibration honesty).
- `due_reviews(session_id) -> Vec<Question>` (`&self`, `#[must_use]`) — pure
  selector: questions with `schedule.due_at_ms ≤ now_ms()`, interleaved across
  topics. No I/O.
- `list_sessions() -> Vec<SessionSummary>` (`&self`) — for the UI list.

**Async actions** (the AI-enabled-component calls — `Ctx<Self>`, LLM via
`tangram::http`, commit via `ctx.mutate`):

- `generate_questions(ctx, session_id, count?)` — read the material *out of the
  doc* (clone it under a brief read, then drop the lock), call the tutor LLM
  (structured `json_schema` output: a list of `{topic, kind, prompt,
  model_answer}`), then `ctx.mutate` to append the `Question`s + `Topic`s with
  `schedule { due_at_ms: now, interval_index: 0 }`. Interleaving is applied at
  *selection* time, not generation.
- `evaluate_answer(ctx, session_id, question_id)` — for the latest unscored
  `Attempt`: send the question + model answer + the learner's attempt to the LLM
  with a structured grading schema (`{ grade: 0..100, feedback, follow_up? }`),
  then `ctx.mutate` to set `grade`/`feedback`, set `revealed = true`, advance the
  `ReviewSchedule` (Leitner step from the grade), and **append the
  question/attempt/correction to `artifact_md`**. A wrong-but-attempted answer
  schedules *sooner*; a confident-wrong answer is flagged in the calibration
  view.
- `socratic_follow_up(ctx, session_id, question_id, learner_reply)` — optional
  multi-turn: feed the learner's elaboration back to the LLM for one Socratic
  nudge; commit the exchange into the artifact.
- `synthesize_artifact(ctx, session_id)` — (optional, on demand) ask the LLM to
  *reorganize* the accumulated raw artifact into a clean study note (headings,
  a summary, the open questions, the review schedule), committed back into
  `artifact_md`. The learner's own-words text is preserved verbatim; the LLM
  only structures around it.

**The LLM call itself** is the nutrition `llm.rs` pattern, unchanged:
`tangram::http::Request::post("https://api.anthropic.com/v1/messages")` with
`anthropic-version`, model `claude-opus-4-8`, `output_config.format =
json_schema` for the structured tutor outputs, `await http::fetch(req)`. Inside
a WASM component there is **no spawner** (dispatch is one synchronous
doc-in/doc-out call), so any multi-step resolution runs **inline**, exactly as
nutrition's back-fill does under `#[cfg(target_family = "wasm")]`
(`apps/nutrition/src/lib.rs:188-191`). Natively it could spawn; the component
path is the binding one.

**Credential.** The component issues a **bare** request (no key in component
memory) and the host attaches the Anthropic credential at the `http-fetch`
egress boundary via an ADR-0005 injection rule (§4 below / §7). This is the same
posture nutrition's `llm.rs` already moved to (the comment block at
`llm.rs:101-111` is being superseded by host-side injection per ADR-0005).

---

## 5. The AI-enabled-component reuse + the egress grant

Guided learning is an **AI-enabled component** in exactly the sense
morning-brief.md owns (and which this doc reuses without redesigning). The
shape, grounded in what already ships:

1. **Closed WIT world, unchanged.** The component exports
   `describe`/`genesis`/`dispatch`/`state-json` and imports only
   `http-fetch` / `log` / `now-ms` (`crates/tangram-host/wit/tangram.wit`). No
   filesystem, no sockets, no inbound HTTP. The LLM call is *one outbound
   `http-fetch`*, nothing more. **No WIT change is needed.**
2. **The egress grant** the app needs in `apps.toml` (and as a marketplace
   manifest entry):

   ```toml
   [apps.guided-learning]
   component  = "…/guided_learning.wasm"
   ui         = "apps/guided-learning/ui"
   allow_hosts = ["api.anthropic.com"]

   # ADR-0005 egress credential injection — the key is attached HOST-SIDE at
   # the http-fetch boundary; it never enters the component's address space.
   [apps.guided-learning.inject]
   "api.anthropic.com" = { header = "x-api-key", secret = "env://ANTHROPIC_API_KEY" }
   # (or `{ bearer = true, secret = "env://ANTHROPIC_AUTH_TOKEN" }` for an
   #  OAuth token — the same sk-ant-oat… split llm.rs already handles.)
   ```

3. **Fine-grained egress (PR #1) scopes the call.** When
   [fine-grained-egress.md](fine-grained-egress.md) lands, the host-keyed inject
   above becomes a **call-level capability** — the credential bound to exactly
   `POST api.anthropic.com/v1/messages`, with `body = { json_method = … }`-style
   constraints if useful, so a compromised tutor component cannot replay the
   Anthropic key against any other endpoint on that host. Guided learning should
   declare its single call so it is `enforce`-clean from day one:

   ```toml
   [[apps.guided-learning.calls]]
   method = "POST"
   host   = "api.anthropic.com"
   path   = "/v1/messages"
   inject = { header = "x-api-key", secret = "env://ANTHROPIC_API_KEY" }
   ```

   This is additive: under today's host-keyed inject it works unchanged; under
   PR #1's compat shim the host-keyed rule desugars to the broad call, and the
   explicit `[[calls]]` above is the tighter, `enforce`-ready form.

4. **LLM-key prerequisite — FLAG.** There is **no Anthropic key wired for the
   app fleet yet**: `.env.example` lists `ANTHROPIC_API_KEY` /
   `ANTHROPIC_AUTH_TOKEN` only as commented examples (and the nutrition `llm`
   strategy reads it for the native path). Running guided learning requires the
   operator to provide `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`) in `.env`.
   **Without it the app must degrade cleanly**, not crash: the
   `capabilities`/`describe()` probe reports the tutor as unavailable (same
   `description_input:false` pattern nutrition uses when its key is unresolvable
   — ADR-0005 Phase 10b), and the UI shows "configure ANTHROPIC_API_KEY to enable
   the tutor; you can still write/edit the artifact by hand." Sync actions
   (artifact editing, reflection) work offline; only the LLM-backed actions need
   the key.

---

## 6. The artifact: production, collaborative editing, and the vault

### 6.1 How the artifact is produced

The `.md` artifact is **co-authored incrementally** as the session runs, not
generated in one shot at the end:

- `start_session` seeds `artifact_md` with a title + the material's provenance.
- Each `evaluate_answer` **appends** a structured block: the question, the
  learner's own attempt (verbatim — this is the elaboration/generation output),
  the correction, and the calibration note ("you were 90% confident; this was
  wrong"). So the artifact *accretes* from effortful acts.
- `record_reflection` appends the closing reflection.
- `synthesize_artifact` (optional) asks the LLM to reorganize the accreted text
  into a clean study note — headings, a one-paragraph summary, "things I got
  wrong", and the review schedule as a checklist — **preserving the learner's
  own words verbatim** (the LLM restructures around them, never replaces them).

The learner's own-words text being preserved is deliberate: *Make It Stick*'s
elaboration benefit comes from *their* generation, so the artifact must keep it,
not paraphrase it away.

### 6.2 Collaborative editing = lean on the CRDT

`artifact_md` is a string field in the app's Automerge document. Concurrent
edits — the learner on two devices, or two learners — **merge via Automerge with
no extra machinery**. The editing path is identical to notes' `update_note` /
the shell vault's body editing: a textarea (v1) or CodeMirror (later) writing
`edit_artifact(session_id, new_md)` as a last-writer-wins body replace, with the
document syncing over the existing HTTP(+SSE) sync transport
([SYNC_PROTOCOL.md](../SYNC_PROTOCOL.md)). No new sync surface; the app's `/sync`
endpoint already replicates the doc.

(Per-character CRDT *text* merging — so two simultaneous typers don't clobber —
is a refinement: Automerge supports a `Text` type, but the SDK's current notes /
shell pattern uses whole-body LWW replace. v1 matches that; finer-grained text
CRDT is an Open Decision, §Open Decisions 4.)

### 6.3 Relationship to the shell vault

The shell's `tangram` app owns the markdown **vault** (`Vec<MdFile>` in *its*
Automerge doc). Guided learning is a **separate app** with its **own** document.
Two honest options for how the artifact reaches the vault:

- **(A) Guided learning owns its artifacts; the shell renders them as an app
  tab.** The artifact lives in guided-learning's doc; the learner opens
  `guided-learning` as an iframe tab in the shell and reads/edits there. **No
  cross-app writes.** Simplest, most contract-honest (apps own their own data),
  zero new plumbing.
- **(B) "Save to vault" exports the artifact into the shell vault.** A button in
  guided-learning calls the shell's `…/tangram/api/actions/create_file` (a
  relative cross-app call under one host, exactly how the shell sidebar already
  calls `…/registry/api/actions/*`) with the artifact path + body, copying the
  `.md` into the vault where it sits alongside hand-written notes. The copy then
  lives in the vault doc; further edits happen there (or are re-exported).

**Recommendation: (A) by default, with (B) as an explicit "Save to vault"
action.** Owning its own artifacts keeps guided learning self-contained and
contract-clean (no app reaches into another's document silently); the export is
an *explicit, learner-initiated* copy into the vault for those who want all
their `.md` in one tree. This mirrors the shell-redesign's own posture (apps own
their data; the shell composes them as iframes) and avoids a two-writers-one-doc
coupling. The owner should rule (Open Decision 1).

A third, rejected option: guided learning writes *directly into the shell
vault's doc* as its only store (no own doc). Rejected — it makes guided learning
depend on the shell app's schema, breaks "apps own their data," and entangles
two apps' sync. Export (B) gets the "all my notes in one place" benefit without
the coupling.

---

## 7. Security / containment (the same theorem as morning-brief)

The containment argument is **identical to morning-brief's** and to nutrition's
LLM strategy — guided learning adds no new capability:

- **The component's entire view of the outside world is `http-fetch` to
  `api.anthropic.com`** (one allowlisted host; one declared call under PR #1).
  No filesystem, no sockets, no inbound HTTP — the WASI ctx is empty
  (`crates/tangram-host/src/runtime.rs`; RUNTIME_PLAN Phase 2). The material,
  every question, every answer, the artifact, and the schedule live **only** in
  the component's Automerge document, which the host owns and persists; the
  component cannot write files or open arbitrary connections.
- **The LLM credential never enters the component.** It's resolved host-side
  (ADR-0004 secret resolver) and injected at the egress boundary (ADR-0005);
  under PR #1 it's bound to the single `POST /v1/messages` call so it cannot be
  replayed elsewhere on the host. A compromised tutor component cannot exfiltrate
  the credential or the material to anywhere but the one declared Anthropic call
  — and the response's auth headers are stripped before the body returns to the
  component (PR #1 §4.2 step 5).
- **The one residual** is the honest one fine-grained-egress names (§8 of that
  doc): a compromised component could smuggle data *inside* the legitimate
  `POST /v1/messages` body (the material already goes there). That is the same
  irreducible "exfil within a declared call" residual every AI-component shares;
  the deterministic boundary shrinks the surface to exactly the LLM call, it
  cannot read intent. Out-of-band mitigations (`max_body_bytes`, the model/human
  tier) are the same as elsewhere — no new exposure unique to guided learning.
- **Multi-tenancy / federation** inherit unchanged: under `/t/<tenant>/…` the
  material and artifact are private per the Phase-5/6 `Principal` gate; the
  document syncs like any app's; per-host secret expansion means a peer without
  the Anthropic key runs guided learning **degraded** (artifact editing works,
  the tutor is offline) with no key leak — exactly the nutrition-offline
  precedent.

Net: guided learning sits squarely inside the existing theorem. It is "another
AI-enabled component," not a new trust surface.

---

## 8. UI design (concrete) + the single-file-vs-build recommendation

### 8.1 What the screen looks like

A focused, single-column "study" layout (the opposite of a dashboard — the book
is about *attention on retrieval*):

```
┌───────────────────────────────────────────────────────────────────────┐
│ Guided Learning · Photosynthesis            session ●live   [Save→vault]│
├───────────────────────────────────────────────────────────────────────┤
│  ┌─────────────────────────────────────────────────────────────────┐  │
│  │  Q3 · connection · topic: light reactions                        │  │  ← QUESTION CARD
│  │                                                                   │  │     (the focus; the
│  │  How does the role of chlorophyll relate to something you         │  │      material is NOT
│  │  already understand about energy capture?                         │  │      shown here)
│  │                                                                   │  │
│  │  ┌─ your answer ────────────────────────────────────────────┐    │  │
│  │  │ (free text — generate BEFORE you reveal)                  │    │  │
│  │  └───────────────────────────────────────────────────────────┘   │  │
│  │  confidence  [▁▁▁▅▇] 80%        [ I don't know ]   [ Reveal ▸ ]   │  │  ← confidence slider;
│  │                                                  (disabled until   │  │     Reveal gated on an
│  │                                                   an attempt)      │  │     attempt (generation)
│  └─────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  ▸ show source (peeking is logged — it weakens retrieval)                │  ← retrieval default
│                                                                          │
│  ── after Reveal ───────────────────────────────────────────────────    │
│  ✗ You said 80% sure — this was off.   grade 35/100                       │  ← CALIBRATION moment
│  Model answer: …    Tutor: "You're close on energy, but…" (Socratic)      │
│  [ explain it back in your own words → ] (elaboration → artifact)         │
│                                                                          │
├──────────────────────────┬───────────────────────────────────────────┤
│ DUE FOR REVIEW (2)        │  STUDY NOTE  (live preview · editable)       │  ← spaced repetition │
│  • Calvin cycle  (now)    │  # Photosynthesis                            │     +  the growing   │
│  • ATP synthase  (now)    │  ## What I got wrong                         │     ARTIFACT (md)    │
│ ─ calibration ─           │  - Confused light vs dark reactions …        │                      │
│  over-confident on 3/8    │  ## In my own words                          │                      │
│  ▇▇▅▂ confidence vs score │  > Chlorophyll captures photons and …        │  [edit] ⇄ [preview]  │
└──────────────────────────┴───────────────────────────────────────────┘
```

### 8.2 The interaction loop

1. **Start:** paste/type material, or pick a `.md` from the vault (§6.3). →
   `start_session` → `generate_questions` (LLM) populates topics + first
   questions.
2. **Retrieve:** the card shows a question, *not* the material. The learner
   types an answer and sets a **confidence** slider — both *before* any reveal
   (generation + calibration).
3. **Reveal (gated):** `Reveal` is disabled until an attempt is committed
   (generation enforced). On reveal → `evaluate_answer` (LLM grades, gives
   Socratic feedback) → the **calibration line** shows confidence-vs-grade → the
   exchange **appends to the study note**.
4. **Elaborate (sometimes):** the tutor asks "explain it back" → free text →
   `socratic_follow_up` / straight into the artifact.
5. **Interleave / continue:** the next card mixes topic and kind; **due reviews**
   are slotted in when they come due.
6. **Reflect & close:** `record_reflection` appends the closing reflection;
   `synthesize_artifact` (optional) tidies the note.
7. Throughout, the **study note panel** renders live (marked + DOMPurify) and is
   directly editable (textarea/CM → `edit_artifact`), syncing as a CRDT.

### 8.3 Single-file vanilla vs a build step — recommendation

**The app contract:** ordinary apps are a single self-contained `ui/index.html`
— no build, no CDN, relative fetch paths only (RUNTIME_PLAN "The app contract";
the shell-redesign §1 spells it out). Only the **`tangram` shell app** has the
ADR-0007 build exception. Guided learning is an *ordinary* app, so the default
is single-file.

**Is the "well-styled, interactive" bar reachable single-file? Yes — clearly.**
The interactive surface here is: a question card with a gated reveal, a slider,
a tab-toggle (edit/preview), a live calibration bar, a due-review list, and a
markdown preview. None of that needs a framework:

- **State** is small and already streamed: the app subscribes to `api/events`
  (SSE) for document state and re-renders from it — exactly how every existing
  app UI (notes/nutrition/registry/marketplace, 600–900 lines of vanilla DOM)
  works.
- **Markdown rendering** for the live artifact preview uses **marked + DOMPurify,
  both vendored as single files** — the *same* decision the shell-redesign
  reached (its Decision B), and genuinely buildless (one `<script>` each, no
  CDN, no dedup problem). The artifact text comes from the learner's own
  document, so the XSS surface is low, but DOMPurify before `innerHTML` is
  defense-in-depth and mandatory once a sync peer can author a note.
- **The calibration chart** is a handful of `<div>` bars or a tiny inline
  `<svg>` — no charting library.
- **Styling** reuses the shared dark "performance-console" design language the
  other apps copy-paste; guided learning should look like a first-class member
  of the fleet, with a calmer, single-column "study" variant.

**The thing that *would* want a bundler** is *live-preview markdown editing*
(CodeMirror 6) — and the shell-redesign already analyzed that exhaustively (its
§4.4): CM6 is multi-module and effectively wants a bundler; buildless-from-CDN is
fragile. **But guided learning does not need CM6 in v1** — a `<textarea>` for
artifact editing (contract-clean, exactly notes' approach) plus a marked preview
is sufficient and matches the shell-redesign's Decision C ("no live-preview
editor in v1; textarea + marked").

**Recommendation: build v1 single-file** (vanilla + vendored marked + DOMPurify,
textarea editing), staying strictly inside the app contract. Do **not** seek an
ADR-0007 exception for this app. *If* a richer editing experience later proves
necessary, the right move is **not** a second build exception but to **embed
guided learning's artifact via the shell** — open it as an app tab in the
`tangram` shell, which *already* has the build pipeline and the CM6 plan
(shell-redesign Phase S4). I.e. let the one privileged app own the heavy editor;
guided learning stays buildless and lets the shell render/edit its artifact if
CM6-grade editing is wanted. This is the "default to the simplest thing; propose
a richer successor only if needed" posture the owner has accepted before — and
here the richer successor is *reusing the shell's exception*, not minting a new
one.

---

## 9. Phased, testable checkpoints (fixture-based — CI needs no live LLM)

The make-or-break for CI is that **no checkpoint requires a live Anthropic key**.
The technique: a **recorded-fixture LLM transport**. The component already issues
its LLM call through `tangram::http`; tests run the host (or the native app) with
the `http-fetch` boundary pointed at a **local fixture server** that replays
canned Anthropic Messages-API responses (the same shape `nutrition`'s tests use,
and the rmcp-golden fixture precedent in `tangram-core/tests/fixtures/`). The
live-key path self-skips (like `egress_injection.rs` /
`marketplace_lifecycle.rs` do today).

Each checkpoint is its own commit with its test; the full gate (`cargo build
--workspace`, `clippy -D warnings`, `fmt --check`, the relevant `cargo test`,
`cargo build -p tangram-core --target wasm32-wasip2`, plus the wasm32-wasip2
component build the integration tests need) is green before commit.

- **GL1 — model + genesis parity (pure, no LLM).** The `#[model]` structs,
  deterministic `Default`, the `due_reviews` interleaving selector, and the
  Leitner schedule step as pure functions. Test: genesis bytes byte-identical
  guest↔native (the `genesis_bytes()` parity check both example apps have);
  `due_reviews` ordering interleaves topics; schedule advances correctly on a
  grade. *No I/O, no wasm runtime needed for the pure parts.*

- **GL2 — sync actions + the generation/calibration gates (pure).**
  `start_session`, `submit_answer` (records confidence *before* reveal),
  `mark_peeked`, `edit_artifact`, `record_reflection` as dispatch tests against
  a doc. Assert the **invariants the techniques require**: `model_answer` /
  `revealed` cannot be set without a prior `Attempt` (generation gate); an
  attempt's `confidence` is captured and `grade` stays `None` until evaluated
  (calibration ordering). *Pure dispatch, no LLM.*

- **GL3 — the fixture LLM transport + `generate_questions`.** Stand up the
  canned-Anthropic fixture server; point the app's `http-fetch` at it. Assert
  `generate_questions` parses the structured `json_schema` output into
  `Question`/`Topic` rows, commits via `ctx.mutate`, and **holds no store lock
  across the await** (the lock-discipline test: the action resolves the response
  before mutating). Reuse nutrition's fixture-LLM test scaffolding.

- **GL4 — `evaluate_answer` → grade, calibration, schedule, artifact append.**
  Against the fixture: a submitted attempt + reveal sets `grade`/`feedback`,
  flips `revealed`, advances the `ReviewSchedule` (wrong → sooner), and
  **appends the exchange to `artifact_md`**. Assert the calibration delta
  (confidence − grade) is computable and a confident-wrong attempt is flagged.

- **GL5 — artifact collaborative-edit + CRDT merge.** Two doc replicas
  concurrently `edit_artifact` / append via `evaluate_answer`; assert they merge
  via Automerge with no lost content (the same convergence property the
  Cloudflare sync e2e and notes rely on). Confirm the artifact renders (the UI's
  marked path) — a DOM-free assertion on the markdown string is enough for CI.

- **GL6 — degrade-without-key + capabilities probe.** With **no** Anthropic key
  resolvable, assert: sync actions still work (artifact editable offline); the
  LLM-backed actions return a clean, actionable error ("configure
  ANTHROPIC_API_KEY"); `describe()`/capabilities reports the tutor unavailable
  (the `description_input:false` nutrition precedent). Mirrors
  `egress_injection.rs`'s configured-iff-resolves assertion.

- **GL7 — egress containment (host integration).** Through the real component
  under `tangram-host`: the declared `POST api.anthropic.com/v1/messages` is the
  *only* egress that succeeds (host-injected credential present); any other host
  or path is denied (and un-credentialed) — the §7 theorem, pinned. Self-skips
  without the wasm component / under PR #1 once the `[[calls]]` form is enforced.
  (If PR #1 hasn't merged, this checkpoint asserts the host-keyed `inject`
  behavior; it tightens to the call-level form when PR #1 lands.)

- **GL8 — UI + apps.toml wiring + docs.** Single-file `ui/index.html` (vendored
  marked+DOMPurify), `[apps.guided-learning]` entry with `allow_hosts` +
  `inject`, a marketplace seed listing (capability manifest naming the Anthropic
  egress), README section, and the CLAUDE.md index pointer. *Run:* gates + a
  smoke that the UI serves and subscribes to `api/events`.

**Estimate:** ~6–8 agent-sessions. GL1–GL2 are quick (pure, mirror notes).
GL3–GL4 are the substance (the tutor prompt/schema design + the fixture
transport — though the fixture pattern and the `llm.rs` call are both already
written, which de-risks it materially). GL5/GL7 reuse existing convergence /
egress test scaffolding. The UI (GL8) is the largest single chunk — a focused
~700–900-line single-file app in the established style, plus prompt-engineering
the tutor (question generation + grading) to produce good *Make It Stick*-shaped
questions, which is iterative.

---

## 10. OPEN DECISIONS FOR THE OWNER

Each is a fork to rule on before/early in the build. Recommendations are the
author's.

- **OD1 — Artifact ↔ vault relationship (§6.3).** (A) guided learning owns its
  artifacts, shell renders as an app tab; (B) explicit "Save to vault" exports a
  copy into the shell vault; (C, rejected) write directly into the vault doc.
  **Recommend: (A) default + (B) as an explicit action.** Contract-honest, no
  cross-app doc coupling.

- **OD2 — Single-file vs reuse the shell's build exception for rich editing
  (§8.3).** Recommend **single-file v1** (vanilla + vendored marked/DOMPurify,
  textarea editing); if CM6-grade editing is later wanted, **reuse the shell's
  ADR-0007 exception by opening the artifact in a shell tab** rather than minting
  a second exception. Confirm this is the preferred successor path.

- **OD3 — Anthropic key prerequisite (§5.4).** No fleet-wide LLM key is wired
  today. Confirm the operator-provides-`ANTHROPIC_API_KEY`-in-`.env` posture and
  the clean-degrade-without-it behavior (vs blocking the app from starting).
  Also: is this the moment to add a *first-class* `[llm]` config section the host
  resolves once and injects for every AI-enabled app (guided learning,
  morning-brief, nutrition's `llm` strategy), rather than each app naming
  `env://ANTHROPIC_API_KEY` separately? (Likely a small follow-up that
  morning-brief should co-own.)

- **OD4 — Per-character text CRDT for the artifact (§6.2).** v1 uses whole-body
  LWW replace (matches notes/shell). Do we want Automerge `Text` for true
  concurrent-typing merge in the artifact? Recommend **defer** (match the
  existing pattern; revisit if real-time co-editing of one note is a goal).

- **OD5 — Multi-learner sessions.** The CRDT allows two people in one session;
  the question flow is designed single-learner. Recommend **single-learner v1**;
  shared-session is a later, deliberate design (whose turn is it to answer?).

- **OD6 — Spaced-repetition return mechanism.** v1 surfaces "due reviews" when
  the learner opens the app — **no daemon/notification**. Is a host-side reminder
  (email/push) wanted later? Recommend defer; it's a host-transport concern, not
  an app concern, and shouldn't gate v1.

- **OD7 — Tutor model & cost.** `claude-opus-4-8` (matching nutrition's `llm`
  strategy and the assistant default) vs a cheaper/faster model for the
  high-frequency grading calls. Recommend **opus for question generation /
  synthesis, a faster model for per-answer grading** if cost matters — both via
  the same single `POST /v1/messages` call (just a different `model` field), so
  no capability change. Owner's call on cost vs quality.

---

## 11. PLACEMENT + MERGE-STRATEGY recommendation

**Placement: a new first-party app at `apps/guided-learning`** (crate
`tangram-app-guided-learning` or similar), built native **and** wasm32-wasip2
like every app, with its single-file `ui/`, an `[apps.guided-learning]` entry in
`apps.toml`, and a marketplace seed listing whose capability manifest names the
Anthropic egress. It is an ordinary app under the contract (no build exception) —
peer to `notes` / `nutrition`, *not* part of the `tangram` shell crate. The
CLAUDE.md "Where things are" index gains a one-line pointer (`apps/guided-learning
— Make It Stick-driven tutor; AI-enabled component`).

**Dependency ordering — this is the crux of the merge strategy:**

1. **The AI-enabled-component pattern (morning-brief) should land first**, or at
   least be agreed, since guided learning *reuses* it and morning-brief *owns*
   the canonical statement. If morning-brief.md isn't in the tree when this
   builds, guided learning designs against the same model (this doc) and a short
   note records that morning-brief owns the pattern — but **two AI apps inventing
   the LLM-egress shape independently is the thing to avoid.** Coordinate so the
   `[apps.*.inject]` Anthropic rule (and any future first-class `[llm]` section,
   OD3) is defined once.
2. **PR #1 (fine-grained-egress) is a *soft* dependency, not a blocker.** Guided
   learning works today under the host-keyed `inject` rule (ADR-0005, shipped).
   PR #1 *tightens* the grant to the single declared call; the design declares
   that `[[calls]]` form (§5.3) so it's `enforce`-clean the moment PR #1 lands,
   but it does not need PR #1 to function. Build against the shipped host-keyed
   inject; adopt the call-level form when PR #1 merges (the compat shim makes
   this seamless).

**Merge approach: build on a dedicated branch and open a PR for review — do NOT
merge-immediately.** Rationale, consistent with how the other AI/egress-touching
work is being handled in this repo (fine-grained-egress and
manifest-verification are explicitly "held for review, not merged"):
- It introduces a **new LLM-egress consumer** and the **first-app-with-an-LLM-key
  prerequisite** — both touch the security boundary (egress + secret handling)
  that warrants a human pass.
- It depends on the morning-brief pattern and (softly) PR #1; a PR lets the
  reviewer confirm the inject rule and the `[[calls]]` declaration match whatever
  morning-brief / PR #1 settle on, rather than racing them in on `main`.
- The fixture-LLM test approach and the degrade-without-key behavior are exactly
  the kind of thing a reviewer should eyeball before it's load-bearing.

Concretely: **branch → all 8 checkpoints green (each its own commit + test) →
push → open PR** referencing morning-brief and PR #1, **held for review**. The
pure model/UI/sync parts (GL1–GL6, GL8) are independently reviewable and could
merge ahead of the egress-tightening (GL7) if the owner prefers to land the app
under the shipped host-keyed inject and follow up with the call-level form.

---

## Sources

- *Make It Stick: The Science of Successful Learning* — Peter C. Brown, Henry L.
  Roediger III, Mark A. McDaniel (Harvard University Press, 2014): retrieval
  practice, spaced/interleaved practice, generation, elaboration, reflection,
  calibration / the illusion of knowing.
- Codebase grounding:
  - `apps/nutrition/src/strategy/llm.rs` — the working Anthropic Messages-API
    call from a component (`claude-opus-4-8`, structured `json_schema` output,
    OAuth-vs-api-key credential split).
  - `apps/nutrition/src/lib.rs:101` (`log_meal`) — the `Ctx`-async,
    resolve-outside-the-lock, `ctx.mutate`-commit pattern + the WASM
    no-spawner-inline-resolution caveat (`:188-191`).
  - `apps/notes/src/lib.rs` — `#[model]` / `#[actions]` / additive-field
    (`Option<T>` + `#[autosurgeon(missing)]`) conventions.
  - `crates/tangram-host/wit/tangram.wit` — the closed `app` world
    (`http-fetch` / `log` / `now-ms`; no fs/sockets/inbound-HTTP).
  - `docs/RUNTIME_PLAN.md` — the app contract; Phase 2 capability model;
    ADR-0007 UI build exception; Phase 10a/10b secret resolver + egress
    injection.
  - [docs/design/fine-grained-egress.md](fine-grained-egress.md) (PR #1) — the
    call-level capability that scopes the LLM egress.
  - [docs/design/tangram-shell-redesign.md](tangram-shell-redesign.md) — the
    vault, marked+DOMPurify decision, the ADR-0007 exception, apps-as-iframes.
  - `docs/design/morning-brief.md` (canonical owner of the AI-enabled-component
    pattern — not yet in the tree as of writing).
