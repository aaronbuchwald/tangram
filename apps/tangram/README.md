# tangram — the shell app

The Obsidian-style default view for a Tangram host: a persistent left sidebar
(a markdown vault folder tree + the live apps on this host) and a main window
with a tab strip whose tabs render `.md` files or embed other apps as iframes.

This is **RUNTIME_PLAN Phase S1** — the foundational slice. Design and
decisions: [docs/design/tangram-shell-redesign.md](../../docs/design/tangram-shell-redesign.md);
the build-pipeline carve-out: [ADR-0007](../../docs/adr/0007-shell-build-pipeline-exception.md);
the iframe/composition model: [docs/design/app-composability-research.md](../../docs/design/app-composability-research.md).

## Two halves

- **Backend (`src/lib.rs`)** — an ordinary wasm component under the unchanged
  capability contract. A markdown **vault**: a flat, deterministic
  `Vec<MdFile>` in the app's replicated Automerge document; folders are
  derived from `/`-separated paths (empty folders are kept alive by a hidden
  `.keep` sentinel). Actions: `list_files`, `read_file`, `create_file`,
  `write_file`, `rename_file`, `delete_file`, `create_folder`,
  `rename_folder`, `delete_folder`. Builds native **and** `wasm32-wasip2`
  like every app; `Default` seeds one deterministic welcome note.

- **Frontend (`ui/`)** — the **one app granted a build pipeline** (ADR-0007).
  Vite + TypeScript bundle marked + DOMPurify (markdown rendering) and the
  shell chrome (sidebar, folder tree, tab strip, app-iframe embedding). Every
  asset URL is relative (`base: "./"` in `vite.config.ts`), so the bundle is
  prefix-mountable under `/tangram/` exactly like a single-file app UI. The
  host serves the built `ui/dist/` as this app's static UI dir.

## Building the frontend

```sh
cd apps/tangram/ui
npm ci          # install from the committed lockfile
npm run build   # tsc --noEmit (typecheck) + vite build -> ui/dist/
```

`ui/dist/` is **committed** — the host serves it directly and
`cargo run -p tangram-host` must work without a Node toolchain. After any
change under `ui/src`, rebuild and commit the result. CI (the `shell-frontend`
job) rebuilds and fails if the committed `dist/` is stale. `npm run dev`
serves a hot-reloading dev server (point its `api/` calls at a running host).

## Running it

The component is registered in the repo's `apps.toml` as `[apps.tangram]`
with `ui = "apps/tangram/ui/dist"`. Build the wasm components and the UI, then
run the host:

```sh
cargo build -p tangram-app-tangram --lib --target wasm32-wasip2 --release
(cd apps/tangram/ui && npm ci && npm run build)
cargo run -p tangram-host --release -- apps.toml
# open http://127.0.0.1:8080/tangram/
```

## Deferred follow-up phases (NOT in S1)

These are intentionally out of scope for the foundational slice. See the
redesign doc for the full rationale; tracked in RUNTIME_PLAN under the shell
phases.

- **CodeMirror 6 live-preview editing** (Phase S4, ADR-0007). S1 edits notes
  in a plain `<textarea>` with a rendered preview pane; a real live-preview
  editor is the build-gated follow-up.
- **Marketplace WASM-blob upload + host-side content-addressed hosting**
  (Phase S3), behind the default-off, loudly-warned `[artifacts]` gate and the
  MUST-FIX security checklist.
- **Folding registry/marketplace management fully into the sidebar** — S1
  shows the live apps list (open-as-iframe-tab) but does not yet host
  enable/disable/install controls inline; the standalone `/registry/` and
  `/marketplace/` UIs remain.
- **Making `tangram` the host's default `/` route** — S1 serves the shell at
  `/tangram/` only; the host index is unchanged.
- **postMessage inter-app / shell↔app coordination** and **app-embedded-in-a-
  note** (composability task; docs/design/app-composability-research.md). S1
  embeds apps as full-tab iframes only.
- **Split/docking panes** (dockview) and **multi-tenant / federated app-set
  views** in the shell chrome.
