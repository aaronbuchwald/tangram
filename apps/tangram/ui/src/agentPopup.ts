// The inline "@agent" LLM popup (demo). Triggered from the vault editor when
// the caret sits right after a literal `@agent` and the user presses Enter (or
// clicks a highlighted `@agent` token — see editor.ts / agentTag.ts). It mirrors
// modal.ts's overlay/dialog visual language (a single-instance overlay appended
// at the shell root, keyboard-first, backdrop-click / Esc to dismiss).
//
// Single-turn only: a prompt is sent to DeepSeek via the host's `/llm/deepseek`
// proxy (relative path `../llm/deepseek/...` — the shell is mounted at
// `/tangram/`, so it resolves to the host's proxy; the host injects the API
// key, never the browser). The response is shown as a chat exchange, then the
// user can Save (replace the `@agent` token with an indented blockquote of the
// prompt + response) or Exit (discard, leave `@agent` untouched).
//
// Scope guard: ONLY DeepSeek, single prompt → response, exactly this flow. No
// multi-turn, no provider choice, no settings — iterate later.

/** What the popup does with the exchange when the user chooses Save. */
export interface AgentPopupCallbacks {
  /** Replace the triggering `@agent` range with the given markdown block and
   *  refocus the editor. Wired by main.ts onto the live MdEditor. */
  onSave: (block: string) => void;
  /** Close with no document change (Exit / dismiss); refocus the editor. */
  onClose: () => void;
}

// Only one popup at a time (like promptName/confirmAction). Re-opening while one
// is up closes the previous instance first.
let current: { dismiss: () => void } | null = null;

/** Format the prompt + response as a markdown blockquote block (each line
 *  prefixed with `> `), e.g.
 *    > Input: <prompt>
 *    > Output: <line 1 of response>
 *    > <line 2 of response>
 *  Multi-line responses keep every line inside the quote so live-preview
 *  renders one clean indented block. */
export function formatAgentBlock(prompt: string, response: string): string {
  const quote = (text: string) =>
    text
      .split("\n")
      .map((line) => `> ${line}`)
      .join("\n");
  const promptLines = quote(`Input: ${prompt}`);
  const lines = response.split("\n");
  const first = `> Output: ${lines[0] ?? ""}`;
  const rest = lines.slice(1).map((line) => `> ${line}`);
  return [promptLines, first, ...rest].join("\n");
}

interface DeepSeekResponse {
  choices?: Array<{ message?: { content?: string } }>;
}

// POST the prompt to DeepSeek through the host proxy. The shell is mounted at
// `/tangram/`, so the RELATIVE path `../llm/deepseek/...` resolves to the host's
// `/llm/deepseek` proxy. No API key here — the host injects it.
async function callDeepSeek(prompt: string): Promise<string> {
  const res = await fetch("../llm/deepseek/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      model: "deepseek-chat",
      messages: [{ role: "user", content: prompt }],
    }),
  });
  if (!res.ok) {
    let detail = "";
    try {
      detail = (await res.text()).slice(0, 300);
    } catch {
      /* ignore */
    }
    throw new Error(`LLM request failed (${res.status})${detail ? `: ${detail}` : ""}`);
  }
  const json = (await res.json()) as DeepSeekResponse;
  const content = json.choices?.[0]?.message?.content;
  if (typeof content !== "string" || content.length === 0) {
    throw new Error("LLM returned an empty response");
  }
  return content;
}

/**
 * Open the "@agent" popup. Single-instance: any open popup is dismissed first.
 * The popup walks Prompt → Waiting → Chat (with Save/Exit) or Error (Retry).
 * Backdrop click / Esc dismiss at any state via `callbacks.onClose`.
 */
