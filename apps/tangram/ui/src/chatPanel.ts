// The right-sidebar copilot chat panel: a ChatGPT/Perplexity-style dock wired
// to an MCP server so the user can ask in natural language and the LLM calls
// tools. It serves TWO contexts:
//   - an APP tab → talks to that app's own MCP server (`../<app>/mcp`), the
//     original behavior, unchanged.
//   - a NOTE / vault tab → talks to the shell's OWN MCP server
//     (`../tangram/mcp`), which exposes the full vault toolset (list/read/
//     create/write/rename/delete files + folders, agents, invocations …), and
//     seeds the open note (path + id + body) into the system prompt so
//     "summarize/edit this note" works.
//
// Self-contained: it owns its own DOM (built lazily into a host slot in
// main.ts), state, and the per-context MCP lifecycle. main.ts only calls
// `setActiveContext(ctx)` whenever the active tab changes — everything else
// lives here and in mcpClient.ts / llmChat.ts. Kept in NEW files so the rebase
// against the in-flight invocation-redesign branch stays trivial.

import { McpClient } from "./mcpClient";
import {
  mcpToolsToOpenAi,
  runChatTurn,
  systemPromptFor,
  vaultSystemPrompt,
  type ChatMessage,
  type OpenAiTool,
  type ToolStep,
  type VaultNote,
} from "./llmChat";

const OPEN_KEY = "chat-sidebar-open";
const WIDTH_KEY = "chat-sidebar-width";

// The shell's own app name; its MCP server (`../tangram/mcp`) exposes the full
// vault toolset. The vault copilot targets this rather than an embedded app.
const VAULT_APP = "tangram";

// The active chat context: either an embedded app (talk to its own MCP server)
// or the vault viewer (talk to the shell's own MCP — full vault toolset, with
// the open note seeded as context, if any).
export type ChatContext =
  | { kind: "app"; app: string; label: string }
  | { kind: "vault"; note: VaultNote | null };

/**
 * Pick the MCP app-name the chat should connect to for a context. App contexts
 * target their own app; the vault copilot targets the shell's own `tangram`
 * MCP server (the full vault toolset). Pure — the unit of the endpoint
 * selection. `McpClient` turns this into `../<name>/mcp`.
 */
export function mcpTargetFor(ctx: ChatContext): string {
  return ctx.kind === "app" ? ctx.app : VAULT_APP;
}

/**
 * The stable key under which a context's conversation is persisted (#35). Two
 * contexts share a key exactly when `sameContext` considers them the same
 * session: an app by its name, a vault context by its open note id (the general
 * copilot — no note — gets a stable empty-id key). Returns null for the
 * null/home/agents context (no conversation to persist). Pure — testable.
 */
export function contextKey(ctx: ChatContext | null): string | null {
  if (ctx === null) return null;
  if (ctx.kind === "app") return `app:${ctx.app}`;
  return `vault:${ctx.note?.id ?? ""}`;
}

/** The chat-head title for a context. */
export function titleFor(ctx: ChatContext): string {
  if (ctx.kind === "app") return `${ctx.label} · chat`;
  return ctx.note ? `${noteName(ctx.note.path)} · copilot` : "Vault copilot";
}

/** Display the leaf file name of a vault path (drop folders + `.md`). */
function noteName(path: string): string {
  const leaf = path.split("/").pop() ?? path;
  return leaf.endsWith(".md") ? leaf.slice(0, -3) : leaf;
}

export interface PanelState {
  ctx: ChatContext | null;
  mcp: McpClient | null;
  tools: OpenAiTool[];
  history: ChatMessage[];
  noTools: boolean;
  sending: boolean;
  // Bumped on each context switch; an in-flight init/turn checks it before
  // touching the DOM so a stale async result can't clobber a newer session.
  epoch: number;
}

/**
 * A persisted conversation for one context (#35). We keep both the message
 * `history` (drives the next LLM turn) and the rendered `logHtml` (the visible
 * transcript, including tool steps and status lines, which is NOT reconstructable
 * from `history` alone). `noTools` rides along so a restored session shows the
 * right degraded-vs-tools state. The live MCP session is intentionally NOT
 * stored — it can expire, so it is re-initialized on restore.
 */
export interface StoredConversation {
  history: ChatMessage[];
  logHtml: string;
  noTools: boolean;
}

let open = localStorage.getItem(OPEN_KEY) !== "false";
let width = parseInt(localStorage.getItem(WIDTH_KEY) ?? "340", 10);

