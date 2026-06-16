# Design: agentgateway observability + per-(user, component, invocation) identity & authorization

**Status:** PROPOSED — approved direction. This is the **canonical design for
agentgateway observability and identity** (design-of-record). Two LOCKED choices
up front: (1) **observability is ON by default**, shipped as a **one-command,
self-hostable Langfuse stack** the host's generated config points at; (2)
identity is established at the **host** (which alone knows the authenticated
`Principal`, the dispatching component, and the agent invocation/run id) and
**propagated to the gateway per call** as a short-lived signed token, so the
gateway **authorizes and labels** every LLM/tool call by `(user, component,
invocation)`.

**The thesis.** Tangram already runs agentgateway as a supervised child that
proxies the LLM (`/llm/<name>`, ADR-0012) and MCP (`/mcp`, `/<app>/mcp`) planes
— and agentgateway already captures everything an LLM/agent observability stack
wants (token counts, cost, latency, model, prompt/completion content, MCP tool
calls). **We discard all of it today.** This design turns it on, ships an
ingester with it, and composes the existing `Principal` seam (auth.md C0–C7) +
the call-level egress grant (ADR-0008) so each trace/metric/log line is
attributed to the user, the component, and — for agents — the specific run.

**Related:** ADR-0012 (LLM proxy via agentgateway — the `/llm/*` routes,
host-injected keys, loopback default), ADR-0008 (call-level egress — the
`(method, host, path, shape)` grant the per-call authz reuses),
[`docs/design/auth.md`](auth.md) (the `Principal` enum + `ScopeSet` + the C0–C7
cadence this mirrors; the per-principal rate-limit), ADR-0005 (host-side
credential injection — the same "secret never leaves the host" posture for the
ingester key), [`docs/design/agents.md`](agents.md) (the `AgentRun`/invocation
model whose runs we attribute; the Agents/History view this telemetry feeds),
ADR-0011 (per-principal scope/rate-limit, the non-loopback gate).

Code anchors this composes:
`crates/tangram-host/src/gateway.rs` (`render_config` — the generated config
where telemetry + per-call authz are emitted; `Gateway::proxy` — the host hop
that owns request context; `free_port()` — the ephemeral-port helper;
`LOOPBACK_RULE` — the CEL authorization the per-call rule extends; the
`statsAddr`/`adminAddr`/`readinessAddr` pinned to `127.0.0.1:0`),
`crates/tangram-host/src/auth.rs` (`Principal`, `Scope`, `ScopeSet` — the
identity we stamp), `apps/tangram/src/lib.rs` (`AgentRun` / `invocation_id` —
the run we attribute), `apps/registry`/`apps/marketplace`/`apps/notes/Dockerfile`
+ `scripts/build-images.sh` (the deploy/packaging patterns the one-command stack
mirrors).

This is a research + design deliverable. No code accompanies it; each
implementation checkpoint (§8, O1–O4) is its own independently-shippable,
held-for-review PR.

---

## 1. The model in one paragraph

