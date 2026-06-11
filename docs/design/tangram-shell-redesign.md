# Tangram Shell Redesign — design & decisions for approval

**Status:** PLANNING ONLY — no code written. This document exists to get the
owner's approval on major design decisions and library choices **before** any
build begins. Nothing here is implemented; the "DECISIONS NEEDING OWNER
APPROVAL" section at the end is the actionable output.

**Scope:** four owner-specified threads — (1) rename/reframe the host's base
view as an app called **tangram**; (2) an Obsidian-style shell (persistent left
sidebar = markdown file tree + app list; main window with multiple tabs;
markdown rendering; apps-as-iframes in tabs); (3) marketplace redesign to
support **uploading a WASM blob** with host-side content-addressed hosting,
open-to-anyone but behind a loudly-warned, default-off gate; (4) registry
cleanup, folding its app-management UI into the shell sidebar.

**Related work:** the parallel composability research (task #26: inter-app
communication + app-embedded-in-a-note). This doc assumes apps-as-iframes is
the composition primitive and defers the message-passing contract and the
app-in-note rendering details to that task. Where the two meet is called out
inline.

---

## 1. Background: the constraints that shape everything

Read alongside [docs/RUNTIME_PLAN.md](../RUNTIME_PLAN.md) ("The app contract"),
[docs/SDK_DESIGN.md](../SDK_DESIGN.md), and
[crates/tangram-host/src/routes.rs](../../crates/tangram-host/src/routes.rs).

The current host (`tangram-host`) already does most of the plumbing:

- The root router (`routes.rs`) serves a centered index listing apps as cards,
  and a dynamic `/<app>/...` dispatcher resolving each request against a **live
  app table** (no rebuild on `apps.toml` change). Per-app it serves the full
  derived surface: `/<app>/` (static UI via `ServeDir`), `/<app>/api/*`,
  `/<app>/sync`, `/<app>/mcp`.
- CSP `frame-ancestors` is already a first-class concern (env `FRAME_ANCESTORS`,
  default `*`) precisely so app UIs can be **iframed** — the owner has confirmed
  apps-as-iframes is the composition model, and this aligns with that work.
- `apps/registry` is the replicated source of truth for installed apps
  (install/enable/remove + `set_*` actions), with a fleet UI at `/registry/`.
- `apps/marketplace` is a catalog whose listings pin a `component_url` +
  `component_sha256`; Install posts those to the registry's `install_app`.

### The HARD app contract (this is the central tension)

Per RUNTIME_PLAN, **a Tangram app's `ui/` is a single self-contained
`index.html`**: zero external/CDN dependencies, **no build step**, **relative
fetch paths only** (so it is prefix-mountable under `/<app>/` and iframable).
Every existing app UI (notes, nutrition, registry, marketplace) is exactly
this: one HTML file with inline `<style>` and inline vanilla-JS `<script>`,
fetching `api/...` relative paths and subscribing to `api/events` over SSE. The
files are 600–900 lines of hand-rolled DOM code and share a dark
"performance-console" design language by copy-paste, not by a shared library.

An **Obsidian-grade shell** — docking/stacked tabs, a markdown renderer, a
collapsible file tree, drag-to-split — is a large amount of stateful UI that is
genuinely painful to hand-roll in single-file vanilla JS. **This is the
decision the owner must make** (Decision A, below): does the `tangram` shell
app get an exception to the no-build/no-deps contract, or do we constrain it to
vanilla + tiny embeddable libraries? Everything in the layout section is
written to work under *either* answer, but the recommendation is explicit.

---

## 2. Goals & non-goals

**Goals**

1. The host's base/default view becomes an app named **tangram** with the blurb
   *"manage the set of apps (tangrams) on your device."* It is simultaneously
   one of the apps in the list *and* the chrome that renders the others.
2. A persistent **left sidebar** (markdown file tree + apps list) that stays
   put while an app or note is open in the main window.
3. A **main window with multiple tabs** (Obsidian-style): each tab holds either
   a rendered markdown file or an embedded app (iframe).