export function openAgentPopup(callbacks: AgentPopupCallbacks): void {
  current?.dismiss();

  const overlay = document.createElement("div");
  overlay.className = "modal-overlay";

  const dialog = document.createElement("div");
  dialog.className = "modal agent-popup";
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  overlay.appendChild(dialog);

  let settled = false;
  function teardown() {
    if (settled) return;
    settled = true;
    document.removeEventListener("keydown", onKey, true);
    overlay.remove();
    if (current?.dismiss === dismiss) current = null;
  }
  // Dismiss = close with no document change (return to editor, @agent stays).
  function dismiss() {
    if (settled) return;
    teardown();
    callbacks.onClose();
  }
  current = { dismiss };

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      dismiss();
    }
  }
  document.addEventListener("keydown", onKey, true);
  // Backdrop click (outside the dialog) dismisses, like the naming modal.
  overlay.addEventListener("mousedown", (e) => {
    if (e.target === overlay) dismiss();
  });

  // ── Prompt state ──────────────────────────────────────────────────────────
  function renderPrompt(initial = "") {
    dialog.replaceChildren();

    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = "Ask the agent";

    const input = document.createElement("textarea");
    input.className = "modal-input agent-input";
    input.rows = 3;
    input.placeholder = "What should the agent do?";
    input.value = initial;
    input.spellcheck = false;

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const submitBtn = document.createElement("button");
    submitBtn.type = "button";
    submitBtn.className = "modal-btn primary";
    submitBtn.textContent = "Submit";
    actions.append(submitBtn);

    dialog.append(title, input, actions);

    function refresh() {
      submitBtn.disabled = input.value.trim().length === 0;
    }
    function submit() {
      const prompt = input.value.trim();
      if (!prompt) return;
      void runPrompt(prompt);
    }

    input.addEventListener("input", refresh);
    // Enter submits (Shift+Enter inserts a newline in the textarea).
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        submit();
      }
    });
    submitBtn.addEventListener("click", submit);

    refresh();
    input.focus();
  }

  // ── Waiting state ───────────────────────────────────────────────────────────
  function renderWaiting(prompt: string) {
    dialog.replaceChildren();
    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = "Ask the agent";

    const chat = document.createElement("div");
    chat.className = "agent-chat";
    chat.appendChild(bubble("user", prompt));

    const thinking = document.createElement("div");
    thinking.className = "agent-thinking";
    const spinner = document.createElement("span");
    spinner.className = "agent-spinner";
    thinking.append(spinner, document.createTextNode("thinking…"));
    chat.appendChild(thinking);

    dialog.append(title, chat);
  }

  // ── Chat (result) state: prompt + response bubbles, then Save / Exit ────────
  function renderChat(prompt: string, response: string) {
    dialog.replaceChildren();
    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = "Ask the agent";

    const chat = document.createElement("div");
    chat.className = "agent-chat";
    chat.appendChild(bubble("user", prompt));
    chat.appendChild(bubble("assistant", response));

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const exitBtn = document.createElement("button");
    exitBtn.type = "button";
    exitBtn.className = "modal-btn";
    exitBtn.textContent = "Exit";
    const saveBtn = document.createElement("button");
    saveBtn.type = "button";
    saveBtn.className = "modal-btn primary";
    saveBtn.textContent = "Save";
    actions.append(exitBtn, saveBtn);

    dialog.append(title, chat, actions);

    // Exit: discard the response, leave the original @agent untouched.
    exitBtn.addEventListener("click", () => dismiss());
    // Save: replace @agent with the indented prompt+response blockquote.
    saveBtn.addEventListener("click", () => {
      teardown();
      callbacks.onSave(formatAgentBlock(prompt, response));
    });
    saveBtn.focus();
  }

  // ── Error state: message + Retry / Cancel ───────────────────────────────────
  function renderError(prompt: string, message: string) {
    dialog.replaceChildren();
    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = "Ask the agent";

    const chat = document.createElement("div");
    chat.className = "agent-chat";
    chat.appendChild(bubble("user", prompt));

    const err = document.createElement("div");
    err.className = "agent-error";
    err.textContent = message;
    chat.appendChild(err);

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.className = "modal-btn";
    cancelBtn.textContent = "Cancel";
    const retryBtn = document.createElement("button");
    retryBtn.type = "button";
    retryBtn.className = "modal-btn primary";
    retryBtn.textContent = "Retry";
    actions.append(cancelBtn, retryBtn);

    dialog.append(title, chat, actions);

    cancelBtn.addEventListener("click", () => dismiss());
    // Retry goes back to the prompt state, preserving the typed prompt.
    retryBtn.addEventListener("click", () => renderPrompt(prompt));
    retryBtn.focus();
  }

  async function runPrompt(prompt: string) {
    renderWaiting(prompt);
    try {
      const response = await callDeepSeek(prompt);
      if (settled) return;
      renderChat(prompt, response);
    } catch (e) {
      if (settled) return;
      renderError(prompt, e instanceof Error ? e.message : String(e));
    }
  }

  document.body.appendChild(overlay);
  renderPrompt();
}

function bubble(role: "user" | "assistant", text: string): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = `agent-bubble ${role}`;
  const who = document.createElement("div");
  who.className = "agent-bubble-role micro";
  who.textContent = role === "user" ? "You" : "Agent";
  const body = document.createElement("div");
  body.className = "agent-bubble-text";
  body.textContent = text;
  wrap.append(who, body);
  return wrap;
}
