// Unit tests for the pure bits behind the right-sidebar app chat: the
// MCP→OpenAI tool-schema conversion (llmChat) and the streamable-HTTP MCP
// response parser (mcpClient), which must handle BOTH a single JSON body and
// an SSE `data:`-framed stream (the live nutrition server answers with SSE).

import { describe, expect, it } from "vitest";
import { mcpToolsToOpenAi, renderToolResult } from "./llmChat";
import { __test } from "./mcpClient";

const { parseMcpBody } = __test;

describe("mcpToolsToOpenAi", () => {
  it("maps an MCP tool's inputSchema straight onto function.parameters", () => {
    const schema = {
      type: "object",
      properties: { id: { type: "string" } },
      required: ["id"],
    };
    const out = mcpToolsToOpenAi([
      { name: "delete_meal", description: "Delete a meal", inputSchema: schema },
    ]);
    expect(out).toEqual([
      {
        type: "function",
        function: {
          name: "delete_meal",
          description: "Delete a meal",
          parameters: schema,
        },
      },
    ]);
  });

  it("supplies an empty object schema when a tool has no inputSchema", () => {
    const out = mcpToolsToOpenAi([{ name: "list_meals" }]);
    expect(out[0].function.parameters).toEqual({
      type: "object",
      properties: {},
    });
  });
});

describe("renderToolResult", () => {
  it("joins text content blocks", () => {
    expect(
      renderToolResult({ content: [{ type: "text", text: "hello" }] }),
    ).toBe("hello");
  });

  it("prefixes errors and falls back when empty", () => {
    expect(renderToolResult({ content: [], isError: true })).toBe(
      "[tool error] (no content)",
    );
  });
});

describe("parseMcpBody (MCP streamable-HTTP)", () => {
  it("parses a plain JSON body", () => {
    const body = JSON.stringify({
      jsonrpc: "2.0",
      id: 2,
      result: { tools: [] },
    });
    const msg = parseMcpBody(body, "application/json", 2);
    expect(msg.result).toEqual({ tools: [] });
  });

  it("parses an SSE data: frame (the live server's shape)", () => {
    // Exactly the framing curl pinned against the nutrition app.
    const body =
      'data: {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}\n\n';
    const msg = parseMcpBody(body, "text/event-stream", 1);
    expect(msg.result).toEqual({ protocolVersion: "2025-06-18" });
  });

  it("picks the response frame matching the request id across multiple frames", () => {
    const body = [
      'data: {"jsonrpc":"2.0","method":"notifications/message","params":{}}',
      "",
      'data: {"jsonrpc":"2.0","id":3,"result":{"isError":false}}',
      "",
    ].join("\n");
    const msg = parseMcpBody(body, "text/event-stream", 3);
    expect(msg.id).toBe(3);
    expect((msg.result as { isError: boolean }).isError).toBe(false);
  });

  it("sniffs SSE framing even if the content-type header is absent", () => {
    const body = 'data: {"jsonrpc":"2.0","id":5,"result":{"ok":true}}\n';
    const msg = parseMcpBody(body, "", 5);
    expect((msg.result as { ok: boolean }).ok).toBe(true);
  });

  it("ignores keepalive/comment frames and [DONE]", () => {
    const body = [
      ": keepalive",
      "data: [DONE]",
      'data: {"jsonrpc":"2.0","id":7,"result":{"v":1}}',
    ].join("\n");
    const msg = parseMcpBody(body, "text/event-stream", 7);
    expect((msg.result as { v: number }).v).toBe(1);
  });
});
