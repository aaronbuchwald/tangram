# ADR-0012: agentgateway as an LLM proxy — path-based selection, config-driven providers, host-injected keys

**Status:** accepted (2026-06-15)
**Deciders:** Aaron (owner), with analysis by Claude
**Related:** ADR-0005 (egress credential injection — the host-side key posture
this reuses: the secret never reaches the component/client), the agentgateway
MCP plane (RUNTIME_PLAN D3, `crates/tangram-host/src/gateway.rs`, README "MCP
through agentgateway"), ADR-0011 + `docs/design/auth.md` (the per-principal
scopes / per-principal rate-limit this defers the non-loopback gate to).

## Context

The host already supervises **agentgateway** as a child process for the MCP
plane: it generates a JSON config from the merged desired state on every
converge (atomic write → hot-reload) and reverse-proxies `/<app>/mcp` + the
aggregate `/mcp` to it. agentgateway *also* natively proxies LLM providers via
`ai` backends, translating an OpenAI-style chat request to each provider's
native API and injecting a provider key configured on the route.

So Tangram can offer a single, host-managed LLM egress — **one boundary that
injects provider API keys** (the ADR-0005 posture: components and local clients
never hold the key) and that can later meter/authorize spend — **without adding
any new moving parts.** We emit extra `ai` routes into the config we already
generate and proxy a new `/llm/...` path to the gateway exactly like `/mcp`.

The question is the v1 surface: how clients select a provider, how providers are
configured, where the key lives, and what the network exposure is.

## Decision

**Path-based selection over a config-driven provider list, with host-side key
injection and a loopback-only default.**

1. **Path-based selection.** Operators declare providers under `[[gateway.llm]]`
   in `apps.toml`; each becomes a route at `/llm/<name>`. A client picks the
   provider/model by URL — `POST /llm/<name>/v1/chat/completions` with an
   OpenAI-style body — and the host proxies it host → agentgateway → the
   provider. No request-body routing, no single OpenAI-compat endpoint in v1.

2. **Config-driven providers.** `LlmProvider { name, provider, model?, key }`.
   `provider` is validated at load against agentgateway's supported AI providers
   (`openai | anthropic | gemini | vertex | bedrock | groq`); `name` is unique
   and path-safe; `model` is optional (omit ⇒ passthrough, the client body's
   `model` is honored). This is operator startup config, consistent with the
   rest of `[gateway]` (read once; restart to change), not converged live.

3. **Host-side key injection.** `key` MUST be an `env://NAME` reference (the
   same rule the egress `inject` uses, so a plaintext key never lands in
   `apps.toml` or a replicated registry doc). It is lowered to agentgateway's
   `backendAuth.key = "$NAME"`; the gateway child inherits the host environment
   (dotenvy `.env`), so the key resolves host-side at the boundary and never
   reaches the client or component (ADR-0005).

4. **Loopback-only default.** Every generated `/llm/<name>` route carries the
   *same* loopback-only `source.address` authorization rule every MCP route
   already does (agentgateway binds its data plane on the wildcard address — the
   same hardening lesson). The LLM proxy is reachable only from the box.

The client-facing contract was verified empirically against the installed
agentgateway (v1.2.1): a `POST` to any subpath under the `/llm/<name>` prefix
reaches the provider — the `ai` backend translates the OpenAI-style body to the
provider's native API and the `/llm/<name>` prefix selects the provider rather
than being forwarded literally upstream. We document and test
`/llm/<name>/v1/chat/completions` (OpenAI-compat) as the client path. The
integration test asserts config generation + route wiring + the loopback rule
and proves the live proxy path with a bogus key (a provider-side auth error),
so no provider tokens are spent in CI.

## Consequences

- One host-managed LLM egress with a single key boundary; apps that need an LLM
  call can issue a bare loopback request and never hold a key, consistent with
  the ADR-0005 egress posture and the AI-enabled-component pattern.
- Reuses the existing generated-config + supervised-child + reverse-proxy
  machinery; the only new code is the `ai` route renderer, the provider config +
  validation, and the `/llm/*` proxy path.
- The LLM proxy is a **spend surface.** v1 is **loopback-trusted only.** Before
  ANY non-loopback exposure it MUST first gate **per-principal** — an `llm`
  scope plus the per-principal rate-limit from the auth work (ADR-0011 /
  `docs/design/auth.md`). This is a hard gate, not a v1 deliverable.

## Alternatives considered

- **A single OpenAI-compat `/v1` endpoint** (provider chosen from the body's
  `model`). Convenient for OpenAI SDKs but pushes routing into request-body
  parsing and couples model names to providers; deferred as a follow-on.
- **Load-balanced / failover provider `groups`.** agentgateway supports them;
  out of scope for v1 (single provider per route).
- **Registry-driven (per-app) provider grants + usage metering.** The natural
  home for spend control once the per-principal gate exists; follow-on.
- **A bespoke in-host LLM client** (no agentgateway). Rejected: it duplicates
  provider translation, retry, and key handling that agentgateway already does,
  and adds a second egress path to audit.
