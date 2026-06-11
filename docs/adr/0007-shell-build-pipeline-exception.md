# ADR-0007: Build-pipeline exception for the `tangram` shell app

**Status:** accepted (2026-06-11)
**Deciders:** Aaron (owner)
**Related:** docs/design/tangram-shell-redesign.md (Decision A); the app
contract in docs/RUNTIME_PLAN.md; docs/design/app-composability-research.md
(the iframe boundary this rests on)

## Context

Ordinary Tangram apps obey a strict UI contract: a single self-contained
`ui/index.html`, no build step, no external deps, relative paths — which keeps
apps trivially portable, inspectable, and embeddable, but caps the achievable
UI at hand-rolled vanilla + single-file vendored libs (render + textarea, no
docking panes, no live-preview editor). The `tangram` shell app (the
Obsidian-style default view: sidebar + tabs + markdown + app-iframes) wants an
Obsidian-grade experience (CodeMirror 6 live-preview, a docking/tab layout),
and the best implementations of those are **bundler-dependent npm packages**
(multi-module, dedup-sensitive) that cannot be dropped into a single file.

## Decision

**The first-party `tangram` shell app gets a build-pipeline exception: its
frontend may use npm + a bundler (a `dist/` produced by Vite/esbuild),
bundling libraries like CodeMirror and a docking layout.** No other app gets
this — ordinary and third-party apps keep the strict single-file/no-build UI
contract.

This is safe because of the **iframe boundary**: the shell embeds other apps
as `<iframe src="/<app>/">`; the shell and an embedded app share nothing but
HTTP (and optional origin-checked `postMessage`). The shell's bundle never
runs in a component's page or vice versa, so the shell's toolchain has no
bearing on how components are built or sandboxed.

Scope of the exception, precisely:
- It applies ONLY to the shell's **frontend/UI** (the `ui/` becomes a built
  bundle instead of one file).
- The shell's **backend is a normal wasm component** under the unchanged
  capability contract (its markdown vault + folder tree live in its replicated
  Automerge document, same as any app's state).
- The wasm/capability/sandbox/egress (ADR-0005) and tenancy (ADR-0006)
  contracts are entirely untouched.

Consequent sub-decisions: the shell ships a real editor (CodeMirror
live-preview) from v1 — no throwaway textarea phase; the shell is the primary
home (registry management folds into its sidebar; marketplace is a tab) but a
lightweight standalone route is kept for headless/no-shell access.

## Consequences

- Introduces the repo's **first build pipeline** — `npm`/lockfile,
  `node_modules`, a bundler config, and a CI build+lint+typecheck job — all
  contained to the shell app's directory. Added toolchain surface (and
  maintenance/rot risk) is the accepted cost of the target UX.
- The app contract in RUNTIME_PLAN must be amended to say: *ordinary apps —
  single-file, no build; the first-party shell app — build pipeline permitted
  for its UI only (ADR-0007).* "No feature may violate the contract" still
  binds every non-shell app.
- CI gains a shell-frontend job (install, typecheck, build, and confirm the
  built assets are what the host serves); the host serves the shell's built
  `dist/` the same way it serves any app's static UI.
- Because the boundary is the iframe, this decision is reversible per-feature:
  a future minimal/embedded build could still ship a buildless shell variant
  without affecting components.
