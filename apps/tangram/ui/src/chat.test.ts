// Unit tests for the pure bits behind the right-sidebar app chat: the
// MCP→OpenAI tool-schema conversion (llmChat) and the streamable-HTTP MCP
// response parser (mcpClient), which must handle BOTH a single JSON body and
// an SSE `data:`-framed stream (the live nutrition server answers with SSE).

import { describe, expect, it, vi } from "vitest";
import { mcpToolsToOpenAi, renderToolResult } from "./llmChat";
import { __test } from "./mcpClient";
import { resetSessionState, type PanelState } from "./chatPanel";

const { parseMcpBody } = __test;

describe("resetSessionState (New-chat / app-switch reset)", () => {
  it("flushes message state and bumps the epoch so a new session re-inits clean", () => {
    const state: PanelState = {
      app: "nutrition",
      label: "Nutrition",
      mcp: {} as unknown as PanelState["mcp"],
      tools: [{ type: "function", function: { name: "x", parameters: {} } }],
      history: [
        { role: "system", content: "sys" },
        { role: "user", content: "hi" },
        { role: "assistant", content: "hello" },
      ],
      noTools: false,
      sending: true,
      epoch: 3,
    };

    const epoch = resetSessionState(state);

    // Message state is cleared...
    expect(state.history).toEqual([]);
    expect(state.tools).toEqual([]);
    expect(state.mcp).toBeNull();
    expect(state.noTools).toBe(false);
    expect(state.sending).toBe(false);
    // ...the active app is preserved (New-chat stays in the same place)...
    expect(state.app).toBe("nutrition");
    expect(state.label).toBe("Nutrition");
    // ...and the epoch advanced so a stale in-flight turn is ignored and the
    // returned epoch is what the caller hands to a fresh connect() (re-init).
    expect(epoch).toBe(4);
    expect(state.epoch).toBe(4);
  });

  it("the returned epoch invalidates a turn captured under the old epoch", () => {
    const state: PanelState = {
      app: "notes",
      label: "Notes",
      mcp: null,
      tools: [],
      history: [{ role: "user", content: "q" }],
      noTools: true,
      sending: true,
      epoch: 0,
    };
    const capturedByInflightTurn = state.epoch;
    // A New-chat happens while a turn is mid-flight.
    resetSessionState(state);
    // The in-flight turn's epoch guard (epoch !== state.epoch) now trips.
    expect(capturedByInflightTurn).not.toBe(state.epoch);
    // And a fresh connect would be invoked with the new epoch.
    const reinit = vi.fn();
    reinit(state.app, state.label, state.epoch);
    expect(reinit).toHaveBeenCalledWith("notes", "Notes", 1);
  });
});

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
