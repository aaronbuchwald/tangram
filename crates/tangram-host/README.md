# tangram-host

The embedded-Wasmtime host: runs Tangram apps as `wasm32-wasip2` components
per `apps.toml` with capability grants. See the repo
[README "Run apps as WASM components"](../../README.md#run-apps-as-wasm-components-tangram-host)
and [RUNTIME_PLAN](../../docs/RUNTIME_PLAN.md) for the full picture.

## Artifact store: upload + hosting (Phase S2b)

The host can host the WASM blob itself, so a publisher uploads a component
instead of self-hosting a URL and asserting a hash:

- **`POST /artifacts`** — accept a raw `wasm32-wasip2` component body. The
  **host** validates it is a real wasm component (magic bytes + a Wasmtime
  parse — garbage and bare core-modules are rejected), computes the sha-256
  **itself** (the uploader never asserts it), and stores the bytes
  content-addressed under `$HOME/.tangram-host/components/<sha256>.wasm` — the
  **same** store the install-by-URL cache uses. Returns
  `{ "sha256": "...", "url": "/artifacts/<sha256>.wasm" }` (HTTP 201).
- **`GET /artifacts/<sha256>.wasm`** — serve a stored artifact by content
  address, with a long `immutable` cache header.

Because the upload lands in the install cache, an uploaded artifact is
**immediately installable** by pointing a spec's `component_url` at this
host's `…/artifacts/<sha>.wasm` with the returned hash — the existing
verify-before-instantiate pipeline runs verbatim. The marketplace UI's
**Upload** flow does exactly this: upload → `add_listing` with the returned
local URL + sha; the URL+hash listing path still works unchanged.

> ### ⚠️ OPEN UPLOAD IS A DEV/DEMO-ONLY CAPABILITY, DEFAULT-OFF. ⚠️
>
> An endpoint that lets anyone store arbitrary binary blobs on your host is
> **arbitrary-blob storage** — on a public bind it is an abuse, DoS, and
> malware/illegal-content-hosting magnet (OWASP
> [Unrestricted File Upload](https://owasp.org/www-community/vulnerabilities/Unrestricted_File_Upload),
> [File Upload Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/File_Upload_Cheat_Sheet.html)).

### The gate (what ships now)

- **Default OFF.** `[artifacts] upload_enabled = false` in `apps.toml`. When
  off, `POST /artifacts` and `GET /artifacts/<sha>.wasm` both **404** (no
  capability oracle).
- When **on**, the host **refuses to start on a non-loopback `BIND_ADDR`
  without `TANGRAM_AUTH_TOKEN`** (mirrors the registry posture). With a token,
  `POST /artifacts` requires `Authorization: Bearer <token>`; without one the
  host is loopback-only, so anonymous upload is local-only.
- A **loud startup WARNING** is emitted whenever upload is enabled.

```toml
# apps.toml — dev/demo only, loopback or token-gated
[artifacts]
upload_enabled = true
```

### MUST-FIX before exposing open upload publicly

Open upload **MUST NOT** be enabled on a public deployment until ALL of these
exist. Only items marked DONE ship today:

1. **AuthN/AuthZ** — behind the bearer gate; never anonymous off loopback.
   — **DONE** (token-gated when a token is set; loopback-only otherwise,
   enforced at startup).
2. **Size limits** — a hard per-upload byte cap (stream-and-reject, never
   buffer a whole blob in memory) and a per-host/account storage **quota**.
   — **NOT YET DONE** (the body is currently buffered whole).
3. **Rate / frequency limits** — cap uploads per principal per window (DoS).
   — **NOT YET DONE.**
4. **Type/shape validation** — accept only valid `wasm32-wasip2` components
   (magic + Wasmtime parse) **and** enforce the closed-world import audit
   (reject `wasi:sockets` / `wasi:http` / a filesystem import) at upload time.
   — wasm-validity **DONE**; the upload-time import-audit reject is **NOT YET
   DONE** (the marketplace *displays* the audit, and the converge-time verifier
   `src/verify.rs` stamps a `granted ⊆ declared ⊆ audited` verdict on the fleet,
   but neither hard-rejects a forbidden import on upload).
5. **Content/abuse controls** — at minimum a hash blocklist of known-bad
   artifacts; ideally a sandboxed smoke-run + the behavioral check the
   marketplace README lists as the third-party-submission TODO. — **NOT YET DONE.**
6. **Operator controls** — delete/garbage-collect blobs and an audit log of
   who uploaded what. — **NOT YET DONE.**

Until (2)–(6) are met this is a dev affordance that lets a single owner
iterate locally without a release pipeline. **Federation note:** a
`/artifacts/<sha>` URL is host-local; a synced/federated listing must point at
a globally reachable URL (a release/CDN), per the RUNTIME_PLAN registry-first
artifact-pipeline deferral.