const state: PanelState = {
  ctx: null,
  mcp: null,
  tools: [],
  history: [],
  noTools: false,
  sending: false,
  epoch: 0,
};

// Per-context conversation store, keyed by `contextKey` (#35). Switching tabs/
// contexts saves the outgoing context here and restores the incoming one, so a
// conversation survives a context switch and is wiped only on tab close (or the
// New-chat ＋). In-memory only — survives switches within the session, not a
// full page reload (per the issue, reload persistence is out of scope).
const conversations = new Map<string, StoredConversation>();

/** True once a conversation carries at least one real (non-system) message —
 *  i.e. the user has actually started talking. An untouched session (just the
 *  seeded system prompt, or empty) is not worth persisting. */
function hasConversation(history: ChatMessage[]): boolean {
  return history.some((m) => m.role !== "system");
}

/** Snapshot the CURRENT context's conversation into the store (#35). No-op when
 *  there is no keyed context, the panel DOM isn't mounted, or nothing has been
 *  said yet (an untouched session needn't be persisted, and skipping it lets a
 *  just-wiped active context stay wiped on the close→switch sequence). Exported
 *  for tests. */
export function saveCurrentConversation(): void {
  const key = contextKey(state.ctx);
  if (key === null) return;
  if (!hasConversation(state.history)) return;
  conversations.set(key, {
    // Clone the history so later in-place mutations don't bleed into the store.
    history: state.history.map((m) => ({ ...m })),
    logHtml: logEl ? logEl.innerHTML : "",
    noTools: state.noTools,
  });
}

/** Wipe the stored conversation for a context key (#35). Called on tab close and
 *  by New-chat. No-op for null/unknown keys. Exported for wiring + tests. */
export function wipeConversation(key: string | null): void {
  if (key === null) return;
  conversations.delete(key);
}

/** Test-only access to the per-context conversation store + live panel state.
 *  Exposed so the persist/restore/wipe lifecycle (#35) is unit-testable without
 *  a live MCP server (the async `connect` is fire-and-forget and degrades to
 *  plain chat when it can't reach the network — the persistence is synchronous). */
