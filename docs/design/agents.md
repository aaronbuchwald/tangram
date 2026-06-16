# Design: Agents & Skills — config/markdown-orchestrated agents over the existing pillars

**Status:** PROPOSED — approved direction. This is the **canonical Agents &
Skills design (design-of-record)**. The execution model below is **LOCKED**:
**v1 is config/markdown-orchestrated** — an agent is a saved config + markdown
instructions, and the **host** runs the LLM↔tool loop in the declared sandbox.
Component-backed "larger system" agents are a later escape hatch (§12), **not
v1**.

**The thesis.** Agents and skills are the **application layer that composes
Tangram's existing pillars** — they introduce essentially no new infrastructure.
The LLM brain is the `/llm/<provider>` agentgateway proxy (ADR-0012,
host-injected keys per ADR-0005); tools are the agentgateway MCP plane; identity
and limits are the auth scopes + per-principal rate-limit (`docs/design/auth.md`,
ADR-0011); sandbox tiers are Wasmtime → gVisor/Seatbelt (ADR-0001, ADR-0010) with
the `granted ⊆ declared ⊆ audited` verification (`tangram-host/src/verify.rs`);
storage/distribution is vault markdown in the Automerge document, shared via the
registry/marketplace (RUNTIME_PLAN Phases 3/8/9); and the risk-gating precedent
is `apps/auto-todo`'s approval lifecycle.

**Related:** ADR-0012 (LLM proxy via agentgateway), ADR-0005 (egress credential
injection — keys host-side only), ADR-0008 (call-level egress — the scoping
model tool grants reuse), ADR-0001 (WASM-first sandbox; gVisor retained),
ADR-0010 (host-side browser/automation capability),
[`docs/design/auth.md`](auth.md) (per-principal scopes + rate-limit, the C0–C7
cadence this roadmap mirrors), `apps/auto-todo`
([`docs/design/auto-todo.md`](auto-todo.md), the gated agent-lifecycle
precedent), the `.agents/skills/*/SKILL.md` format (the Skill convention), and
RUNTIME_PLAN Phases 3/8/9 (registry source-of-truth, marketplace, federation).

Code anchors that already exist and that this design composes:
`crates/tangram-host/src/gateway.rs` (the supervised agentgateway child + the
`/llm/*` and `/mcp` routes), `crates/tangram-host/src/verify.rs` (manifest
verification), `crates/tangram-host/src/egress.rs` (the call-grant grammar),
`apps/tangram/ui/src/agentTag.ts` + `agentPopup.ts` + `editor.ts` (the shipped P0
inline `@agent` demo, on `feat/vault-agent-demo`).

This is a research + design deliverable. No production code accompanies it; each
roadmap checkpoint (§11, P0–P7) is its own independently shippable,
held-for-review PR.

---

## 1. The model in one paragraph

An **agent** is the base executable unit: a vault markdown file whose YAML
frontmatter declares *instructions + model + tools + sandbox + identity +
labels + version* — a **pure capability** that carries **no trigger** (R1: the
trigger lives on the invocation, §6). Its body is the natural-language
instruction. To
*run* an agent, the host drives an LLM↔tool loop — it sends the instructions
(plus context) to the agent's `model` through the `/llm/<provider>` proxy, lets
the model call the agent's declared MCP `tools` through the agentgateway MCP
plane, and runs the whole loop in the declared `sandbox` tier under the invoking
**principal**'s identity, scopes, and rate-limit. Output is written back into the
vault (a block in the source file, or a new note), which — because the vault is
an Automerge document — gives free version history, replication, and
distribution.

---

## 2. Agent vs Skill

- **Agent** — the base class. Full frontmatter (instructions + model + tools +
  trigger + sandbox + identity + labels + version). May be one-time, event-driven,
  or scheduled. Invoked by `@name`.
