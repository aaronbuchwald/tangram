# Tangram SDK — Design

**Status:** original design / pre-implementation vision — kept as the rationale
record. The shipped architecture has since diverged in several concrete ways;
where this doc and the code disagree, the code wins. Notably: the workspace is
`tangram` + `tangram-core` + `tangram-host` + `tangram-egress` +
`tangram-automation` + `tangram-macros` (NOT the `tangram-mcp` / `-web` /
`-sync` / `-auth` / `-model` split sketched below); sync is raw Automerge over
HTTP(+SSE), not samod and not WebSockets (see `docs/SYNC_PROTOCOL.md`); and the
model macros are `#[model]` / `#[actions]`, not `#[derive(Model)]`. Auth is
designed but not yet built (`docs/design/auth.md`). For the as-built picture see
`AGENTS.md`, `docs/RUNTIME_PLAN.md`, and `docs/TOUR.md`.
**Goal:** make it trivial to vibe-code a Tangram app: define a plain Rust data
model + methods, and get — automatically — a local-first replicated store,
cross-device sync, multiplayer with access control, an MCP server, a minimal
web UI, and a typed client for custom frontends.

## The one-page pitch

```rust
use tangram::prelude::*;

#[derive(Model)]                 // CRDT-backed, persisted, syncable
struct Scratchpad {
    title: Text,                 // collaborative text
    notes: List<Note>,           // replicated list
}

#[derive(Model)]
struct Note {
    text: String,
    done: bool,
}

#[actions]                       // each method becomes: MCP tool + HTTP endpoint
impl Scratchpad {                //   + generated TS client fn + optimistic local mutation
    /// Add a note to the scratchpad.        ← doc comment = MCP tool description
    pub fn add_note(&mut self, text: String) -> NoteId {
        self.notes.push(Note { text, done: false })
    }

    /// Mark a note as done.
    pub fn complete(&mut self, id: NoteId) -> Result<(), NotFound> {
        self.notes.get_mut(id).ok_or(NotFound)?.done = true;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tangram::App::<Scratchpad>::new()
        .sync_remote(env_opt("TANGRAM_REMOTE"))   // optional; fully local without it
        .serve()                                  // /, /mcp, /api, /sync, /healthz
        .await
}
```

That is the entire app. Everything below exists to make this program honest.

## Constraints (from the product brief)

1. **Replicated data structure**: the backend model must be convertible into a
   CRDT-backed replicated structure served by a sync library — your data
   follows you across devices and merges seamlessly.
2. **Local-first**: runs fully offline/local; syncing to a remote is an
   optional enhancement, not a requirement.
3. **Multiplayer**: multiple devices of one user handled gracefully, and
   multiple users with access — which requires access control at some level.
4. **Separation of concerns**: the *model* (data + application logic) is
   separate from the *derived surfaces* — an MCP server, a minimal default
   frontend, and the ability to build custom frontends — all generated
   automagically with syntactic sugar.

## Substrate decision: Automerge

The load-bearing choice. Candidates evaluated (state of the Rust ecosystem,
mid-2026):

| | Automerge | Loro | Yrs (y-crdt) |
|---|---|---|---|
| Typed Rust mapping | **autosurgeon** (`derive(Reconcile, Hydrate)`) — serde-like, exists today | none; would build ourselves | none |
| Repo/sync layer in Rust | **samod** (wire-compatible with JS automerge-repo) | Loro Protocol (rooms over WebSocket) | y-sync, community |
| Browser peer for custom frontends | automerge-repo JS — same wire protocol | loro-crdt npm (wasm) | best-in-class JS ecosystem |
| Access control trajectory | **Keyhive** (Ink & Switch): capabilities + E2EE, built *for* Automerge | app-layer | app-layer |
| Performance / types | good; JSON-like + Text + Counter | **better** perf; movable list/tree, rich text | good |
| History / time travel | full, compact binary | full, checkout/fork | limited GC'd history |

**Pick Automerge** because the three things this SDK needs most all exist
there and nowhere else together:

1. **autosurgeon** is the "syntactic sugar" layer nearly for free — our
   `#[derive(Model)]` lowers to `Reconcile`/`Hydrate` rather than us
   inventing a struct↔CRDT mapping.
2. **Wire-compatible browser peers**: a custom frontend is not an API
   consumer, it's *another replica* — automerge-repo JS in the browser syncs
   the same document our Rust server holds (samod speaks the same protocol).
3. **Keyhive** gives us a researched path from doc-level ACL (v1) to
   cryptographic capabilities + end-to-end encryption (later) without
   changing substrate.

