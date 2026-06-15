# tangram — the shell app

The Obsidian-style default view for a Tangram host: a persistent left sidebar
(a folder-aware markdown vault tree + the live apps on this host, with icon
buttons and a custom naming modal) and a main window with a tab strip
(tab-dedup, no self-nesting) whose tabs render `.md` files in a CodeMirror
live-preview editor or embed other apps as iframes. `tangram-host` serves it at
`/tangram/` and 307-redirects `/` there.

This is **RUNTIME_PLAN Phase S1** (foundational slice) plus the live-preview
editor and vault polish that followed. Design and decisions:
[docs/design/tangram-shell-redesign.md](../../docs/design/tangram-shell-redesign.md);
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
  Vite + TypeScript bundle CodeMirror 6 (the live-preview markdown editor,
  `src/livePreview.ts` + `src/editor.ts`), marked + DOMPurify (rendering), and
  the shell chrome (sidebar, folder tree, tab strip, app-iframe embedding). Every
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

## Shipped beyond the foundational slice

The CodeMirror 6 live-preview editor and the folder-aware vault polish
(icon buttons, custom naming modal, tab-dedup, no self-nesting) shipped on top
of S1, and `tangram` is now the host's default `/` route (307 redirect from
`/`).

## Deferred follow-up phases (NOT yet built)

These remain out of scope. See the redesign doc for the full rationale; tracked
in RUNTIME_PLAN under the shell phases.

- **Marketplace WASM-blob upload + host-side content-addressed hosting**
  (Phase S3), behind the default-off, loudly-warned `[artifacts]` gate and the
  MUST-FIX security checklist.
- **Folding registry/marketplace management fully into the sidebar** — the
  shell shows the live apps list (open-as-iframe-tab) but does not yet host
  enable/disable/install controls inline; the standalone `/registry/` and
  `/marketplace/` UIs remain.
- **postMessage inter-app / shell↔app coordination** and **app-embedded-in-a-
  note** (composability task; docs/design/app-composability-research.md). Today
  apps embed as full-tab iframes only.
- **Split/docking panes** (dockview) and **multi-tenant / federated app-set
  views** in the shell chrome.
