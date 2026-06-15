# Design: Morning Brief — Tangram's first AI-enabled component

**Status:** IMPLEMENTED (offline core, MB1–MB5) — shipped as
`apps/morning-brief`: the model, config/feedback actions, the pluggable source
seam with checked-in fixtures, the prompt builder, and a fully offline
`run_brief` (input_mode "fixture", zero-network, CI's flagship). The egress +
live tier (real Google/Anthropic calls, MB6–MB8) is the app's own later PR; the
host-side egress and verification machinery it will use already ships
(ADR-0008, verify.rs). This document is retained as the design record; where it
says "no production code", read it as the original plan.
**Date:** 2026-06-12
**Author:** Aaron (owner), with research + design by Claude
**Related (read these for the spine):**
[`fine-grained-egress.md`](fine-grained-egress.md) (PR #1, `fine-grained-egress-section-1`
— **not merged**; this design is built *against* it),
[ADR-0005](../adr/0005-egress-credential-injection.md) (host-side credential
injection — the component never holds the key),
[ADR-0006](../adr/0006-tenant-isolation-posture.md) (tiered tenancy; egress
injection as the load-bearing mitigation),
[`manifest-verification-plan.md`](manifest-verification-plan.md) (PR #2 — the
mechanical proof that a component imports only what it claims),
[RUNTIME_PLAN](../RUNTIME_PLAN.md) (**the app contract — no feature may
violate it**), [SDK_DESIGN](../SDK_DESIGN.md),
`crates/tangram-host/wit/tangram.wit`, `crates/tangram-host/src/runtime.rs`,
`apps/nutrition` (the async-`Ctx` + strategy + injected-egress shape),
`apps/notes` (the single-file model + UI shape).

---

## 0. Why this document exists

Morning Brief is Tangram's **first AI-enabled component**, and the goal is not
merely "an app that calls an LLM." It is to establish the **canonical
AI-enabled-component pattern** that the later Guided-Learning plan (and every
subsequent AI feature) reuses. The owner's framing is the whole point:

> "Run inside a component so that it's explicit *exactly* where that information
> and the summaries can go — it should only be possible to fetch the data, make
> an LLM call, and then write the resulting summary confined to the tangram,
> with **no opportunity to export it elsewhere**."

So the deliverable's center of gravity is the **containment argument**, and the
product (a configurable morning brief over calendar + email, with a
human-in-the-loop "dreaming"/refinement loop) is the vehicle that proves it.
The capability model already in the codebase — the closed WIT world
(`http-fetch`/`log`/`now-ms` and nothing else), host-side credential injection
(ADR-0005), and the call-level grant grammar (PR #1) — is exactly the machinery
that makes the containment a *theorem*, not a hope. This design shows how.

---

## 1. Problem & goals

### 1.1 Product

A **Morning Brief** is a once-a-day (or on-demand) AI-generated digest of the
user's day, assembled from their **Google Calendar** and **Gmail**, shaped by a
**user-configurable prompt** into a **user-configurable set of output sections**
(summary, highlights, action items, …). The user can run it against live or
fixture inputs, read the result, rate/annotate/correct it, and fold that
feedback back into the prompt — an **in-tangram prompt-iteration loop with
history** (the owner's "dreaming").

### 1.2 Goals (in priority order)

1. **Containment by construction.** The data fetched and the summaries produced
   can leave the component *only* through capabilities the operator explicitly
   granted, and the *only* egress capabilities granted are (read a data source)
   + (call the LLM). Writing the brief to the component's own replicated
   document is **not an egress**, so the brief's content physically cannot be
   exfiltrated — there is no allowlisted host to send it to. (§3, the
   containment theorem.)
2. **Least privilege, demonstrable.** Every granted capability is the narrowest
   that makes the feature work, expressed at the call grain (PR #1), and is
   *mechanically provable* via manifest verification (PR #2).
3. **Configurable, data-driven product.** The prompt and the output sections are
   first-class, editable parts of the replicated model — not hard-coded.
4. **Tight human feedback loop.** Running, inspecting, rating, and refining a
   brief is trivially easy and lives entirely inside the tangram, with run
   history.
5. **Pluggable sources.** Calendar + Gmail today, following the
   `apps/nutrition/src/strategy/` seam, so a new source is a new module, not a
   model change.
6. **Honors the app contract.** Single HTTP surface, env-only config, one data
   dir, declared egress; single-file no-build UI (the ADR-0007 build exception
   is shell-only). Every user-facing operation is a registered action.

### 1.3 Non-goals

- Sending email, creating/modifying calendar events, or *any* write to Google.
  Sources are **read-only** (this is also what makes containment clean).
- Acting on the brief (no "reply to this email" button that egresses). The
  brief is *advisory output that stays in the tangram*; acting on it is a
  separate, later, explicitly-granted capability.
- The general policy engine and arbitrary body inspection (fine-grained-egress
  §9.2) — out of scope there, out of scope here.

---

## 2. The AI-enabled-component pattern (canonical)

This is the reusable definition. Three capabilities, one containment theorem.

```
            ┌──────────────────────────────────────────────────────────┐
            │  morning-brief  (wasm32-wasip2 component)                  │
            │                                                            │
   (in)     │   ┌── async action: run_brief(Ctx) ──────────────────┐    │
   sources ─┼──▶│  1. fetch sources   (bare http-fetch ×N)         │    │
            │   │  2. build prompt    (pure, in-memory)            │    │
   LLM   ◀──┼───│  3. call LLM        (bare http-fetch)             │    │
            │   │  4. ctx.mutate(...) ── write brief to OWN doc ───┼──┐ │
            │   └──────────────────────────────────────────────────┘  │ │
            └────────────────────────────────────────────────────────│─┘
                                                                       │
   imports: ONLY  http-fetch, log, now-ms   (closed WIT world)        │
   grants:  ONLY  [read-source call(s)] + [call-LLM call]             │
            writing to local Automerge state is NOT an egress  ───────┘
```

### 2.1 LLM call = a scoped `http-fetch`

The component issues a **bare** HTTP request to the LLM endpoint — it never
names a credential. The host:

- enforces the request is to an allowlisted host *and* matches a declared call
  (PR #1): exactly `POST api.anthropic.com /v1/messages`;
- injects the API key host-side at the egress boundary (ADR-0005) — as a header
  (`x-api-key`) or bearer, per the chosen credential — so the key lives only in
  the host process, only for the duration of one outbound request.

The component therefore **never holds the key and cannot call any other
endpoint.** This is already exactly how `apps/nutrition/src/strategy/llm.rs`
talks to `api.anthropic.com/v1/messages` with `model = "claude-opus-4-8"` and a
structured `json_schema` output — Morning Brief reuses that call shape verbatim
(see §6.1, and that file is the working reference for the request body, the
`anthropic-version` header, and the OAuth-token-vs-api-key header distinction).

> **PREREQUISITE TO FLAG (owner decision):** there is currently **no LLM API key
> in `.env`** — it holds `GH_TOKEN`, `OP_SERVICE_ACCOUNT_TOKEN`,
> `CALORIENINJAS_API_KEY`, `TANGRAM_AUTH_TOKEN`, and **no `ANTHROPIC_API_KEY`**.
> Morning Brief requires an LLM key, injected via egress injection (never placed
> in the component). The recommendation is **Claude via the Anthropic Messages
> API**, keyed `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN` for an OAuth
> token), matching the nutrition LLM strategy and `.env.example` (which already
> documents both var names). Model: default to **`claude-opus-4-8`** for brief
> quality, with a cheaper tier (e.g. a Sonnet/Haiku-class model) selectable per
> the open decision in §11 — summaries are short and run daily, so cost favors a
> cheaper default for the routine path while keeping Opus available for a
> "deep" brief.

### 2.2 Data source = scoped read-only fetch (Google Calendar + Gmail)

Two routes evaluated; **recommendation below.**

**Route A — via a Google MCP server (RECOMMENDED).** The owner's stated
preference: "a google calendar mcp server w/ only the required access." The
component does a JSON-RPC `http-fetch` (`POST <mcp-endpoint>` with a
`{"jsonrpc","method","params",...}` body) to a Google Calendar/Gmail MCP server,
scoped by the **JSON-RPC-method body rung (PR #1, EC4)** to exactly the
read-only methods and nothing else:

```toml
[[apps.morning-brief.calls]]
method = "POST"
host   = "calendar-mcp.internal"        # or a hosted MCP endpoint
path   = "/mcp"
inject = { bearer = true, secret = "env://GOOGLE_MCP_TOKEN" }
# THE SHOWCASE: parse the JSON-RPC body ONLY because this matcher is present,
# and allow iff $.method ∈ this literal set. tools/call is allowed but the
# specific tool is itself read-only on the MCP server's side.
body   = { json_method = ["initialize", "tools/list", "tools/call"] }
```

This is the cleanest demonstration of *why fine-grained egress matters*: the
host enforces, at the egress boundary, that the component may invoke only the
declared JSON-RPC methods on that one endpoint — it cannot reach a
`create_event`/`send_message` method even if the MCP server exposes one, and it
cannot reach any other host. "Only the required access" is realized **twice**:
(i) the MCP server is configured/credentialed with read-only Google scopes
(`calendar.readonly`, `gmail.readonly` / `gmail.metadata`), and (ii) Tangram's
call-level grant pins which JSON-RPC methods the component may even ask for.
(A stricter posture, if the MCP server exposes write tools, is to declare two
calls and use the method rung to admit only the read-listing tools — but the
EC4 rung matches on `$.method`, i.e. `tools/call`, not on the *tool name* inside
`params`; matching the tool name would require a deeper body selector that
fine-grained-egress deliberately does **not** provide. The honest mitigation is
therefore "point at an MCP server that only *has* read tools, scoped by Google
OAuth," with the method rung as the outer fence — see §9 residual risk.)

**Route B — directly to the Google REST APIs.** The component does scoped
fetches to `www.googleapis.com` / `gmail.googleapis.com`, with an OAuth access
token injected host-side (ADR-0005), each call pinned to a read-only path:

```toml
[[apps.morning-brief.calls]]
method = "GET"
host   = "www.googleapis.com"
path   = "/calendar/v3/calendars/{calendarId}/events"
inject = { bearer = true, secret = "env://GOOGLE_OAUTH_TOKEN" }

[[apps.morning-brief.calls]]
method = "GET"
host   = "gmail.googleapis.com"
path   = "/gmail/v1/users/me/messages"
inject = { bearer = true, secret = "env://GOOGLE_OAUTH_TOKEN" }
```

"Only the required access" here is the **OAuth scope** minted into the token
(`calendar.readonly`, `gmail.readonly`/`gmail.metadata`) plus the path pins
(only the list/get read endpoints are declared; the host denies and
un-credentials any undeclared path on those hosts — closing the same-host
write-endpoint footgun fine-grained-egress §1 names).

**Recommendation: Route A (MCP) for v1, Route B as the fallback.** Rationale:
(i) the owner asked for it; (ii) the MCP server centralizes the OAuth dance,
token refresh, and Google API churn *outside* the Tangram host, so the host's
job stays "inject a bearer for one endpoint, gate the JSON-RPC method"; (iii) it
is the headline showcase for the EC4 method rung, which is exactly the pattern
the Guided-Learning plan and any future MCP-backed source will reuse; (iv) it
keeps Morning Brief's own egress declarations tiny (one host) regardless of how
many Google endpoints the brief touches. Route B's advantage — no extra service
to run — makes it the right *fallback* if no read-only Google MCP server is
available at build time, and the **source strategy seam (§7) makes the two
interchangeable** without touching the model or actions. The token-refresh
question (who refreshes the Google OAuth token, and whether the host's secret
resolver or the MCP server owns it) is an open decision (§11).

### 2.3 Output = written to the component's OWN Automerge state (not an egress)

The brief — summary, per-section content, highlights, action items — is written
into the component's replicated document via `ctx.mutate(...)`, exactly like
nutrition caches a resolved nutrient panel. **Writing local state is not an
egress capability**; it uses no host import beyond the doc-in/doc-out dispatch
contract. The brief then surfaces through the *existing* per-app surface the
host already serves (UI, `/api/state`, `/sync`, `/mcp`) — i.e. it goes to the
user's own synced replicas and to authorized readers of the document, and
nowhere else.

### 2.4 The containment theorem

> **Claim.** Given (a) the closed WIT world (`http-fetch`, `log`, `now-ms`; empty
> WASI ctx — no filesystem, no sockets, no inbound HTTP), and (b) an effective
> grant whose declared calls are exactly {read-source, call-LLM}, the brief's
> content can reach **only** the user's own document (via local state writes that
> the host syncs) and can be **sent outbound to no destination other than the
> already-declared source/LLM endpoints** — and a source/LLM endpoint is not a
> place a *brief* would be exfiltrated to.

> **Proof sketch.**
> 1. The only way bytes leave the component is `http-fetch` (the WIT world has no
>    other I/O — `runtime.rs` links wasip2 with an empty WASI ctx; `log` goes to
>    the host's tracing, `now-ms` returns a scalar). State writes are returned as
>    doc bytes through `dispatch` and merged by the host into the user's *own*
>    document — not an outbound network path.
> 2. Every `http-fetch` is gated host-side (`HostState::http_fetch`) by the host
>    fence (`allow_hosts`) **and** the call match (PR #1): a request matching no
>    declared call is **denied** and **un-credentialed** before any byte leaves
>    the host process.
> 3. The declared calls are exactly {read-source, call-LLM}. There is no declared
>    call to an attacker-controllable host, so there is no allowlisted
>    destination to exfiltrate a brief to.
> 4. The credential for each call is injected host-side and bound to that call
>    (ADR-0005 + PR #1): the component cannot replay it to a sibling endpoint, and
>    cannot read it at all.
> ∴ The brief's content is confined to the tangram. ∎

> **Tie to manifest verification (PR #2).** The theorem rests on premise (a) —
> "imports only `http-fetch`/`log`/`now-ms`, nothing else" — and premise (b) —
> "granted ⊆ declared." PR #2 makes both **mechanically checked at converge**:
> the audit reads the component's *actual* function-level imports (it proves the
> component imports `http-fetch` and **no `wasi:sockets`/`wasi:http`** — the
> closed-world invariant), and the subset chain (`granted ⊆ declared ⊆ audited`)
> proves the operator granted no host/call/inject beyond what the manifest
> declared. So the containment theorem is not a code-review assertion; it is a
> property the host re-verifies on every install. Morning Brief is the
> motivating example that makes PR #2's call-grain arm (CP6) concrete: its
> declared calls are exactly two, and the verifier proves the grant doesn't
> exceed them.

> **The honest residual** (carried from fine-grained-egress §8 and
> manifest-verification §5): *exfil within a declared call*. A compromised
> component can still put exfiltrated data **in the body of a declared call** —
> e.g. smuggle a stolen email into the `messages` array of the legitimate
> `POST api.anthropic.com/v1/messages` (the LLM call has a large attacker-usable
> body) or into the JSON-RPC `params` of a declared MCP call. Call-level scoping
> shrinks the egress surface to exactly the declared calls and binds the
> credential to them; it cannot read intent within a call. This is the same
> residual Anthropic acknowledges (the deterministic boundary narrows the
> surface; it does not judge intent), and it is mitigated only out of band
> (`max_body_bytes` caps on the source calls, the human-review/model layer, and —
> critically — that the LLM endpoint is *Anthropic's own*, not an
> attacker-controlled "LLM"). It must be stated plainly in the security section
> (§9), not hidden.

---

## 3. The model (Automerge shape)

Conventions enforced (CLAUDE.md): deterministic `Default` (it becomes the shared
genesis commit); `Vec`, never `HashMap`; any field *added later* to a shipped
model is `Option<T>` + `#[autosurgeon(missing = "Option::default")]`. The
**initial** model below can use non-`Option` fields freely (they exist from
genesis); the `Option`+`missing` discipline binds *evolution*, and §3.4 calls
out the fields most likely to grow.

```rust
use tangram::prelude::*;

#[model]
pub struct MorningBrief {
    /// The editable global prompt: the principal axis the user tunes —
    /// "what to surface" vs "what needs immediate action." First-class,
    /// not hard-coded. (§4 product requirement.)
    config: BriefConfig,
    /// Data-driven output sections; order is render order. NOT hard-coded.
    sections: Vec<OutputSection>,
    /// Which sources to pull and how (pluggable; see §7). Each entry is a
    /// source *selection* + its scope knobs — never a secret.
    sources: Vec<SourceConfig>,
    /// Run history — the spine of the dreaming/feedback loop (§8). Newest
    /// last; capped by `config.max_runs` at mutate time (see §3.3).
    runs: Vec<BriefRun>,
    /// Learned few-shot examples distilled from human feedback, folded into
    /// the prompt preamble on the next run (§8). Data, not code.
    learned: Vec<LearnedExample>,
}

#[model]
pub struct BriefConfig {
    /// The master instruction. The axis the owner named lives here as plain
    /// text the user edits: what to merely surface vs. what is action-now.
    system_prompt: String,
    /// Default model tier for a run: "default" | "deep" (maps to a model id
    /// host-side / in a const table; the component never embeds a key).
    model_tier: String,
    /// Cap on retained runs (oldest evicted at mutate time — bounded doc).
    max_runs: u32,
}

#[model]
pub struct OutputSection {
    id: String,
    /// "Summary", "Highlights", "Action items" — user-named.
    title: String,
    /// The sub-prompt for THIS section. Data-driven: adding a section is an
    /// action, not a code change.
    prompt: String,
    /// Render hint: "prose" | "bullets" | "checklist".
    format: String,
    /// Render/run order.
    position: i64,
    enabled: bool,
}

#[model]
pub struct SourceConfig {
    /// "calendar" | "gmail" (matches a Source strategy name — §7).
    kind: String,
    enabled: bool,
    /// Scope knobs that are NOT secrets: how far back/forward, max items,
    /// which labels/calendars by id. Secrets are NEVER here — credentials
    /// are injected host-side (ADR-0005).
    window_hours_back: i64,
    window_hours_fwd: i64,
    max_items: i64,
    /// Free-form selector (calendar ids / gmail label query) — opaque to the
    /// model, passed to the source strategy.
    selector: String,
}

#[model]
pub struct BriefRun {
    id: String,
    created_at_ms: i64,
    /// "fixture" | "live" — a run against fixtures needs no Google/LLM egress
    /// (§8.2), which is what makes CI and "dreaming" cheap and offline.
    input_mode: String,
    /// Snapshot of the inputs the run actually saw (redactable digest +
    /// item count + a bounded preview), so a run is reproducible and the
    /// human can see what the model saw. NOT the raw mailbox.
    input_summary: String,
    /// The produced sections, parallel to `sections` by id at run time.
    outputs: Vec<SectionOutput>,
    /// The exact prompt sent (post learned-preamble fold) — auditable.
    effective_prompt: String,
    /// "ok" | "error: ..." — a failed run is recorded, not silently dropped.
    status: String,
    /// Human feedback on this run (the dreaming loop — §8).
    feedback: Option<RunFeedback>,
}

#[model]
pub struct SectionOutput {
    section_id: String,
    title: String,
    /// The model's text for this section.
    content: String,
}

#[model]
pub struct RunFeedback {
    /// 1..=5, or 0 for "unrated."
    rating: i64,
    /// Free-text annotation/correction from the human.
    note: String,
    /// Per-section corrections the human typed (id → corrected text); these
    /// become candidate few-shot examples (§8).
    corrections: Vec<SectionCorrection>,
    created_at_ms: i64,
}

#[model]
pub struct SectionCorrection {
    section_id: String,
    corrected: String,
}

#[model]
pub struct LearnedExample {
    /// Distilled from a highly/poorly-rated run + its corrections: a compact
    /// "given inputs like X, prefer output like Y" pair folded into the
    /// preamble. Curated by an action, capped, and editable.
    id: String,
    note: String,
    weight: i64,
    created_at_ms: i64,
}
```

### 3.1 Deterministic `Default`

`Default` seeds a sensible, **deterministic** starting config so a fresh
instance is immediately useful and every replica derives the *same* genesis
(byte-identical `genesis()` is what lets host-managed docs merge — see
`store.rs` `GENESIS_ACTOR`/zero-timestamp). Seed:

- `config`: a default `system_prompt` that states the surface-vs-act axis
  explicitly ("Surface what I should be aware of; separately, flag only what
  needs my action today and why."); `model_tier = "default"`; `max_runs = 30`.
- `sections`: the **default output sections** (§11 open decision) — recommend
  shipping three: `Summary` (prose), `Highlights` (bullets), `Action items`
  (checklist), with deterministic ids/positions.
- `sources`: calendar + gmail entries, `enabled = false` by default (so a fresh
  instance does **no egress** until the user opts in and the operator grants the
  calls — fail-safe), with sensible windows.
- `runs`, `learned`: empty.

No `now_ms()` / `uuid` in `Default` (those are nondeterministic) — genesis ids
are fixed literals; runtime-created ids (runs, learned, user-added sections) use
`uuid`/`now_ms()` inside actions, exactly as notes/nutrition do.

### 3.2 Why `Vec` not `HashMap`, and lookups

All collections are `Vec` (CLAUDE.md). Lookups are by scanning for an `id`
(`sections.iter().find(|s| s.id == ...)`), as nutrition/notes do — fine at this
scale (sections and runs are small; runs are capped).

### 3.3 Bounded growth

`runs` is capped at `config.max_runs`: the `record_run` action evicts the oldest
when over cap (a deterministic, attributed mutation). This keeps the replicated
doc bounded — a brief a day for a year is ~365 runs uncapped; the cap keeps it
small and the history meaningful (recent + feedback-bearing runs retained
preferentially — see §8).

### 3.4 Evolution discipline (the fields most likely to grow)

When this ships and later grows: `BriefConfig` will gain fields (e.g. a
per-section default model, a timezone, a quiet-hours window) and `BriefRun` will
gain fields (e.g. token/cost accounting, latency). **Every such added field must
be `Option<T>` + `#[autosurgeon(missing = "Option::default")]`** (the
`updated_at_ms`-on-notes lesson). `RunFeedback` is already `Option` on
`BriefRun` precisely because a run exists before feedback does. New
output-section `format` values and source `kind`s are *data* (strings), so they
need no schema change — that is the point of the data-driven model.

---

## 4. Actions (sync vs `Ctx`-async)

The store lock is never held across an await. Sync actions are pure state
transitions; the one async action resolves sources + the LLM **outside** the
lock and commits via `ctx.mutate` — exactly nutrition's `log_meal` shape.

### 4.1 Sync actions (pure, `&mut self`)

Configuration and feedback are all pure mutations (instant, optimistic,
attributed, undoable):

- `set_system_prompt(prompt: String)` — edit the master prompt.
- `set_model_tier(tier: String)`.
- `add_section(title, prompt, format) -> String` / `update_section(id, ...)` /
  `remove_section(id)` / `reorder_section(id, position)` / `set_section_enabled(id, bool)`
  — the data-driven output sections.
- `set_source(kind, enabled, window_hours_back, window_hours_fwd, max_items, selector)`
  — configure a source (no secrets).
- `rate_run(run_id, rating, note)` / `correct_section(run_id, section_id, corrected)`
  — the human feedback (§8).
- `promote_to_learned(run_id, note, weight) -> String` / `remove_learned(id)` /
  `set_learned_weight(id, weight)` — curate the learned preamble (§8).
- `delete_run(run_id)` — prune history by hand.
- `list_runs() -> Vec<BriefRun>` / `get_run(run_id) -> Result<BriefRun,_>` /
  `list_sections()` — read views (the auto-UI and MCP read these).

### 4.2 The async action (`Ctx`, network)

```rust
/// Generate a brief now. `input_mode = "live"` fetches the configured
/// sources and calls the LLM (both via the host's allowlist+call-gated
/// http-fetch); `input_mode = "fixture"` uses the bundled fixtures and
/// makes NO egress (CI/dreaming path — §8.2). Resolves everything OUTSIDE
/// the lock; commits the run via ctx.mutate. Returns the new run id.
pub async fn run_brief(ctx: Ctx<Self>, input_mode: Option<String>) -> Result<String, String> {
    let cfg = ctx.state()?;                     // snapshot, lock released
    let mode = input_mode.unwrap_or_else(|| "live".into());

    // 1. Resolve sources OUTSIDE the lock (each Source strategy issues bare
    //    http-fetch — Route A: one JSON-RPC POST to the MCP endpoint; the
    //    host injects the bearer and gates the JSON-RPC method).
    let inputs = match mode.as_str() {
        "fixture" => Sources::fixtures(&cfg.sources),
        _ => Sources::resolve(&cfg.sources).await?,   // network, never under lock
    };

    // 2. Build the prompt (pure): system_prompt + learned preamble + the
    //    per-section sub-prompts + the resolved inputs. The surface-vs-act
    //    axis lives in the prompt text.
    let prompt = build_prompt(&cfg, &inputs);

    // 3. Call the LLM (bare http-fetch to api.anthropic.com/v1/messages;
    //    host injects the key — ADR-0005 — and the call is pinned by PR #1).
    //    Reuses the exact request shape in nutrition/src/strategy/llm.rs.
    let outputs = match mode.as_str() {
        "fixture" if cfg!(fixture_offline) => Llm::fixture(&prompt),  // see §8.2
        _ => Llm::generate(&cfg, &prompt).await?,
    };

    // 4. Commit the run to OUR OWN doc (the only "output" — not an egress).
    ctx.mutate("run_brief", |m| m.record_run(mode, inputs.summary(), prompt, outputs))
        .map_err(|e| e.to_string())
}
```

`record_run` is a **private** sync helper (not an action) that pushes a
`BriefRun`, applies the `max_runs` cap, and returns the id — the
nutrition-`commit_meal` pattern. Note the WASM/native split nutrition documents:
inside a component there is no background spawner, so all of `run_brief` runs
inline in the single dispatch call (which is correct here — a brief is a
foreground operation the user waits on).

### 4.3 What stays a custom route, not an action

Per the contract, every *operation* is an action. The only non-operation is a
**capabilities probe** (`GET /api/capabilities`, the nutrition pattern): it
reports whether the LLM credential and each source are *configured* — derived
host-side and ANDed in (ADR-0005: the component can't see the key, so the host
decides "configured"), so the UI can show "LLM not configured — set
`ANTHROPIC_API_KEY`" without the component ever holding the secret. No other
custom routes.

---

## 5. Egress grants (`[[calls]]`) and why each is least-privilege

Designed against PR #1's grammar. Morning Brief declares **exactly the calls it
makes**, nothing broader. `enforcement = "enforce"` (the app declares ≥1 call,
so PR #1's migration default puts it straight into enforce — §7.2 of that doc).

```toml
[apps.morning-brief]
component = "target/wasm32-wasip2/release/morning_brief.wasm"
ui = "apps/morning-brief/ui"
allow_hosts = ["api.anthropic.com", "calendar-mcp.internal"]  # coarse outer fence
require_auth = true   # the brief is personal; gate mutating routes (see §6.3)

# (1) The LLM call — the ONLY way to reach a model. Pinned to one method+path.
[[apps.morning-brief.calls]]
method = "POST"
host   = "api.anthropic.com"
path   = "/v1/messages"
inject = { header = "x-api-key", secret = "env://ANTHROPIC_API_KEY" }
# or { bearer = true, secret = "env://ANTHROPIC_AUTH_TOKEN" } for an OAuth token
headers = { required = { "content-type" = "application/json" } }
# max_body_bytes caps how much can ride out in the prompt body (residual §9).

# (2) The source call — the ONLY way to reach Google data (Route A: MCP).
[[apps.morning-brief.calls]]
method = "POST"
host   = "calendar-mcp.internal"
path   = "/mcp"
inject = { bearer = true, secret = "env://GOOGLE_MCP_TOKEN" }
# THE EC4 SHOWCASE: parse the JSON-RPC body and admit only these methods.
body   = { json_method = ["initialize", "tools/list", "tools/call"] }
```

Why each is least-privilege:

- **LLM call**: a *single* `(method, host, path)`. The component cannot reach any
  other Anthropic endpoint (no Files API, no batches), cannot reach any other
  host, and cannot read the key. This is precisely the closing of the
  "every function on an allowlisted domain is an attack surface" class
  (fine-grained-egress §2 — the api.anthropic.com Cowork incident).
- **Source call (Route A)**: a *single* host + path, and the **JSON-RPC method
  rung** restricts which RPCs the component may even invoke. No `create_event`,
  no `send`, no `resources/read` of arbitrary URIs. The bearer is bound to this
  call only.
- **No `data_dir` egress, no other hosts.** The brief's storage is the
  component's own doc — not a grant.

If Route B (direct Google) is chosen, the LLM call is unchanged and the single
MCP call is replaced by the two read-only Google calls in §2.2 (each path-pinned,
each bearer-injected with a read-only-scoped OAuth token). Either way the
*declared call count is 2–3*, which is what makes the manifest-verification
subset chain (PR #2) trivially auditable.

---

## 6. UI (single-file, no build — the app contract)

A single `apps/morning-brief/ui/index.html` (+ inline `<style>`/`<script>`),
relative-path fetches only, prefix-mountable, iframable — the notes/nutrition
shape, the shared dark "performance-console" design language (see
`apps/notes/ui/index.html`'s comment for the palette). **No build step** (the
ADR-0007 Vite carve-out is shell-only; "no feature may violate the contract"
binds this app). It renders over the existing `/api/state` + SSE `/api/events`
and posts actions to `/api/actions/{name}`.

Three panes (responsive to the 420px tile like notes):

1. **Today's brief** — the most recent `BriefRun` rendered section-by-section
   (prose / bullets / checklist per `format`), with a prominent **Run brief**
   button (calls `run_brief`, `input_mode: "live"`) and a **Dry-run on fixtures**
   button (`input_mode: "fixture"` — no egress; the dreaming entry point).
2. **Configure** — edit the master `system_prompt` (the surface-vs-act axis,
   front and center), add/reorder/edit/toggle output sections (each with its
   sub-prompt), and toggle/scope sources. The capabilities probe drives an
   inline "LLM/source configured?" indicator.
3. **History & dreaming** — the run list (newest first), each expandable to its
   inputs-summary + outputs + effective prompt; per-run **rate (1–5)**,
   **annotate**, and **inline-correct a section**; a **Promote to example**
   action that distills a run+feedback into a `LearnedExample`.

### 6.3 Personal-data posture

The brief contains personal data (calendar/email-derived). It is *already*
contained to the user's own document, but the surface that serves it should be
gated: set `require_auth = true` so mutating routes need the bearer, and in
multi-tenant deployments the brief lives under `/t/<tenant>/morning-brief/`
where *every* request resolves a `Principal` (Phase 5/6 — reads included). For
the canonical single-owner localhost deployment, the trusted-loopback posture
applies as for every app.

---

## 7. Pluggable sources (the strategy seam)

Mirror `apps/nutrition/src/strategy/`. A `Source` is *how a kind of input is
fetched and normalized into a common `BriefInput` shape*:

```
apps/morning-brief/src/source.rs          // the Source trait + selection
apps/morning-brief/src/source/calendar.rs // Route A: JSON-RPC to the MCP server
apps/morning-brief/src/source/gmail.rs     // (read-only methods only)
apps/morning-brief/src/source/fixtures.rs  // bundled fixtures (no egress) — §8.2
```

```rust
/// One normalized input item the prompt is built from (calendar event,
/// email summary, …) — source-agnostic.
pub struct BriefInput { pub kind: String, pub when_ms: i64, pub title: String, pub detail: String }

pub trait Source {
    fn name(&self) -> &'static str;                 // "calendar" | "gmail"
    /// Fetch over the network via the host's http-fetch (Route A: one
    /// JSON-RPC POST). NEVER under the store lock. Read-only by construction.
    async fn fetch(&self, cfg: &SourceConfig) -> anyhow::Result<Vec<BriefInput>>;
}
```

Selection is data-driven from `SourceConfig.kind` (the nutrition
`Strategy::from_env` analog, but driven by the *model* not env, since which
sources to pull is a user choice, not a deployment secret). Adding a source =
add a module + a `kind` string; **no model change**. The LLM call is likewise a
thin `Llm` module reusing `nutrition/src/strategy/llm.rs` almost verbatim
(swap the system prompt + schema; keep the Messages API call, the
`anthropic-version` header, and the OAuth-vs-api-key header logic — though under
ADR-0005 the *host* attaches the credential, so the component issues the bare
request and the header-selection logic moves to the inject rule).

---

## 8. The dream / feedback design

The owner's emphasis: make it **very easy** to run the brief, inspect, give
feedback, and refine — in-tangram prompt iteration with history.

### 8.1 The loop

1. **Run** (`run_brief`) → a `BriefRun` is stored with its inputs-summary,
   effective prompt, and outputs.
2. **Inspect** — the History pane shows exactly what the model saw
   (`input_summary`) and produced, and the *effective prompt* (so the human can
   see how their config + learned preamble combined).
3. **Feedback** — `rate_run` (1–5 + note) and `correct_section` (type the
   corrected text for a section). All pure, attributed mutations — instantly
   replicated, undoable, and (via Tangram's per-actor attribution) showing
   "human edited the AI's output."
4. **Refine** — two folds back into the next run:
   - **Prompt edits** are immediate (`set_system_prompt` / `update_section`).
   - **Learned few-shot**: `promote_to_learned` distills a run + its corrections
     into a `LearnedExample` (a compact "prefer Y for inputs like X" pair). The
     prompt builder (§4.2 step 2) folds the highest-weighted learned examples
     into the system preamble, capped to a token budget. This is *in-tangram
     learning* without external state — the examples live in the replicated doc,
     sync across devices, and are editable/removable.

### 8.2 Fixtures = cheap, offline dreaming (and CI)

`input_mode: "fixture"` runs against **bundled fixture inputs**
(`source/fixtures.rs` + a checked-in `fixtures/` of representative
calendar/email JSON), making **no Google egress**. Two sub-modes for the LLM:

- **fixture-live LLM**: real `api.anthropic.com` call against fixture inputs —
  iterate on the *prompt* against stable inputs without touching the mailbox.
  (Cheap, deterministic-ish inputs; the model is the only variable.)
- **fixture-offline LLM** (CI): a bundled canned model response, so the *entire*
  pipeline runs with **zero network** — the path CI exercises (§10), proving
  model shape, section rendering, run recording, and feedback folding with no
  live Google/LLM.

This makes "dreaming" the *default* developer and user move: dry-run on fixtures,
tune the prompt, see the diff, promote a good result to a learned example — all
without spending an LLM call or touching real email until the user runs live.

### 8.3 Why history is bounded but feedback-preserving

The `max_runs` eviction (§3.3) evicts *oldest unrated* runs first and keeps runs
that carry feedback/corrections (they are the training signal). This keeps the
doc small while never throwing away the human's investment.

---

## 9. Security analysis

- **Containment (the headline).** §2.4's theorem: the only egress capabilities
  are {read-source, call-LLM}; local state writes are not egress; therefore the
  brief cannot leave. PR #2 mechanically re-verifies the two premises at every
  converge (closed-world import audit + `granted ⊆ declared` subset chain).
- **Least-privilege scopes.** LLM call pinned to one method+path on one host;
  source access pinned to one MCP endpoint with the JSON-RPC method rung (or two
  read-only Google paths). Google "only the required access" = read-only OAuth
  scopes (`calendar.readonly`, `gmail.readonly`/`gmail.metadata`) on the
  token/MCP server, **plus** Tangram's call-level gate. The credential is never
  in the component (ADR-0005) and is bound to its call (PR #1) — no replay to a
  sibling endpoint, no key exfil.
- **Prerequisite LLM key (gap to close).** `.env` has **no `ANTHROPIC_API_KEY`**.
  The app is fail-safe without it: sources `enabled = false` by default, the
  capabilities probe reports "LLM not configured," and a live `run_brief`
  returns a clear "set ANTHROPIC_API_KEY" error (the nutrition
  degraded-not-crashed pattern) — but it does nothing useful until the owner
  provisions the key and grants the LLM call. This is an **owner prerequisite**,
  not something the design can paper over.
- **Residual risk — exfil within a declared call (honest, unclosed).** Carried
  from fine-grained-egress §8 / manifest-verification §5: a compromised component
  can smuggle data in the *body* of a declared call — e.g. put stolen email text
  in the `messages` of the legitimate LLM POST, or in the JSON-RPC `params` of a
  declared MCP call. Mitigations are out of band: `max_body_bytes` caps on source
  calls; the destinations being *trusted* (Anthropic's own API, your own read-only
  MCP server) rather than attacker-controlled; the human-review/model layer; and
  the per-actor attribution that makes AI writes reviewable/undoable. The
  call-level model **narrows the surface to exactly the declared calls and binds
  the credential to them — it does not read intent within a call.** This must be
  stated, not hidden.
- **Method-rung granularity caveat (Route A).** EC4 matches `$.method`
  (`tools/call`), not the *tool name* inside `params`. If the MCP server exposes
  write tools, the rung alone cannot forbid `tools/call` of a write tool — the
  authoritative control is to point at an MCP server that *only has* read tools,
  scoped by Google OAuth. The rung is the outer fence; the server's tool set +
  OAuth scope is the inner truth. Route B sidesteps this (paths are method-pinned
  directly) at the cost of running the OAuth dance in-host.
- **Microarchitectural side-channels (ADR-0006).** Orthogonal and unchanged.
  First-party in-process WASM is sufficient at Tier 1; for an untrusted/
  multi-tenant deployment, ADR-0006's tiering (and the not-yet-built per-component
  fuel/memory limits) governs — but note ADR-0005 already removes the brief's
  high-value secret (the LLM/Google credential) from the component, which is the
  load-bearing mitigation.
- **Parser-differential discipline (PR #1).** Egress matching canonicalizes once
  at the seam; Morning Brief inherits that and adds no value/regex matching. The
  adversarial canonicalization tests live in PR #1 (EC1) and are shared by the
  manifest verifier (PR #2 CP6) — Morning Brief is a *consumer* of that seam, not
  a place to re-implement it.

---

## 10. Phased, testable checkpoints

Each checkpoint is independently shippable with its own test; full gate green
before each (`cargo build --workspace`, `clippy -D warnings`, `fmt --check`,
plus the wasm32-wasip2 component build the integration tests need). **All
"live" paths are fixture-backed for CI** (§8.2) so no live Google/LLM is needed.

- **MB1 — Model + config actions (no network).** The `MorningBrief` model
  (deterministic `Default` seeding the default sections), all the sync
  config/section/source actions, and the read views. *Test:*
  `let mut m = MorningBrief::default(); add_section/...; list_sections()` — pure
  unit tests (the trivial-test property of pure actions); assert genesis is
  deterministic (two `Default`s reconcile byte-identically — the
  `genesis_bytes()` invariant). Independently shippable: a configurable but
  not-yet-running brief.

- **MB2 — Source seam + fixtures (no network).** The `Source` trait, the
  `calendar`/`gmail` modules (Route A JSON-RPC request *construction* only), and
  `fixtures.rs` with a checked-in `fixtures/` set. *Test:* `Sources::fixtures`
  yields the expected normalized `BriefInput`s; the request *builders* produce
  the exact JSON-RPC body/path/method the grants pin (assert on the constructed
  request, not by sending it). No egress in this test.

- **MB3 — Prompt builder + LLM module with fixture-offline mode.** `build_prompt`
  (system + learned preamble + per-section sub-prompts + inputs), the `Llm`
  module, and `run_brief` end to end in **fixture-offline** mode (canned model
  response). *Test:* `run_brief(input_mode: "fixture")` with the offline LLM
  produces a `BriefRun` with one `SectionOutput` per enabled section, records it,
  applies the `max_runs` cap, and makes **zero** `http-fetch` calls. This is the
  CI flagship — the whole pipeline, no network.

- **MB4 — Feedback + dreaming loop.** `rate_run`, `correct_section`,
  `promote_to_learned`, and the preamble fold. *Test:* a poorly-rated run +
  corrections → `promote_to_learned` → a subsequent fixture run's
  `effective_prompt` contains the learned example; the `max_runs` eviction keeps
  feedback-bearing runs. Pure/sync — trivial to test.

- **MB5 — UI (single-file, no build).** `ui/index.html` rendering the three
  panes over `/api/state` + SSE, posting actions, with the Run / Dry-run buttons
  and the rate/correct/promote affordances. *Test:* a host integration test (or
  the existing fleet smoke harness) serves `/morning-brief/` and the relative
  fetches resolve under the prefix; the capabilities probe drives the
  "configured?" indicator. (Manual `verify` skill pass for the live UX.)

- **MB6 — Egress grants + enforcement (gated on PR #1).** Add the `[[calls]]`
  block (§5); a host integration test proves the **declared** LLM call is
  credentialed and an **undeclared** call (e.g. `POST api.anthropic.com/v1/files`
  or a write JSON-RPC method) is **denied + un-credentialed** under
  `enforcement = "enforce"`, and the EC4 method rung admits `tools/call` but
  denies an undeclared method. *Run:* mirrors PR #1's `egress_enforcement` /
  `jsonrpc_method_match` shape, parameterized for morning-brief. **This is the
  checkpoint that demonstrates the containment theorem operationally** — it only
  exists once PR #1 lands.

- **MB7 — Live source + LLM (manual / opt-in, owner key required).** With
  `ANTHROPIC_API_KEY` and a Google MCP token provisioned, a manual `run_brief`
  against the live MCP server + Anthropic produces a real brief. *Test:* a
  self-skipping integration test (the nutrition-egress-test pattern: skips
  without the live key/server), plus a `verify`-skill manual pass. Not in CI's
  required set.

- **MB8 — Manifest verification ties in (gated on PR #2).** Morning Brief's
  declared calls (exactly 2–3) are verified by the subset chain; a deliberately
  over-granted spec (an extra host) hard-fails converge; an honest spec reports
  `verified: true` in `/api/fleet`. *Run:* the PR #2 `over_grant_fails_converge`
  / `call_grain_subset` checkpoints, applied to morning-brief as the motivating
  example.

Ordering lets the owner sign off incrementally: MB1–MB4 deliver a fully working,
fixture-driven, offline brief with the dreaming loop (shippable and CI-green with
no network and no LLM key); MB5 the UI; MB6/MB8 land the egress + verification
proofs as their prerequisite PRs merge; MB7 is the live demo behind the owner's
key.

---

## 11. Effort estimate

Sizing convention as in the sibling plans: **1 agent-session** ≈ one focused
`/dev` implement+test+verify loop ending green on all gates, owner review
between sessions.

| Phase | What | Size | Complexity | Risk |
|---|---|---|---|---|
| MB1 | Model + config actions + deterministic `Default` | 1 session | Low | model-evolution discipline |
| MB2 | Source seam + fixtures (request construction) | 1 session | Low–Med | matching the MCP/Google request shape exactly |
| MB3 | Prompt builder + LLM module + fixture-offline `run_brief` | 1–2 sessions | Med | prompt assembly; reusing the nutrition LLM call cleanly |
| MB4 | Feedback + learned-preamble fold | 1 session | Low–Med | eviction-keeps-feedback logic |
| MB5 | Single-file UI (three panes, no build) | 1–2 sessions | Med | dreaming UX in one HTML file |
| MB6 | Egress grants + enforcement test | 0.5 session | Low | **gated on PR #1** |
| MB7 | Live source + LLM (manual) | 0.5 session | Med | OAuth token provisioning (owner) |
| MB8 | Manifest-verification tie-in | 0.5 session | Low | **gated on PR #2** |

**Total: ~6–8 agent-sessions / ~4–5 working days** of agent time with owner
review between phases. The offline core (MB1–MB5) is ~5–6 sessions and is fully
shippable and CI-green *before* PR #1/#2 land; MB6/MB8 are thin once they do.

---

## 12. Open decisions for the owner

1. **LLM provider/model + key (prerequisite).** Recommend **Claude via the
   Anthropic Messages API**, `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`),
   injected via egress injection — matching `nutrition/src/strategy/llm.rs` and
   `.env.example`. **Model:** default **`claude-opus-4-8`** for brief quality,
   with a cheaper tier (Sonnet/Haiku-class) for the routine daily path
   (`BriefConfig.model_tier`); recommend the cheaper tier as the *default* run
   and Opus as a "deep" run, since briefs are short and daily. **Owner must
   provision the key** — there is none in `.env` today.
2. **Source route: MCP (Route A) vs direct Google (Route B).** Recommend **MCP**
   (owner's stated preference; centralizes OAuth/refresh outside the host;
   showcases the EC4 method rung). Decide: which read-only Google MCP server, and
   **who owns Google OAuth token refresh** (the host's secret resolver vs. the
   MCP server). Route B is the seam-compatible fallback.
3. **Default output sections.** Recommend shipping three by default — **Summary**
   (prose), **Highlights** (bullets), **Action items** (checklist), with the
   master prompt's surface-vs-act axis explicit. Owner to confirm/add (e.g.
   "Prep for first meeting," "Unanswered threads").
4. **Read-only scope shape for Gmail.** `gmail.readonly` (full read) vs
   `gmail.metadata` (headers/labels only, no body). Metadata-only is the stronger
   least-privilege posture if the brief can work from subjects/senders;
   `readonly` if body content materially improves summaries. Owner's call on the
   privacy/quality trade.
5. **`max_runs` default and eviction policy** (recommend 30, keep
   feedback-bearing — §8.3). Confirm.

---

## 13. Placement + merge-strategy recommendation

**Placement: a new first-party app at `apps/morning-brief/`** (crate
`tangram-morning-brief`, on-host name `morning-brief`), structured exactly like
`apps/nutrition`:

```
apps/morning-brief/
  Cargo.toml            # native bin + crate-type = ["cdylib"] for wasm32-wasip2
  src/lib.rs            # #[model] MorningBrief + #[actions] + export_component!
  src/source.rs         # the Source trait + selection
  src/source/{calendar,gmail,fixtures}.rs
  src/llm.rs            # the Anthropic Messages call (reused from nutrition)
  src/main.rs           # native standalone entry
  ui/index.html         # single-file, no build (the app contract)
  fixtures/             # checked-in calendar/email + canned LLM response (CI)
  README.md
```

It is an ordinary app (the ADR-0007 build carve-out is shell-only — Morning
Brief gets **no** build step), it compiles native **and** wasm32-wasip2 like
every app, and it slots into `apps.toml` and the registry/marketplace exactly
like notes/nutrition. Add an `[apps.morning-brief]` block (disabled or
`enabled = false` until the key + grants are provisioned) and update the CLAUDE.md
index with a one-line pointer (a newcomer should learn "the AI-enabled-component
pattern lives in `apps/morning-brief` + this design doc").

**Merge strategy: land as a PR (not a direct-to-main merge), and sequence the
egress-dependent parts behind PR #1.** Reasoning:

- **The offline core (MB1–MB5) has no hard dependency on PR #1/#2** — it builds,
  tests (fixture-offline, zero network), and ships on `main` today. It can be its
  own PR and merge early, giving a working configurable brief with the dreaming
  loop immediately.
- **But the *containment guarantee* — the entire point — depends on PR #1
  (`fine-grained-egress-section-1`).** The `[[calls]]` grammar, the JSON-RPC
  method rung (EC4), and `enforcement = "enforce"` are what make the egress
  least-privilege and the theorem operational (MB6). PR #1 is **in flight and not
  merged**, and §9.2 of that doc shows the policy-engine variant lands on a
  pushed-but-unmerged branch — so Morning Brief must be designed against PR #1's
  shape (done here) and **MB6/MB8 must rebase onto it once it merges.** Until
  then the brief runs under today's host-grained allowlist + host-keyed inject
  (the PR #1 compat shim treats that as the maximally-broad call) — *functional
  but not yet least-privilege*, which would undersell the headline. The honest
  move is: **ship the offline core as a PR now; gate the live LLM/source enabling
  on PR #1 landing** so the first time Morning Brief touches real data, the
  call-level containment is actually in force.
- **Do not merge directly to main**: this introduces a new public-facing
  AI-enabled surface touching personal data and the egress hot path's intended
  configuration — it warrants the same review pass PR #1/#2 get. A PR keeps the
  containment argument reviewable and ties the merge to its prerequisites.

Concretely: **PR-A** = MB1–MB5 (offline core + UI), mergeable now; **PR-B** =
MB6 + MB7 + MB8 (egress grants, live enabling, verification tie-in), opened
against `main` *after* PR #1 merges (and PR #2 for MB8), rebased onto its
grammar. The owner provisions `ANTHROPIC_API_KEY` + the Google source token as
the gate to flipping `enabled = true` / sources on.

---

## 14. Codebase references grounding this design

- `crates/tangram-host/wit/tangram.wit` — the closed world (`http-fetch`/`log`/
  `now-ms`, empty WASI ctx): the no-other-egress premise of the containment
  theorem.
- `crates/tangram-host/src/runtime.rs` — `HostState::http_fetch` (host fence +
  host-keyed inject today; PR #1 moves the match to call-grain), empty WASI ctx,
  `secrecy::SecretString` discipline.
- `apps/nutrition/src/strategy/llm.rs` — the **working** Anthropic Messages call
  (`api.anthropic.com/v1/messages`, `claude-opus-4-8`, `json_schema` output,
  `anthropic-version`, OAuth-vs-api-key header logic) the LLM module reuses.
- `apps/nutrition/src/lib.rs` — the async-`Ctx` action shape (`log_meal`:
  resolve outside the lock, commit via `ctx.mutate`), the WASM/native inline-vs-
  spawn split, the `capabilities` probe ANDed with whether the egress secret
  resolves (ADR-0005), the deterministic `Default` seed.
- `apps/nutrition/src/strategy.rs` — the pluggable-strategy seam the `Source`
  trait mirrors.
- `apps/notes/src/lib.rs` — the minimal `#[model]`/`#[actions]` + the
  `Option<T>` + `#[autosurgeon(missing)]` evolution rule; `apps/notes/ui/index.html`
  — the single-file no-build UI + shared design language.
- `crates/tangram-core/src/store.rs` — `Ctx` (`state`/`mutate`, lock never held
  across an await), `GENESIS_ACTOR` + deterministic genesis.
- `docs/design/fine-grained-egress.md` (PR #1) — `[[calls]]` grammar, the EC4
  JSON-RPC method rung, the three enforcement modes, the §8 "exfil within a
  declared call" residual.
- `docs/adr/0005-egress-credential-injection.md` — host-side credential
  injection; `docs/adr/0006-tenant-isolation-posture.md` — tiering;
  `docs/design/manifest-verification-plan.md` (PR #2) — the mechanical
  `granted ⊆ declared ⊆ audited` proof and its call-grain arm (CP6).
- `apps.toml` / `.env.example` — where the grant + the (currently absent) LLM key
  live.
```