- **Skill ⊂ Agent** — a *subclass*: an agent whose definition is a **single
  markdown task doc**, invoked by `/skill-name` (matching the existing
  `.agents/skills/<name>/SKILL.md` convention — same frontmatter shape: `name`,
  `description`, `argument-hint`, `allowed-tools`, then the body). A skill is the
  ergonomic, one-shot, autocompleted special case of an agent: `trigger:
  one-time`, narrow tools, body == the task. Everything that runs an agent runs a
  skill; the only difference is the handle (`/` vs `@`) and the expectation of a
  single self-contained task doc.

---

## 3. Frontmatter schema

An agent file is `frontmatter + markdown body`. Schema (all keys optional except
`kind` and `name`; sensible defaults shown):

| Key | Type | Default | Meaning |
|---|---|---|---|
| `kind` | `agent` \| `skill` | — | base class or the single-task-doc subclass |
| `name` | path-safe string | — | the handle: `@name` (agent) / `/name` (skill) |
| `model` | provider/model string | host default | routed via `/llm/<provider>` (ADR-0012) |
| `tools` | list of scoped tool ids | `[]` | MCP tools the loop may call (§7) |
| `sandbox` | `none` \| `wasm` \| `gvisor` \| `seatbelt` | `none` | execution tier (§9) |
| `labels` | list of strings | `[]` | free tags for the Agents view + query bar |
| `meta` | map of key→value | `{}` | arbitrary kv for sort/filter (e.g. `cost_tier`) |
| `version` | semver string | `0.0.0` | published-version pin (§10) |

Worked example (a skill):

```markdown
---
kind: skill
name: summarize           # /summarize  (or @summarize inline)
model: deepseek-chat      # via the /llm proxy
tools: [vault.read, web.fetch]
sandbox: none
labels: [writing, fast]
meta: { owner: aaron, cost_tier: low }
version: 0.2.0
---
Summarize the selected text into 3 terse bullets.
```

The definition carries **no trigger** — it is a pure capability. The trigger
(one-time vs cron vs event) lives on the **invocation**, a ```` ```agent ````
block that links here via `use: summarize` (§6).

`model: deepseek-chat` resolves to `POST /llm/deepseek/v1/chat/completions`
(ADR-0012's path-based selection; the route is already wired and tested). The
key never appears here — it lives host-side as `env://DEEPSEEK_API_KEY` on the
gateway route (ADR-0005). `meta` is open: any key the operator invents
(`cost_tier`, `owner`, `team`) becomes a sortable/filterable column in the Agents
view (§8).

---

## 4. Substrate-reuse table

The load-bearing claim of this design: **every capability an agent needs already
exists as a Tangram pillar.** Agents/skills compose them; they are not new infra.

| Capability the agent needs | Existing Tangram mechanism | Anchor |
|---|---|---|
| LLM "brain" (the model call) | the `/llm/<provider>` agentgateway proxy, host-injected key, OpenAI-shaped body | ADR-0012; `gateway.rs` |
| Keys never in the agent file/client | host-side injection at the egress/proxy boundary | ADR-0005 |
| Tools (call external systems) | the agentgateway **MCP plane** (`/mcp`, `/<app>/mcp`) | RUNTIME_PLAN D3; `gateway.rs` |
| Scoping a tool/egress grant to a call | the call-level egress grant `(method, host, path, shape)` | ADR-0008; `egress.rs` |
| Identity, scopes, "who may run this" | `Principal` + scope set + per-principal rate-limit | `docs/design/auth.md`; ADR-0011 |
| Sandbox tiers (none → wasm → OS sandbox) | Wasmtime default; gVisor (Linux) / Seatbelt (macOS) | ADR-0001, ADR-0010 |
| Capability verification at converge | `granted ⊆ declared ⊆ audited` | `verify.rs`; manifest-verification-plan |
| Storage + version history + replication | vault markdown in the Automerge document | RUNTIME_PLAN; notes/vault pattern |
| Distribution / install across a fleet | registry source-of-truth + marketplace (sha256-pinned) | RUNTIME_PLAN Phases 3/8/9 |
| Gating risk-bearing execution | the auto-todo approval lifecycle (plan-hash-bound, per-step `confirm()`) | `apps/auto-todo`; `docs/design/auto-todo.md` |
| Supervised long-running children | the agentgateway/browser supervision pattern (Backoff/shutdown) | `gateway.rs`; `tangram-automation/runner.rs` |

