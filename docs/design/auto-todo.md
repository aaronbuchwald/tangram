# Design: Auto-completing TODO list (`apps/auto-todo`)

**Status:** design / proposal (no production code). Read-only research + design
+ effort estimate + testable checkpoints + open decisions.
**Date:** 2026-06-12
**Author:** Aaron (owner), with research + design by Claude

**Concept (owner):** a TODO list where **each item dispatches an agent** that
(1) figures out the permissions / connections / human assistance (incl. 2FA) it
will likely need, (2) discovers whether tools (MCP servers, APIs) exist or
whether it needs browser automation (with possible human help), (3) **requests
the permissions, states its plan, and asks for human approval** before acting,
and (4) provisions **scoped credentials** — either by **minting a 1Password
service account on the fly** or by **delegating access from credentials the
parent already has**, doled out per well-defined, well-scoped task.

**Related (read these):**
- `CLAUDE.md` / `AGENTS.md` — the app contract conventions; `docs/RUNTIME_PLAN.md`
  — the binding **app contract** (single HTTP listener, env-only config, one
  data dir, declared egress; "no feature may violate the contract").
- **`docs/design/task-automation-browser.md` — the SIBLING substrate plan**
  (browser-automation runner + browser egress gate + credential broker +
  record/replay/LLM-fallback). **NOT PRESENT at authoring time** (2026-06-12),
  so this plan designs *against its described substrate* and marks every
  dependency on it as **[DEP: substrate]**. When that file lands, reconcile the
  named seams (§4) against its actual surface before any build.
