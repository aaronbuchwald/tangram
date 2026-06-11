// Tangram on Cloudflare Workers + Durable Objects.
//
// One Durable Object instance per app holds that app's automerge document
// and serves the FULL Tangram surface (RUNTIME_PLAN Phase 7, ADR-0002):
//
//   - the exact HTTP sync interface from docs/SYNC_PROTOCOL.md (unchanged
//     from the Phase-4 relay — a replica pointed at `/<app>/sync` cannot
//     tell this host from a native Tangram instance);
//   - the app's own logic, as the SAME jco-transpiled wasm32-wasip2
//     component tangram-host runs: `POST /api/actions/{name}` dispatches
//     doc-in/doc-out against the DO-stored document, `GET /api/state` and
//     `GET /api/events` render through the component's state-json, genesis
//     comes from its deterministic genesis() (byte-identical to native);
//   - `/mcp`, driven by tangram-core's sans-io MCP machine (the tangram:mcp
//     component) over the same dispatch path;
//   - the app's static UI, bundled from apps/<app>/ui.
//
// Apps are routed by the APPS var; an APPS entry without a bundled
// component (src/components.ts) degrades to the plain sync-relay surface.
//
// The `workerd` entrypoint of @automerge/automerge imports its WASM build
// directly; wrangler bundles it as a WebAssembly.Module, no init dance needed.
import * as Automerge from "@automerge/automerge";
import { DurableObject } from "cloudflare:workers";

import { authorizeTenant, handleAccount, handleAuth } from "./account";
import { TangramAccounts, tenantUnauthorized } from "./auth";
import { AppLogic, Env, appUi, hasComponent, instantiateApp } from "./components";
import { McpEndpoint, ToolOutcome } from "./mcp";
import { errorPayload } from "./shim";

export type { Env };
// The accounts Durable Object (RUNTIME_PLAN Phase 6) — exported here so
// wrangler's class binding finds it on the entry module.
export { TangramAccounts };

const OCTET_STREAM = { "Content-Type": "application/octet-stream" };
const JSON_TYPE = { "Content-Type": "application/json" };

/** A live app inside the DO: the component instance + its MCP endpoint. */
interface AppHost {
  logic: AppLogic;
  mcp: McpEndpoint;
}

export class TangramDoc extends DurableObject<Env> {
  /** The app's automerge document, loaded from storage on first use. */
  doc: Automerge.Doc<unknown> | null = null;
  /**
   * Per-peer sync states, keyed by the client's X-Tangram-Session id. Held
   * in memory only: a DO restart resets them, which is harmless — the
   * protocol re-converges from a fresh SyncState (see SYNC_PROTOCOL.md).
   */
  sessions = new Map<string, Automerge.SyncState>();
  /** Open SSE poke streams (`/sync/events`). */
  pokes = new Set<ReadableStreamDefaultController<Uint8Array>>();
  /** Open SSE full-state streams (`/api/events`). */
  stateStreams = new Set<ReadableStreamDefaultController<Uint8Array>>();
  /** The lazily-instantiated app component (null = no component bundled). */
  private appHost: Promise<AppHost | null> | null = null;
  /** Serializes action dispatches, mirroring tangram-host's per-app
   * serialization (JSPI dispatch awaits fetch(), which opens the DO's input
   * gate — without this two actions could race doc-in/doc-out). */
  private chain: Promise<unknown> = Promise.resolve();