---

## 5. Invocation & handles

Two inline handles, both resolving against the **agent index** (§8):

- **`@name`** — inline agent invocation in the **file's context**: the agent runs
  with the surrounding note as input and writes a completion block back into the
  file. A bare **`@`** (no name yet) opens a **picker** over every indexed agent.
- **`/skill-name`** — inline command, **autocompleted from the index**; runs the
  skill's task doc against the selection/file and inserts the result. `@skill`
  also works (a skill is an agent), but `/` is the ergonomic command form.

**Shipped P0 demo** (`feat/vault-agent-demo`): the inline `@agent` tag in the
vault editor (`apps/tangram/ui/src/agentTag.ts` + `agentPopup.ts` + `editor.ts`)
already does the single-turn version — type `@agent`, a popup collects a prompt,
the shell issues a DeepSeek chat completion through `/llm/deepseek`, and the
result is saved as a block in the note. P1+ generalize this from an ad-hoc prompt
to a *named, saved* agent definition (§11).

---

## 6. Triggers — the trigger belongs to the INVOCATION, not the definition (R1)

**A definition does not carry a trigger.** The definition (the `agents/…` note,
§3) is a **pure capability** — kind, name, model, instructions, labels. What
decides *whether and how* an agent runs is a separate, durable **invocation**: a
fenced ```` ```agent ```` block placed inside *any* markdown note. The
invocation is the **source of truth** for the trigger + the prompt, it links to
a definition via `use:`, and it is **indexed** (derived from the file text) so
editing or removing the block self-cleans — there are no stray references and no
extra bookkeeping.

```text
```agent
use: standup                # the definition this invocation runs
trigger: cron every 1h      # cron every <N>m|h · cron @hourly · cron @daily · one-time
prompt: Summarize today's notes into three bullets.
```                         # the prompt may span multiple lines until the fence
```

- **Definition** = capability (no trigger). Editing it is trigger-agnostic.
- **Invocation** = a ```` ```agent ```` block. Owns `trigger` + `prompt`, links
  to the def by `use:`, identified by a stable **`invocationId`** =
  `hash(hostFileId + use + trigger + prompt)` (FNV-1a, identical in
  `apps/tangram/ui/src/invocations.ts` and `apps/tangram/src/agents.rs`). An
  unedited block keeps its id; editing it = a new id; removing it drops it.
- **Last-run bookkeeping is keyed by `invocationId`** (the `agent_runs` `Vec` in
  the vault model), so two distinct invocations of the same def run on their own
  schedules, and an edited block re-fires (its id changed) rather than inheriting
  a stale last-run.