agentgateway is the **single LLM/tool boundary** on the box. It already parses
each provider's `usage` into a CEL `llm.*` object (tokens, cost, latency,
models, prompt/completion content) and can emit that as **OTLP traces**, a
**JSON access log**, and a **Prometheus metric** — but Tangram's integration
emits none of it (the stats port is ephemeral, no `tracing`/`accessLog` is in the
generated config, and `Gateway::proxy` streams bytes through uncaptured). This
design (a) turns the gateway telemetry **on by default** in `render_config` and
ships a **one-command Langfuse stack** the OTLP exporter points at, so a fresh
deploy has LLM/tool observability with zero extra steps; and (b) has the host
**mint a short-lived per-call identity token** stamping `sub=user:<id>`,
`component=<app>`, `invocation=<run-id>`, `scope=<…>`, which the gateway
**authorizes** (deny if that `(user, component, call)` isn't permitted) and
**labels** every trace/metric/log line with. The result is per-run LLM/tool
telemetry, attributed to exactly who/what made the call, flowing into the
History/Agents views — built entirely from the gateway + the `Principal` seam +
the ADR-0008 grant, no new egress path to audit.

---

## 2. What agentgateway already captures (and what we throw away today)

agentgateway's `ai` backend auto-parses the provider response `usage` into a CEL
`llm` object available to tracing/logging/metrics
([agentgateway: Observe traffic](https://agentgateway.dev/docs/standalone/latest/llm/observability/),
[Traces reference](https://agentgateway.dev/docs/standalone/main/reference/observability/traces/),
[solo.io: LLM observability with agentgateway + Langfuse](https://www.solo.io/blog/llm-observability-agentgateway-langfuse)):

| Surface | What it carries | Tangram today |
|---|---|---|
| **`llm.*` CEL object** | `requestModel`, `responseModel`, `provider`, `inputTokens`, `outputTokens`, `totalTokens`, `cost.*`, `timeToFirstToken`, `timePerOutputToken`, and (opt-in) `prompt` / `completion` content | parsed by the gateway, **never read** |
| **OTLP traces** | GenAI spans, GenAI semantic conventions (`gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.operation.name`, …); MCP `tools/list` + `tools/call` spans with params/results + per-backend latency | **not emitted** (no `config.tracing`) |
| **Access log** | text or JSON, per request, with `gen_ai.*` fields; `accessLog.add` CEL to add fields; OTLP shipping | **not emitted** (child stdout IS forwarded to host tracing via `forward_output`, but it's the default line log, not the structured access log) |
| **Prometheus** | `agentgateway_gen_ai_client_token_usage` histogram (dims `gen_ai_token_type`, `gen_ai_system`, `gen_ai_request_model`), on the stats listener | **unscrapeable** — `statsAddr` pinned to `127.0.0.1:0` (ephemeral; gateway.rs:450) |

The integration is *throwing away a complete LLM/agent observability feed.* This
design's O1 is almost entirely "stop discarding it."

---

## 3. The ingester — recommendation

**Recommended: [Langfuse](https://agentgateway.dev/docs/standalone/latest/integrations/llm-observability/langfuse/)
(OSS, self-hostable, OTLP-ingest, purpose-built for LLM traces).**

| | **Langfuse (recommend)** | Generic OTLP → Arize Phoenix (runner-up) |
|---|---|---|
| agentgateway docs | **First-class**: a dedicated standalone integration page + the solo.io blog | Generic OTLP path (same exporter) |
| Built for | LLM traces/spans, **token cost**, sessions, prompt/completion review | LLM traces/evals; broader OTLP also feeds Jaeger / Grafana Tempo |
| Self-host | `docker compose` (Langfuse + Postgres), OSS | `docker run`/compose, OSS |
| OTLP ingest | `/api/public/otel`, **HTTP** (Basic-auth public/secret keys) | OTLP gRPC/HTTP |
| Why default | Purpose-built for exactly the `llm.*` Tangram emits; cost dashboards out of the box; the path agentgateway itself documents | Pick this if a team already runs Phoenix/Tempo/Jaeger and wants one OTLP sink for everything |

Both consume the **same** agentgateway OTLP exporter — switching is a one-line
endpoint change, so the choice is non-committal. **Default to Langfuse**; expose
the endpoint as config so the runner-up (or any OTLP backend) is a drop-in.

> **OTLP-protocol gotcha (documented):** Langfuse's ingest endpoint speaks
> **OTLP/HTTP**, not gRPC; agentgateway defaults to gRPC. Either set the
> exporter to HTTP (`otlpProtocol: http` per the standalone→Langfuse-direct
> path) **or** front Langfuse with a tiny OTel Collector that receives gRPC and
> forwards OTLP/HTTP. We default to **direct OTLP/HTTP** (no collector — fewest
> moving parts; the
> [standalone-direct path](https://maniak.io/articles/2026-06-10-agentgateway-standalone-langfuse-direct-otlp/)),
> and document the collector as the fan-out option (§4.1).

---

## 4. Turn observability ON by default (O1)

Three changes to `gateway::render_config` (and the `Gateway` struct that holds
the ports), all in the generated config the host already writes atomically + the
gateway hot-reloads:

**(a) Emit OTLP tracing** — add a `config.tracing` block pointing at the
ingester, with GenAI field mappings from the `llm.*` object (the solo.io shape,
adapted to the standalone `config.tracing` reference):

```jsonc
// config: { adminAddr, statsAddr, readinessAddr, tracing }   ← tracing is NEW
"tracing": {
  "otlpEndpoint": "http://127.0.0.1:3000/api/public/otel",  // local Langfuse (§4.1)
  "otlpProtocol": "http",                                    // Langfuse ingest is HTTP, not gRPC
  "randomSampling": true,
  "fields": { "add": {
    "gen_ai.operation.name":        "\"chat\"",
    "gen_ai.system":                "llm.provider",
    "gen_ai.request.model":         "llm.requestModel",
    "gen_ai.response.model":        "llm.responseModel",
    "gen_ai.usage.prompt_tokens":   "llm.inputTokens",
    "gen_ai.usage.completion_tokens":"llm.outputTokens",
    "gen_ai.usage.total_tokens":    "llm.totalTokens",
    // identity labels (§5) — stamped from the per-call token's claims:
    "tangram.user":                 "<claim:sub>",
    "tangram.component":            "<claim:component>",
    "tangram.invocation":           "<claim:invocation>"
    // gen_ai.prompt / gen_ai.completion are OPT-IN (§7), omitted by default
  } }
}
```

**(b) Emit a JSON access log** with the `llm.*` token/cost/latency fields (so
even with no ingester, the host's own log carries structured per-call usage),
labelled by the identity claims:

```jsonc
"accessLog": {
  "format": "json",
  "add": {
    "input_tokens":  "llm.inputTokens",
    "output_tokens": "llm.outputTokens",
    "total_tokens":  "llm.totalTokens",
    "cost":          "llm.cost",
    "ttft_ms":       "llm.timeToFirstToken",
    "provider":      "llm.provider",
    "model":         "llm.responseModel",
    "user":          "<claim:sub>",
    "component":     "<claim:component>",
    "invocation":    "<claim:invocation>"
  }
}
```

The host already forwards the child's stdout to its own `tracing` (gateway.rs
`forward_output`), so the JSON access log lands in Tangram's logs automatically —
no new pipe.

**(c) Pin the stats port off the ephemeral slot** so Prometheus is scrapeable.
Today `statsAddr = "127.0.0.1:0"` (gateway.rs:450) — Prometheus can never find
it. Mint a stable loopback port at startup with the existing `free_port()`
helper, store it on `Gateway`, and render it:

```jsonc
"config": { "adminAddr": "127.0.0.1:0", "readinessAddr": "127.0.0.1:0",
            "statsAddr": "127.0.0.1:<free_port()>", "tracing": { … } }
```

`adminAddr`/`readinessAddr` stay ephemeral (no scrape target); only `statsAddr`
becomes addressable. The host logs the chosen `statsAddr` so an operator (or the
bundled Prometheus, O4) can scrape `http://127.0.0.1:<stats>/metrics` for
`agentgateway_gen_ai_client_token_usage`.

### 4.1 Capture point — OTLP-direct vs the proxy hop

Two places can capture a call; **recommend (a) as primary, (b) as the
attribution enricher**, not either/or:

| Capture point | Richness | What it knows | Verdict |
|---|---|---|---|
| **(a) Gateway → ingester (OTLP)** | **Richest** — full GenAI spans, token/cost/latency, MCP tool spans, prompt/completion, zero app code | Only what's in the request + the identity *claims the host stamped* (§5) | **Primary.** This is the feed. |
| **(b) Host `Gateway::proxy` hop** | Coarse — status, bytes, wall-clock; **not** parsed tokens/cost | **Owns full Tangram request context** — the live `Principal`, the dispatching app, the agent `invocation_id` | **Enricher.** This is where the host *mints the identity token* (§5) so (a) is attributed. |

The synthesis: the **proxy hop is where Tangram stamps identity** (it's the only
place that has the authenticated `Principal` + invocation context), and the
**gateway OTLP export is where the rich telemetry leaves** — already carrying
those identity labels because the gateway read them off the token the proxy hop
injected. We do **not** build a second, parallel telemetry pipeline in the proxy
hop; it only attaches identity and (optionally) a coarse host-side span that the
ingester stitches by trace id.

---

## 5. Per-(user, component, invocation) identity & authorization (O2)

### 5.1 Where identity lives

Only the **host** can establish identity: the gateway sees a loopback request
with no Tangram principal. The host, at the `Gateway::proxy` boundary, knows all
three grains:

| Grain | Source | Claim |
|---|---|---|
| **user** | the authenticated `Principal` (auth.md C0–C7): `User { user_id }` / `Tenant(name)` / `LocalUser` | `sub = user:<id>` (or `local`) |
| **component** | the app/route dispatching the call (`/<app>/mcp`, `/llm/<name>`, or the agent's host loop) | `component = <app>` |
| **invocation** | for agents, the run id — `AgentRun.invocation_id` (agents.md §6, `apps/tangram/src/lib.rs`) | `invocation = <id>` |
| **scope** | the action's required `Scope` + the LLM/tool grant (`llm:<provider>`, `tool:<app>/<name>`) | `scope = <…>` |

### 5.2 Propagation — a short-lived signed token per call

The host **mints a short-lived token per call** and forwards it to the gateway
(the proxy hop already rewrites/strips hop headers — gateway.rs
`skip_request_header` — so it owns the request it sends upstream). The token's
claims are exactly the four grains above:

```jsonc
// minted host-side, per call, ~30–60s TTL, HS256 over a host-only key:
{ "sub": "user:alice", "component": "guided-learning",
  "invocation": "inv_7a1c…", "scope": "llm:anthropic",
  "iat": …, "exp": … }   // exp short — a leaked token is near-useless
```

Recommend **JWT (HS256) over a host-local signing key** (the gateway and host
share the secret; no asymmetric key distribution on a single box). The
signed-header alternative is simpler to mint but lacks a standard verifier in the
gateway; **default to JWT** (open decision §9).

### 5.3 The gateway authorizes AND labels on it

agentgateway validates the JWT (its native JWT/authz plane) and runs a **CEL
authorization rule** that **composes with** the existing loopback rule
(`LOOPBACK_RULE`, gateway.rs:233) — both must pass:

```text
// today, every route:           string(source.address).startsWith("127.") || == "::1"
// O2 adds, per route:           jwt.valid && (jwt.scope in route.allowed_scopes)
//                               && ((jwt.component, route.call) ∈ granted)   // ADR-0008
```

- **LLM routes** (`/llm/<name>`): deny unless `jwt.scope == llm:<that provider>`
  and `(jwt.component, jwt.user)` is permitted to use it.
- **MCP routes** (`/<app>/mcp`, `/mcp`): deny unless the `(user, component,
  tool-call)` is in the granted set — this is the **ADR-0008 call-level grant**
  evaluated at the gateway. `tools/call` of an undeclared tool is denied here, in
  addition to the host's existing `mcp_guard` (auth.rs) — defense in depth.

And the **same claims become labels** on every trace/metric/log (§4), so every
data point is attributed to `(user, component, invocation)` with no extra work.

### 5.4 The granularity spectrum — stamp all three

| Granularity | Buys you | Cost |
|---|---|---|
| user only | per-user cost/usage, per-user rate-limit | misses which app/agent spent it |
| (user, component) | + per-app/agent attribution & authz | misses which *run* |
| **(user, component, invocation)** | + **per-run** cost/latency/status → the History/Agents view (§6); precise debugging ("which run burned the tokens") | one more claim — free |

**Recommend stamping all three.** The marginal cost is one claim and one label;
the payoff (per-run telemetry in the in-app surface, §6) needs the invocation
grain. `invocation` is simply empty for a non-agent, ad-hoc call.

### 5.5 How it composes the three existing systems

```
Principal (auth.md C0–C7)  ─┐
   user_id / tenant / scopes │
                             ├─▶  host mints per-call JWT  ─▶  agentgateway
ADR-0008 call grant        ─┤      sub/component/invocation/scope      │
   (method,host,path,shape) │                                          ├─ authorize (CEL: loopback ∧ jwt ∧ grant)
                             │                                          └─ label  (trace/metric/log by claims)
AgentRun.invocation_id     ─┘                                                     │
   (agents.md §6)                                                                 ▼
                                                                          Langfuse (per-run trace)
```

Auth supplies *who*; ADR-0008 supplies *what call is allowed*; the gateway
supplies *authorize + observe*. Each already exists; this wires them.

---

## 6. In-app surface — telemetry into History/Agents (O3)

The per-run telemetry (model, tokens, latency, cost, status), labelled by
`invocation`, flows back into the **Agents view** (`apps/agents`, agents.md §8)
and the per-run **History** the `AgentRun` model already keys by `invocation_id`
(`apps/tangram/src/lib.rs`):

- The host, on a completed run, reads the call's usage (from the gateway access
  log / metric, keyed by the `invocation` label it stamped) and records it
  alongside the existing `AgentRun { invocation_id, last_run_ms }` — extended
  (additive `Option<…>` fields, per the model-evolution rule) with
  `model`, `input_tokens`, `output_tokens`, `cost`, `latency_ms`, `status`.
- The **Agents view** gains columns: model · tokens · cost · latency · status,
  sortable/filterable by the same query bar (agents.md §8) —
  `cost>0.01`, `status:error`, `model:claude-*`.
- **Deep-link to the trace UI** (optional): a per-run link to the Langfuse trace
  (`<langfuse>/trace/<trace-id>`), so a run row jumps to the full span tree. The
  trace id is the one the host stamped; the link is built, not stored as a
  secret. Loopback-local by default (§7), so the link only resolves on the box
  unless the operator exposed the ingester.

This needs the **invocation grain** from §5 — it's why we stamp all three.

---

## 7. One-command, observability-by-default deploy (the headline, O1)

A single command brings up the **ingester AND wires the gateway's OTLP exporter
at it**, so a fresh Tangram deploy has LLM/tool observability with no extra
steps. Mirror the existing packaging pattern (`apps/*/Dockerfile`,
`scripts/build-images.sh`) with a compose stack + a thin script:

```sh
scripts/observability-up.sh        # = docker compose -f deploy/observability/compose.yml up -d
```

**What it stands up** (all bound to loopback by default):

| Service | Image | Port (loopback) | Role |
|---|---|---|---|
| `langfuse` | `langfuse/langfuse:latest` | `127.0.0.1:3000` | trace/cost UI + OTLP ingest at `/api/public/otel` |
| `langfuse-db` | `postgres:16` | internal only | Langfuse store |
| *(optional)* `otel-collector` | `otel/opentelemetry-collector` | `127.0.0.1:4318` | only if fanning out to Jaeger/Tempo/Datadog (§4.1) |

The script:

1. `docker compose up -d` the stack (Langfuse + Postgres), waiting for health.
2. On first run, provisions a Langfuse project + OTLP key pair and writes them to
   `.env` (gitignored) as `LANGFUSE_PUBLIC_KEY` / `LANGFUSE_SECRET_KEY` — the
   gateway's OTLP `Authorization: Basic base64(public:secret)` header resolves
   from there host-side (the **ADR-0005 posture**: the ingester key lives in
   `.env`, never inline, never in a replicated doc, lowered to the gateway's
   `$VAR` like provider keys are).
3. The host's generated config (§4) already points `otlpEndpoint` at
   `http://127.0.0.1:3000/api/public/otel`, so **no further wiring** — start
   tangram-host and traces appear in Langfuse at `http://127.0.0.1:3000`.

> **One command, observability by default:** `scripts/observability-up.sh`
> followed by the normal `cargo run -p tangram-host -- apps.toml` — the gateway
> is born exporting to a live Langfuse. Tearing down:
> `scripts/observability-down.sh`.

**Defaults & exposure.** Everything binds loopback; the deploy is local-first.
For a non-loopback ingester (a shared team Langfuse), the doc notes: bind it
behind TLS + auth (Langfuse has its own auth), point `otlpEndpoint` at it, keep
the keys in `.env`, and — critically — the **identity claims become mandatory**
(§5) so a shared ingester's traces stay attributable. A missing/disabled
ingester is **non-fatal**: the gateway still emits the JSON access log + the
Prometheus metric (§4), so observability degrades to host-local logs, never off.

---

## 8. Phased, testable checkpoints (O1–O4)

Each is independently shippable + reviewable, mirroring the auth C0–C7 cadence.
Each has a one-line **review gate**.

| # | Checkpoint | Review gate |
|---|---|---|
| **O1** | **Gateway telemetry ON + one-command Langfuse stack.** Emit `config.tracing` (OTLP→Langfuse) + a JSON `accessLog` with `llm.*` fields from `render_config`; pin `statsAddr` to a `free_port()` slot; ship `deploy/observability/compose.yml` + `scripts/observability-up.sh`. | `scripts/observability-up.sh` then a `/llm/<name>` call → a GenAI trace appears in Langfuse and `agentgateway_gen_ai_client_token_usage` is scrapeable; ingester down ⇒ host still logs structured usage |
| **O2** | **Host→gateway composite-identity propagation + authorize/label.** Mint a short-lived JWT per call at `Gateway::proxy` with `sub`/`component`/`invocation`/`scope`; gateway CEL authorizes (loopback ∧ jwt ∧ ADR-0008 grant) and labels traces/logs/metrics by the claims. | a call with the wrong `(user, component)` scope is denied at the gateway; a permitted call's trace/log/metric carries `tangram.user` + `tangram.component` + `tangram.invocation` labels |
| **O3** | **Per-run telemetry in History/Agents.** Extend `AgentRun` (additive `Option<…>`) with model/tokens/cost/latency/status; record from the gateway feed keyed by the `invocation` label; Agents-view columns + sort/filter + optional Langfuse deep-link (§6). | a finished agent run shows tokens/cost/latency/status in the Agents view; the row deep-links to its Langfuse trace; `cost>…` / `status:error` filter the table |
| **O4** | **Prometheus / dashboards / cost budgets.** Bundle a Prometheus scrape of `statsAddr` + a starter dashboard; per-principal/per-agent **cost budget** that trips on `agentgateway_gen_ai_client_token_usage` (reuses the per-principal rate-limit, ADR-0011). | a dashboard shows per-(user,component) token spend; a budget cap denies further `/llm` calls for a principal over budget and recovers when the window rolls |

O1–O2 are the core (observability-by-default + attributed); O3 surfaces it
in-app; O4 is dashboards + spend control.

---

## 9. Open decisions (with recommended defaults)

| Decision | Options | **Recommend** |
|---|---|---|
| Identity propagation | JWT (HS256, host-local key) vs signed headers | **JWT** — agentgateway has a native JWT verifier; standard, short-TTL `exp` |
| Capture point | OTLP-direct vs proxy-hop vs both | **OTLP-direct primary** (rich telemetry) + **proxy-hop for identity-mint only** (§4.1) — not a parallel pipeline |
| Ingester | Langfuse vs generic OTLP→Phoenix | **Langfuse** (agentgateway-documented, LLM-purpose-built, cost dashboards); runner-up Phoenix is a one-line endpoint swap |
| Collector | direct OTLP/HTTP vs OTel Collector | **Direct OTLP/HTTP** (fewest moving parts); collector only to fan out to Jaeger/Tempo/Datadog |
| Prompt/completion capture | on vs **off** | **OFF by default** (§10) — counts/latency/cost always on; content opt-in per app/principal |
| Identity claims when single-user | mandatory vs optional | **Optional on loopback self-host** (the `LocalUser` case); **mandatory** the moment the ingester or gateway is exposed beyond loopback / multi-tenant |

---

## 10. Security checklist

- [ ] **Never log secrets/credentials.** Provider keys and the ingester key stay
  host-side (`env://` → `$VAR`, ADR-0005), never in the generated config, never
  in a trace/log/metric. Auth-bearing headers are already stripped at the egress
  boundary (ADR-0008); identity tokens are the only credential added, and they
  carry no secret.
- [ ] **Prompt/completion content capture is OPT-IN.** Token counts, cost,
  latency, model, status are **always on** (no sensitive content). The raw
  `gen_ai.prompt` / `gen_ai.completion` fields are emitted only when an app /
  principal explicitly enables content capture — off by default.
- [ ] **The ingester is host-local by default.** Langfuse + Postgres bind
  loopback; a non-loopback ingester requires TLS + its own auth, and flips
  identity claims to mandatory (§7).
- [ ] **Identity tokens are short-lived.** ~30–60s `exp`; a leaked token is
  near-useless and cannot widen scope (claims are minted host-side from the
  authenticated `Principal` + the ADR-0008 grant, never client-supplied).
- [ ] **The gateway stays loopback-only by default.** O2's per-call authz
  **composes with** (never replaces) the existing `LOOPBACK_RULE` — both must
  pass. Non-loopback exposure of the LLM/MCP plane stays hard-gated on the
  per-principal scope + rate-limit (ADR-0012 §4, ADR-0011), unchanged.
- [ ] **Don't break the self-hosted/loopback default.** Observability is on, but
  identity claims are *optional* on a single-user loopback box (the `LocalUser`
  case) — zero config, no token plumbing required — and become mandatory only
  when exposed/multi-tenant. A missing ingester degrades to host-local logs +
  Prometheus, never an error.
- [ ] **Per-call authz is defense-in-depth.** The gateway's `(user, component,
  call)` deny composes with — does not replace — the host's `mcp_guard` /
  `bearer_guard` (auth.rs); an undeclared tool call is refused at both.
- [ ] **Cost budgets fail closed** (O4): over-budget denies further spend; the
  budget check is a revocation-generation/rate-limit style gate (auth.md §12),
  not a cache that could lag.

---

## 11. Placement & merge strategy

- **This design doc lands first** (DOCS-ONLY) as a held-for-review PR on
  `docs/gateway-observability-identity`, merged `--ff-only` to `main`. No code.
- **Implementation lands host-side** in `crates/tangram-host/src/gateway.rs`
  (the telemetry render in `render_config`, the stable `statsAddr` via
  `free_port()`, the per-call JWT mint in `Gateway::proxy`) — `tangram-core`
  stays wasm-clean (the gateway integration is native-only tokio, as today).
- **The deploy stack** lands in `deploy/observability/` (compose +
  Prometheus/dashboard configs) + `scripts/observability-up.sh` /
  `observability-down.sh`, mirroring `scripts/build-images.sh` + the
  `apps/*/Dockerfile` packaging pattern. An index line in `CLAUDE.md` /
  README points at it.
- **The in-app surface** (O3) extends `apps/tangram/src/lib.rs` (`AgentRun`
  additive fields) + the Agents view (`apps/agents`, agents.md §8) — additive,
  back-compatible (the new fields are `Option<…>` with `missing` defaults, per
  the model-evolution rule).
- **An ADR should accompany O2** (the identity-propagation decision: JWT vs
  signed-header, the gateway authz/labels plane), the way ADR-0012 recorded the
  LLM-proxy decision — recording the per-call composite-identity model as a
  durable decision next to ADR-0008 (egress grant) and ADR-0011 (per-principal
  scope).
- **Conflict note:** O2 intersects the in-flight auth C-series (`auth.rs`
  `Principal`/scopes) and the egress work (`egress.rs` call grants). Sequence O2
  after C3 (the `Principal::User` + scope plumbing it consumes) lands, or at
  minimum rebase on it — the identity mint *reads* the resolved principal.

---

*This doc is the single source of truth for agentgateway observability +
per-(user, component, invocation) identity. The direction (observability ON by
default via a one-command Langfuse stack; host-minted per-call composite
identity the gateway authorizes and labels by; all three grains stamped) is
approved; implementation proceeds as the independently-reviewable checkpoints
O1–O4.*

**Sources:**
[agentgateway × Langfuse integration](https://agentgateway.dev/docs/standalone/latest/integrations/llm-observability/langfuse/) ·
[agentgateway: Observe traffic (standalone)](https://agentgateway.dev/docs/standalone/latest/llm/observability/) ·
[agentgateway: Traces reference](https://agentgateway.dev/docs/standalone/main/reference/observability/traces/) ·
[solo.io: LLM observability with agentgateway + Langfuse](https://www.solo.io/blog/llm-observability-agentgateway-langfuse) ·
[agentgateway standalone → Langfuse direct OTLP (no collector)](https://maniak.io/articles/2026-06-10-agentgateway-standalone-langfuse-direct-otlp/) ·
[agentgateway: LLM cost tracking](https://agentgateway.dev/docs/kubernetes/main/llm/cost-tracking/)
