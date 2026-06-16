// The right-sidebar app chat panel: a ChatGPT/Perplexity-style dock that, when
// an app tab is active, talks to DeepSeek and is auto-connected to that app's
// MCP server so the user can ask the app to do things in natural language and
// the LLM calls the app's MCP tools.
//
// Self-contained: it owns its own DOM (built lazily into a host slot in
// main.ts), state, and the per-app MCP lifecycle. main.ts only calls
// `setActiveApp(app, label)` whenever the active tab changes — everything else
// lives here and in mcpClient.ts / llmChat.ts. Kept in NEW files so the rebase
// against the in-flight invocation-redesign branch stays trivial.

import { McpClient } from "./mcpClient";
import {
  mcpToolsToOpenAi,
  runChatTurn,
  systemPromptFor,
  type ChatMessage,
  type OpenAiTool,
  type ToolStep,
} from "./llmChat";

const OPEN_KEY = "chat-sidebar-open";
const WIDTH_KEY = "chat-sidebar-width";

export interface PanelState {
  app: string | null;
  label: string;
  mcp: McpClient | null;
  tools: OpenAiTool[];
  history: ChatMessage[];
  noTools: boolean;
  sending: boolean;
  // Bumped on each app switch; an in-flight init/turn checks it before
  // touching the DOM so a stale async result can't clobber a newer session.
  epoch: number;
}

let open = localStorage.getItem(OPEN_KEY) !== "false";
let width = parseInt(localStorage.getItem(WIDTH_KEY) ?? "340", 10);

const state: PanelState = {
  app: null,
  label: "",
  mcp: null,
  tools: [],
  history: [],
  noTools: false,
  sending: false,
  epoch: 0,
};

// DOM handles, populated by mount().
let aside: HTMLElement | null = null;
let logEl: HTMLElement | null = null;
let inputEl: HTMLTextAreaElement | null = null;
let sendBtn: HTMLButtonElement | null = null;
let titleEl: HTMLElement | null = null;
let newBtn: HTMLButtonElement | null = null;
let toggleBtn: HTMLButtonElement | null = null;

/**
 * Reset the conversation state in place: drop the MCP client, tools and
 * message history, clear the sending flag, and bump the epoch so any in-flight
 * init/turn for the old session is ignored before it touches the DOM. Returns
 * the new epoch (the caller hands it to `connect`). Pure w.r.t. the DOM, so the
 * shared reset path used by both app-switch and New-chat is unit-testable.
 */
export function resetSessionState(s: PanelState): number {
  const epoch = ++s.epoch;
  s.mcp = null;
  s.tools = [];
  s.history = [];
  s.noTools = false;
  s.sending = false;
  return epoch;
}

function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