export const __test = {
  conversations,
  saveCurrentConversation,
  state,
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
 * init/turn for the old session is ignored before it touches the DOM. The
 * active context (`ctx`) is preserved — New-chat stays in the same place.
 * Returns the new epoch (the caller hands it to `connect`). Pure w.r.t. the
 * DOM, so the shared reset path used by both context-switch and New-chat is
 * unit-testable. This is the SAME reset path used on every context switch
 * (app↔app, note↔note, app↔vault) so a stale tool/session can't leak across.
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
          placeholder="Ask the copilot to do something…" aria-label="Chat message"></textarea>
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

// The panel is visible only when open AND a chat context is active — an app
// tab OR a note/vault tab (the vault copilot). On a home/agents tab there is no
// context, so it hides and the FAB hides too.
function applyLayout(): void {
  if (!aside || !toggleBtn) return;
  const hasCtx = state.ctx !== null;
  const showAside = hasCtx && open;
  aside.style.display = showAside ? "flex" : "none";
  aside.style.width = `${width}px`;
  // The FAB lets the user reopen the panel for a chat context when collapsed.
  toggleBtn.style.display = hasCtx && !open ? "block" : "none";
}

/**
 * True if two contexts address the same session — same app, or the same open
 * vault note (by id; null/null is the same general vault copilot). When they
 * differ the chat resets + reconnects to the right MCP target; when they match
 * the conversation is kept. Pure (no DOM), so the switch semantics are testable.
 */
export function sameContext(a: ChatContext | null, b: ChatContext | null): boolean {
  if (a === null || b === null) return a === b;
  if (a.kind !== b.kind) return false;
  if (a.kind === "app" && b.kind === "app") return a.app === b.app;
  if (a.kind === "vault" && b.kind === "vault") {
    return (a.note?.id ?? null) === (b.note?.id ?? null);
  }
  return false;
}

/**
 * Called by main.ts whenever the active tab changes. `ctx` is the app context
 * for an app tab, the vault context (with the open note, if any) for a note
 * tab, or null for a home/agents tab. A fresh MCP session + chat is started per
 * context: switching apps, notes, or app↔vault resets the conversation and
 * reconnects to the correct MCP target (the same reset path as New-chat), so
 * no stale tool/session state leaks across the switch.
 */
export function setActiveContext(ctx: ChatContext | null): void {
  if (sameContext(ctx, state.ctx)) {
    // Same context still active — keep the conversation; just refresh layout.
    applyLayout();
    return;
  }
  // Persist the OUTGOING conversation so switching away never loses it (#35).
  saveCurrentConversation();

  state.ctx = ctx;
  const epoch = resetSessionState(state);
  if (titleEl) titleEl.textContent = ctx ? titleFor(ctx) : "Chat";
  if (logEl) logEl.replaceChildren();
  applyLayout();
  if (!ctx) return;

  // If we have a stored conversation for the INCOMING context, restore its
  // history + rendered log and re-init only the MCP session (it may have
  // expired). Otherwise start a fresh session that seeds the system prompt.
  const stored = conversations.get(contextKey(ctx) ?? "");
  if (stored) {
    state.history = stored.history.map((m) => ({ ...m }));
    state.noTools = stored.noTools;
    if (logEl) logEl.innerHTML = stored.logHtml;
    scrollToEnd();
    void connect(ctx, epoch, { restore: true });
  } else {
    void connect(ctx, epoch);
  }
}

/**
 * New-chat: flush the current conversation and start a fresh one in place for
 * the currently-active context. Resets message state AND re-initializes the
 * MCP session (the same reset path as switching contexts), without navigating
 * or closing the panel. No-op when no chat context is active.
 */
export function newChat(): void {
  if (state.ctx === null) return;
  // Drop the persisted conversation for this context — New-chat is an explicit
  // flush, same as closing the tab (#35).
  wipeConversation(contextKey(state.ctx));
  const epoch = resetSessionState(state);
  if (logEl) logEl.replaceChildren();
  if (inputEl) inputEl.value = "";
  if (sendBtn) sendBtn.disabled = false;
  void connect(state.ctx, epoch);
}

/**
 * Wipe the persisted conversation for a closed tab's context (#35). main.ts
 * wires this to `TabStore.subscribeClose`. Closing the tab is the one moment it
 * is OK to discard the chat history; if the closed tab was the active one, the
 * subsequent `setActiveContext` switch finds nothing stored and starts fresh.
 */
export function forgetContext(ctx: ChatContext | null): void {
  const key = contextKey(ctx);
  if (key === null) return;
  wipeConversation(key);
  // If the closed tab was the ACTIVE one, also drop the live transcript so the
  // follow-up `setActiveContext` (driven by the same close → re-render) doesn't
  // re-persist what we just wiped. The fresh tab then starts clean.
  if (key === contextKey(state.ctx)) {
    state.history = [];
    if (logEl) logEl.replaceChildren();
  }
}

// Initialize the MCP client + tools for a context, degrading to plain chat if
// the handshake fails (the chat still works; we show a small "no tools" note).
// App contexts target their own MCP server; the vault copilot targets the
// shell's own `tangram` MCP server (the full vault toolset).
async function connect(
  ctx: ChatContext,
  epoch: number,
  opts: { restore?: boolean } = {},
): Promise<void> {
  const restore = opts.restore === true;
  const target = mcpTargetFor(ctx);
  // On a fresh session we own the (empty) log and show a connecting line. On a
  // restore the prior transcript is already on screen, so we leave it untouched
  // and only re-init the MCP session in the background.
  if (!restore) {
    appendStatus(
      ctx.kind === "vault"
        ? "Connecting to the vault tools…"
        : "Connecting to the app's tools…",
    );
  }
  let toolsLabel = "";
  try {
    const mcp = new McpClient(target);
    await mcp.initialize();
    const tools = await mcp.listTools();
    if (epoch !== state.epoch) return; // a newer context switch superseded us
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
    console.warn("MCP connect failed for", target, e);
    toolsLabel = "No tools available — plain chat.";
  }
  if (epoch !== state.epoch) return;
  if (restore) {
    // The restored history (including its original system prompt) is kept as-is
    // — only the live MCP session/tools were refreshed. Leave the transcript be.
    return;
  }
  // Seed the system prompt scoped to this context: the vault copilot gets the
  // read+modify vault prompt with the open note seeded; an app context keeps
  // the existing per-app prompt.
  state.history = [
    {
      role: "system",
      content:
        ctx.kind === "vault"
          ? vaultSystemPrompt(ctx.note, state.tools.length > 0)
          : systemPromptFor(ctx.label, state.tools.length > 0),
    },
  ];
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
  if (!inputEl || state.sending || state.ctx === null) return;
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