4. Basic **markdown (.md) rendering** in the main window.
5. **Apps open as iframes** in tabs; the sidebar persists around them.
6. Marketplace supports **uploading a WASM blob + metadata**; the host hosts
   those blobs in a **content-addressed store** and computes the hash itself.
   Upload is open to anyone *only* behind a default-off, loudly-warned flag —
   flagged as a **MUST-FIX-before-public** item.
7. The registry's app-management UI is folded into the shell sidebar; the
   registry app's *model/API* is cleaned up but stays the source of truth.

**Non-goals (this round)**

- Inter-app message passing / app-in-note embedding internals — owned by task
  #26; this doc only guarantees iframes are the substrate.
- A general plugin API for the shell. The shell is one app, not a platform.
- Replacing the registry as desired-state authority. We reuse it.
- Markdown *editing* with live preview as a v1 requirement (rendering is the
  requirement; editing is a phased follow-on — see Decision C).
- Multi-tenant shell chrome. The shell is the top-level (trusted-localhost)
  surface for now; tenants keep their existing namespaced index.

---

## 3. Layout design

### 3.1 The model

```
┌──────────────────────────────────────────────────────────────────────────┐
│ tangram                                                       [⚙] [live ●] │  ← top bar (app title, settings, sync status)
├───────────────────────┬────────────────────────────────────────────────────┤
│ SIDEBAR (persists)    │ MAIN WINDOW                                        │
│                       │ ┌────────┬───────────┬──────────┬──┐               │
│ ▾ NOTES (markdown)    │ │ Welcome│ notes ✕   │ readme.md│+ │  ← tab strip  │
│   ▾ projects/         │ ├────────┴───────────┴──────────┴──┤               │
│     • roadmap.md      │ │                                  │               │
│     • ideas.md        │ │   (active tab content)           │               │
│   • readme.md         │ │                                  │               │
│   • scratch.md        │ │   - a rendered .md file, OR      │               │
│                       │ │   - an app  <iframe src=         │               │
│ ▾ APPS                │ │       "/notes/">                 │               │
│   ● notes             │ │                                  │               │
│   ● nutrition    [on] │ │                                  │               │
│   ○ marketplace  [on] │ │                                  │               │
│   ○ registry          │ │                                  │               │
│   + Install app…      │ │                                  │               │
│                       │ │                                  │               │
│ ─────────────         │ │                                  │               │
│ ⊕ New note            │ │                                  │               │
└───────────────────────┴────────────────────────────────────────────────────┘
```

