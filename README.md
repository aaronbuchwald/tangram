# Tangram App Template

A cookie-cutter for building small Rust servers in the **Tangram** style: one
backend, multiple frontends. Each app is a single binary that exposes its
capabilities three ways at once:

| Surface | Endpoint | Consumed by |
|---|---|---|
| **MCP** (streamable HTTP) | `/mcp` | AI agents — Claude Code, Claude Desktop, any MCP client |
| **Web UI** | `/` | Humans in a browser |
| **Embeddable Web UI** | `/` in an `<iframe>` | Note-taking apps (Obsidian), the Tangram shell, dashboards |

Tangram is about running and arranging small apps into a cohesive whole —
making it as easy as possible to securely run, connect, and share small apps
for AI. The pattern this template enforces: keep the domain logic in a shared
backend state, and treat MCP and the web UI as thin, parallel frontends over
it. AI and humans see the same live data.

The included example is a shared **scratchpad**: an agent can add notes via
MCP tools while a human watches them appear in the UI (and vice versa).

## Quickstart

```sh
cp .env.example .env   # optional; defaults work for local dev
cargo run
```

Then:

- Web UI: <http://127.0.0.1:8080/>
- Health: <http://127.0.0.1:8080/healthz>
- Connect an MCP client:

  ```sh
  claude mcp add --transport http scratchpad http://127.0.0.1:8080/mcp
  ```

  Ask Claude to "add a note to the scratchpad" and watch it appear in the UI.

### Embedding in Obsidian

Paste an iframe into any note (or use it in Canvas):

```html
<iframe src="http://127.0.0.1:8080/" style="width:100%;height:300px;border:none;"></iframe>
```

The server sends a `Content-Security-Policy: frame-ancestors` header instead
of `X-Frame-Options`, so embedding works anywhere by default and can be
restricted per deployment via `FRAME_ANCESTORS` in `.env`.

## Using this template

1. Clone, then rename the package in `Cargo.toml` (and `TangramMcp` if you like).
2. Replace the scratchpad in `src/state.rs` with your domain logic.
3. Expose it to AI: add `#[tool]` methods in `src/mcp.rs`.
4. Expose it to humans: add routes in `src/web.rs` and build out `ui/`.

### Layout

```
src/
  main.rs    server assembly: config, logging, CSP, MCP mount, graceful shutdown
  state.rs   shared backend state — your app's actual logic lives here
  mcp.rs     MCP frontend (rmcp tools over streamable HTTP)
  web.rs     web frontend (JSON API + static files from ui/)
ui/
  index.html dependency-free UI; theme-aware so it looks right embedded or standalone
```

### Configuration (`.env`)

| Variable | Default | Purpose |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:8080` | Listen address; `0.0.0.0:…` to expose |
| `FRAME_ANCESTORS` | `*` | Who may iframe the UI (CSP `frame-ancestors`) |
| `RUST_LOG` | `info` | Log filter |

`.env` is gitignored; commit `.env.example` instead.

## Security notes

The defaults favor local development and easy embedding. Before sharing an
app beyond your machine:

- **Bind**: keep `127.0.0.1` unless you mean to expose it; put TLS/auth in
  front (e.g. a reverse proxy or tailnet) if you do.
- **Framing**: set `FRAME_ANCESTORS` to the specific hosts that should embed
  the UI, e.g. `'self' app://obsidian.md`.
- **CORS**: `src/web.rs` uses `CorsLayer::permissive()` so embedding hosts can
  call the API; tighten it for apps holding sensitive data.
- **MCP**: `/mcp` is unauthenticated by default. rmcp has an `auth` feature
  (OAuth 2.0) when you need it.

## Stack

- [axum](https://docs.rs/axum) — HTTP server and routing
- [rmcp](https://docs.rs/rmcp) — official Rust MCP SDK (streamable HTTP server transport)
- [tower-http](https://docs.rs/tower-http) — static files, CORS, headers, tracing
- `ui/` is served from disk for fast iteration; for single-binary distribution,
  embed it with [rust-embed](https://docs.rs/rust-embed) or `include_str!`.