Loro is genuinely better on raw performance and data types (movable trees,
rich text). Hedge: the SDK's `Replica` trait is the only module that touches
automerge APIs directly; models declare intent (`List<T>`, `Text`, `Counter`,
`Map<K,V>`), not automerge calls. If Loro's typed-mapping and auth stories
mature, it can become an alternative backend.

## Architecture

```
            your crate:  #[derive(Model)] + #[actions] impl + main()
┌───────────────────────────────────────────────────────────────────┐
│ tangram (facade)   App::<M>::new().serve() — assembles everything │
├──────────────┬──────────────┬──────────────┬──────────────────────┤
│ tangram-mcp  │ tangram-web  │ tangram-sync │ tangram-auth         │
│ actions →    │ actions →    │ samod wire   │ device keypairs,     │
│ rmcp tools;  │ JSON+SSE API;│ protocol;    │ doc-level ACL,       │
│ model →      │ schema-      │ WebSocket    │ invites; Keyhive     │
│ resources    │ driven       │ /sync; LAN + │ when it lands        │
│              │ auto-UI;     │ remote peers │                      │
│              │ TS codegen   │              │                      │
├──────────────┴──────────────┴──────────────┴──────────────────────┤
│ tangram-model                                                     │
│ Replica trait over automerge+autosurgeon · persistence (fs/sqlite)│
│ history/undo/attribution · typed handles: List, Map, Text, Counter│
└───────────────────────────────────────────────────────────────────┘
```

### tangram-model: the model is the app

- `#[derive(Model)]` lowers to autosurgeon `Reconcile + Hydrate` plus a
  JSON Schema (one schema feeds the MCP tool defs, the auto-UI, and the TS
  codegen — single source of truth).
- A **Document** is the unit of replication, persistence, *and sharing*
  (this scoping decision does a lot of work — see access control). An app
  serves one document type but many document instances ("boards", "pads",
  "projects").
- Storage: append-only automerge binary chunks on disk (sqlite or flat
  files); compaction on save. The whole history is the database — undo,
  time travel, and audit fall out for free.
- **Attribution**: every change is tagged with the actor (device key +
  user + *agent flag*). "Show me everything the AI changed yesterday, and
  revert it" is a first-class query. This is a headline feature for AI
  apps, not an afterthought.

### `#[actions]`: logic as the chokepoint

Rule: **actions are pure functions over the model** — no I/O, no clocks, no
network inside. `&mut self` mutations are recorded as one CRDT transaction
(atomic, attributed, undoable). Because they're pure:

- the same logic compiles to the server binary *and* to WASM in the browser,
  so custom frontends apply actions **optimistically and locally** — that's
  what makes it feel local-first rather than client/server with extra steps;
- tests are trivial (`let mut s = Scratchpad::default(); s.add_note(...)`);
- an action is serializable as `(name, args)` — which is exactly an MCP tool
  call, an HTTP POST body, and a sync-log entry.

Escape hatch: `#[action(server)]` for things that genuinely need server-side
effects (fetch a URL, call another Tangram app); these don't run client-side.

### tangram-sync: every process is a peer

samod gives us automerge-repo's wire protocol, so the topology is symmetric:

- **Local only**: no remote configured → app is fully functional offline.
- **Same user, many devices**: each device runs the app (or a browser
  replica); a remote relay (any Tangram app run with `--relay`, or a shared
  server) stores encrypted-at-rest chunks and forwards changes. Devices can
  also sync directly over LAN (mDNS discovery, later milestone).
- **Multiple users**: same mechanism; the relay is the policy enforcement
  point (below).
- **Presence** (cursors, "who's here", agent-is-typing) is an ephemeral
  side-channel on the same WebSocket — never persisted into the document.

### tangram-auth: access control, honestly scoped

Fine-grained, field-level permissions *inside* a CRDT document are an open
research problem (this is precisely what Keyhive is working on). v1 does not
pretend otherwise:

- **Identity**: each device generates a keypair; a *user* is a small signed
  group of device keys (linking a new device = scanning a QR/invite from an
  existing one). Agents get their own keys, flagged as agents.
- **The document is the unit of access control.** Roles per doc: `owner`,
  `writer`, `reader`. Grants are issued as signed invite tokens
  (capability-style URLs — easy to share, easy to revoke at the relay).
- **Enforcement at sync admission**: the relay (and any honest peer) rejects
  changes from actors without `writer` on that doc, and doesn't serve doc
  chunks to non-`reader`s. Within a doc, writers are mutually trusted —
  matching the actual trust model of "I shared this board with you."
