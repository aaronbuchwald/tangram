// The LLM tool-calling loop: DeepSeek (OpenAI-compatible) + the active app's
// MCP tools. The shell is served at `/tangram/`, the host injects the DeepSeek
// key at its egress boundary (ADR-0005), so we POST the RELATIVE proxy path
// with no key. The request shape mirrors the agent `run_definition` call in
// apps/tangram/src/lib.rs and the guided-learning tutor.

import type { McpClient, McpTool, McpCallResult } from "./mcpClient";

const LLM_URL = "../llm/deepseek/v1/chat/completions";
const MODEL = "deepseek-chat";
const MAX_ITERATIONS = 6;

// OpenAI-style chat message (the subset we use).
export interface ChatMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string | null;
  // assistant turn that requests tools:
  tool_calls?: ToolCall[];
  // a `tool` result message answers a specific call:
  tool_call_id?: string;
  name?: string;
}

export interface ToolCall {
  id: string;
  type: "function";
  function: { name: string; arguments: string };
}

// An OpenAI `tools` entry (function-calling schema).
export interface OpenAiTool {
  type: "function";
  function: {
    name: string;
    description?: string;
    parameters: Record<string, unknown>;
  };
}

/**
 * Convert MCP tools → OpenAI function-tool schemas. The MCP `inputSchema` IS a
 * JSON Schema object, so it maps straight onto `function.parameters`. A tool
 * with no schema gets an empty object schema (no args).
 */
export function mcpToolsToOpenAi(tools: McpTool[]): OpenAiTool[] {
  return tools.map((t) => ({
    type: "function",
    function: {
      name: t.name,
      description: t.description,
      parameters:
        t.inputSchema && typeof t.inputSchema === "object"
          ? t.inputSchema
          : { type: "object", properties: {} },
    },
  }));
}

/** Flatten an MCP call result's content into a string for the `tool` message. */
export function renderToolResult(result: McpCallResult): string {
  const text = (result.content ?? [])
    .map((c) => (c.type === "text" ? (c.text ?? "") : JSON.stringify(c)))
    .join("\n")
    .trim();
  const prefix = result.isError ? "[tool error] " : "";
  return prefix + (text || "(no content)");
}

export interface ToolStep {
  name: string;
  args: Record<string, unknown>;
  result: string;
  isError: boolean;
}

export interface ChatRunResult {
  reply: string;
  steps: ToolStep[];
}

// Raw shape of DeepSeek's `choices[0].message`.
interface ApiMessage {
  role: "assistant";
  content: string | null;
  tool_calls?: ToolCall[];
}

async function postChat(
  messages: ChatMessage[],
  tools: OpenAiTool[],
): Promise<ApiMessage> {
  const body: Record<string, unknown> = { model: MODEL, messages };
  if (tools.length > 0) {
    body.tools = tools;
    body.tool_choice = "auto";
  }
  const resp = await fetch(LLM_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const payload = await resp.json();
  if (!resp.ok) {
    throw new Error(
      `DeepSeek request failed (${resp.status}): ${JSON.stringify(payload)}`,
    );
  }
  const message = payload?.choices?.[0]?.message;
  if (!message) {
    throw new Error(
      `DeepSeek response had no message: ${JSON.stringify(payload)}`,
    );
  }
  return message as ApiMessage;
}

/** Build the system prompt scoping the assistant to the active app. */
export function systemPromptFor(appLabel: string, hasTools: boolean): string {
  if (hasTools) {
    return (
      `You are an assistant embedded in the ${appLabel} app inside Tangram. ` +
      `Use the app's tools to answer questions and to take the actions the ` +
      `user asks for. Prefer calling a tool over guessing. When you have the ` +
      `information, answer concisely.`
    );
  }
  return (
    `You are an assistant embedded in the ${appLabel} app inside Tangram. ` +
    `No app tools are available right now, so answer as a helpful general ` +
    `assistant.`
  );
}

/**
 * Run one user turn through the tool-calling loop. `history` is the running
 * conversation (system + prior user/assistant turns). This mutates `history`
 * in place to append the assistant turn(s) and tool results, and returns the
 * final assistant reply plus the tool steps taken (for compact UI rendering).
 *
 * `onStep` (optional) fires as each tool call completes so the UI can stream
 * the "🔧 called <tool>" lines live.
 */
export async function runChatTurn(
  history: ChatMessage[],
  mcp: McpClient | null,
  tools: OpenAiTool[],
  onStep?: (step: ToolStep) => void,
): Promise<ChatRunResult> {
  const steps: ToolStep[] = [];

  for (let i = 0; i < MAX_ITERATIONS; i++) {
    const message = await postChat(history, tools);

    // No tool calls → normal answer; we're done.
    if (!message.tool_calls || message.tool_calls.length === 0) {
      const reply = message.content ?? "";
      history.push({ role: "assistant", content: reply });
      return { reply, steps };
    }

    // The assistant asked for tools. Record the assistant turn verbatim
    // (content may be null), then execute each call and append `tool` results.
    history.push({
      role: "assistant",
      content: message.content ?? null,
      tool_calls: message.tool_calls,
    });

    for (const call of message.tool_calls) {
      let args: Record<string, unknown> = {};
      try {
        args = call.function.arguments
          ? JSON.parse(call.function.arguments)
          : {};
      } catch {
        args = {};
      }

      let resultText: string;
      let isError = false;
      if (!mcp) {
        resultText = "[tool error] no MCP client available";
        isError = true;
      } else {
        try {
          const r = await mcp.callTool(call.function.name, args);
          resultText = renderToolResult(r);
          isError = !!r.isError;
        } catch (e) {
          resultText = `[tool error] ${e instanceof Error ? e.message : String(e)}`;
          isError = true;
        }
      }

      const step: ToolStep = {
        name: call.function.name,
        args,
        result: resultText,
        isError,
      };
      steps.push(step);
      onStep?.(step);

      history.push({
        role: "tool",
        tool_call_id: call.id,
        name: call.function.name,
        content: resultText,
      });
    }
  }

  // Hit the iteration cap without a final answer.
  const reply =
    "(stopped after the maximum number of tool steps without a final answer)";
  history.push({ role: "assistant", content: reply });
  return { reply, steps };
}
