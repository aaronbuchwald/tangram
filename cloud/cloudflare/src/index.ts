// Tangram sync relay on Cloudflare Workers + Durable Objects.
//
// One Durable Object instance per app name holds that app's automerge
// document and speaks the exact HTTP sync interface from
// docs/SYNC_PROTOCOL.md, so a replica pointed at
// `https://<worker>/<app>/sync` cannot tell this relay from a native
// Tangram instance.
//
// The `workerd` entrypoint of @automerge/automerge imports its WASM build
// directly; wrangler bundles it as a WebAssembly.Module, no init dance needed.
import * as Automerge from "@automerge/automerge";
import { DurableObject } from "cloudflare:workers";

export interface Env {
  TANGRAM_DOC: DurableObjectNamespace<TangramDoc>;
  /** Comma-separated app names this relay serves, e.g. "notes,nutrition". */
  APPS: string;
}

const OCTET_STREAM = { "Content-Type": "application/octet-stream" };

export class TangramDoc extends DurableObject<Env> {
  /** The app's automerge document, loaded from storage on first use. */
  doc: Automerge.Doc<unknown> | null = null;
  /**
   * Per-peer sync states, keyed by the client's X-Tangram-Session id. Held
   * in memory only: a DO restart resets them, which is harmless — the
   * protocol re-converges from a fresh SyncState (see SYNC_PROTOCOL.md).
   */
  sessions = new Map<string, Automerge.SyncState>();
  /** Open SSE poke streams. */
  pokes = new Set<ReadableStreamDefaultController<Uint8Array>>();

  async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (path === "/healthz") return new Response("ok");
    if (path === "/sync" && request.method === "POST") return this.sync(request);
    if (path === "/sync/events") return this.syncEvents();
    if (path === "/api/state") {
      return Response.json(await this.loadDoc());
    }
    return new Response("not found", { status: 404 });
  }

  /**
   * `POST /sync`: apply the peer's sync message (if any), persist + poke on
   * change, then respond with every message we owe that peer, each framed
   * as [u32 big-endian length][bytes].
   */
  async sync(request: Request): Promise<Response> {
    const session = request.headers.get("X-Tangram-Session");
    if (!session) {
      return new Response("missing X-Tangram-Session header", { status: 400 });
    }
    // Read the body before touching the doc: from loadDoc() on, the only
    // awaits are DO storage ops, which the DO's input gate covers — so two
    // concurrent POSTs can't interleave doc updates.
    const body = new Uint8Array(await request.arrayBuffer());
    let doc = await this.loadDoc();
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

  /** Wake every connected peer; drop streams whose client went away. */
  poke(): void {
    for (const controller of this.pokes) {
      try {
        controller.enqueue(POKE);
      } catch {
        this.pokes.delete(controller);
      }
    }
  }

  async loadDoc(): Promise<Automerge.Doc<unknown>> {
    if (!this.doc) {
      const bytes = await this.ctx.storage.get<Uint8Array>("doc");
      // Genesis matters: an empty relay starts as a literal empty document
      // (no commits at all), NOT a document with its own genesis commit —
      // inventing one would fork the history the apps share. The app's
      // deterministic genesis merges in on first sync because this empty
      // doc has no conflicting history.
      this.doc = bytes ? Automerge.load(bytes) : Automerge.init();
    }
    return this.doc;
  }
}

const encoder = new TextEncoder();
const POKE = encoder.encode("event: poke\ndata:\n\n");
const PING = encoder.encode(": keep-alive\n\n");

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

// ── worker entry: routes /<app>/... to that app's Durable Object ────────────

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const apps = (env.APPS ?? "")
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);

    if (url.pathname === "/") {
      const lines = apps.map((app) => `  /${app}/sync   /${app}/sync/events   /${app}/api/state`);
      return new Response(`tangram relay\n\napps:\n${lines.join("\n")}\n`);
    }

    const [, app, ...rest] = url.pathname.split("/");
    if (!apps.includes(app)) {
      return new Response(`unknown app "${app}" (configured: ${apps.join(", ")})`, { status: 404 });
    }
    const stub = env.TANGRAM_DOC.get(env.TANGRAM_DOC.idFromName(app));
    url.pathname = `/${rest.join("/")}`;
    return stub.fetch(new Request(url, request));
  },
} satisfies ExportedHandler<Env>;