  async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    // Which app this DO embodies; set by the Worker router on every request.
    const app = request.headers.get("x-tangram-app") ?? "";
    if (path === "/healthz") return new Response("ok");
    if (path === "/sync" && request.method === "POST") return this.sync(request, app);
    if (path === "/sync/events") return this.syncEvents();
    if (path === "/api/state") return this.apiState(app);
    if (path === "/api/actions" && request.method === "GET") return this.apiActions(app);
    if (path.startsWith("/api/actions/") && request.method === "POST") {
      return this.apiAction(app, path.slice("/api/actions/".length), await request.text());
    }
    if (path === "/api/events") return this.apiEvents(app);
    if (path === "/api/capabilities") return this.apiCapabilities(app);
    if (path === "/api/genesis") return this.apiGenesis(app);
    if (path === "/mcp") {
      const host = await this.ensureApp(app);
      if (!host) return new Response("app has no component", { status: 404 });
      return host.mcp.fetch(request);
    }
    return new Response("not found", { status: 404 });
  }

  // ── app logic ──────────────────────────────────────────────────────────────

  /** Instantiate the app's component + MCP machine once per DO instance.
   * A failed instantiation is not cached, so the next request retries. */
  private ensureApp(app: string): Promise<AppHost | null> {
    if (!this.appHost) {
      this.appHost = (async (): Promise<AppHost | null> => {
        const logic = await instantiateApp(app, this.env);
        if (!logic) return null;
        const mcp = await McpEndpoint.create(logic.describe, (name, argsJson) =>
          this.dispatchAction(app, name, argsJson),
        );
        return { logic, mcp };
      })().catch((e) => {
        this.appHost = null;
        throw e;
      });
    }
    return this.appHost;
  }

  /**
   * Run one action through the app component — the single dispatch path
   * shared by `POST /api/actions/{name}` and MCP `tools/call`, mirroring
   * tangram-host's `AppRuntime::dispatch`. Doc-in/doc-out: the guest gets
   * the current save; a mutated save is merged back, persisted to DO
   * storage, and announced to every subscriber (sync pokes + state SSE).
   */
  private dispatchAction(app: string, action: string, argsJson: string): Promise<ToolOutcome> {
    const run = async (): Promise<ToolOutcome> => {
      const host = await this.ensureApp(app);
      if (!host || !host.logic.describe.actions.some((a) => a.name === action)) {
        return { kind: "unknown", message: `unknown action: ${action}` };
      }
      const docBytes = Automerge.save(await this.loadDoc(app));
      let result;
      try {
        result = await host.logic.guest.dispatch(action, argsJson, docBytes);
      } catch (e) {
        // Guest-rendered ActionErrors arrive as result<_, string> payloads
        // (thrown strings / ComponentError.payload), classified by their
        // stable prefixes exactly like tangram-host's DispatchError; a real
        // Error without a payload is a trap/engine failure.
        const isPayload =
          typeof e === "string" || (!!e && typeof e === "object" && "payload" in e);
        const message = errorPayload(e);
        if (!isPayload) return { kind: "internal", message: `internal error: ${message}` };
        if (message.startsWith("unknown action:")) return { kind: "unknown", message };
        if (message.startsWith("invalid arguments:")) return { kind: "bad-args", message };
        if (message.startsWith("internal error:")) return { kind: "internal", message };
        return { kind: "failed", message };
      }
      if (result.doc) {
        const current = await this.loadDoc(app);
        const before = Automerge.getHeads(current);
        const merged = Automerge.merge(current, Automerge.load(result.doc));
        this.doc = merged;
        if (!headsEqual(before, Automerge.getHeads(merged))) {
          await this.ctx.storage.put("doc", Automerge.save(merged));
          this.poke();
          this.ctx.waitUntil(this.notifyState(app));
        }
      }
      return { kind: "ok", resultJson: result.resultJson };
    };
    // Serialize with predecessors, but never let their failures leak in.
    const next = this.chain.then(run, run);
    this.chain = next.catch(() => {});
    return next;
  }

  /** The current state as JSON text, exactly as the component rendered it
   * (served verbatim — reparsing floats is lossy; see tangram-host). */
  private async renderState(app: string): Promise<string | null> {
    const host = await this.ensureApp(app);
    if (!host) return null;
    return host.logic.guest.stateJson(Automerge.save(await this.loadDoc(app)));
  }

  // ── the JSON API (native web.rs shapes) ────────────────────────────────────

  async apiState(app: string): Promise<Response> {
    const state = await this.renderState(app);
    if (state !== null) return new Response(state, { headers: JSON_TYPE });
    // No component: the read-only relay view of the stored document.
    return Response.json(await this.loadDoc(app));
  }

  async apiActions(app: string): Promise<Response> {
    const host = await this.ensureApp(app);
    if (!host) return new Response("app has no component", { status: 404 });
    return Response.json({ actions: host.logic.describe.actions });
  }

  /** `POST /api/actions/{name}` with the SDK's status/error envelope. */
  async apiAction(app: string, name: string, body: string): Promise<Response> {
    const outcome = await this.dispatchAction(app, name, body.trim() ? body : "{}");
    switch (outcome.kind) {
      case "ok":
        return new Response(`{"result":${outcome.resultJson}}`, { headers: JSON_TYPE });
      case "unknown":
        return Response.json({ error: outcome.message }, { status: 404 });
      case "bad-args":
        return Response.json({ error: outcome.message }, { status: 400 });
      case "failed":
        return Response.json({ error: outcome.message }, { status: 422 });
      case "internal":
        return Response.json({ error: outcome.message }, { status: 500 });
    }
  }

  /** `GET /api/events`: SSE full-state stream — the current state on
   * connect, then again on every change (action, MCP call, or sync). */
  async apiEvents(app: string): Promise<Response> {
    const initial = await this.renderState(app);
    if (initial === null) return new Response("app has no component", { status: 404 });
    let heartbeat: ReturnType<typeof setInterval>;
    let ctrl: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({
      start: (controller) => {
        ctrl = controller;
        this.stateStreams.add(controller);
        controller.enqueue(stateEvent(initial));
        heartbeat = setInterval(() => {
          try {
            controller.enqueue(PING);
          } catch {
            clearInterval(heartbeat);
          }
        }, 15_000);
      },
      cancel: () => {
        clearInterval(heartbeat);
        this.stateStreams.delete(ctrl);
      },
    });
    return new Response(stream, {
      headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" },
    });
  }

  async apiCapabilities(app: string): Promise<Response> {
    const host = await this.ensureApp(app);
    // Apps that publish no capabilities 404, matching a native app
    // without the probe (and tangram-host).
    if (!host || host.logic.describe.capabilities === undefined) {
      return new Response("not found", { status: 404 });
    }
    return Response.json(host.logic.describe.capabilities);
  }

  /** The component's deterministic genesis bytes — byte-identical to a
   * native instance's by construction; served so tests can assert parity. */
  async apiGenesis(app: string): Promise<Response> {
    const host = await this.ensureApp(app);
    if (!host) return new Response("app has no component", { status: 404 });
    return new Response(host.logic.guest.genesis(), { headers: OCTET_STREAM });
  }

  /** Push the freshly-rendered state to every `/api/events` listener. */
  async notifyState(app: string): Promise<void> {
    if (this.stateStreams.size === 0) return;
    const state = await this.renderState(app);
    if (state === null) return;
    const event = stateEvent(state);
    for (const controller of this.stateStreams) {
      try {
        controller.enqueue(event);
      } catch {
        this.stateStreams.delete(controller);
      }
    }
  }

  // ── sync (docs/SYNC_PROTOCOL.md — unchanged wire behavior) ─────────────────

  /**
   * `POST /sync`: apply the peer's sync message (if any), persist + poke on
   * change, then respond with every message we owe that peer, each framed
   * as [u32 big-endian length][bytes].
   */
  async sync(request: Request, app: string): Promise<Response> {
    const session = request.headers.get("X-Tangram-Session");
    if (!session) {
      return new Response("missing X-Tangram-Session header", { status: 400 });
    }
    // Read the body before touching the doc: from loadDoc() on, the only
    // awaits are DO storage ops, which the DO's input gate covers — so two
    // concurrent POSTs can't interleave doc updates.
    const body = new Uint8Array(await request.arrayBuffer());
    let doc = await this.loadDoc(app);
    let state = this.sessions.get(session) ?? Automerge.initSyncState();
    if (body.length > 0) {
      const before = Automerge.getHeads(doc);
      try {
        [doc, state] = Automerge.receiveSyncMessage(doc, state, body);
      } catch (e) {
        return new Response(`bad sync message: ${e}`, { status: 400 });
      }
      if (!headsEqual(before, Automerge.getHeads(doc))) {
        // Tangram documents are small app states; a single storage value
        // holds up to 2 MiB, plenty for Automerge.save() here. Chunk the
        // bytes if an app ever outgrows that.
        await this.ctx.storage.put("doc", Automerge.save(doc));
        this.poke();
        this.ctx.waitUntil(this.notifyState(app));
      }
    }

    const frames: Uint8Array[] = [];
    for (;;) {
      const [next, message] = Automerge.generateSyncMessage(doc, state);
      state = next;
      if (!message) break;
      frames.push(message);
    }
    this.doc = doc;
    this.sessions.set(session, state);
    return new Response(frame(frames), { headers: OCTET_STREAM });
  }

  /**
   * `GET /sync/events`: SSE stream of `event: poke` — one immediately on
   * connect, then one per document change. A comment line every 30s keeps
   * intermediaries from closing the idle stream.
   */
  syncEvents(): Response {
    let heartbeat: ReturnType<typeof setInterval>;
    let ctrl: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({
      start: (controller) => {
        ctrl = controller;
        this.pokes.add(controller);
        controller.enqueue(POKE);
        heartbeat = setInterval(() => controller.enqueue(PING), 30_000);
      },
      cancel: () => {
        clearInterval(heartbeat);
        this.pokes.delete(ctrl);
      },
    });
    return new Response(stream, {
      headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" },
    });
  }

  /** Wake every connected sync peer; drop streams whose client went away. */
  poke(): void {
    for (const controller of this.pokes) {
      try {
        controller.enqueue(POKE);
      } catch {
        this.pokes.delete(controller);
      }
    }
  }

  /**
   * Load the document from storage. An empty DO whose app has a component
   * starts from the component's deterministic genesis (byte-identical to a
   * native instance's, so the histories share one root and merge — the
   * SYNC_PROTOCOL genesis rule only forbids relays that DON'T know the
   * model from inventing one). Componentless apps keep the Phase-4 relay
   * behavior: a literal empty document the app's genesis merges into.
   */
  async loadDoc(app: string): Promise<Automerge.Doc<unknown>> {
    if (!this.doc) {
      const bytes = await this.ctx.storage.get<Uint8Array>("doc");
      if (bytes) {
        this.doc = Automerge.load(bytes);
      } else {
        const host = await this.ensureApp(app);
        this.doc = host ? Automerge.load(host.logic.guest.genesis()) : Automerge.init();
      }
    }
    return this.doc;
  }
}

