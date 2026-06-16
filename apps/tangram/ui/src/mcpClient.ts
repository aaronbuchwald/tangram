// A minimal browser streamable-HTTP MCP client over `fetch`, talking to a
// per-app MCP endpoint (`../<app>/mcp`) which the host fronts via agentgateway
// on the same origin as the shell — so there is no CORS concern.
//
// Empirically pinned against the LIVE server (nutrition app, 2026-06):
//   POST ../<app>/mcp  with  Accept: application/json, text/event-stream
//   - `initialize` returns the session in the `Mcp-Session-Id` RESPONSE header;
//     that id is REQUIRED on every subsequent request (the server rejects a
//     non-initialize request with no session header: "mcp: session header is
//     required for non-initialize requests").
//   - Every response body is an SSE stream (content-type text/event-stream):
//     one or more `data: <json>` lines; the JSON-RPC response is the `data:`
//     payload whose `id` matches the request. (We also handle a plain JSON
//     body defensively, in case a future server answers without SSE.)
//   - The server advertises protocolVersion "2025-06-18"; we send that.
//   - tools/call result: { content: [{ type: "text", text: "..." }], isError }.

export interface McpTool {
  name: string;
  description?: string;
  // JSON Schema for the tool's arguments (MCP `inputSchema`).
  inputSchema?: Record<string, unknown>;
}

export interface McpContent {
  type: string;
  text?: string;
  [k: string]: unknown;
}

export interface McpCallResult {
  content: McpContent[];
  isError?: boolean;
}

const PROTOCOL_VERSION = "2025-06-18";

interface JsonRpcResponse {
  jsonrpc: "2.0";
  id?: number | string | null;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

// Parse a streamable-HTTP MCP response body. The body is either a single JSON
// object or an SSE stream of `data:` lines. We return the JSON-RPC response
// message (the `data:` whose payload carries `result`/`error`, preferring one
// matching `wantId`).
function parseMcpBody(
  body: string,
  contentType: string,
  wantId: number | string,
): JsonRpcResponse {
  const isSse =
    contentType.includes("text/event-stream") ||
    // Some bodies arrive as SSE without the header surviving the fetch layer;
    // sniff the leading `data:` framing too.
    /^\s*(event:|data:)/m.test(body);

  if (!isSse) {
    return JSON.parse(body) as JsonRpcResponse;
  }

  // Collect every `data:` JSON payload, then pick the response message. SSE
  // frames are separated by blank lines; a frame may span multiple `data:`
  // lines (concatenated). We keep it simple: gather each `data:` line's JSON.
  const messages: JsonRpcResponse[] = [];
  for (const rawLine of body.split(/\r?\n/)) {
    const line = rawLine.trimStart();
    if (!line.startsWith("data:")) continue;
    const payload = line.slice("data:".length).trim();
    if (!payload || payload === "[DONE]") continue;
    try {
      messages.push(JSON.parse(payload) as JsonRpcResponse);
    } catch {
      // Ignore non-JSON keepalive/comment frames.
    }
  }
  if (messages.length === 0) {
    throw new Error("MCP SSE response had no JSON data frame");
  }
  // Prefer the frame whose id matches the request; else the first that carries
  // a result/error; else the last.
  return (
    messages.find((m) => m.id === wantId && (m.result || m.error)) ??
    messages.find((m) => m.result || m.error) ??
    messages[messages.length - 1]
  );
}

export class McpClient {
  /** `../<app>/mcp` — relative to the shell mount (`/tangram/`). */
  readonly endpoint: string;
  private sessionId: string | null = null;
  private nextId = 1;
  private initialized = false;

  constructor(app: string) {
    this.endpoint = `../${app}/mcp`;
  }

  private headers(): HeadersInit {
    const h: Record<string, string> = {
      "Content-Type": "application/json",
      Accept: "application/json, text/event-stream",
    };
    if (this.sessionId) h["Mcp-Session-Id"] = this.sessionId;
    return h;
  }

  private async rpc(method: string, params: unknown): Promise<unknown> {
    const id = this.nextId++;
    const resp = await fetch(this.endpoint, {
      method: "POST",
      headers: this.headers(),
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
    });
    // Capture/refresh the session id from the initialize response header.
    const sid = resp.headers.get("Mcp-Session-Id");
    if (sid) this.sessionId = sid;
    const text = await resp.text();
    if (!resp.ok && !text) {
      throw new Error(`MCP ${method} failed: HTTP ${resp.status}`);
    }
    const msg = parseMcpBody(
      text,
      resp.headers.get("Content-Type") ?? "",
      id,
    );
    if (msg.error) {
      throw new Error(`MCP ${method} error ${msg.error.code}: ${msg.error.message}`);
    }
    return msg.result;
  }

  /** JSON-RPC `initialize`; captures the session id. Idempotent. */
  async initialize(): Promise<void> {
    if (this.initialized) return;
    await this.rpc("initialize", {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "tangram-shell-chat", version: "0.1.0" },
    });
    this.initialized = true;
  }

  /** `tools/list` → the app's tools. */
  async listTools(): Promise<McpTool[]> {
    const result = (await this.rpc("tools/list", {})) as { tools?: McpTool[] };
    return result?.tools ?? [];
  }

  /** `tools/call` → the result content. */
  async callTool(
    name: string,
    args: Record<string, unknown>,
  ): Promise<McpCallResult> {
    const result = (await this.rpc("tools/call", {
      name,
      arguments: args ?? {},
    })) as McpCallResult;
    return result ?? { content: [] };
  }
}

// Exposed for unit testing the body parser without a live server.
export const __test = { parseMcpBody };
