# App composability & embedding — research

**Status:** research / pre-design (no production code changed)
**Date:** 2026-06-11
**Author:** Claude (research), for Aaron
**Scope:** how Tangram apps should (1) embed one app's UI inside another app's
view (the "app-in-a-note" iframe) and (2) talk to each other, across both
first-party and untrusted-publisher trust boundaries.
**Grounding:** README; `docs/SDK_DESIGN.md`; `docs/RUNTIME_PLAN.md` ("The app
contract"); `docs/SYNC_PROTOCOL.md`; `crates/tangram-host/src/routes.rs`
(per-app surfaces, dispatch, tenant namespace); `crates/tangram/src/web.rs`
(state/actions/SSE/sync surface); `crates/tangram/src/app.rs` (the
`FRAME_ANCESTORS`/CSP header); `crates/tangram-host/src/mcp.rs` (the MCP
bridge); ADR-0005 (egress credential injection); ADR-0006 (tenant isolation).

---

## Executive summary

Tangram already gives every app, under one port and a `/<app>/…` prefix, the
five surfaces composition needs: a UI, a JSON action API (`POST
/<app>/api/actions/{name}`), a live SSE state stream (`GET
/<app>/api/events`), a CRDT sync endpoint (`/<app>/sync`), and an MCP endpoint
(`/<app>/mcp`). Apps are prefix-mounted and already iframe-embeddable: the SDK
emits a `Content-Security-Policy: frame-ancestors …` header driven by
`FRAME_ANCESTORS` (default `*`). So "render an app inside a note" is, at the
HTML level, one `<iframe src="/<app>/">`.

The hard part is **not** drawing the iframe; it is the trust model created by
the single-port design. Because every app is served from the *same origin*,
the browser's iframe `sandbox` attribute cannot truly contain a hostile
embedded app: the well-documented `allow-scripts` + `allow-same-origin` footgun
lets same-origin framed content remove its own sandbox. And `frame-ancestors`
(a per-page *defense against being framed*) and `sandbox` (a parent's
*restriction on what the frame may do*) point in opposite directions — each
only solves half the problem. This shapes the whole recommendation.

The recommended layering:

1. **Embedding (app-in-note):** embed via `<iframe src="/<app>/instance">`.
   Treat first-party apps on the shared origin as a *cooperative trust domain*
   — the iframe is for layout/lifecycle isolation, not a security boundary. To
   make the iframe a real boundary (needed for any third-party or marketplace
   app), serve embedded UIs from a **distinct origin** (a per-app subdomain or
   a dedicated `*.usercontent` host) so `sandbox` becomes enforceable, and keep
   `frame-ancestors` as the complementary anti-clickjacking lock.
2. **Shallow UI events (host ↔ frame, frame ↔ frame):** an **origin-checked
   `postMessage` bus brokered by the host shell**, fronted by a tiny typed RPC
   (penpal/Comlink-style handshake). Good for "the note tells the embedded
   chart which row is selected," resize negotiation, and theme/auth-token
   handoff. Never trust a `postMessage` payload as authorization.
3. **Server-side composition (one app invoking another):** the existing
   **action API + MCP tools** are already the composition substrate. Keep it
   server-mediated through the host, gated by the bearer/principal model and
   the per-app capability allowlist (this is the natural home for a brokered
   capability/event bus). The SDK already reserves `#[action(server)]` for an
   app calling another app — that is the typed front door.
4. **Deep data composition (apps sharing state):** **linked Automerge
   documents** — one app stores another doc's sync handle / URL and renders
   from it as a second replica over `/<app>/sync`. This is the strongest form
   of "talk to each other," but it shares the *whole* document and so is safe
   today only **within a trust domain** (writers in an Automerge doc are
   mutually trusting; per-field ACL is the open Keyhive problem noted in
   `SDK_DESIGN.md`).

Safe-across-trust-boundaries vs first-party-only, at a glance:

| Mechanism | First-party | Untrusted publisher |
|---|---|---|
| iframe embed, same origin | OK (cooperative) | **Unsafe** — sandbox is defeatable same-origin |
| iframe embed, distinct origin + `sandbox` | OK | OK (the only safe embed) |
| `postMessage` UI bus (origin-checked) | OK | OK for *events*; never for authz |
| Action API / MCP call (host-brokered, capability-gated) | OK | OK — this is the designed seam |
| Linked/shared CRDT document | OK | **Unsafe** until per-doc capabilities (Keyhive) |

---

## 1. Embedding: the app-in-a-note iframe

### What exists today

- Every app is reachable at `/<app>/…` on one port; `routes.rs::dispatch_app`
  resolves the app from the live table per request and forwards. The UI is a
  `ServeDir` fallback; UIs fetch **relative** paths (AGENTS.md), which is what
  lets them work under any prefix mount (`/notes/`, `/t/alice/notes/`).
- The SDK sets the framing policy itself: `crates/tangram/src/app.rs` reads
  `FRAME_ANCESTORS` (default `*`) and emits `Content-Security-Policy:
  frame-ancestors {value}` over the whole router. The README documents this env
  var (the "iframed into Obsidian / a Tangram shell" row of the surfaces table).
- The host's per-app router (`routes.rs::AppEntry::new`) does **not** itself
  re-emit a CSP — the component-backed surface is served behind the host's
  dispatcher; framing policy for host-run apps is a gap to close in design
  (today only the native SDK path sets `frame-ancestors`).

So embedding is literally:

```html
<!-- inside the notes UI, rendering a markdown note that references an app -->
<iframe src="/nutrition/" title="Nutrition" loading="lazy"></iframe>
```

### The two controls do *opposite* jobs — use both

These are routinely conflated. They are complementary, not alternatives
([MDN][csp-fa], [OWASP clickjacking][owasp-cj]):

- **`Content-Security-Policy: frame-ancestors`** — set by the *embedded* app
  (Tangram already does this). It answers "**who may frame me?**" It is the
  modern clickjacking defense and supersedes `X-Frame-Options`. For
  app-in-note we want this to *allow* the shell/note origin, not `*`.
- **iframe `sandbox` / `allow` attributes** — set by the *embedding* app on the
  `<iframe>`. They answer "**what may this frame do once loaded?**" (scripts,
  forms, popups, top-navigation, storage, camera, etc.).

`frame-ancestors` protects the embedded app from a hostile *host*; `sandbox`
protects the host from a hostile *embedded app*. App-in-note needs both.

### The single-origin trap (the load-bearing finding)

Tangram serves every app from one origin (single port, path prefixes). MDN and
web.dev are explicit: **if framed content is same-origin and you grant both
`allow-scripts` and `allow-same-origin`, the framed document can reach up and
remove its own `sandbox` attribute** — "no more secure than not using sandbox
at all" ([MDN iframe][mdn-iframe], [web.dev sandboxed iframes][webdev-sandbox]).
A Tangram app needs scripts to function and, if same-origin, also has
same-origin DOM/storage access. So **on the shared origin the `sandbox`
attribute is not a real boundary** between apps.

Consequence for the trust model:

- **First-party apps** (the only tier shipping today per ADR-0006) are a
  *cooperative trust domain*. The iframe is still worth using — for layout
  isolation, independent lifecycle/reload, CSS encapsulation, and crash
  containment — but treat it as a convenience, not a security perimeter. A
  buggy first-party embed can already reach the shared origin's APIs.
- **Untrusted / different-publisher apps** (the marketplace future;
  `apps/marketplace` and RUNTIME_PLAN Phase 8) **must not** share the origin if
  they are embedded with scripts. The fix is the same one VS Code uses for
  webviews: serve untrusted embedded UIs from a **distinct origin** so that
  `sandbox="allow-scripts"` *without* `allow-same-origin` yields a true unique
  opaque origin the embed cannot escape ([VS Code webview][vscode-webview]).
  Options, in rough order of effort:
  - per-app subdomain (`notes.apps.example`, `nutrition.apps.example`) —
    cleanest, needs wildcard DNS/TLS;
  - a single dedicated `usercontent` origin distinct from the shell origin
    (GitHub's `*.githubusercontent.com` pattern);
  - sandboxed iframe with no `allow-same-origin` even on the shared host — the
    embed becomes a null origin, which blocks the same-origin escape but also
    blocks the embed's *own* `fetch('/<app>/api/...')` (it is no longer
    same-origin to its API), so it must talk through the postMessage bus
    (Layer 2) instead. This is viable and needs no DNS changes.

### Sandbox/allow recipe (starting point for design)

For an **untrusted** embed on a distinct origin:

```html
<iframe
  src="https://nutrition.apps.example/instance/abc"
  sandbox="allow-scripts allow-forms"          <!-- NOT allow-same-origin -->
  allow="clipboard-write"                       <!-- deny camera, geolocation, etc. by default -->
  referrerpolicy="no-referrer"
  loading="lazy"></iframe>
```

and the embedded app responds with `Content-Security-Policy: frame-ancestors
https://shell.example` (i.e. tighten `FRAME_ANCESTORS` from `*` to the shell
origin). For **first-party** embeds on the shared origin, `allow-same-origin`
is acceptable (so the embed's relative-path `fetch` to its own API works) —
with the explicit understanding that this is a cooperative-domain decision,
documented as such.

### Sizing / responsiveness

iframes don't auto-size to content. Two standard approaches, both fitting
Tangram's posture:

- **postMessage resize handshake** (Layer 2): embed measures
  `document.documentElement.scrollHeight` (e.g. via `ResizeObserver`) and posts
  `{type:"resize", height}` to the parent, which sets the iframe height. This
  is the cross-origin-safe path (the parent can't read the child's size
  cross-origin). Penpal/Comlink wrap this cleanly.
- For **aspect-ratio / fluid** tiles where exact content height doesn't matter,
  CSS `aspect-ratio` + `width:100%` on the iframe avoids messaging entirely.

The note renderer should pick a default height and let the embed negotiate up.

---

## 2. Inter-app communication — substrate comparison

Four substrates, each mapping onto a surface Tangram already exposes.

### 2a. `window.postMessage` (host shell ↔ embedded frame; frame ↔ frame)

- **What it's good for:** UI-level coordination that must not round-trip to a
  server — selection sync ("note row 3 selected → chart highlights series 3"),
  resize negotiation, theme/locale handoff, "save" / "dirty" lifecycle
  signals, and passing a short-lived auth token from shell to embed. It is the
  *only* channel that works when the embed is on a distinct/opaque origin.
- **Security implications:** `postMessage` is unauthenticated and any frame can
  post to any window it has a handle to. You **must** check `event.origin`
  against an allowlist on receive and pass an explicit `targetOrigin` (never
  `"*"`) on send. The VS Code OAuth-token-theft CVE class is precisely a
  webview `postMessage` handler that trusted a forged message and crossed a
  privilege boundary ([Trail of Bits][tob-vscode], [VS Code
  webview][vscode-webview]). Rule: **`postMessage` carries events, never
  authorization.** A message saying "delete note 5" must still go through the
  action API where the principal/bearer is enforced.
- **Fit:** the host shell is the natural broker — it owns the parent window and
  knows each child iframe's origin, so it can mediate a star topology (frame →
  shell → frame) and enforce the who-may-talk-to-whom policy in one place,
  rather than letting frames discover each other. Wrap raw `postMessage` in a
  small typed RPC with a handshake and origin allowlist (penpal
  `allowedOrigins`, Comlink `windowEndpoint` + `allowedOrigins`, or postmate's
  handshake validation — see prior art). This is the cheapest layer to add and
  needs no Rust changes.

### 2b. One app calling another's action API / MCP tools (server-side)

- **What it's good for:** *behavioral* composition — a "daily review" app that
  calls `notes.list_notes` and `nutrition.summary`; an agent orchestrating
  several apps; a workflow app that writes into another via its mutating
  actions. This is composition by *operation*, not by shared memory.
- **What exists:** every action is already `POST /<app>/api/actions/{name}` and
  every action is already an MCP tool at `/<app>/mcp` (`routes.rs`, `mcp.rs`).
  With a gateway, all apps' tools aggregate at `/mcp`, namespaced `<app>_<tool>`
  (`routes.rs::aggregate_mcp`) — an agent already composes apps this way. The
  SDK *design* reserves `#[action(server)]` "for things that genuinely need
  server-side effects (fetch a URL, **call another Tangram app**)"
  (`SDK_DESIGN.md`) — i.e. the typed, in-process front door for app→app calls
  already has a name.
- **Security implications:** mutating actions are bearer-gated (`AppEntry::new`
  wires `auth::bearer_guard` over `POST /api/actions/*` and `auth::mcp_guard`
  over mutating MCP tools; tenant routes resolve a `Principal`). So app→app
  calls inherit the existing authz: the caller must present a credential the
  callee accepts. The per-app **capability allowlist** (`allow_hosts`,
  RUNTIME_PLAN "app contract") and ADR-0005 egress injection govern *outbound*
  HTTP — an app calling another app's HTTP API is outbound egress and is
  already inside that model. This is where a **brokered capability/event bus**
  belongs: the host already mediates who may reach what, so "app A may call app
  B's action X" is a host-side grant in the same shape as `allow_hosts`, not a
  new subsystem.
- **Fit:** strongest for cross-trust-boundary composition, because the call is
  *server-mediated* and *capability-scoped* — A never touches B's memory or
  document, only B's published, authorized operations. This is the seam to
  invest in for untrusted apps.

### 2c. Shared / linked CRDT documents (deep composition)

- **What it's good for:** the deepest "talk to each other" — two apps rendering
  and editing the *same* live state. Automerge supports this natively:
  documents have URLs (`automerge:<base58>`), a root document can hold a list of
  other documents' URLs, and a peer loads/links them by URL ([Automerge
  load-by-url][am-url], [Automerge multiple task lists][am-multi]). In Tangram
  terms: app A's document stores app B's sync handle, and A opens a *second
  replica* of B's document over `/<app-b>/sync` (`docs/SYNC_PROTOCOL.md`) — A
  now sees B's state live, merges, and the SSE stream pushes B's changes into
  A's UI.