const encoder = new TextEncoder();
const POKE = encoder.encode("event: poke\ndata:\n\n");
const PING = encoder.encode(": keep-alive\n\n");

function stateEvent(stateJson: string): Uint8Array {
  return encoder.encode(`event: state\ndata: ${stateJson}\n\n`);
}

/** Concatenate sync messages as repeated [u32 big-endian length][bytes]. */
function frame(messages: Uint8Array[]): Uint8Array {
  const total = messages.reduce((n, m) => n + 4 + m.length, 0);
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let offset = 0;
  for (const message of messages) {
    view.setUint32(offset, message.length, false);
    out.set(message, offset + 4);
    offset += 4 + message.length;
  }
  return out;
}

function headsEqual(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((h, i) => h === b[i]);
}

// ── worker entry ─────────────────────────────────────────────────────────────
//
// Three surfaces (RUNTIME_PLAN Phases 7 + 6):
//   /<app>/...            the original single-user surface — open, kept
//                         byte-compatible (existing DOs and replicas)
//   /auth/* + /account*   OAuth sign-in (GitHub) and the account page (PATs)
//   /t/<tenant>/<app>/... per-account namespaces — EVERY request requires a
//                         PAT bearer or the session cookie; tenant data
//                         lives in its own DO (id from "t/<tenant>/<app>")