The run popup opened by `/<name>` is the **options pass** where a user picks the
trigger (and, later, more): **prompt** + **Trigger** {**One-time** (default,
runs now) · **Cron** (reveals a schedule input) · **Event** (disabled — future,
issue #33)} + greyed placeholders for **MCP / Tools**, **Multi-step**, and
**Tags / Labels** (each with a hover tooltip). The `/agent` define popup stays
trigger-agnostic.

`trigger` forms:

- **`one-time`** (default) — runs on explicit invocation (`/<name>` → run now);
  it does **not** write a durable ```` ```agent ```` block.
- **`cron <schedule>`** — runs on a schedule. Picking Cron in the popup writes
  the ```` ```agent ```` block; the host scheduler (below) picks it up. v1
  schedule grammar: `every <N>m`, `every <N>h`, `@hourly`, `@daily` (a full
  5-field cron is intentionally not supported).
- **`event`** — runs on a **vault CRDT change** (note created, label added, or
  another agent's output landing). **Disabled in the popup; future work,
  issue #33.**

Cron is driven by a **host-supervised scheduler** (`tangram-host/src/scheduler.rs`),
built on the **same supervision pattern** the agentgateway and browser children
use (`gateway.rs` Backoff/shutdown; `tangram-automation/runner.rs`) — a 60s
interval loop that dispatches the `tangram` shell's `tick_agents` action. The
component scans every note body for ```` ```agent ```` blocks, resolves each
`use:` to a definition, computes due per invocation, and appends each run's
output right after its block — all in one `Ctx::mutate`. Output is written back
into the vault, so a triggered run is itself a CRDT change that may (once event
triggers ship) fire downstream agents (bounded; see §12).

---

## 7. Tools / MCP connections

An agent declares scoped MCP tools in `tools:`. At run time the host routes those
calls **through agentgateway's MCP plane** (the `/mcp` aggregate and per-app
`/<app>/mcp` routes the host already proxies), and **audits** each call. A tool
grant is **scoped exactly like the call-level egress model** (ADR-0008): the
grant is the declared *call* — `(tool, method, host, path, shape)` — not a blanket
"this agent may use the internet", and the credential is bound to the matched
call. This means an agent's tool surface is verifiable and auditable with the
**same machinery** the host already runs for app egress
(`egress.rs` + `verify.rs`), and tool calls land in the same audit trail as
mutating actions (`docs/design/auth.md` §6). Untrusted/third-party agents
therefore inherit the `granted ⊆ declared ⊆ audited` guarantee for free.

---

## 8. Agents view

A new Tangram app — **`apps/agents`**, a sibling to `registry`/`marketplace` —
that **indexes every vault file carrying agent frontmatter** into a
GitHub-issues-style **sortable, filterable table**:

| Column | Source |
|---|---|
| name / kind | frontmatter |
| model | frontmatter |
| trigger | the **invocation** (```` ```agent ```` block: `one-time` / `cron:…` / `event:…`), not the definition (R1, §6) |
| last-run / status | run log (success/error/never) |
| version | frontmatter semver |
| labels | frontmatter `labels` |
| *(any `meta` key)* | frontmatter `meta` → arbitrary sortable/filterable column |

A **query bar** drives it with a compact syntax, e.g.
`label:writing cost_tier=low kind:skill` — `label:` matches `labels`,
`kind:`/`model:`/`trigger:` match top-level fields, and `key=value` matches an
arbitrary `meta` entry. The view also supports **create-a-label** and
sort/filter on any `meta` key (so `cost_tier`, `owner`, `team` become
first-class facets without a schema change).

**The same index powers `/skill-name` and `@name` autocomplete** — one index, two
consumers (the table and the editor handles). The index is derived from the
Automerge vault (it watches the same change stream §6 uses), so it is always
consistent with the files.

---

## 9. Sandbox tiers

The `sandbox` field selects the execution tier, declared in a **capability
manifest** and **verified at converge** (the `granted ⊆ declared ⊆ audited` chain,
`verify.rs`):

| Tier | What runs | Use case |
|---|---|---|
| **`none`** | pure LLM loop, no tools beyond `/llm` | summarize/rewrite/classify (the §3 example) |
| **`wasm`** | deterministic steps + scoped MCP tools in a Wasmtime component | structured tool use, reproducible runs (ADR-0001 default tier) |
| **`gvisor`** / **`seatbelt`** | fs/process/browser — the "larger system" | agents that touch the filesystem, spawn processes, or drive a browser (ADR-0010); gVisor on Linux, Seatbelt on macOS (ADR-0001's two-host reality) |

Tier escalation is monotonic in risk and is **declared, not implicit**: a `none`
agent cannot silently gain fs access, and a `gvisor`/`seatbelt` agent's
capability manifest is checked against what the host will grant before it runs.
This is the same posture app components already get; agents reuse it unchanged.

---

## 10. Versioning & sharing

- **Free history** — because an agent is a vault file in an Automerge document,
  every edit is already versioned; the Agents view can show **history/diff** with
  no extra storage.
- **Semver `version`** — the frontmatter pin; the human-meaningful version on top
  of the CRDT history.
- **Immutable published versions** — publishing an agent **content-addresses** it
  exactly like a marketplace artifact (sha256-pinned bytes; RUNTIME_PLAN Phase 8,
  `apps/marketplace`). A published `version` is immutable; editing produces a new
  version.
- **Install across a federated fleet** — an agent is **installed via the
  registry** (the source of truth; RUNTIME_PLAN Phase 3), so installing/removing
  one on any host **propagates to the whole federated fleet** (Phase 9). Sharing
  an agent reuses the exact distribution path apps already use.

---

## 11. Roadmap — P0…P7

Each checkpoint is **independently shippable + reviewable**, mirroring the
auth C0–C7 cadence. Each has a one-line **review gate** (what the owner checks
before merge).

| # | Checkpoint | Review gate |
|---|---|---|
| **P0** | **Inline `@agent`** — single-turn DeepSeek → save block (**DONE**, deployed on `feat/vault-agent-demo`) | typing `@agent` in a note yields a saved completion block via `/llm/deepseek` |
| **P1** | **Named agents as vault files** — the §3 frontmatter schema + an indexer; `@name` / `/skill-name` resolve to a **saved definition** and use *its* config (model/tools/instructions) | a saved `summarize.md` runs from `/summarize` using its own frontmatter, not an ad-hoc prompt |
| **P2** | **Agents view** — the sortable/filterable table + labels + arbitrary `meta` key-value filter (§8) | `label:writing cost_tier=low kind:skill` filters the table; a new label/`meta` key sorts |
| **P3** | **Agent config popup (full) + save-as-agent** — edit every frontmatter field in the UI; promote an inline prompt to a saved agent | the popup round-trips all §3 fields; "save as agent" writes a valid vault file |
| **P4** | **Tools / MCP connections** — declare scoped tools; calls routed + audited through agentgateway, scoped per ADR-0008 (§7) | an agent calls a declared MCP tool; an undeclared call is refused; the call is audited |
| **P5** | **Triggers: event + cron** — the host-supervised scheduler/watcher; output written back to the vault (§6) | a folder-create event and a cron expr each fire the right agent; restart-clean supervision |
| **P6** | **Sandbox tiers** — `wasm` → `gvisor`/`seatbelt` + capability manifest + verification (§9) | a `gvisor` agent's manifest is verified `granted ⊆ declared ⊆ audited` at converge; over-grant refused |
| **P7** | **Versioning + publish/share** — semver + content-addressed immutable versions + install via the registry across the fleet (§10) | publishing pins bytes by sha256; installing on one host propagates to a federated peer |

P0–P3 are the inline-agent core (no new sandboxing, all loopback); P4–P7 add
tools, automation, isolation, and distribution — each gated, each reusing an
existing pillar.

---

## 12. Open decisions

Owner to ratify; recommended defaults given.

- **`@` vs `/` resolution + picker UX.** *Recommend:* `@name` = inline agent in
  file context, bare `@` = picker; `/skill-name` = autocompleted command. Both
  resolve against the §8 index. Open: exact precedence when a name is both.
- **Trigger/event engine — build vs reuse.** *Recommend:* build a thin host-side
  scheduler/watcher on the existing supervision pattern (§6) rather than pull in a
  cron/queue dependency — it must observe the Automerge change stream the host
  already has, which an off-the-shelf scheduler does not.
- **Agents-view query syntax.** *Recommend:* the compact `key:value` / `key=value`
  grammar in §8; open whether to support boolean operators / saved queries.
- **How component-backed agents (the escape hatch) plug in.** *Recommend:* an
  agent may set a field pointing at a WASM component that *is* the loop (the
  "larger system" agent); it then runs as a normal app component under the same
  sandbox/verify path. Deliberately **post-v1**; the seam is the `sandbox: wasm`
  tier (§9).
- **Per-agent cost / rate budgets.** *Recommend:* reuse the per-principal
  rate-limit (ADR-0011) and add an optional per-agent `meta.cost_tier`-driven cap;
  open whether spend metering lands here or in the LLM-proxy follow-on (ADR-0012
  "usage metering").
- **Whether agents can spawn agents.** *Recommend:* allow via the event trigger
  (an agent's output is a CRDT change that fires another), but **bound depth/rate**
  to prevent runaway loops; open the exact budget + cycle detection.

---

## 13. Security posture

- **Keys host-side only.** The model call goes through `/llm/<provider>`; the
  provider key is injected at the boundary and never reaches the agent file, the
  client, or a replicated document (ADR-0005, ADR-0012).
- **Tool/egress grants scoped + audited.** An agent's tools are call-scoped
  (ADR-0008), routed through agentgateway, and audited like every mutating action
  (`auth.md` §6) — args **digested, not plaintext**, to avoid logging injected
  secrets.
- **Sandbox tier verified.** The declared tier's capability manifest is checked
  `granted ⊆ declared ⊆ audited` at converge (`verify.rs`); an agent cannot run
  with more capability than its manifest declares, nor more than the host grants.
- **Per-principal / per-agent rate-limit before any non-loopback exposure.** The
  LLM proxy and tool plane are loopback-only by default (ADR-0012 §4); exposing
  agents beyond loopback is **hard-gated** on the per-principal scope + rate-limit
  (ADR-0011 / `auth.md`), exactly as the LLM proxy is.
- **Risk-bearing execution is gated like auto-todo.** Any agent action that
  mutates state, spends, or reaches a "larger system" follows the
  `apps/auto-todo` precedent: a plan/approval gate bound to the plan and a
  per-step `confirm()` checkpoint, so the load-bearing safety is in the machine
  invariants, not the model's judgement.
- **Credentials never replicate.** Agent files replicate; the credentials the
  host injects do not (the same rule the registry/tenant layers enforce).

---

## 14. Placement & merge

- **A new `apps/agents` app** hosts the Agents view (§8) — sibling to
  `apps/registry` / `apps/marketplace`, indexing vault files into the
  sortable/filterable table and exposing the index that powers autocomplete.
- **Shell-editor extensions** for `@` / `/` land in `apps/tangram/ui/src/`,
  generalizing the shipped P0 `agentTag.ts` / `agentPopup.ts` / `editor.ts` from
  an ad-hoc prompt to a saved-definition resolver.
- **A host-side scheduler/event-watcher** (P5) lands in `crates/tangram-host/src/`
  as a supervised child reusing the `gateway.rs` Backoff/shutdown pattern.
- **Reuse, don't rebuild:** the LLM call reuses the `/llm/*` proxy (`gateway.rs`,
  ADR-0012); tools reuse the MCP plane + `egress.rs` scoping (ADR-0008); identity
  reuses the `Principal`/scope/rate-limit seam (`auth.rs`, `auth.md`); sandbox
  tiers + manifest verification reuse `verify.rs` (ADR-0001/0010); distribution
  reuses the registry/marketplace path.
- **v1 keeps `tangram-core` wasm-clean.** The orchestration loop, scheduler, and
  agentgateway integration are **host-side** (`tangram-host`, native-only tokio);
  `tangram-core` must keep compiling for `wasm32-wasip2` (CI-checked), so none of
  the agent runtime lands there — only the portable app/store contract, as today.

---

*This doc is the single source of truth for Tangram Agents & Skills. The
direction and the execution model (config/markdown-orchestrated v1, Skill ⊂
Agent, vault-markdown storage, existing-pillar reuse) are approved;
implementation proceeds as the independently-reviewable checkpoints P0–P7 (P0
landed on `feat/vault-agent-demo`).*