- **Security implications:** this is the **least safe across trust
  boundaries.** In Automerge the *document* is the unit of access control and
  "within a doc, writers are mutually trusted" (`SDK_DESIGN.md`,
  tangram-auth). Linking a document grants whole-document read (and, if a
  writer, whole-document write). Field-level permissions inside a CRDT are an
  open research problem — exactly what Keyhive is for, and not shipped. So
  shared CRDT docs are a **first-party / same-trust-domain** mechanism today.
  There is also no secondary-index / referential-integrity guarantee across
  documents ([Automerge discussion #695][am-695]) — bidirectional links are
  denormalized and can drift; apps must tolerate dangling references.
- **Fit:** excellent *within* a trust domain (e.g. a "projects" app and a
  "tasks" app the same owner controls, sharing a tasks document). For the
  app-in-note case specifically, a lighter pattern is usually better: the note
  stores only a **reference** (app name + instance/doc id) and embeds the app's
  own UI (Layer 1) rather than re-rendering B's raw document — the embed
  already syncs its own document and pushes live updates, so you get liveness
  without A having to understand B's schema.

### 2d. Brokered capability / event bus (host-mediated)

- **What it's good for:** the policy layer over 2a–2c. Rather than apps
  discovering and calling each other ad hoc, the **host** is the broker that
  decides who-may-call-whom and who-may-subscribe-to-what, tying composition
  into the same allowlist/grant model as `allow_hosts` and the tenant
  principal. Concretely: a host-side registry of grants ("note-app may invoke
  chart-app.render", "dashboard may subscribe to sales.events") plus a host
  endpoint that frames and apps route through, so every cross-app interaction
  is observable and revocable in one place.
- **Security implications & fit:** this is the *right home* for cross-trust
  composition because it composes with ADR-0006 (the host is already the
  isolation arbiter for co-resident WASM) and ADR-0005 (the host is already the
  egress chokepoint where credentials are attached). It does **not** remove the
  side-channel reality of co-resident WASM (ADR-0006): a brokered bus governs
  *intended* information flow, not microarchitectural leakage — for genuinely
  untrusted multi-tenant composition the ADR-0006 untrusted-tier controls
  (process-per-tenant, SMT off, CAT) still apply underneath. The bus is largely
  an *organizing principle* over the action/MCP layer (2b) plus the postMessage
  layer (2a), not a fourth wire format.

---

## 3. Prior art

### Obsidian — the closest analogue to "app in a note"

Obsidian is the reference design for embedding live content in a note. Native
**transclusion** (`![[note]]`, `![[note#heading]]`, `![[note#^block]]`) inlines
other notes. For arbitrary HTML/apps, community plugins (HTML Embed, Local HTML
Embed, the iframe-renderer family) register a markdown post-processor
(`registerMarkdownPostProcessor`) that swaps a code block for a **sandboxed
iframe**. Crucially, Obsidian's embeds run with a sandbox like `allow-scripts
allow-same-origin allow-forms allow-popups`, and **cookies / localStorage /
IndexedDB are intentionally unavailable** — the docs state that enabling them
"would require same-origin privileges, which would also let HTML share origin
with Obsidian instead of staying isolated" ([HTML Embed
plugin][obs-html-embed], [Obsidian sandbox forum][obs-sandbox]). That is exactly
the single-origin trap from §1, acknowledged by a mature product: they accept a
sandbox-with-same-origin posture for *local* content and deliberately starve it
of storage. **Lesson for Tangram:** the note embeds an app by reference (a
code-block / link), a post-processor renders the iframe, and the security
depends entirely on the origin story — match VS Code's distinct-origin model
rather than Obsidian's local-trust model for anything untrusted.

### Micro-frontends — for composing multiple apps in one shell

The standard taxonomy ([single-spa setup][single-spa], [freeCodeCamp
microfrontends][fcc-mfe], [Module Federation 2025][elysiate-mfe]):

- **iframes** — strongest isolation (separate document/JS context), weakest
  integration (clumsy shared state, sizing pain). This is Tangram's natural fit:
  apps are already independent deployables behind a prefix.
- **Web components / custom elements** — a `<tangram-app>` element wrapping the
  embed; Shadow DOM gives CSS encapsulation without an iframe, but **no JS
  isolation** (same realm) — so same trust caveats as same-origin iframes, with
  less containment. Useful as the *host-side wrapper* element that owns the
  iframe + postMessage plumbing.
- **Module Federation (webpack/Vite)** — runtime sharing of JS modules across
  independently-deployed frontends; powerful but assumes a shared build
  toolchain and *mutual trust* (federated code runs in the host realm). Wrong
  fit for untrusted Tangram apps; overkill for first-party ones given the apps
  are plain static UIs.
- **Import maps / native federation** — lightweight module resolution; same
  shared-realm trust assumption. Not needed for iframe-based composition.

Net: Tangram is already an iframe micro-frontend architecture; the design work
is the *origin* story and the *messaging* story, not adopting a framework.

### iframe-RPC libraries — for Layer 2

All three mainstream libraries wrap `postMessage` in promise-based RPC with
**explicit origin validation**, which is the security-relevant feature:

- **Penpal** — `allowedOrigins` on the messenger (e.g. `[new URL(iframe.src)
  .origin]`); promise-based methods over postMessage ([penpal][penpal]).
- **Comlink** (Google) — `expose(obj, endpoint, allowedOrigins)` and
  `windowEndpoint()` to target an iframe; `allowedOrigins` defaults to `['*']`
  so it **must be set explicitly** ([Comlink][comlink]).
- **Postmate** — handshake-based; origin, message type, and mime-type are
  verified against the handshake ([postmate][postmate]).

Recommendation: adopt the *pattern* (typed RPC + strict `allowedOrigins`), keep
the dependency tiny or vendored — the shell needs only resize + a small event
vocabulary, not a general RPC graph.

### Capability-based / sandboxed-UI composition

- **VS Code webviews** — the gold standard for *untrusted UI in a trusted
  host*. Webview content is an `<iframe>` from a **separate `vscode-webview://`
  origin**, distinct from the editor's origin, so the framed JS cannot reach
  editor/Node APIs; all communication is `postMessage`; extensions must set a
  CSP and sanitize input. The recent one-click OAuth-token-theft bug was a
  `postMessage` handler trusting a forged event — underscoring "events not
  authz" ([VS Code webview][vscode-webview], [Trail of Bits][tob-vscode],
  [iframe sandbox best practices][feroot-iframe]). **This is the model Tangram
  should copy for marketplace apps:** distinct origin + postMessage + CSP + no
  implicit trust of messages.
- **Jupyter widgets (ipywidgets)** — interactive widgets backed by a
  kernel-side model synced to a front-end view over a documented comm protocol;
  the *model is the source of truth and the view is derived/live* ([Jupyter
  widgets][jupyter-widgets]). This is structurally Tangram's own
  model→SSE→UI pattern, and validates the "embed renders from a live synced
  model" approach (Layer 1 + 2c) over screenshotting state across.
- **NASA OpenMCT** — plugins extend a host with new object/telemetry/view
  providers via a typed registration API ([OpenMCT plugins][openmct]); a
  *registry of capabilities the host exposes* — the same shape as Tangram's
  host-brokered capability bus (Layer 2d).

---

## 4. Recommendation — a layered approach for Tangram

Layered so each piece can ship independently and each respects the
single-port/prefix-mount model, the capability/isolation posture (ADR-0005/6),
and the app contract.

### Layer 0 — close the framing-policy gap (prerequisite, small)

- Make the **host-run** per-app surface emit `frame-ancestors` too (today only
  the native SDK path in `app.rs` does). Drive it from the app spec /
  `FRAME_ANCESTORS` so a host can scope embedding to the shell origin instead of
  `*`. Without this, host-served apps have no clickjacking policy.

### Layer 1 — app-in-note embedding

- The note stores a **reference**, not the app: a markdown directive / link like
  `![app](tangram://nutrition/instance/abc)` rendered by a post-processor into
  an `<iframe src="/nutrition/instance/abc">` (Obsidian's model, minus the
  local-trust assumption).
- **First-party (today):** same-origin iframe, `sandbox="allow-scripts
  allow-same-origin allow-forms"`, tightened `frame-ancestors` to the shell.
  Documented as a *cooperative trust domain* — the iframe is isolation of
  layout/lifecycle, not security.
- **Untrusted (marketplace, Phase 8):** serve the embed from a **distinct
  origin** (per-app subdomain or a single `usercontent` host) and drop
  `allow-same-origin` so `sandbox` is a real boundary — mirror VS Code webviews.
  This is the only embed safe across publishers; gate it behind the ADR-0006
  untrusted-tier controls.
- Sizing via the Layer-2 resize handshake; default height with negotiate-up.

### Layer 2 — shallow UI events (host-brokered postMessage)

- A small typed RPC over `postMessage`, **brokered by the shell** in a star
  topology (frame → shell → frame), with **strict `allowedOrigins`** and
  explicit `targetOrigin`. Vocabulary: `resize`, `selection`/`context`,
  `theme`, lifecycle (`ready`/`dirty`/`saved`), and short-lived token handoff.
- **Invariant:** messages are events, never authorization. Any state change a
  message implies is executed through the action API (Layer 3) under the
  principal/bearer — the message only *requests* it.

### Layer 3 — server-side composition (action API + MCP), capability-gated

- App→app behavioral composition uses the **existing** action/MCP surfaces,
  server-mediated. Realize `#[action(server)]`'s reserved "call another Tangram
  app" path as the typed front door, routed through the host so it is subject to
  the bearer/principal gate and to a **host-side grant** ("app A may invoke app
  B.tool") in the same shape as `allow_hosts` — this *is* the brokered
  capability/event bus (Layer 2d), implemented as policy over the action/MCP
  layer rather than a new transport.
- Safe across trust boundaries: A never touches B's memory or document, only B's
  published, authorized operations. Invest here for untrusted apps.

### Layer 4 — deep data composition (linked CRDT documents)

- For **first-party / same-trust-domain** apps only: let an app link another
  app's document by sync handle and open it as a second replica over
  `/<app>/sync`, rendering live from the shared state. Tolerate dangling
  cross-document references (no referential integrity).
- **Do not** expose this across publishers until per-document capabilities
  (Keyhive, `SDK_DESIGN.md` trajectory) land — sharing a CRDT doc today shares
  the whole doc with mutually-trusting writers. For app-in-note, prefer Layer 1
  (embed B's own UI, which already syncs B's doc) over A re-rendering B's raw
  document.

### What is safe where (the bottom line)

- **Within first-party / one trust domain:** all four layers are fine; the
  shared origin and shared-CRDT shortcuts are acceptable conveniences.
- **Across trust boundaries (different publishers, untrusted marketplace
  apps):** only **distinct-origin sandboxed iframes** (Layer 1, untrusted
  variant), **origin-checked postMessage events** (Layer 2, events-only), and
  **host-brokered capability-gated action/MCP calls** (Layer 3) are safe.
  Same-origin embedding and shared CRDT documents are **not** — and even the
  safe layers sit on top of, not instead of, the ADR-0006 co-residency controls.

---

## Sources

- MDN — `<iframe>` (sandbox; the allow-scripts + allow-same-origin escape):
  <https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/iframe> [mdn-iframe]
- web.dev — Play safely in sandboxed iframes:
  <https://web.dev/articles/sandboxed-iframes> [webdev-sandbox]
- MDN — CSP `frame-ancestors`:
  <https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Security-Policy/frame-ancestors> [csp-fa]
- OWASP — Clickjacking Defense Cheat Sheet (frame-ancestors vs X-Frame-Options):
  <https://cheatsheetseries.owasp.org/cheatsheets/Clickjacking_Defense_Cheat_Sheet.html> [owasp-cj]
- Feroot — iframe security best practices 2025:
  <https://www.feroot.com/blog/how-to-secure-iframe-compliance-2025/> [feroot-iframe]
- VS Code — Webview API (separate origin, CSP, postMessage):
  <https://code.visualstudio.com/api/extension-guides/webview> [vscode-webview]
- Trail of Bits — Escaping misconfigured VS Code extensions (webview postMessage):
  <https://blog.trailofbits.com/2023/02/21/vscode-extension-escape-vulnerability/> [tob-vscode]
- Obsidian — HTML Embed plugin (sandbox flags, storage starvation):
  <https://community.obsidian.md/plugins/html-embed> [obs-html-embed]
- Obsidian forum — iframe sandbox restrictions:
  <https://forum.obsidian.md/t/can-iframe-sandbox-restrictions-be-removed-via-a-plugin/27909> [obs-sandbox]
- single-spa — recommended setup (micro-frontends):
  <https://single-spa.js.org/docs/recommended-setup/> [single-spa]
- freeCodeCamp — How microfrontends work: iframes to Module Federation:
  <https://www.freecodecamp.org/news/how-microfrontends-work-iframes-to-module-federation/> [fcc-mfe]
- Elysiate — Micro-frontends with Module Federation (2025):
  <https://www.elysiate.com/blog/micro-frontends-architecture-module-federation-2025> [elysiate-mfe]
- Penpal (iframe RPC, allowedOrigins):
  <https://github.com/Aaronius/penpal> [penpal]
- Comlink (postMessage RPC, allowedOrigins / windowEndpoint):
  <https://github.com/GoogleChromeLabs/comlink> [comlink]
- Postmate (handshake-validated postMessage RPC):
  <https://github.com/dollarshaveclub/postmate> [postmate]
- Automerge — Load documents by URL:
  <https://automerge.org/docs/tutorial/load_by_url/> [am-url]
- Automerge — Multiple task lists (root doc linking docs by URL):
  <https://automerge.org/docs/tutorial/multiple-task-lists/> [am-multi]
- Automerge — discussion #695 (related documents / no secondary indexes):
  <https://github.com/automerge/automerge/discussions/695> [am-695]
- Jupyter — Interactive Widgets (model↔view comm):
  <https://jupyter.org/widgets> [jupyter-widgets]
- NASA OpenMCT — Plugins (capability registration):
  <https://nasa.github.io/openmct/plugins/> [openmct]

Internal grounding (this repo): `README.md`; `docs/SDK_DESIGN.md`;
`docs/RUNTIME_PLAN.md`; `docs/SYNC_PROTOCOL.md`; `AGENTS.md`;
`crates/tangram/src/app.rs` (FRAME_ANCESTORS / CSP);
`crates/tangram/src/web.rs` (state/actions/SSE/sync surface);
`crates/tangram-host/src/routes.rs` (dispatch, per-app router, tenant
namespace, aggregate MCP); `crates/tangram-host/src/mcp.rs` (MCP bridge);
`apps/marketplace/` (curated catalog + Phase-8 third-party TODO);
`docs/adr/0005-egress-credential-injection.md`;
`docs/adr/0006-tenant-isolation-posture.md`.