/** Serve one app surface: UI worker-side, everything else through the app's
 * Durable Object. `docName` keys the DO id — `<app>` for the single-user
 * surface, `t/<tenant>/<app>` per tenant (full data isolation; app names
 * never contain "/", so the keyspaces cannot collide). */
function serveApp(
  request: Request,
  env: Env,
  url: URL,
  apps: string[],
  app: string,
  docName: string,
  rest: string[],
  prefix: string,
): Response | Promise<Response> {
  if (!apps.includes(app)) {
    return new Response(`unknown app "${app}" (configured: ${apps.join(", ")})`, { status: 404 });
  }
  // The static UI is served Worker-side (no DO hop). `<prefix>/<app>`
  // redirects to `<prefix>/<app>/` so the page's relative fetches
  // ("api/state") resolve under the app prefix.
  if (rest.length === 0) {
    return Response.redirect(`${url.origin}${prefix}/${app}/`, 301);
  }
  const sub = rest.join("/");
  if (request.method === "GET" && (sub === "" || sub === "index.html")) {
    const ui = appUi(app);
    if (ui) {
      return new Response(ui, { headers: { "Content-Type": "text/html; charset=utf-8" } });
    }
  }

  const stub = env.TANGRAM_DOC.get(env.TANGRAM_DOC.idFromName(docName));
  url.pathname = `/${sub}`;
  const headers = new Headers(request.headers);
  headers.set("x-tangram-app", app);
  return stub.fetch(new Request(new Request(url, request), { headers }));
}