// Lightweight assistant-text rendering: escape, then turn `**bold**`, `` `code` ``
// and blank-line paragraphs / single newlines into minimal HTML. No markdown
// dependency exists in the shell, so this stays deliberately small and safe.
function renderText(s: string): string {
  let html = esc(s);
  html = html.replace(/`([^`]+)`/g, "<code>$1</code>");
  html = html.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  html = html.replace(/\n/g, "<br>");
  return html;
}

/** Build the panel DOM into `slot` (idempotent). Call once at boot. */
export function mountChatPanel(slot: HTMLElement): void {
  if (aside) return;
  slot.innerHTML = `
    <aside class="chat-sidebar" id="chat-sidebar">
      <div class="chat-resizer" id="chat-resizer"></div>
      <div class="chat-head">
        <span class="chat-title" id="chat-title">Chat</span>
        <button class="chat-new" id="chat-new" title="New chat" aria-label="New chat">＋</button>
        <button class="chat-close" id="chat-close" title="Hide chat" aria-label="Hide chat">×</button>
      </div>
      <div class="chat-log" id="chat-log"></div>
      <div class="chat-compose">
        <textarea class="chat-input" id="chat-input" rows="2"
          placeholder="Ask this app to do something…" aria-label="Chat message"></textarea>
        <button class="chat-send" id="chat-send" title="Send">Send</button>
      </div>
    </aside>
    <button class="chat-fab" id="chat-fab" title="Open chat" aria-label="Open chat">💬</button>
  `;
  aside = slot.querySelector("#chat-sidebar");
  logEl = slot.querySelector("#chat-log");
  inputEl = slot.querySelector("#chat-input");
  sendBtn = slot.querySelector("#chat-send");
  titleEl = slot.querySelector("#chat-title");
  newBtn = slot.querySelector("#chat-new");
  toggleBtn = slot.querySelector("#chat-fab");

  newBtn!.addEventListener("click", () => newChat());
  slot.querySelector("#chat-close")!.addEventListener("click", () => setOpen(false));
  toggleBtn!.addEventListener("click", () => setOpen(true));
  sendBtn!.addEventListener("click", () => void send());
  inputEl!.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  });

  // Drag-to-resize (mirrors the left sidebar's resizer pattern).
  slot.querySelector("#chat-resizer")!.addEventListener("mousedown", (ev) => {
    const startEvt = ev as MouseEvent;
    startEvt.preventDefault();
    const startX = startEvt.clientX;
    const startW = width;
    const onMove = (e: MouseEvent) => {
      width = Math.max(280, Math.min(560, startW - (e.clientX - startX)));
      if (aside) aside.style.width = `${width}px`;
      localStorage.setItem(WIDTH_KEY, String(width));
    };
    const onUp = () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
  });

  applyLayout();
}

function setOpen(v: boolean): void {
  open = v;
  localStorage.setItem(OPEN_KEY, String(v));
  applyLayout();
}

// The panel is visible only when open AND an app tab is active. On a note/home
// tab there is no app to chat with, so it hides and the FAB hides too.
function applyLayout(): void {
  if (!aside || !toggleBtn) return;
  const hasApp = state.app !== null;
  const showAside = hasApp && open;
  aside.style.display = showAside ? "flex" : "none";
  aside.style.width = `${width}px`;
  // The FAB lets the user reopen the panel for an app tab when collapsed.
  toggleBtn.style.display = hasApp && !open ? "block" : "none";
}

/**
 * Called by main.ts whenever the active tab changes. `app` is the app name for
 * an app tab, or null for a note/home/agents tab. A fresh MCP session + chat is
 * started per app (switching apps resets the conversation).
 */
export function setActiveApp(app: string | null, label: string): void {
  if (app === state.app) {
    // Same app still active — keep the conversation; just refresh layout.
    applyLayout();
    return;
  }
  state.app = app;
  state.label = label;
  const epoch = resetSessionState(state);
  if (titleEl) titleEl.textContent = app ? `${label} · chat` : "Chat";
  if (logEl) logEl.replaceChildren();
  applyLayout();
  if (app) void connect(app, label, epoch);
}

/**
 * New-chat: flush the current conversation and start a fresh one in place for
 * the currently-active app. Resets message state AND re-initializes the MCP
 * session (the same reset path as switching apps), without navigating or
 * closing the panel. No-op when no app tab is active.
 */
export function newChat(): void {
  if (state.app === null) return;
  const epoch = resetSessionState(state);
  if (logEl) logEl.replaceChildren();
  if (inputEl) inputEl.value = "";
  if (sendBtn) sendBtn.disabled = false;
  void connect(state.app, state.label, epoch);
}

// Initialize the MCP client + tools for an app, degrading to plain chat if the
// handshake fails (the chat still works; we show a small "no tools" note).
async function connect(app: string, label: string, epoch: number): Promise<void> {
  appendStatus("Connecting to the app's tools…");
  let toolsLabel = "";
  try {
    const mcp = new McpClient(app);
    await mcp.initialize();
    const tools = await mcp.listTools();
    if (epoch !== state.epoch) return; // a newer app switch superseded us
    state.mcp = mcp;
    state.tools = mcpToolsToOpenAi(tools);
    state.noTools = tools.length === 0;
    toolsLabel = state.noTools
      ? "No tools available — plain chat."
      : `${tools.length} tool${tools.length === 1 ? "" : "s"} ready.`;
  } catch (e) {
    if (epoch !== state.epoch) return;
    state.mcp = null;
    state.tools = [];
    state.noTools = true;
    console.warn("MCP connect failed for", app, e);
    toolsLabel = "No tools available — plain chat.";
  }
  // Seed the system prompt scoped to this app.
  state.history = [
    {
      role: "system",
      content: systemPromptFor(label, state.tools.length > 0),
    },
  ];
  if (epoch !== state.epoch) return;
  clearLog();
  appendStatus(toolsLabel);
}

function clearLog(): void {
  logEl?.replaceChildren();
}

function appendStatus(text: string): HTMLElement {
  const el = document.createElement("div");
  el.className = "chat-status";
  el.textContent = text;
  logEl?.appendChild(el);
  scrollToEnd();
  return el;
}

function appendBubble(role: "user" | "assistant", html: string): HTMLElement {
  const el = document.createElement("div");
  el.className = `chat-msg chat-${role}`;
  el.innerHTML = html;
  logEl?.appendChild(el);
  scrollToEnd();
  return el;
}

function appendToolStep(step: ToolStep): void {
  const el = document.createElement("div");
  el.className = "chat-tool" + (step.isError ? " chat-tool-error" : "");
  el.textContent = `🔧 ${step.name}`;
  el.title = step.result.slice(0, 600);
  logEl?.appendChild(el);
  scrollToEnd();
}

function scrollToEnd(): void {
  if (logEl) logEl.scrollTop = logEl.scrollHeight;
}

async function send(): Promise<void> {
  if (!inputEl || state.sending || state.app === null) return;
  const text = inputEl.value.trim();
  if (!text) return;
  const epoch = state.epoch;

  inputEl.value = "";
  state.sending = true;
  if (sendBtn) sendBtn.disabled = true;
  appendBubble("user", renderText(text));
  state.history.push({ role: "user", content: text });

  const thinking = appendStatus("Thinking…");

  try {
    const { reply } = await runChatTurn(
      state.history,
      state.mcp,
      state.tools,
      (step) => {
        if (epoch === state.epoch) appendToolStep(step);
      },
    );
    if (epoch !== state.epoch) return; // app switched mid-turn
    thinking.remove();
    appendBubble("assistant", renderText(reply || "(no reply)"));
  } catch (e) {
    if (epoch !== state.epoch) return;
    thinking.remove();
    const msg = e instanceof Error ? e.message : String(e);
    appendBubble("assistant", `<span class="chat-err">Error: ${esc(msg)}</span>`);
  } finally {
    if (epoch === state.epoch) {
      state.sending = false;
      if (sendBtn) sendBtn.disabled = false;
      inputEl?.focus();
    }
  }
}
