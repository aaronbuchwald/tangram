# Marketplace

A catalog of installable Tangram apps (RUNTIME_PLAN Phase 8) — itself an
ordinary Tangram app: replicated document, actions (`list_listings`,
`add_listing`, `remove_listing`), MCP tools, sync, and the web UI in `ui/`.

Each listing carries the full trust story for an install:

- **`component_url` + `component_sha256`** — the artifact and the digest it
  must hash to. The HOST downloads, verifies the sha-256 **before
  instantiation**, and caches the artifact immutably by hash under
  `$HOME/.tangram-host/components/` (re-converging on the same hash never
  refetches; a mismatch is a converge error in `GET /api/fleet` and the app
  does not run).
- **a REQUIRED capability manifest** — outbound hosts, env keys, and a data
  note: exactly what an install will GRANT, rendered prominently in the UI
  ("this app can reach: api.calorieninjas.com; env: CALORIENINJAS_API_KEY").
- **an import audit** — the `world root` block of
  `wasm-tools component wit <artifact>`: the mechanical proof of the
  component's closed world (no sockets, no filesystem, no wasi:http — the
  only reach is `tangram:app/host`, gated by the host's `allow_hosts`).

Installing is the registry's job: the UI's Install button posts the
listing's url + sha + the manifest's grants to the LOCAL registry at
`../registry/api/actions/install_app` (relative cross-app call under one
host), with the bearer token the user supplies (same localStorage slot as
the registry UI). Env grants are passed as `${KEY}` values so the host
expands them from its own `.env` — secret values never enter a replicated
document.

Run it under `tangram-host` with `require_auth = true` (see the repo's
`apps.toml`): browsing stays open, curating the catalog needs the token.

## Seeds

`Default` (the deterministic genesis) seeds the three first-party apps —
notes, nutrition, registry — with REAL sha-256 digests of the builds at
commit time and their real import audits, both checked in under `seed/` and
embedded at compile time. **Seeds are refreshed per release**: run
`seed/refresh.sh` after a release build and commit the diff. The seed
`component_url`s document the self-hosting pattern (a GitHub release
publishing the exact bytes the digests pin); any static file server works
the same.

## TODO — third-party submissions: NOT BUILT

Deliberately out of scope and recorded here (and in
`docs/RUNTIME_PLAN.md` Phase 8 + the UI footer): the catalog is
operator-curated via `add_listing`. Accepting third-party submissions
requires a submission pipeline that gates listing approval on:

1. **automated capability verification** — the declared manifest is a
   SUBSET of the audited component imports (manifest ⊆ imports);
2. **a sandboxed smoke-run** of the artifact;
3. **an LLM behavioral sanity check**.

None of that exists yet; nothing in this app pretends it does.