/** `/t/<tenant>/...`: resolve the principal FIRST — an unknown tenant, a
 * wrong/revoked token, another tenant's token, and no token at all answer
 * the same 401 (no existence oracle, mirroring tangram-host Phase 5). The
 * per-tenant app set is the worker's bundled APPS (a per-tenant registry on
 * CF is out of scope — see RUNTIME_PLAN Phase 6). */
async function handleTenant(
  request: Request,
  env: Env,
  url: URL,
  apps: string[],
): Promise<Response> {
  const [, , tenant, app, ...rest] = url.pathname.split("/");
  if (!tenant || !(await authorizeTenant(request, env, tenant))) {
    return tenantUnauthorized();
  }
  if (app === undefined || app === "") {
    const lines = apps.map(
      (a) => `  /t/${tenant}/${a}/   …/api/{state,actions,events}   …/sync(+/events)   …/mcp`,
    );
    return new Response(`tangram — tenant ${tenant}\n\napps:\n${lines.join("\n")}\n`);
  }
  return serveApp(request, env, url, apps, app, `t/${tenant}/${app}`, rest, `/t/${tenant}`);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const apps = (env.APPS ?? "")
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);

    if (url.pathname.startsWith("/auth/")) return handleAuth(request, env);
    if (url.pathname === "/account" || url.pathname.startsWith("/account/")) {
      return handleAccount(request, env, apps);
    }
    if (url.pathname === "/t" || url.pathname.startsWith("/t/")) {
      return handleTenant(request, env, url, apps);
    }

    if (url.pathname === "/") {
      const lines = apps.map((app) =>
        hasComponent(app)
          ? `  /${app}/   /${app}/api/{state,actions,events,capabilities}   /${app}/sync(+/events)   /${app}/mcp`
          : `  /${app}/sync   /${app}/sync/events   /${app}/api/state   (sync relay only)`,
      );
      return new Response(
        `tangram\n\napps:\n${lines.join("\n")}\n\n` +
          `account: /account (OAuth sign-in; your apps live under /t/<you>/)\n`,
      );
    }

    const [, app, ...rest] = url.pathname.split("/");
    return serveApp(request, env, url, apps, app, app, rest, "");
  },
} satisfies ExportedHandler<Env>;
