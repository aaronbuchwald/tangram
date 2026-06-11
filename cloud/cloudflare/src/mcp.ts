// The /mcp endpoint: HTTP adaptation around tangram-core's sans-io MCP
// state machine, running as the jco-transpiled `tangram:mcp` component
// (cloud/cloudflare/mcp-core — ADR-0002: reuse the Rust protocol layer, do
// not reimplement the wire contract in TS). This file only moves bytes:
// HTTP request → machine.handle() → either a finished response or a tool
// call, which is executed through the SAME app dispatch path as
// `POST /api/actions/{name}` and resolved back into the machine.

import { load as loadMcp } from "../dist/components/mcp/cores.js";
import { Describe } from "./components";
import { wasiImports } from "./shim";

// The tangram:mcp/machine surface (camelCased by jco; u64 token → bigint).
interface McpRequest {
  method: string;
  accept?: string;
  contentType?: string;
  sessionId?: string;
  body: Uint8Array;
}

type BodyKind = "empty" | "text" | "sse-message" | "sse-stream";

interface McpResponse {
  status: number;
  sessionId?: string;
  bodyKind: BodyKind;
  body: string;
}

interface McpToolCall {
  token: bigint;
  name: string;
  argsJson: string;
}

type Handled =
  | { tag: "response"; val: McpResponse }
  | { tag: "tool-call"; val: McpToolCall };

interface McpMachine {
  init(name: string, version: string, instructions: string | undefined, toolsJson: string): void;
  handle(request: McpRequest): Handled;
  resolve(token: bigint, outcome: string, text: string): McpResponse;
}

/** How the app's dispatch ended, classified like tangram-host's
 * `DispatchError` so the MCP mapping matches its bridge exactly. */
export type ToolOutcome =
  | { kind: "ok"; resultJson: string }
  | { kind: "unknown" | "bad-args" | "failed" | "internal"; message: string };

const KEEPALIVE_MS = 30_000;
const encoder = new TextEncoder();

export class McpEndpoint {
  private constructor(
    private machine: McpMachine,
    private dispatch: (name: string, argsJson: string) => Promise<ToolOutcome>,
  ) {}

  /** One endpoint per app DO: a fresh machine instance initialized with the
   * app's tool list (from describe(), same derivation as tangram-host). */
  static async create(
    describe: Describe,
    dispatch: (name: string, argsJson: string) => Promise<ToolOutcome>,
  ): Promise<McpEndpoint> {
    const root = await loadMcp(wasiImports([]));
    const machine = root.machine as unknown as McpMachine;
    const tools = describe.actions.map((a) => ({
      name: a.name,
      description: a.description,
      input_schema: a.input_schema,
    }));
    machine.init(describe.name, "0.1.0", describe.instructions, JSON.stringify(tools));
    return new McpEndpoint(machine, dispatch);
  }

  async fetch(request: Request): Promise<Response> {
    const handled = this.machine.handle({
      method: request.method,
      accept: request.headers.get("accept") ?? undefined,
      contentType: request.headers.get("content-type") ?? undefined,
      sessionId: request.headers.get("mcp-session-id") ?? undefined,
      body: new Uint8Array(await request.arrayBuffer()),
    });
    if (handled.tag === "response") return toHttp(handled.val);

    // A validated tools/call: run it through the app's dispatch path and
    // resolve with tangram-host's McpBridge mapping — domain failures are
    // tool results the agent can read; only unknown tools and internal
    // faults become JSON-RPC errors.
    const call = handled.val;
    let outcome: ToolOutcome;
    try {
      outcome = await this.dispatch(call.name, call.argsJson);
    } catch (e) {
      outcome = { kind: "internal", message: `internal error: ${e}` };
    }
    switch (outcome.kind) {
      case "ok": {
        // Pretty-print like the native bridge's to_string_pretty.
        let text = outcome.resultJson;
        try {
          text = JSON.stringify(JSON.parse(outcome.resultJson), null, 2);
        } catch {
          // serve the raw result text
        }
        return toHttp(this.machine.resolve(call.token, "ok", text));
      }
      case "bad-args":
      case "failed":
        return toHttp(this.machine.resolve(call.token, "fail", outcome.message));
      case "unknown":
        return toHttp(this.machine.resolve(call.token, "unknown-tool", outcome.message));
      case "internal":
        return toHttp(this.machine.resolve(call.token, "internal-error", outcome.message));
    }
  }
}

/** Frame a machine response for the transport (the embedder duties from
 * tangram_core::mcp: content-type headers, session header, SSE keep-alive
 * on the endless GET stream). */
function toHttp(response: McpResponse): Response {
  const headers = new Headers();
  if (response.sessionId) headers.set("mcp-session-id", response.sessionId);
  switch (response.bodyKind) {
    case "empty":
      return new Response(null, { status: response.status, headers });
    case "text":
      headers.set("content-type", "text/plain");
      return new Response(response.body, { status: response.status, headers });
    case "sse-message":
      headers.set("content-type", "text/event-stream");
      headers.set("cache-control", "no-cache");
      return new Response(response.body, { status: response.status, headers });
    case "sse-stream": {
      headers.set("content-type", "text/event-stream");
      headers.set("cache-control", "no-cache");
      let heartbeat: ReturnType<typeof setInterval>;
      const stream = new ReadableStream<Uint8Array>({
        start: (controller) => {
          if (response.body) controller.enqueue(encoder.encode(response.body));
          heartbeat = setInterval(() => {
            try {
              controller.enqueue(encoder.encode(": keep-alive\n\n"));
            } catch {
              clearInterval(heartbeat);
            }
          }, KEEPALIVE_MS);
        },
        cancel: () => clearInterval(heartbeat),
      });
      return new Response(stream, { status: response.status, headers });
    }
  }
}
