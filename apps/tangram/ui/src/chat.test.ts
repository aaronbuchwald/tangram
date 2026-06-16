// Unit tests for the pure bits behind the right-sidebar app chat: the
// MCP→OpenAI tool-schema conversion (llmChat) and the streamable-HTTP MCP
// response parser (mcpClient), which must handle BOTH a single JSON body and
// an SSE `data:`-framed stream (the live nutrition server answers with SSE).

import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import {
  mcpToolsToOpenAi,
  renderToolResult,
  vaultSystemPrompt,
  type VaultNote,
} from "./llmChat";
import { McpClient, __test } from "./mcpClient";
import {
  contextKey,
  forgetContext,
  mcpTargetFor,
  mountChatPanel,
  newChat,
  resetSessionState,
  sameContext,
  setActiveContext,
  titleFor,
  __test as chatInternals,
  type ChatContext,
  type PanelState,
} from "./chatPanel";

const { parseMcpBody } = __test;

describe("resetSessionState (New-chat / context-switch reset)", () => {
  it("flushes message state and bumps the epoch so a new session re-inits clean", () => {
    const ctx: ChatContext = { kind: "app", app: "nutrition", label: "Nutrition" };
    const state: PanelState = {
      ctx,
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
    // ...the active context is preserved (New-chat stays in the same place)...
    expect(state.ctx).toBe(ctx);
    // ...and the epoch advanced so a stale in-flight turn is ignored and the
    // returned epoch is what the caller hands to a fresh connect() (re-init).
    expect(epoch).toBe(4);
    expect(state.epoch).toBe(4);
  });

  it("the returned epoch invalidates a turn captured under the old epoch", () => {
    const state: PanelState = {
      ctx: { kind: "vault", note: null },
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
    // And a fresh connect would be invoked with the new epoch + preserved ctx.
    const reinit = vi.fn();
    reinit(state.ctx, state.epoch);
    expect(reinit).toHaveBeenCalledWith({ kind: "vault", note: null }, 1);
  });
});

describe("mcpTargetFor (context → MCP endpoint selection)", () => {
  it("an app context targets its own app's MCP server", () => {
    expect(mcpTargetFor({ kind: "app", app: "nutrition", label: "Nutrition" })).toBe(
      "nutrition",
    );
    // McpClient turns the target into `../<target>/mcp`.
    expect(new McpClient("nutrition").endpoint).toBe("../nutrition/mcp");
  });

  it("a vault context targets the shell's own `tangram` MCP (full vault toolset)", () => {
    const note: VaultNote = { id: "f1", path: "a/b.md", body: "hi" };
    expect(mcpTargetFor({ kind: "vault", note })).toBe("tangram");
    expect(mcpTargetFor({ kind: "vault", note: null })).toBe("tangram");
    expect(new McpClient("tangram").endpoint).toBe("../tangram/mcp");
  });
});

describe("sameContext (when to reset vs keep the conversation)", () => {
  const appA: ChatContext = { kind: "app", app: "notes", label: "Notes" };
  const appB: ChatContext = { kind: "app", app: "nutrition", label: "Nutrition" };
  const note1: ChatContext = {
    kind: "vault",
    note: { id: "f1", path: "a.md", body: "x" },
  };
  const note1Edited: ChatContext = {
    kind: "vault",
    note: { id: "f1", path: "a.md", body: "EDITED BODY" },
  };
  const note2: ChatContext = {
    kind: "vault",
    note: { id: "f2", path: "b.md", body: "y" },
  };
  const vaultNone: ChatContext = { kind: "vault", note: null };

  it("same app keeps the conversation", () => {
    expect(sameContext(appA, { ...appA })).toBe(true);
  });
  it("different app resets", () => {
    expect(sameContext(appA, appB)).toBe(false);
  });
  it("app↔vault always resets", () => {
    expect(sameContext(appA, note1)).toBe(false);
    expect(sameContext(note1, appA)).toBe(false);
  });
  it("same note id keeps the conversation even if the body changed (no reset on edit)", () => {
    expect(sameContext(note1, note1Edited)).toBe(true);
  });
  it("a different note id resets (refresh-on-switch)", () => {
    expect(sameContext(note1, note2)).toBe(false);
  });
  it("the general vault copilot (no note) matches itself but not a seeded note", () => {
    expect(sameContext(vaultNone, { kind: "vault", note: null })).toBe(true);
    expect(sameContext(vaultNone, note1)).toBe(false);
  });
  it("null (home/agents) matches only null", () => {
    expect(sameContext(null, null)).toBe(true);
    expect(sameContext(null, appA)).toBe(false);
    expect(sameContext(note1, null)).toBe(false);
  });
});

describe("contextKey (per-context persistence key, #35)", () => {
  it("keys an app by its name and matches sameContext's grouping", () => {
    const a: ChatContext = { kind: "app", app: "notes", label: "Notes" };
    const b: ChatContext = { kind: "app", app: "nutrition", label: "Nutrition" };
    expect(contextKey(a)).toBe("app:notes");
    // Same key iff sameContext considers them the same session.
    expect(contextKey(a) === contextKey({ ...a })).toBe(sameContext(a, { ...a }));
    expect(contextKey(a) === contextKey(b)).toBe(sameContext(a, b));
  });

  it("keys a vault context by note id; the general copilot gets a stable empty key", () => {
    const note: ChatContext = {
      kind: "vault",
      note: { id: "f1", path: "a.md", body: "x" },
    };
    const noteEdited: ChatContext = {
      kind: "vault",
      note: { id: "f1", path: "a.md", body: "EDITED" },
    };
    expect(contextKey(note)).toBe("vault:f1");
    // The body changing does NOT change the key (no reset on edit).
    expect(contextKey(note)).toBe(contextKey(noteEdited));
    expect(contextKey({ kind: "vault", note: null })).toBe("vault:");
  });

  it("the null/home/agents context has no key (nothing to persist)", () => {
    expect(contextKey(null)).toBeNull();
  });
});

describe("conversation persistence across context switches (#35)", () => {
  const { conversations, state } = chatInternals;
  const appA: ChatContext = { kind: "app", app: "notes", label: "Notes" };
  const appB: ChatContext = { kind: "app", app: "nutrition", label: "Nutrition" };

  // Mount the panel once (it owns the chat-log DOM the persistence reads/writes)
  // and reset all shared state before each test for isolation.
  beforeAll(() => {
    document.body.innerHTML = '<div id="slot"></div>';
    mountChatPanel(document.getElementById("slot")!);
    // The async, fire-and-forget MCP connect can't reach a server under jsdom
    // and degrades to plain chat (logged via console.warn). The persistence we
    // exercise here is synchronous, so silence that expected background noise.
    vi.spyOn(console, "warn").mockImplementation(() => {});
  });
  beforeEach(() => {
    conversations.clear();
    state.ctx = null;
    state.history = [];
    state.mcp = null;
    state.tools = [];
    state.noTools = false;
    state.sending = false;
    const log = document.getElementById("chat-log")!;
    log.replaceChildren();
  });

  // Simulate "the user had a conversation in the active context": a seeded
  // system prompt + a user/assistant exchange rendered into the log.
  function seedConversation(user: string, assistant: string): void {
    state.history.push(
      { role: "system", content: "sys" },
      { role: "user", content: user },
      { role: "assistant", content: assistant },
    );
    const log = document.getElementById("chat-log")!;
    log.innerHTML =
      `<div class="chat-msg chat-user">${user}</div>` +
      `<div class="chat-msg chat-assistant">${assistant}</div>`;
  }

  it("switching away and back PRESERVES the message history and the rendered log", () => {
    setActiveContext(appA);
    seedConversation("hello A", "hi from A");

    // Switch to B — A's conversation must be saved, not wiped.
    setActiveContext(appB);
    expect(conversations.get("app:notes")).toBeDefined();
    expect(conversations.get("app:notes")!.history).toEqual([
      { role: "system", content: "sys" },
      { role: "user", content: "hello A" },
      { role: "assistant", content: "hi from A" },
    ]);

    // Switch back to A — history restored, log re-rendered (the bug: it used to
    // be wiped on switch).
    setActiveContext(appA);
    expect(state.history).toEqual([
      { role: "system", content: "sys" },
      { role: "user", content: "hello A" },
      { role: "assistant", content: "hi from A" },
    ]);
    const log = document.getElementById("chat-log")!;
    expect(log.textContent).toContain("hello A");
    expect(log.textContent).toContain("hi from A");
  });

  it("an untouched session (system prompt only) is not persisted", () => {
    setActiveContext(appA);
    state.history.push({ role: "system", content: "sys" });
    setActiveContext(appB);
    expect(conversations.has("app:notes")).toBe(false);
  });

  it("tab close WIPES that context's stored conversation", () => {
    setActiveContext(appA);
    seedConversation("hello A", "hi from A");
    setActiveContext(appB); // persist A
    expect(conversations.has("app:notes")).toBe(true);

    // Closing A's tab forgets it; switching back finds nothing and starts fresh.
    forgetContext(appA);
    expect(conversations.has("app:notes")).toBe(false);
  });

  it("closing the ACTIVE tab does not re-persist it on the follow-up switch", () => {
    setActiveContext(appA);
    seedConversation("hello A", "hi from A");

    // Close the active tab, then the close→re-render drives a switch to B.
    forgetContext(appA);
    setActiveContext(appB);
    expect(conversations.has("app:notes")).toBe(false);

    // Reopening A starts a clean session (no restored history beyond the fresh
    // connect, which is async/offline here so history is left empty).
    setActiveContext(appA);
    const log = document.getElementById("chat-log")!;
    expect(log.textContent).not.toContain("hello A");
  });

  it("New-chat wipes the CURRENT context's stored conversation and clears the log", () => {
    setActiveContext(appA);
    seedConversation("hello A", "hi from A");
    setActiveContext(appB);
    setActiveContext(appA); // restored
    expect(conversations.has("app:notes")).toBe(true);

    newChat();
    // New-chat flushes both the live log and the persisted entry for this ctx.
    expect(conversations.has("app:notes")).toBe(false);
    const log = document.getElementById("chat-log")!;
    expect(log.textContent).not.toContain("hello A");
  });
});

describe("titleFor", () => {
  it("app context titles with the app label", () => {
    expect(titleFor({ kind: "app", app: "notes", label: "Notes" })).toBe(
      "Notes · chat",
    );
  });
  it("a seeded note titles with the note leaf name; bare vault is the general copilot", () => {
    expect(
      titleFor({ kind: "vault", note: { id: "f", path: "proj/Plan.md", body: "" } }),
    ).toBe("Plan · copilot");
    expect(titleFor({ kind: "vault", note: null })).toBe("Vault copilot");
  });
});

describe("vaultSystemPrompt (vault context system-prompt assembly)", () => {
  const note: VaultNote = {
    id: "note-42",
    path: "projects/Q3.md",
    body: "# Q3 plan\nShip the thing.",
  };

  it("states it can read AND modify the vault when tools are present", () => {
    const p = vaultSystemPrompt(note, true);
    expect(p).toContain("copilot for this Obsidian-style vault");
    expect(p).toMatch(/READ and MODIFY/);
    expect(p).toContain("read, search, create, edit, rename and delete");
  });

  it("seeds the open note's path, id and full body so 'this note' resolves", () => {
    const p = vaultSystemPrompt(note, true);
    expect(p).toContain("`projects/Q3.md`");
    expect(p).toContain("`note-42`");
    expect(p).toContain('they mean that one');
    expect(p).toContain("Current note body:\n# Q3 plan\nShip the thing.");
  });

  it("with no open note, omits the note-body section (general vault copilot)", () => {
    const p = vaultSystemPrompt(null, true);
    expect(p).not.toContain("Current note body");
    expect(p).not.toContain("currently viewing");
  });

  it("degrades to a plain assistant when no vault tools are available", () => {
    const p = vaultSystemPrompt(note, false);
    expect(p).toContain("No vault tools are available");
    // Still seeds the open note so the assistant knows what is on screen.
    expect(p).toContain("`projects/Q3.md`");
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