- **Left sidebar (persistent):** two stacked sections.
  - **NOTES** — a collapsible folder tree of `.md` files (from the tangram
    app's own data; see 3.3). Click → opens the file rendered in a tab.
  - **APPS** — the live fleet list (replaces the registry's standalone UI):
    each app with a status dot (healthy/parked/error, from `GET /api/fleet`)
    and an enable/disable toggle; click the name → opens the app in a tab as an
    iframe. An **Install app…** affordance opens the marketplace (itself an app
    tab) or an inline install form.
- **Main window:** an Obsidian-style **tab strip** + the active tab's content.
  A tab is one of:
  - **note tab** — a rendered markdown document (`MarkdownView`);
  - **app tab** — `<iframe src="/<app>/">` (the existing per-app surface,
    unchanged; CSP `frame-ancestors` already permits this).
  - **home tab** — the "tangram" landing/manage view (the reframed base view):
    the blurb, quick links, fleet summary.
- **Top bar:** app title, a settings affordance, and the existing live/sync
  indicator pattern reused from the app UIs.

### 3.2 Why iframes (and what that costs)

Apps stay **completely isolated** (separate origin-path, separate document,
their own JS) — which is exactly the runtime's security posture and why
`frame-ancestors` exists. The shell never imports app code; it only points an
iframe at `/<app>/`. Costs to accept:

- **No shared scroll/keyboard by default.** Shortcuts inside an iframe don't
  reach the shell. Acceptable for v1; task #26's message channel is how a tab
  later learns "the app wants to open a note."
- **Tab state is the shell's job.** The set of open tabs, their order, and the
  active tab is shell state. Whether that persists across reloads — and whether
  it *replicates* (the tangram app is a Tangram app, so it could) — is
  Decision D.
- **Deep-linking into an app** (open app X at sub-route Y) requires the iframe
  `src` to carry the sub-path; fine since app routing is relative.

### 3.3 Where the markdown files live

The **tangram** app is a real Tangram app (`#[model]` + `#[actions]`), so its
markdown files should live in **its replicated Automerge document**, not loose
files on disk. That gives sync, history, and multi-device for free and keeps
the file tree honest with the runtime (no app touches the host filesystem; the
host owns data dirs). Sketch model:

```rust
#[model] struct Tangram { files: Vec<MdFile> }
#[model] struct MdFile { path: String, body: String, updated_at_ms: i64 }
```

- Folder tree is derived from `/`-separated `path`s (same trick notes uses for
  titles). Actions: `create_file`, `write_file`, `move_file`, `delete_file`,
  `list_files`. Markdown *rendering* is client-side; the model stores raw text.
- This means "basic markdown rendering" needs a markdown→HTML step in the
  shell UI (see Decision B for the library), fed by the document state over the
  existing SSE `api/events` stream — identical to how every other app UI works.

*(Alternative considered: read `.md` from a host directory via a new route.
Rejected — it violates "the host owns data, apps don't name files" and gives up
sync/history. Storing markdown in the app document is the contract-honest path.)*

---

## 4. Prior-art & library survey

Sources are cited inline; a consolidated list is at the end.

### 4.1 Tabbed / docking pane layout

| Library | Stars / downloads | Deps | Vanilla-JS? | Verdict |
|---|---|---|---|---|
| **dockview** | ~3.3k★, ~99k/wk, **zero-dependency**, React/Vue/Angular **and vanilla TS** | none | **yes (dockview-core)** | Best fit if we adopt a lib |
| FlexLayout (caplin) | ~1.3k★, ~68k/wk | React | no (React-only) | Out — React |
| golden-layout | ~6.7k★ but "**somewhat abandoned**" | minimal | yes-ish | Out — maintenance risk |
| rc-dock | ~0.8k★ | React | no (React) | Out — React |

dockview is the clear pick *if* a layout library is used at all: zero runtime
deps, an actively maintained vanilla-TS core (`dockview-core`), and it does
exactly the Obsidian feature set (tabs, groups, splits, drag-rearrange,
serialize/restore layout). ([dockview.dev](https://dockview.dev/),
[github.com/mathuo/dockview](https://github.com/mathuo/dockview),
[npmtrends](https://npmtrends.com/dockview-vs-flexlayout-react-vs-golden-layout-vs-rc-dock))

**But** v1 does not need *docking* (drag-to-split, floating windows). It needs a
**tab strip with close/reorder + one content area**. That is a few hundred
lines of vanilla JS and a flexbox — well within the contract. **Recommendation:
ship v1 with a hand-rolled tab strip (no library); reserve dockview for a later
"split panes" milestone**, adopting it then only if Decision A grants the build
exception (dockview-core is multi-file TS — buildless via importmap is awkward,
see 4.4).

### 4.2 Markdown rendering

| Library | Size (min+gz, approx) | Browser drop-in? | Notes |
|---|---|---|---|
| **marked** | ~35 kB | **yes — single UMD/ESM file, CDN or vendored** | Fast, one file, trivial `marked.parse(text)`. Not a security boundary — pair with sanitization. ([macwright "Don't use marked"](https://macwright.com/2024/01/28/dont-use-marked)) |
| markdown-it | ~100 kB+ | yes (single browser bundle) | Most extensible (plugins), CommonMark-strict; heavier. Used by Joplin's renderer. |
| micromark | small core, but **many ESM packages** | awkward | CommonMark-correct but split into many modules → dedup friction buildless. |
| CodeMirror 6 (markdown lang) | n/a (editor, not renderer) | **bundler-effectively-required** | For *editing*, not rendering. See 4.4. |

For **rendering raw markdown text to HTML in a single-file UI**, **marked** is
the best fit: one file, vendor it next to `index.html`, call `marked.parse()`.
Because the markdown comes from the user's own replicated document (not
arbitrary third parties) the XSS surface is low, **but** we still pair it with a
tiny sanitizer (DOMPurify, ~45 kB, also a single vendored file) before
`innerHTML` — defense in depth, and mandatory the moment a note could be
authored by a sync peer. **Recommendation: marked + DOMPurify, both vendored as
single files** (no CDN at runtime — see 4.4 on why vendoring beats CDN here).
([npm-compare markdown libs](https://npm-compare.com/markdown-it,marked,micromark,remark,showdown))

If we later want **live-preview editing** (Obsidian's signature), the field
standard is **CodeMirror 6** (Obsidian, SilverBullet) or **Milkdown**
(ProseMirror + remark). Both are multi-package and effectively want a bundler
(4.4). That is a Decision-A-gated, later-phase capability — see Decision C.

### 4.3 Obsidian-like open-source apps — what they use

From the survey (one line each; URLs in Sources):

- **Obsidian** (closed, but documented): Electron + **CodeMirror 6** (heavily
  customized) for Live Preview; its own markdown renderer for Reading view;
  mature tab/split/sidebar workspace — the canonical layout reference. Build
  step.
- **SilverBullet** (web-first, the closest analog): Preact + **CodeMirror 6**
  live-preview; **esbuild build step** — even the most web-native of these is
  *not* buildless.
- **Trilium**: vanilla TS widget system (no React) over Node; **CKEditor 5**
  primary editor, CodeMirror for code; explicit **left/center/right pane +
  tabbed ribbon** — the strongest *pane-layout* reference after Obsidian.
- **Joplin**: Electron+React / React Native; **markdown-it**-based renderer
  (`joplin-renderer`) → HTML, with a CodeMirror source pane. Confirms
  markdown-it/marked-class renderers are the norm for *rendering*.
- **Logseq**: ClojureScript + React (Rum), DataScript; custom block editor;
  heavy `shadow-cljs` build. Not a buildless model.
- **Foam**: a VS Code extension — *delegates* editor/tabs/splits to the host
  (VS Code/Monaco). The "ride a host, don't build a shell" approach; N/A since
  we *are* the host.
- **AppFlowy**: Flutter + Rust, custom block editor. Not a web stack.
- **Milkdown**: an embeddable ProseMirror+remark WYSIWYG markdown editor
  framework (~40 kB core). Best "drop-in editor component" option, but shares
  CM6's ProseMirror-dep-dedup caveat for buildless use.

**What's worth borrowing:** Trilium's explicit *pane regions + tabbed ribbon*
as the layout template; Joplin's *render-markdown-to-HTML* approach (marked /
markdown-it, not an editor) for the rendering requirement; Obsidian's *tab +
sidebar* interaction model as the UX north star. The clear signal across all of
them: **a real knowledge-UI has a build step.** None ship a single buildless
HTML file — which is the crux of Decision A.

### 4.4 Can we do CodeMirror 6 / dockview buildless from a CDN?

Researched specifically. **CM6 is technically ESM-importable but effectively
wants a bundler:** it is split into many packages (`@codemirror/state`,
`/view`, `/commands`, language packs) that *all* must share **one** copy of
`@codemirror/state`; auto-bundling CDNs (esm.sh, Skypack) routinely pull
multiple copies, breaking CM6's `instanceof`-based facet checks. Workarounds —
an **import map** pinning every `@codemirror/*` to one `state` version, or
esm.sh's experimental `?bundle=` — exist but are fragile and not "drop a script
tag and go." dockview-core and Milkdown have the same multi-module character.
([discuss.codemirror](https://discuss.codemirror.net/t/esm-compatible-codemirror-build-directly-importable-in-browser/5933),
[codemirror/dev#1208](https://github.com/codemirror/dev/issues/1208),
[esm.sh bundling](https://dev.to/louwers/bundling-without-a-bundler-with-esmsh-497d))

By contrast **marked** and **DOMPurify** each ship a *single* browser
file — genuinely buildless when **vendored** (committed next to `index.html`, no
network at runtime, which also preserves "zero external deps" honestly: a CDN
`<script src=https://…>` is an external dependency and an iframe-CSP and
offline-first liability). This is why the v1 recommendation leans on
single-file libraries and a hand-rolled tab strip, and pushes CM6/dockview into
a build-gated later phase.

---

## 5. Marketplace redesign: blob upload + content-addressed hosting

### 5.1 Today vs. proposed

**Today:** a publisher must host the `.wasm` at a URL and supply its
`component_sha256`; the host fetches + verifies + caches it content-addressed
under `$HOME/.tangram-host/components/<sha256>.wasm` (Phase 8). Listings carry
url+sha+capability-manifest+import-audit.

**Proposed (additive — does not remove install-by-URL):** allow a publisher to
**upload the WASM blob directly** along with name/description/metadata. The host
**computes the sha-256 itself**, stores the blob in its content-addressed store,
and serves it back at a stable URL — so the *existing* install-by-URL pipeline
(fetch + verify + cache) is reused verbatim, just pointed at the host's own
artifact route.

### 5.2 The content-addressed blob store + serving route

- **Store:** reuse / generalize the Phase-8 cache dir
  `$HOME/.tangram-host/components/<sha256>.wasm`. Upload writes to a temp file,
  streams-and-hashes, then atomically renames to `<sha256>.wasm` (write-once,
  immutable, dedup by hash). The host computes the hash — the client never
  asserts it.
- **Upload route (host-side, NEW):** `POST /artifacts` (multipart or raw body)
  → on success returns `{ "sha256": "...", "url": "/artifacts/<sha256>.wasm" }`.
  This is a **host capability**, not an app action (it writes host-owned blob
  storage and is a non-operation in the app-contract sense — like the existing
  capability probes / static assets carve-out). It lives in `tangram-host`
  routes, gated as in 5.3.
- **Serve route (NEW):** `GET /artifacts/<sha256>.wasm` streams the blob with a
  long immutable cache header. Because it is content-addressed, the existing
  registry install-by-URL can target `…/artifacts/<sha>.wasm` with that sha and
  the verify-before-instantiate path is unchanged.
- **Marketplace flow:** the marketplace UI gains an **Upload** affordance →
  `POST /artifacts` → fills the listing's `component_url` with the returned
  `/artifacts/<sha>.wasm` and `component_sha256` with the returned hash →
  `add_listing`. From there Install is exactly today's path.
- **Federation note:** a `/artifacts/<sha>` URL is host-local. For a *federated*
  fleet a synced listing must point at a globally reachable URL (GitHub
  release, R2/CDN), per the RUNTIME_PLAN "registry-first + artifact pipeline"
  deferral. Host-local upload is for the single-host / dev-demo case; this is
  noted as a limit, not solved here.

### 5.3 SECURITY — MUST FIX BEFORE OPENING PUBLICLY (loud, unmissable)

> ### ⚠️ OPEN UPLOAD IS A DEV/DEMO-ONLY CAPABILITY, DEFAULT-OFF. ⚠️
>
> **An endpoint that lets anyone store arbitrary binary blobs on your host is
> arbitrary-blob-storage. Left open on the public internet it is an abuse, DoS,
> and malware/illegal-content-hosting magnet** (this is OWASP "Unrestricted File
> Upload" territory:
> [OWASP File Upload Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/File_Upload_Cheat_Sheet.html),
> [OWASP Unrestricted File Upload](https://owasp.org/www-community/vulnerabilities/Unrestricted_File_Upload)).
>
> **It MUST NOT be enabled on a public deployment until ALL of the following
> exist:**
> 1. **AuthN/AuthZ** — upload behind the existing bearer/`Principal` gate
>    (never anonymous on a non-loopback bind), ideally tenant-scoped.
> 2. **Size limits** — hard per-blob byte cap (stream-and-reject, never buffer
>    a whole blob in memory) and a per-account/host **storage quota**.
> 3. **Rate / frequency limits** — cap uploads per principal per window (DoS).
> 4. **Type/shape validation** — accept only valid `wasm32-wasip2` components
>    (magic bytes + `wasm-tools`/wasmtime parse + the closed-world import audit
>    the marketplace already displays: reject anything importing
>    `wasi:sockets` / `wasi:http` / a filesystem).
> 5. **Content controls** — at minimum a hash-blocklist of known-bad artifacts;
>    a sandboxed smoke-run and the LLM behavioral check the marketplace README
>    already lists as the third-party-submission TODO.
> 6. **Abuse/operator controls** — delete/garbage-collect blobs, an audit log
>    of who uploaded what.

**Gating design (v1):**
- A config flag, **default OFF**: `[artifacts] upload_enabled = false` in
  `apps.toml` (and/or env). When off, `POST /artifacts` returns 404 and the
  marketplace Upload affordance is hidden.
- When **on**, the host **refuses to enable open upload on a non-loopback
  `BIND_ADDR` without auth** — mirroring the existing rule that refuses to run a
  registry app unauthenticated off loopback. Startup logs a prominent warning:
  *"open artifact upload is enabled — dev/demo only; see the MUST-FIX list
  before exposing publicly."*
- The marketplace README and the Upload UI both carry the warning banner
  verbatim. This is the same posture the marketplace already takes toward
  third-party submissions (operator-curated until the verification pipeline
  exists) — we are extending it, not loosening it.

Until that checklist is met, the *honest* default is what exists today:
operator-curated listings pinning artifacts the operator published. Open upload
is the dev-affordance that lets a single owner iterate locally without a
release pipeline.

---

## 6. Registry cleanup + fold into the shell sidebar

The registry **stays the desired-state authority** — we do not replace it. We
clean up the UX seam and the model:

- **Fold the UI in:** the registry's standalone `/registry/` fleet UI is
  absorbed into the shell's **APPS** sidebar section (list with status dots +
  enable/disable/remove; Install opens the marketplace tab or an inline form).
  The shell talks to `…/registry/api/actions/*` and `GET /api/fleet` exactly as
  the registry UI does today (relative cross-app calls under one host), so no
  new host API is required for management. The standalone `/registry/` UI can be
  kept as a thin fallback or retired once the sidebar covers it (Decision E).
- **Model cleanup (proposed, low-risk):** the registry model is sound; the
  cleanup is mostly cosmetic/ergonomic and must respect the additive-field
  rule (new fields `Option<T>` + `#[autosurgeon(missing = …)]`):
  - keep `install_app`'s many-arg signature but document the
    component-source/inject invariants in one place (currently split across
    `validate_*`);
  - consider an explicit `display_name`/`description` field for nicer sidebar
    labels (additive);
  - no schema-breaking changes — federated/persisted docs must keep hydrating.
- **What does NOT move:** live status stays a host observation
  (`GET /api/fleet`), never replicated into the registry doc — unchanged and
  correct.

---

## 7. How apps-as-iframes compose into tabs

This is the seam with task #26 (composability). What *this* design fixes:

- A tab's content for an app is literally `<iframe src="/<app>/">` (optionally
  `/<app>/<subpath>` for deep links). The per-app surface is unchanged; CSP
  `frame-ancestors` already allows the shell origin to frame it.
- The shell owns tab lifecycle (open/close/reorder/activate) and the sidebar;
  the iframe owns the app.

What this design **defers to task #26** (called out so it isn't silently
assumed):

- **Inter-app / shell↔app messaging:** a `postMessage` contract (e.g. an app
  asks the shell to "open note X" or "open app Y"; the shell tells an app it's
  been backgrounded). Needs an agreed message schema + origin checks.
- **App-embedded-in-a-note:** rendering an app *inside* a markdown document
  (e.g. a fenced ```tangram-app notes``` block → an inline iframe), not just as
  a full tab. This is squarely task #26's "app-in-note."

This doc commits only to: iframes are the substrate, tabs are shell state, and
the message channel will be additive (no app code change required to be
*framed*; messaging is opt-in).

---

## 8. Phased implementation plan

Each phase is shippable and respects whatever Decision A resolves to.

- **Phase S0 — reframe (tiny, no new deps).** Rename the base view to
  **tangram** with the blurb; the host's `index` becomes a `tangram` home view.
  No layout change yet. Establishes the name everywhere.
- **Phase S1 — sidebar + tabs shell (vanilla).** Persistent left sidebar (APPS
  section first, fed by `GET /api/fleet` + registry state) and a hand-rolled tab
  strip; app tabs open as iframes. Folds in the registry management UI
  (Section 6). No markdown yet. This is the bulk of the UX and stays within the
  contract.
- **Phase S2 — markdown files + rendering.** Add the `tangram` app's `MdFile`
  model + actions; NOTES sidebar tree; note tabs render via vendored
  marked+DOMPurify. Markdown content syncs like any app document.
- **Phase S3 — marketplace upload + artifact store.** Host `POST /artifacts` +
  `GET /artifacts/<sha>` content-addressed store; marketplace Upload affordance;
  the **default-off, warned** gate and the MUST-FIX checklist enforcement
  (auth-off-loopback refusal, size cap, wasm validation). Ships open-upload as
  dev/demo only.
- **Phase S4 (optional, Decision-A-gated) — richer panes/editing.** If the
  build exception is granted: dockview for split panes and/or CodeMirror 6 /
  Milkdown for live-preview markdown editing, via a shell build pipeline. Only
  if v1 demonstrably needs it.
- **Phase S5 (task #26) — composability.** `postMessage` contract + app-in-note
  embedding. Tracked separately.

---

## 9. DECISIONS NEEDING OWNER APPROVAL

Each is a fork the owner should rule on before build. Recommendations are the
author's; the owner decides.

### Decision A ⭐ (CENTRAL) — Build-step exception for the `tangram` shell app?

Does the **tangram** shell app get an exception to the no-build/no-deps app
contract (its own build pipeline + bundled libraries like dockview / CodeMirror
6 / Milkdown), while ordinary apps stay strictly single-file?

- **Option A1 — No exception; shell stays buildless** (vanilla JS + a couple of
  *vendored single-file* libs: marked + DOMPurify). Hand-rolled tab strip and
  file tree. **Pro:** the contract stays universal and honest; the shell is
  dogfooding proof the contract is enough; zero toolchain, offline, trivial to
  audit. **Con:** no docking/split panes and no live-preview markdown editing
  without real pain; the shell is a large hand-rolled UI; we forgo the
  best-in-class libraries every comparable app uses.
- **Option A2 — Bounded exception for the shell only.** The `tangram` app may
  have a build step and bundle libraries; its built output is still a
  self-contained `ui/` of static files served the same way (so the *runtime*
  contract — relative paths, prefix-mountable, iframable, no host FS — is
  unchanged; only the *authoring* "single hand-written file, no build" rule is
  waived, and only for this one privileged app). **Pro:** unlocks dockview +
  CM6/Milkdown; matches what Obsidian/SilverBullet/Trilium/Joplin all do (every
  comparable OSS app has a build step). **Con:** a precedent — needs a written,
  narrow carve-out in RUNTIME_PLAN so it doesn't erode the contract for ordinary
  apps; adds a toolchain to the repo.
- **Recommendation: A2, but deferred to when actually needed.** Build v1
  (Phases S0–S3) under **A1** to prove how far buildless gets and to keep
  momentum; codify a **narrow, shell-only** build exception now (so it's
  pre-approved) and *exercise* it only at Phase S4 when split panes / live
  editing are the actual requirement. Net: contract stays universal for normal
  apps; the shell gets a pre-blessed escape hatch it doesn't spend until it
  must.

### Decision B — Markdown rendering library

- Options: **marked** (single file, fast, vendor it) · markdown-it (heavier,
  most extensible) · micromark (correct but multi-module, buildless-awkward).
- **Recommendation: marked + DOMPurify, both vendored as single files** (no
  runtime CDN). Sanitize before `innerHTML`. Revisit markdown-it only if a
  plugin (math/mermaid) becomes a requirement.

### Decision C — Markdown editing in v1?

- Rendering is required; *editing with live preview* (CM6/Milkdown) is the
  Obsidian signature but is build-gated (Decision A) and multi-module.
- **Recommendation: NO live-preview editor in v1.** Edit via a plain
  `<textarea>` (contract-clean) writing to the model; render with marked. Add
  CM6/Milkdown only in Phase S4 if A2 is exercised.

### Decision D — Does shell/tab state persist & replicate?

The tangram app is a Tangram app, so open-tabs/active-tab/sidebar-collapse
*could* live in its replicated document (sync your workspace across devices) or
stay local (`localStorage`).

- **Recommendation: local (`localStorage`) for v1.** Workspace layout is
  device-specific (Obsidian keeps it per-vault-per-device); replicating it
  invites merge weirdness. Markdown *files* replicate (they're content); tab
  *layout* does not. Revisit if "my workspace follows me" is desired.

### Decision E — Retire the standalone `/registry/` and `/marketplace/` UIs?

Once management is folded into the shell sidebar, the standalone registry UI is
redundant; the marketplace is still useful as its own browsable surface.

- **Recommendation:** keep **marketplace** as a standalone app (a natural
  full-window experience, opened in a tab); **retire the standalone registry
  UI** in favor of the sidebar, or keep it as a minimal fallback. Low stakes;
  decide at S1.

### Decision F — Open-upload gate default & enforcement

Confirm the security posture in Section 5.3.

- **Recommendation (strong):** ship `[artifacts] upload_enabled = false` by
  default; refuse open upload on a non-loopback bind without auth; enforce
  size + wasm-validity at minimum before the flag is allowed on; carry the
  MUST-FIX banner in docs + UI. Treat full public open-upload (quotas,
  rate-limits, scanning, abuse controls) as **out of scope until that checklist
  is built** — same posture as the existing third-party-submission TODO.

### Decision G — Where the `tangram` app's markdown files live

- Options: in the **tangram app's replicated Automerge document** (sync,
  history, contract-honest) vs. a host directory exposed via a new route
  (breaks "apps don't name files").
- **Recommendation: in the app document** (Section 3.3).

---

## Sources

- Layout libs: [dockview.dev](https://dockview.dev/) ·
  [github.com/mathuo/dockview](https://github.com/mathuo/dockview) ·
  [npm trends: dockview vs flexlayout vs golden-layout vs rc-dock](https://npmtrends.com/dockview-vs-flexlayout-react-vs-golden-layout-vs-rc-dock) ·
  [github.com/caplin/FlexLayout](https://github.com/caplin/FlexLayout) ·
  [rc-dock](https://ticlo.github.io/rc-dock/)
- Markdown libs: [npm-compare: markdown-it/marked/micromark/remark/showdown](https://npm-compare.com/markdown-it,marked,micromark,remark,showdown) ·
  [macwright — "Don't use marked"](https://macwright.com/2024/01/28/dont-use-marked) ·
  [npm trends: markdown libs](https://npmtrends.com/markdown-vs-markdown-it-vs-marked-vs-micromark-vs-remarkable)
- CodeMirror 6 buildless: [discuss.codemirror — ESM build importable in browser](https://discuss.codemirror.net/t/esm-compatible-codemirror-build-directly-importable-in-browser/5933) ·
  [codemirror/dev#1208 — load via importmap](https://github.com/codemirror/dev/issues/1208) ·
  [esm.sh bundling without a bundler](https://dev.to/louwers/bundling-without-a-bundler-with-esmsh-497d)
- Obsidian-like OSS stacks: [Obsidian editor extensions docs](https://docs.obsidian.md/Plugins/Editor/Editor+extensions) ·
  [SilverBullet](https://github.com/silverbulletmd/silverbullet) ·
  [Trilium architecture](https://docs.triliumnotes.org/developer-guide/architecture) ·
  [Joplin architecture](https://joplinapp.org/help/dev/spec/architecture/) ·
  [Logseq codebase overview](https://github.com/logseq/logseq/blob/master/CODEBASE_OVERVIEW.md) ·
  [Foam](https://github.com/foambubble/foam) ·
  [AppFlowy tech design](https://appflowy.com/blog/tech-design-flutter-rust) ·
  [Milkdown](https://milkdown.dev/)
- Upload security: [OWASP File Upload Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/File_Upload_Cheat_Sheet.html) ·
  [OWASP Unrestricted File Upload](https://owasp.org/www-community/vulnerabilities/Unrestricted_File_Upload)