- **Trajectory**: when Keyhive stabilizes, grants become cryptographic
  capabilities and chunks become E2EE — the relay degrades to a dumb,
  untrusted store-and-forward node. The invite/role *API* is designed to
  survive that swap unchanged.

### tangram-mcp: the agent is a collaborator

- Every `#[action]` → an MCP tool (rmcp, streamable HTTP at `/mcp`), schema
  derived from the same JSON Schema as everything else; doc comments become
  descriptions.
- The model itself is exposed as MCP **resources** (current state as JSON,
  schema, recent-changes feed) so agents can read without bespoke "list_*"
  tools.
- Agent writes go through the same action + ACL + attribution pipeline as
  human writes. Combined with history, this gives review/undo of AI changes
  — the safety story for letting agents loose on user data.

### tangram-web: two tiers of frontend

1. **Auto-UI (the "minimal basic frontend")**: schema-driven rendering of
   the document (lists → lists, Text → editor, bools → checkboxes) plus a
   form per action, live over SSE. Iframe-embeddable with the CSP/
   frame-ancestors handling already in the template. Zero frontend code; it
   is the admin panel / dev view / good-enough tile.
2. **Custom frontends**: `tangram gen ts` emits TS types (from the same
   schema) + a thin client. Two modes:
   - **replica mode** (preferred): automerge-repo in the browser syncs the
     doc over `/sync`; actions run locally via the WASM build; works offline.
   - **thin mode**: plain `fetch` against `/api/{doc}/actions/{name}` + SSE
     state stream, for dumb embeds where shipping WASM is overkill.

## What "vibe-codeable" means as acceptance criteria

The SDK is done when an LLM (or a human in a hurry) can reliably produce a
working multiplayer local-first app with **only** these concepts: *Model,
action, Document, share*. Concretely:

- Scratchpad rewritten on the SDK ≤ 40 lines, no mention of CRDTs, axum,
  rmcp, or WebSockets.
- `cargo new && cargo add tangram && cargo run` yields a working synced app
  with UI + MCP on first try.
- Ship `llms.txt` + a single-page "API by example" doc; error messages name
  the fix ("`Vec<Note>` is not replicable — use `List<Note>`").
- No required config; every default safe-local (`127.0.0.1`, no remote).

## Milestones

1. **M1 — model + surfaces, single device**: tangram-model over
   automerge/autosurgeon, `#[actions]` macro, MCP + HTTP derivation.
   Deliverable: scratchpad on the SDK, feature-parity with this template.
2. **M2 — sync**: samod integration, `/sync` endpoint, `--relay` mode,
   multi-device same-user; browser replica mode proven against JS
   automerge-repo.
3. **M3 — multiplayer**: identity, doc roles, signed invites, relay-side
   enforcement; agent attribution + "review AI changes" view in auto-UI.
4. **M4 — polish**: auto-UI v2, TS codegen, presence channel, LAN sync.

## Known risks / open questions

- **samod maturity**: 0.6.x and self-described experimental. Mitigation: the
  wire protocol is small and documented; worst case we maintain a fork — the
  `Replica` trait isolates the blast radius.
- **autosurgeon hydration vs. concurrent edits**: hydrate-into-structs is a
  snapshot view; merged states that violate app invariants (e.g. two users
  "claim" the same slot) need app-level resolution hooks. Provide
  `#[model(invariant = ...)]` repair callbacks rather than pretending merges
  are always semantically clean.
- **Schema evolution**: documents outlive code. v1 policy: additive-only
  changes are free (autosurgeon tolerates missing/unknown fields); breaking
  changes require an explicit, versioned `migrate(old) -> new` document
  rewrite. Revisit lenses/Cambria-style approaches later.
- **Blobs**: CRDT docs are for structured state, not media. Content-addressed
  blob store alongside the doc (sync by hash) in M2+.

## Sources

- automerge: <https://automerge.org/> · <https://github.com/automerge/automerge>
- autosurgeon (typed Rust ↔ automerge): <https://github.com/automerge/autosurgeon>
- samod (Rust automerge-repo, wire-compatible with JS): <https://lib.rs/crates/samod>
- automerge-repo (JS) + sync server: <https://github.com/automerge/automerge-repo>
- Keyhive — local-first access control (Ink & Switch): <https://www.inkandswitch.com/keyhive/notebook/>
- Loro (evaluated alternative): <https://loro.dev> · <https://github.com/loro-dev/loro>
- p2panda on decentralized access control: <https://p2panda.org/2025/07/28/access-control.html>