- `docs/design/fine-grained-egress.md` (PR #1) — the call-level egress
  capability model (`[[apps.<app>.calls]]`, host-side credential injection,
  `observe`→`warn`→`enforce`) that this app's permission grants **build on**.
  Marked **[DEP: PR#1]**.
- `docs/adr/0005-egress-credential-injection.md` — components never hold
  plaintext secrets; the host attaches them at the `http-fetch` egress boundary.
- `docs/adr/0006-tenant-isolation-posture.md` — co-resident WASM isolation
  tiers; an auto-dispatching credential-using agent is squarely a higher-trust
  concern.
- `apps/notes/src/lib.rs` — the closest existing shape (a list model + single
  deterministic `Default` + `Option`+`autosurgeon(missing)`); `apps/notes/ui/`
  — single-file, no-build UI (the app contract).
- `apps/registry/src/lib.rs` — the spec-list model whose `Inject` / `allow_hosts`
  / call grants this app proposes installing per item.

---

## 0. Scope line (read this first)

This plan owns the **layer ABOVE the substrate**: the per-item planning/dispatch
loop, the permission-discovery → tool-vs-browser classification, the
scoped-credential provisioning strategy, and the plan+approval protocol surfaced
in the tangram UI. It does **NOT** redesign the browser-automation runner, the
browser egress gate, the credential broker mechanism, or record/replay/LLM
fallback — those are the sibling substrate's (`task-automation-browser.md`),
**reused**, not reinvented. Where this plan needs a substrate facility it names
the seam and marks it **[DEP: substrate]**.

It also explicitly does **NOT** build a general agent-orchestration framework.
The "agent per item" is a constrained, gated lifecycle (§3), not an open-ended
autonomous loop. The strongest design stance here is **human gates before any
credential use or irreversible action** (§8) — the auto-dispatch is the risky
part, and the gates are the load-bearing safety.

---

## 1. Problem & goals

A free-text todo ("renew my domain", "RSVP yes to the Tuesday invite and add it
to my calendar", "download last month's invoices from the billing portal") is
the unit. The owner wants each item to be *driven to completion by an agent*,
but safely: the agent must **reason about what it needs before it acts**, prefer
**existing tools** over brittle browser automation, **scope its credentials to
exactly the task**, and **stop at a human approval gate** before it does
anything with reach or irreversibility.

**Goals**
1. **Per-item agent lifecycle** as an explicit, replicated **state machine**
   (§3): discover → classify → provision → plan+approve → execute → report.
2. **Permission discovery** (§5): infer the capabilities / connections /
   credentials / human-assistance (2FA) a free-text item implies, and discover
   which are satisfiable by an existing tool/MCP vs. need browser automation vs.
   need a human.
3. **Scoped-credential provisioning** (§6): the two owner-named modes
   (1Password-SA-on-the-fly vs. scoped delegation from the parent), least
   privilege throughout, **credentials brokered host-side, never in the
   item-agent's LLM context** (reuse the substrate broker; align with ADR-0005).
4. **Plan + approval protocol** (§7): a structured "here's what I need, here's
   my plan, here's the human assistance required and when, approve?" surfaced in
   the tangram UI, granted/denied per item.
5. **Phased, testable checkpoints** (§9): start with **read-only / tool-based**
   items needing no browser and no creds; gate browser+credential items later,
   after the substrate lands.

**Non-goals**: redesigning the substrate (§0); a general autonomous-agent
runtime; unattended execution of irreversible/credential actions (always gated);
opening this to untrusted third parties (this is a first-party, owner-operated
app — ADR-0006 tier 1, but with a credential-handling risk profile that argues
for the strongest gates and a PR-not-merge posture, §10).

---

## 2. How it fits the app contract (and where the agent actually runs)

A Tangram app is a `#[model]` + `#[actions]` component: deterministic `Default`,
state in one Automerge document, **registered actions only** (sync `&mut self`
state transitions, or `async fn(Ctx<Self>)` for I/O resolved outside the lock),
relative-path single-file UI, declared egress. The auto-todo app holds **the
todo items and their per-item agent state** in that document and exposes the
lifecycle transitions as actions.

**Critical architectural decision — where the agent's reasoning runs.** A
Tangram WASM component has a *closed world* (`http-fetch`, `log`, `now-ms`; no
fs/sockets/inbound-HTTP — RUNTIME_PLAN Phase 2). It is the wrong place to run a
multi-step LLM agent loop that drives MCP servers and a browser. So this app
splits cleanly along the existing contract seam:

- **The component (in-sandbox) owns:** the items, the inferred-requirements
  records, the plan, the approval state, and the result — i.e. the *replicated
  state and the gated transitions*. Discovery (§5) that is pure reasoning over
  the item text can be done by an LLM **via the component's `http-fetch` to the
  Anthropic API**, exactly as `apps/nutrition`'s `llm` strategy already does
  (keyless/offline fallback when no key) — the discovery output is data written
  to the document, not an action taken in the world.
- **The substrate (host-side) owns:** the actual *execution* — driving MCP tool
  calls and the browser runner, holding credentials, record/replay. The
  component never holds a credential and never drives the browser directly; it
  hands the **approved plan** to the substrate and receives a **structured
  result** back. **[DEP: substrate]** defines this dispatch seam; §4 proposes it.

This keeps "no feature may violate the contract": the component stays a pure
state machine over a document; all reach (network, browser, credentials) is
host-mediated and capability-gated, identical in shape to how nutrition's API
egress and ADR-0005 injection already work.

---

## 3. The per-item agent lifecycle (state machine)

Each item carries a `phase`. Transitions are **registered actions**; the
risk-bearing ones (`provision`, `execute`) are **bearer-gated and
human-approval-gated**. The machine is deliberately linear with explicit
**blocked/needs-human** and **rejected** off-ramps — no hidden autonomy.

```
            (user adds free-text item)
                      │
                      ▼
              ┌──────────────┐
              │   DRAFTED     │  item text only; nothing inferred yet
              └──────┬───────┘
                     │ discover()            ← LLM reasoning over item text (component http-fetch)
                     ▼
              ┌──────────────┐
              │  DISCOVERED   │  inferred: capabilities, connections, credentials,
              └──────┬───────┘   human-assistance (incl. 2FA points)
                     │ classify()            ← match needs to available tools (§5.2)
                     ▼
              ┌──────────────┐
              │  CLASSIFIED   │  per-need disposition: TOOL(mcp/api) | BROWSER | NEEDS_HUMAN
              └──────┬───────┘
                     │ plan()                ← assemble execution plan + credential strategy (§6)
                     ▼
              ┌──────────────┐
              │ PLAN_PROPOSED │  structured plan + requested grants + human-assist schedule
              └──────┬───────┘
            approve()│           reject() ─────────────► REJECTED (terminal; reason recorded)
       (HUMAN GATE, bearer)      request_changes() ────► back to DISCOVERED/CLASSIFIED
                     ▼
              ┌──────────────┐
              │   APPROVED    │  human granted the plan + grants; not yet provisioned
              └──────┬───────┘
                     │ provision()           ← scoped creds minted/delegated host-side (§6)
       (HUMAN GATE if creds   ▼                  [DEP: substrate broker]
        are credential-using) ┌──────────────┐
                              │ PROVISIONED   │  scoped credential handle exists host-side
                              └──────┬───────┘   (NEVER in the component / LLM context)
                                     │ execute()  ← hand approved plan to substrate runner
                  (HUMAN GATE before │              [DEP: substrate]
                   first irreversible│
                   /credential step) ▼
                              ┌──────────────┐    ┌───────────────┐
                              │  EXECUTING    │───►│ BLOCKED_HUMAN │ (2FA / CAPTCHA / ambiguity)
                              └──────┬───────┘    └──────┬────────┘
                                     │ report()          │ resume() (human provided assist)
                                     ▼                   └──────────► EXECUTING
                              ┌──────────────┐
                              │   DONE        │  result + audit trail; creds revoked
                              └──────────────┘  (revoke() runs on DONE and on REJECTED-after-PROVISIONED)
```

**Invariants** (each enforced as an action precondition / host gate):
- No transition into `EXECUTING` without a recorded `approve()` by an authed
  principal on the *current* plan hash (re-approval required if the plan
  changed). This is the same human-in-the-loop discipline as the rest of the
  project (registry/marketplace bearer gates).
- `provision()` and the first credential-using or irreversible step inside
  `execute()` are **separately gated** — approving a *plan* is not approving the
  *moment a credential is used*. The substrate must call back to a `confirm()`
  gate before the first such step (§7, the "checkpoint" approvals).
- `BLOCKED_HUMAN` is a first-class state, not an error: 2FA, CAPTCHA, an
  unexpected page, or a low-confidence decision parks the item awaiting a human,
  with the substrate's record/replay holding the session. **[DEP: substrate]**
- Credentials are **revoked on every exit** from a provisioned state (`DONE`,
  `REJECTED`, timeout). Revocation is host-side (§6).

---

## 4. Composition with the substrate & the egress model

This app sits on top of, and is gated by, two pieces of in-flight work. Named
seams (subject to reconciliation when the substrate file lands):

### 4.1 The substrate dispatch seam **[DEP: substrate — reconciled]**
The substrate plan (`task-automation-browser.md`) landed during this plan's
authoring; its §4 settles the seam this section had marked open. The chosen
shape — which this app **builds on directly** — is:

- A new native-only crate **`crates/tangram-automation`**, owned and supervised
  by `tangram-host` exactly like `gateway.rs`'s agentgateway child (`[automation]`
  in apps.toml; missing driver → disabled, never a hard dependency). The browser,
  the LLM-planner, the 1Password token, and the recorded scripts all live
  host-side. **The WIT world stays closed and unchanged** — automation is *not* a
  component capability.
- The component **requests** an automation through the **existing action/data
  plane, not MCP tools and not a new WIT import** (substrate §4.3): an action
  produces an **`AutomationRequest`** — a JSON record naming a **pre-approved task
  template by id** (not free-form browser commands), its parameters, and the
  **credential *reference*** it needs (`op://…`, a reference not a value). The
  host's runner observes pending requests, intersects them with operator
  `[automation]` policy, executes host-side, and writes the result back into the
  document the app reads — the same shape as any async action result.
- The substrate's **credential broker** (§6, primitive C) resolves the `op://`
  reference host-side into a `SecretString` and `fill`s/attaches it at the point
  of use, never returning plaintext to the component or the LLM (ADR-0005 shape,
  generalized to the browser).
- **`BLOCKED_HUMAN`** maps onto the substrate's **stop-gate** steps (§7/§8): a
  template marks irreversible/2FA steps as hard barriers; replay halts and
  requires explicit human approval to proceed.

The load-bearing property the substrate guarantees, which this app relies on:
**an `AutomationRequest` is an upper bound intersected with operator policy,
never authority on its own** — the component cannot name browser commands
directly (only an approved template id), cannot widen its domain allowlist
(intersected with the operator browser ceiling), cannot choose an arbitrary
credential (intersected with what the operator granted), and cannot observe the
credential value. This is the same "request, not grant" posture as
`describe()`-carried egress declarations (fine-grained-egress §6).

**What auto-todo adds on top:** the substrate runs *one* automation request
against a template; auto-todo is the layer that (i) infers *which* template +
credential reference a free-text item needs (§5), (ii) assembles the multi-step
plan and the human-approval gate around it (§7), and (iii) tracks the per-item
lifecycle (§3). The substrate's task **templates** are pre-approved by the
operator; a key reconciliation item (§12, decision 1) is **who authors a
template for a novel item** — auto-todo's discovery can *propose* a template, but
the operator approving it is the gate.

### 4.2 The egress capability model **[DEP: PR#1]**
Every outbound reach this app uses is a **declared call** under
`fine-grained-egress.md`'s `[[apps.auto-todo.calls]]` grammar
(method + host + path-template + optional shape), with credentials injected
host-side per-call (ADR-0005). Concretely:
- **Discovery LLM call:** `POST api.anthropic.com /v1/messages`, credential
  injected host-side (the parent's `ANTHROPIC_API_KEY`, never in the component).
- **Tool/MCP execution:** routed through the substrate session, not direct from
  the component — so the *per-tool* scoping is the substrate broker's job, but
  the component's own grant to *reach the substrate* is one declared call.
- **Per-item provisioned credentials** are **never** added to the component's
  `allow_hosts`/`inject`; they live in the substrate broker. The component's
  static grant is minimal and fixed (Anthropic API + the substrate endpoint).

The crucial property: **the set of hosts/calls the component can reach is static
and small**; the *dynamic, per-item* reach happens inside the substrate session
where the broker enforces the scoped credential against the substrate's egress
gate. This is what keeps an auto-dispatching agent from being a
self-widening-egress footgun — the component cannot grant itself a new call
(fine-grained-egress §6: declarations are a request, not a grant).

---

## 5. Permission discovery & the tool-vs-browser classification

### 5.1 Inferring requirements from free text (`discover()`)
Given the item text, an LLM call (component → Anthropic API, §4.2) produces a
**structured requirements record** (written to the document, reviewable by the
human — it is *data*, not an action):

```
InferredRequirements {
  summary:            String,             // restated goal
  capabilities:       Vec<Capability>,    // e.g. "calendar.write", "email.send", "web.purchase"
  connections:        Vec<Connection>,    // named services: Google Calendar, the domain registrar, ...
  credentials:        Vec<CredentialNeed>,// what auth each connection needs (api key / oauth / login)
  human_assistance:   Vec<HumanAssist>,   // 2FA, CAPTCHA, a judgment call, payment confirmation
  irreversibility:    Reversibility,      // none | reversible | irreversible(spends money / deletes / sends)
  confidence:         f32,
}
```

The prompt instructs the model to **over-disclose** needs (false positives are
cheap — the human reviews; a missed 2FA point is expensive) and to **flag
irreversibility explicitly** (it drives the gate strictness in §8). Low
confidence → the item lands in `DISCOVERED` with a "needs clarification" note
rather than auto-advancing.

### 5.2 Classifying each need: TOOL | BROWSER | NEEDS_HUMAN (`classify()`)
For each `connection`/`capability`, decide the cheapest safe path. The decision
order (prefer deterministic tools over brittle browser; prefer no-creds over
creds):

1. **TOOL (existing MCP / API) — preferred.** Is there a connected MCP server or
   a known API that covers this capability? The session already exposes a rich
   connector set — **Google Calendar/Gmail/Drive, Slack, Notion**, and many more
   via the claude.ai connectors, **plus the `playwright` browser MCP**. The
   classifier matches the inferred capability against an **available-tools
   catalog**:
   - The component can enumerate the tools reachable on the host's **MCP plane**
     (agentgateway aggregates every app's `/mcp` as namespaced targets —
     RUNTIME_PLAN Phase 2/D3) and any connector tools the substrate session
     exposes. Matching is by capability description (an LLM/embedding match of
     "needs: calendar.write" against tool descriptions). **[DEP: substrate]** for
     how the connector catalog is surfaced to an app.
   - A tool match with an **already-connected** credential → lowest risk; no
     provisioning needed. A tool match needing a *new* connection → record a
     `CredentialNeed`.
2. **BROWSER — fallback when no tool exists.** The capability is only reachable
   through a website with no API/MCP (the domain registrar's billing page, a
   portal download). Route to the substrate's **browser runner** with a declared
   navigation plan. Browser automation is *expensive and brittle* and carries
   the highest credential exposure, so it is the explicit second choice, and its
   credential use is the most strongly gated (§8).
3. **NEEDS_HUMAN — when neither suffices or a human is intrinsically required.**
   2FA approval, a CAPTCHA, a payment confirmation, a genuine judgment call, or
   a task with no automatable path. These become scheduled **human-assist
   checkpoints** in the plan (§7), not failures.

Output: a per-need `Disposition` annotating the `InferredRequirements`, so the
plan (§7) is a concrete sequence of tool calls / browser steps / human
checkpoints. The classification is **shown to the human** in the approval card —
"I'll use the Google Calendar MCP for the RSVP (already connected), and I'll need
you to approve the 2FA when the registrar texts you."

---

## 6. Scoped-credential provisioning (`provision()`)

The owner named two modes. Recommendation: **default to scoped delegation
(mode b); reserve 1Password-SA-on-the-fly (mode a) for the cases delegation
can't serve, and only as the API actually supports.** Both are brokered
host-side; the component / LLM context **never** sees plaintext (ADR-0005).

### 6.1 Mode (b) — scoped delegation from the parent's existing credentials [DEFAULT]
The parent (the host operator) already holds credentials: connected MCP/OAuth
connections, API keys in the host `.env`, and `OP_SERVICE_ACCOUNT_TOKEN` (the
parent 1Password service account). **Delegation** means: for one approved item,
the substrate broker is handed a **narrowly-scoped handle** to exactly the
credential the plan needs, for exactly the declared calls, with a TTL, revoked
on item completion. Mechanically this reuses the existing machinery:
- For an **API-key capability**: the per-item grant is a **call-level egress
  declaration** (`fine-grained-egress.md` `[[calls]]`) bound to the credential
  via host-side injection (ADR-0005) — i.e. the credential is usable *only* for
  the declared `(method, host, path)` of this item's plan, never the whole host.
  This is exactly the same-host-exfil mitigation PR#1 was built for, now applied
  per *task*. **[DEP: PR#1]**
- For an **OAuth/MCP connection**: delegation is "this item's substrate session
  may call *these specific tools* on *this* connection," enforced by the broker
  — a tool-level allowlist scoped to the session, expiring with it.
- The credential never leaves the broker; the component receives only an opaque
  **credential handle** id to put in the item's record for audit.

**When to use:** the default and overwhelmingly common case. The capability is
covered by a credential the parent already has, and the scope can be expressed
as "these calls / these tools, this TTL." Lowest setup cost, no new accounts,
revocation is immediate (drop the handle).

### 6.2 Mode (a) — mint a 1Password service account on the fly [SPECIAL CASE]
Intended shape: for an item, programmatically create a **fresh 1Password service
account** scoped to just the vault(s)/secret(s) the task needs, hand its token to
the broker, and delete/expire it when the item finishes — true per-task least
privilege with a clean cryptographic boundary.

**Feasibility note (researched 2026-06-12 — important caveat).** 1Password
service accounts *are* the right primitive conceptually: they are
non-user-tied, token-based, and **scoped to specific vaults + restricted to
specific permissions** — explicitly marketed for "secure, scoped, runtime access
to the secrets [AI agents] need." **BUT programmatic *creation* of a service
account (and of vaults) on the fly is NOT supported by the 1Password SDK today**
— it is an open, unanswered feature request (`onepassword-sdk-go` issue #236),
and the community reports a service account **cannot mint Connect tokens**
either (403). So "mint a brand-new SA per task via API" is **not currently
feasible**; what *is* feasible:
- The parent `OP_SERVICE_ACCOUNT_TOKEN` can be used by the broker to **read**
  exactly the scoped secret an item needs from a pre-provisioned, per-purpose
  vault (created once, manually, by the operator) — i.e. the *scoping* is done
  by **vault/permission layout established ahead of time**, and the per-item
  "provisioning" is a scoped *read* through the broker, not a fresh SA.
- True per-task SA *creation* should be treated as **future work gated on
  1Password shipping the API** — design the broker so that when SA-creation
  lands, mode (a) becomes a drop-in (the broker already abstracts
  "obtain-scoped-credential-handle"). **Verify against current 1Password docs at
  build time**; this is a moving target.

**Recommendation:** ship mode (b) (scoped delegation) as the credential strategy;
implement mode (a) as **"scoped read from a pre-laid-out 1Password vault via the
parent SA token"** (feasible today) behind the *same broker abstraction* as (b),
and leave **"mint a fresh SA per task"** as a clearly-marked seam to fill when
the 1Password API supports it. Both modes resolve to the substrate broker's
**`op://` (and `env://`) credential-reference** interface (substrate §6, the
`op://` `SecretResolver` it implements over ADR-0004) — the app's item record
carries only a credential *reference* (`op://<vault>/<item>/<field>`), and the
broker resolves it host-side at the point of use into a `SecretString`,
never returning plaintext. The app does not care which mechanism backs it; the
1Password-side vault/permission layout (and the parent SA token's scope) is what
makes the reference least-privilege today (§6.2). **[DEP: substrate broker]**

### 6.3 Least-privilege invariants (all modes)
- The credential is scoped to the **declared calls/tools of the approved plan**,
  nothing wider (PR#1 call-level grain).
- It has a **TTL** and is **revoked on item exit** (DONE/REJECTED/timeout).
- It lives **only in the broker**, never in the component, the LLM context, the
  replicated document, or any sync relay (ADR-0005). The document stores only an
  opaque handle id and the *names* of what was granted (for the audit trail).
- The grant is **per item**, not per app — two items never share a credential
  handle.

---

## 7. Plan + approval protocol & UI

### 7.1 The plan artifact
`plan()` assembles, from the classified requirements, a **structured plan**
written to the item:

```
Plan {
  steps:            Vec<PlanStep>,     // ordered: ToolCall{tool, args_summary} | Browser{nav_summary} | HumanCheckpoint{kind, when}
  requested_grants: Vec<Grant>,        // the calls/tools + credential mode per step (mode a/b, scope, TTL)
  human_assist:     Vec<AssistPoint>,  // 2FA / CAPTCHA / payment / judgment — with WHEN in the sequence
  reversibility:    Reversibility,     // worst-case across steps; drives gate strictness
  plan_hash:        String,            // approval binds to THIS hash; any change re-opens approval
}
```

### 7.2 The approval protocol (the human gate)
The card surfaced in the UI says, in plain language: **"Here's what I want to
do, here's what I need access to, here's where I'll need you (and when),
approve?"** The human can:
- **Approve** — records `approve(item, plan_hash, principal)` (bearer-gated;
  same auth as registry/marketplace mutations). Binds to the exact plan hash.
- **Request changes** — sends it back with a note (re-discovers/re-plans).
- **Reject** — terminal, reason recorded.
- **Approve with narrower grants** — the human can *strike* a requested grant or
  tighten a scope before approving; the plan re-hashes and only the narrowed set
  is provisioned.

**Two-tier gating** (the load-bearing safety, §8): approving the *plan* is not
approving each *credential use*. For steps marked irreversible or
credential-using, the substrate pauses at a **per-step `confirm()` checkpoint**
("about to submit payment of $X on registrar.example — confirm?") that the human
must clear in real time. Read-only / tool-based steps with already-connected
creds need no per-step confirm (only the plan approval) — this is what makes the
**Phase-1 read-only tier frictionless** (§9).

### 7.3 UI
Single-file, no-build UI (the app contract), same shape as `apps/notes/ui` (one
`index.html`, relative fetches, prefix-mountable under the shell). The list view
shows each item with its `phase` and a colored disposition. Clicking an item in
`PLAN_PROPOSED` opens the **approval card**: the restated goal, the step list
with TOOL/BROWSER/HUMAN badges, the requested grants (with mode a/b, scope,
TTL), and the human-assist schedule, plus Approve / Request-changes / Reject /
narrow-grants controls. `BLOCKED_HUMAN` items surface a prominent "needs you
now" banner (2FA code entry / "I've approved it, resume"). The result and a
**full audit trail** (what tool was called, what credential handle, when revoked)
show on `DONE`. Live updates via the existing `/api/events` SSE stream.

---

## 8. Security analysis

Auto-dispatching agents *with credentials* is the central risk; the gates are
the design's reason for existing. The honest framing: **the deterministic
boundaries (egress allowlist, call-level grants, host-side broker) are what hold
when the model's judgment misses** (Anthropic's framing, cited in
`fine-grained-egress.md` §2). The model layer (discovery/classification) is
*advisory*; the *gates and grants* are *authoritative*.

**What the design relies on (and why it holds):**
- **Component never holds a credential** (ADR-0005) — the LLM driving the loop
  cannot exfiltrate what it never sees; a prompt-injected item text cannot make
  the component leak a key it doesn't have. The broker is the only holder.
- **Call-level grants per item** (PR#1) — even the credential the *broker*
  attaches is bound to the declared `(method, host, path)` of the approved plan,
  so a compromised step can't replay the credential to a sibling endpoint on the
  same host (the exact same-host-exfil class PR#1 closes), and an undeclared call
  is denied *and* un-credentialed.
- **Human gate before every irreversible / credential-using step** — not just at
  plan time but at the *moment* of the action (§7.2). This is the strongest
  control: even a fully-hijacked plan cannot spend money / send mail / delete
  without a real-time human confirm. Read-only tool calls are exempt (keeps the
  safe tier frictionless).
- **Per-item, TTL'd, revoked-on-exit credentials** (§6.3) — blast radius of any
  single item is one task's scope for one task's duration.
- **Approval binds to a plan hash** — a re-planned or model-mutated plan must be
  re-approved; the human can't be tricked into approving plan A and executing
  plan B.

**Residual risks (honest):**
- **Exfil within a declared, approved call.** If the human approves
  `POST .../send-email`, a hijacked step can put exfiltrated data *in that
  email's body* — call-level scoping shrinks the surface, it doesn't read intent
  (same residual as `fine-grained-egress.md` §8). Mitigation is the per-step
  confirm + the human actually reading the body summary; out-of-band otherwise.
- **Prompt injection in the item text or in tool/page content** steering
  discovery/planning. Mitigations: discovery output is *data the human reviews*,
  not an action; grants are narrow; the per-step confirm is the backstop. Treat
  fetched page/tool content as untrusted in the planner prompt.
- **Confused-deputy via the parent's credentials.** Delegation (mode b) hands
  *the parent's* authority to an item; the scope+TTL+per-step-confirm bound it,
  but the operator must treat "approve" as "I authorize this with my access."
  The UI must make the *scope and the identity being used* explicit.
- **Microarchitectural side-channels** (ADR-0006) — orthogonal; this is a
  first-party (tier-1) app. If it ever ran untrusted plans, ADR-0006's untrusted
  tier (process-per-tenant etc.) and ADR-0005 would be prerequisites.
- **Browser automation is the weakest leg** — highest credential exposure,
  brittle, screen-scrapes untrusted content. Hence it's the *last* classification
  choice and the *most* gated, and it's gated behind the substrate landing (§9).

---

## 9. Phased, testable checkpoints

Each checkpoint is its own commit with its test; full gate green before commit
(`cargo build --workspace`, `clippy -D warnings`, `fmt --check`, the app's tests,
`cargo build -p tangram-core --target wasm32-wasip2`, and the wasm component
build the host integration tests need). The ordering is **risk-ascending**: the
entire credential/browser/auto-dispatch surface is gated behind later phases, so
early phases are landable and demonstrably safe.

- **AC1 — Model + lifecycle skeleton, no agency.** The `#[model]` (§3 states as
  data: items, `InferredRequirements`, `Plan`, approval state, result — all
  `Option`+`autosurgeon(missing)`, `Vec` not map, deterministic `Default`) and
  the transition actions as **pure state machine** with the invariants (§3) — but
  `execute()` is a no-op that just records "would execute." Bearer-gated mutating
  actions. UI: list + manual phase advance. **No LLM, no creds, no browser.**
  *Test:* state-machine unit tests (illegal transitions rejected; approval binds
  to plan hash; revoke-on-exit bookkeeping); a host lifecycle test like
  `registry_lifecycle`. **This is fully landable and safe on its own.**

- **AC2 — Discovery + classification (read-only, tool-based, NO creds, NO
  browser).** `discover()` via the Anthropic API (component `http-fetch`, one
  declared call, keyless/offline fallback like nutrition); `classify()` against a
  **read-only tool catalog** — only tools that need no new credential and take no
  irreversible action (e.g. `Google_Calendar.list_events`, a read-only Notion
  query). `execute()` runs *only* TOOL steps that are read-only on
  already-connected connections; anything needing creds/browser/irreversibility
  lands in `NEEDS_HUMAN`/`BLOCKED_HUMAN`. *Test:* discovery on canned items
  produces sane requirements; classification routes a "what's on my calendar
  Tuesday" item to a read-only tool and a "renew my domain" item to
  NEEDS_HUMAN/BROWSER (deferred). **This is the owner's "start read-only" tier —
  genuinely useful, no credential risk.**

- **AC3 — The approval protocol + UI (still no creds).** The full §7 card:
  structured plan display, Approve/Reject/Request-changes/narrow-grants, plan-hash
  binding, the per-step `confirm()` checkpoint *mechanism* (exercised by a fake
  irreversible step). *Test:* approval gates enforced; re-plan re-opens approval;
  narrowing strikes a grant.

- **AC4 — Scoped delegation (mode b) for API-key tools, gated by PR#1.**
  **[DEP: PR#1]** Per-item call-level grant + host-side injection for a single
  credentialed-but-reversible tool capability; the credential handle lifecycle
  (provision/TTL/revoke) through the broker. **[DEP: substrate broker]** *Test:*
  the item's credentialed call is authorized only for the declared call and is
  revoked on DONE; a sibling call on the same host is denied + un-credentialed.
  **Gated on PR#1 merging.**

- **AC5 — Browser items, gated by the substrate.** **[DEP: substrate]** Route a
  no-API capability to the substrate browser runner; `BLOCKED_HUMAN` round-trip
  for 2FA/CAPTCHA; record/replay reuse. The per-step confirm before the first
  irreversible browser action. *Test:* a scripted browser task (against a local
  fixture site) completes through a 2FA checkpoint with a human-resume; the
  credential is broker-held, never in the component. **Gated on the substrate
  plan landing.**

- **AC6 — 1Password mode (a), to the extent the API allows.** Scoped read from a
  pre-laid-out vault via the parent `OP_SERVICE_ACCOUNT_TOKEN` through the broker
  (feasible today); the "mint fresh SA per task" path stubbed behind the broker
  abstraction with a clear "blocked on 1Password API" note (re-verify docs).
  *Test:* a scoped secret is read for an item and the handle revoked; the
  fresh-SA path returns a clear not-yet-supported result.

---

## 10. Placement + merge-strategy recommendation

**Placement: a new first-party app `apps/auto-todo`** (crate
`tangram-app-auto-todo`, on-host name `auto-todo`). It is a list-shaped `#[model]`
+ `#[actions]` app with a single-file UI — the same shape as `apps/notes`, which
is the closest existing template. It builds native + `wasm32-wasip2` like every
app, mounts under the shell at `/auto-todo/`, and gets its egress grants
(`[[apps.auto-todo.calls]]`) and substrate access via apps.toml /
registry-install like any other app. It deliberately does **not** live inside
`tangram-host` — it's an *app on the platform*, and all its reach is
host-mediated and capability-gated, satisfying the app contract.

**Merge strategy: PR, held for review — do NOT merge-immediately.** Argument:
this is the project's first app that **auto-dispatches agents that use
credentials and take actions in the world**. Even with the gates, the
credential-handling and auto-dispatch surface warrants the **same careful human
pass** the project already reserves for egress-on-the-hot-path and
manifest-verification work (`fine-grained-egress.md` §10 and
`manifest-verification-plan.md` are both explicitly *held for review, not
merged*). The risk profile is strictly higher than a CRUD app. Concretely:
- **AC1–AC3** (pure state machine + discovery/classification read-only +
  approval UI, **no creds, no browser**) are low-risk and *could* be a first PR
  that merges on its own — they deliver the "read-only auto-todo" tier with no
  credential exposure.
- **AC4–AC6** (credentials + browser + 1Password) **must be a reviewed PR, held**,
  and are **dependency-gated**: AC4 on **PR#1** (call-level egress) merging; AC5
  on the **substrate plan** (`task-automation-browser.md`) landing; AC6 on both
  plus the 1Password API reality.

So: **split into a safe-tier PR (AC1–AC3) that can merge, and a
credential/browser-tier PR (AC4–AC6) that is built to merge-ready quality, pushed
to the remote, and held for the owner's review** — never auto-merged, given the
auto-dispatch-with-credentials risk. Record the credential/auto-dispatch posture
as an **ADR** when AC4+ is built (paralleling ADR-0005/0006).

**Hard dependencies to state in the PR:** `task-automation-browser.md` (the
substrate — **not present at this plan's authoring; reconcile the §4 seams when
it lands**), `fine-grained-egress.md` / PR #1 (call-level grants), and ADR-0005
(host-side credential injection). Until those land, only AC1–AC3 are buildable.

---

## 11. Effort estimate

~7–9 agent-sessions, risk-ascending:
- **AC1** ~1 session (model + state machine; mechanical, well-templated by notes).
- **AC2** ~1.5 sessions (discovery prompt + classification catalog; the LLM-match
  quality is the soft part).
- **AC3** ~1.5 sessions (approval UI + plan-hash + per-step confirm mechanism).
- **AC4** ~1.5 sessions, **gated on PR#1** (broker + per-item call-level grant).
- **AC5** ~2 sessions, **gated on substrate** (runner integration + BLOCKED_HUMAN
  + record/replay reuse — the brittle/expensive part).
- **AC6** ~1 session (1Password scoped-read; the fresh-SA path is a stub until
  the API exists).

AC2 and AC5 are the highest-uncertainty (LLM classification quality;
substrate-seam reconciliation + browser brittleness). AC1–AC3 are independently
shippable and deliver real value with zero credential risk.

---

## 12. Open decisions for the owner

1. **Template authorship for novel items (the reconciled-seam question).** The
   substrate (now landed, §4.1) runs **operator-pre-approved task templates by
   id**, not free-form plans — which is the right safety posture. So for a
   genuinely *novel* free-text item with no matching template, what happens?
   Options: (a) auto-todo's discovery **proposes a new template** that the
   operator must approve before the item can use the browser path (recommended —
   keeps the "request, not grant" invariant; the approval *is* the gate); (b)
   novel browser items are simply `NEEDS_HUMAN` until an operator authors a
   template. This is the single most important thing to settle *with* the
   substrate owner, since it defines how much of "auto-completing" actually
   auto-completes vs. requires template curation.
2. **Default credential mode** — confirm mode (b) scoped-delegation as the
   default (recommended), with mode (a) as the pre-laid-out-vault scoped read
   until 1Password ships SA-creation. Or does the owner want to pursue/charter
   the 1Password feature request as a dependency?
3. **Per-step confirm strictness** — is the bar "confirm before every
   credential-using step" (safest, more clicks) or "confirm before every
   *irreversible* step, credential-using-but-reversible steps ride the plan
   approval" (less friction)? Recommendation: irreversible always; credentialed
   configurable per item, defaulting to confirm.
4. **Discovery model & cost** — Anthropic API per item (recommended; matches
   nutrition's `llm` strategy and the project's Claude bias) vs. a cheaper/local
   model. Also: cap discovery calls (one per `discover()`, re-run only on
   request-changes) to bound cost.
5. **Read-only tier autonomy** — may read-only, already-connected tool steps run
   **without** a plan approval (truly auto-completing for safe items), or does
   *every* item require at least one approval? Recommendation: read-only +
   already-connected + reversible may auto-execute with a post-hoc notification;
   everything else gates. This is the line between "auto-completing" and
   "approval-gated" and is the owner's call.
6. **Where multi-tenancy lands** — single-owner first (tier 1). If this ever
   becomes multi-tenant or marketplace-distributed, ADR-0006's untrusted-tier
   controls and the manifest-verification gate become prerequisites — note it,
   don't build it now.
