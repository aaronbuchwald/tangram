// The inline `/<name>` agent/skill INVOCATION popup. Triggered from the vault
// editor when the caret sits right after a `/<name>` that resolves to a saved
// definition and the user presses Enter (or clicks a highlighted token — see
// editor.ts / slashTrigger.ts). It mirrors modal.ts's overlay/dialog visual
// language (a single-instance overlay at the shell root, keyboard-first,
// backdrop-click / Esc to dismiss).
//
// R1 — the trigger belongs to the INVOCATION, not the definition. This popup is
// where the user picks how a def runs (the OPTIONS PASS), below the prompt:
//
//   - Trigger: One-time (default) · Schedule · Event (disabled, future — #33).
//     Schedule reveals a recurrence sub-selector — Daily (default) · Weekly ·
//     Interval — that drives the calendar-style picker.
//   - MCP / Tools, Multi-step, Tags / Labels: disabled "Coming soon"
//     placeholders (grouped under a divider, tooltips kept).
//
// Submit behavior:
//   - One-time → run NOW via DeepSeek through the host's `/llm/deepseek` proxy
//     (relative `../llm/deepseek/...`; the host injects the key, never the
//     browser), show the chat, and on Save replace the `/<name>` token with the
//     `> Agent:` Input/Output block (unchanged behavior). No durable block.
//   - Schedule → write a durable ```agent block (use/trigger/prompt) by
//     replacing the `/<name>` token; do NOT run now — the scheduler picks it up.
//
// The call is BOUND to the def: its `model` is passed through and its
// `instructions` become the system message.
//
// The CREATE/DEFINE popup (`/agent`) lives in createAgentPopup.ts; definitions
// stay trigger-agnostic.

import { DEFAULT_MODEL, type AgentDef } from "./agents";
import {
  buildTrigger,
  browserTz,
  parseSchedule,
  WEEKDAYS,
  type Recurrence,
  type Schedule,
  type Weekday,
} from "./invocations";

/** What the popup does with the exchange / the chosen trigger. */
export interface AgentPopupCallbacks {
  /** Replace the triggering `/<name>` range with the given markdown block and
   *  refocus the editor. Wired by main.ts onto the live MdEditor. Used for the
   *  one-time Output block (on Save). */
  onSave: (block: string) => void;
  /** A Schedule submit: the user picked a recurring trigger + prompt. The
   *  caller mints a UUID, inserts the inline `[⚡ <agent>](agent://<id>)` link in
   *  place of the `/<name>` token, and records the invocation in the replicated
   *  index (`create_invocation`). Optional — when omitted, the Schedule tab is
   *  inert (e.g. a quick-open run popup with no editor target). */
  onSchedule?: (trigger: string, prompt: string) => void;
  /** Close with no document change (Exit / dismiss); refocus the editor. */
  onClose: () => void;
}

/** The top-level trigger the user chose in the options pass. `schedule` reveals
 *  the recurrence sub-selector (Daily/Weekly/Interval); `event` is greyed
 *  (future, #33). */
type TriggerMode = "one-time" | "schedule" | "event";

// Only one popup at a time (like promptName/confirmAction). Re-opening while one
// is up closes the previous instance first.
let current: { dismiss: () => void } | null = null;

/** True while the run popup is open (used by the editor's auto-open guard). */
export function isAgentPopupOpen(): boolean {
  return current !== null;
}

/** Format the prompt + response as a markdown blockquote block (each line
 *  prefixed with `> `), LEADING with an `Agent:` provenance line that names the
 *  skill/agent + model that generated it (Fix 3, refined), e.g.
 *    > Agent: <name> · model: <model>
 *    > Input: <prompt>
 *    > Output: <line 1 of response>
 *    > <line 2 of response>
 *
 *  Leading (not trailing) so the attribution is always visible at the top of
 *  the block — a trailing line gets lost after a long multi-line Output, which
 *  read as "no agent reference at all". Multi-line responses keep every line
 *  inside the quote so live-preview renders one clean indented block.
 *
 *  The `<name>` segment is built separately so it can later become a backlink
 *  (`[[<name>]]` / a live `/<name>` reference) without reformatting the line;
 *  `model` rides on the same line via ` · `, and more ` · `-joined segments
 *  (e.g. `version: x.y.z`) can be appended later. A blank/missing name falls
 *  back to a sensible label so the line never renders empty. */
export function formatAgentBlock(
  prompt: string,
  response: string,
  name: string,
  model: string,
): string {
  const quote = (text: string) =>
    text
      .split("\n")
      .map((line) => `> ${line}`)
      .join("\n");
  // The name segment is isolated so it can be wrapped as a backlink later
  // (e.g. `[[${safeName}]]`) without touching the rest of the line. It should
  // never be empty in either run path, but fall back defensively.
  const safeName = name.trim() || "agent";
  const nameSegment = `/${safeName}`;
  // Forward-compatible provenance: ` · `-joined segments, so `version: x.y.z`
  // (or other metadata) can be appended later without reformatting the line.
  const provenanceSegments = [nameSegment];
  if (model.trim().length > 0) provenanceSegments.push(`model: ${model.trim()}`);
  const agentLine = `> Agent: ${provenanceSegments.join(" · ")}`;
  const promptLines = quote(`Input: ${prompt}`);
  const lines = response.split("\n");
  const first = `> Output: ${lines[0] ?? ""}`;
  const rest = lines.slice(1).map((line) => `> ${line}`);
  return [agentLine, promptLines, first, ...rest].join("\n");
}

interface DeepSeekResponse {
  choices?: Array<{ message?: { content?: string } }>;
}

// POST the prompt to DeepSeek through the host proxy. The shell is mounted at
// `/tangram/`, so the RELATIVE path `../llm/deepseek/...` resolves to the host's
// `/llm/deepseek` proxy. No API key here — the host injects it. The call is
// bound to the def: `model` is passed through and `instructions` (if any) ride
// as the system message ahead of the user's prompt. (P1 always uses the
// `/llm/deepseek` route; provider routing by model is later.)
async function callDeepSeek(
  prompt: string,
  model: string,
  instructions: string,
): Promise<string> {
  const messages: Array<{ role: string; content: string }> = [];
  if (instructions.trim().length > 0) {
    messages.push({ role: "system", content: instructions });
  }
  messages.push({ role: "user", content: prompt });
  const res = await fetch("../llm/deepseek/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      model: model || DEFAULT_MODEL,
      messages,
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
 * Open the `/<name>` RUN popup, bound to the resolved `def`. Single-instance:
 * any open popup is dismissed first. The popup walks Prompt → Waiting → Chat
 * (with Save/Exit) or Error (Retry); the title reads "Running: <name>".
 * Backdrop click / Esc dismiss at any state via `callbacks.onClose`.
 */
export function openAgentPopup(def: AgentDef, callbacks: AgentPopupCallbacks): void {
  current?.dismiss();

  const runTitle = `Running: ${def.name}`;

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

  // ── Prompt + options state (R1 OPTIONS PASS) ───────────────────────────────
  function renderPrompt(initial = "") {
    dialog.replaceChildren();

    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = runTitle;

    const input = document.createElement("textarea");
    input.className = "modal-input agent-input";
    input.rows = 3;
    input.placeholder = `What should ${def.name} do?`;
    input.value = initial;
    input.spellcheck = false;

    // Options block: trigger/recurrence selector + disabled placeholders.
    const opts = document.createElement("div");
    opts.className = "agent-options";

    // ── Top-level trigger (active): One-time · Schedule · Event ──
    let mode: TriggerMode = "one-time";
    const trigRow = optionRow("Trigger");
    const seg = segGroup("Trigger");
    const modeBtns: Record<Exclude<TriggerMode, "event">, HTMLButtonElement> = {
      "one-time": segButton("One-time"),
      schedule: segButton("Schedule"),
    };
    const eventBtn = segButton("Event");
    eventBtn.disabled = true;
    eventBtn.classList.add("disabled");
    eventBtn.title =
      "Run when a vault event occurs — note created, label added, another agent's output. Future feature (#33).";
    seg.append(modeBtns["one-time"], modeBtns.schedule, eventBtn);
    trigRow.append(seg);

    // The recurrence picker (revealed when Schedule is selected). It owns the
    // recurrence sub-selector (Daily/Weekly/Interval, default Daily) plus the
    // calendar sub-controls, and exposes the chosen trigger + validity.
    const picker = buildRecurrencePicker(() => refresh());
    opts.append(trigRow, picker.el);

    // ── Coming-soon placeholders (future phases), grouped under a divider ──
    const soon = document.createElement("div");
    soon.className = "agent-soon-group";
    const soonHead = document.createElement("div");
    soonHead.className = "agent-soon-head micro";
    soonHead.textContent = "Coming soon";
    soon.append(
      soonHead,
      disabledRow("MCP / Tools", "Connect MCP servers/tools the agent may call. Coming in the Tools phase."),
      disabledRow("Multi-step", "Multi-step / graph workflows (LangGraph-style). Future feature."),
      disabledRow("Tags / Labels", "Tag this invocation/run for filtering. Coming soon."),
    );
    opts.append(soon);

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const submitBtn = document.createElement("button");
    submitBtn.type = "button";
    submitBtn.className = "modal-btn primary";
    submitBtn.textContent = "Submit";
    actions.append(submitBtn);

    dialog.append(title, input, opts, actions);

    function applyMode() {
      for (const [m, btn] of Object.entries(modeBtns)) {
        const active = m === mode;
        btn.classList.toggle("active", active);
        btn.setAttribute("aria-pressed", String(active));
      }
      // The picker reveals its recurrence sub-selector only under Schedule;
      // when shown it defaults its sub-mode to Daily.
      picker.show(mode === "schedule");
      refresh();
    }
    for (const [m, btn] of Object.entries(modeBtns) as [
      Exclude<TriggerMode, "event">,
      HTMLButtonElement,
    ][]) {
      btn.addEventListener("click", () => {
        mode = m;
        applyMode();
        if (mode === "one-time") input.focus();
      });
    }

    function refresh() {
      const hasPrompt = input.value.trim().length > 0;
      const ok = mode === "one-time" ? hasPrompt : hasPrompt && picker.isValid();
      submitBtn.disabled = !ok;
    }
    function submit() {
      const prompt = input.value.trim();
      if (!prompt) return;
      if (mode === "one-time") {
        void runPrompt(prompt);
        return;
      }
      const trigger = picker.trigger();
      if (!trigger) return;
      if (!callbacks.onSchedule) return; // Schedule inert without a target
      // Hand the trigger + prompt to the caller; it mints the UUID, inserts the
      // inline `agent://<id>` link, and records the invocation in the index. Do
      // NOT run now — the scheduler picks it up.
      teardown();
      callbacks.onSchedule(trigger, prompt);
    }

    input.addEventListener("input", refresh);
    // Enter submits from the prompt (Shift+Enter inserts a newline).
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        submit();
      }
    });
    submitBtn.addEventListener("click", submit);

    applyMode();
    input.focus();
  }

  // ── Waiting state ───────────────────────────────────────────────────────────
  function renderWaiting(prompt: string) {
    dialog.replaceChildren();
    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = runTitle;

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
    title.textContent = runTitle;

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

    // Exit: discard the response, leave the original `/<name>` untouched.
    exitBtn.addEventListener("click", () => dismiss());
    // Save: replace `/<name>` with the indented blockquote that LEADS with the
    // def's name + model (`> Agent: /<name> · model: <model>`) above
    // Input/Output (Fix 3, refined).
    saveBtn.addEventListener("click", () => {
      teardown();
      callbacks.onSave(formatAgentBlock(prompt, response, def.name, def.model));
    });
    saveBtn.focus();
  }

  // ── Error state: message + Retry / Cancel ───────────────────────────────────
  function renderError(prompt: string, message: string) {
    dialog.replaceChildren();
    const title = document.createElement("div");
    title.className = "modal-title";
    title.textContent = runTitle;

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
      const response = await callDeepSeek(prompt, def.model, def.instructions);
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

// ── options-pass helpers ──────────────────────────────────────────────────────

/** A labelled option row (label on the left, control(s) appended by caller). */
function optionRow(label: string): HTMLElement {
  const row = document.createElement("div");
  row.className = "agent-option-row";
  const lab = document.createElement("span");
  lab.className = "agent-option-label micro";
  lab.textContent = label;
  row.append(lab);
  return row;
}

/** A segmented-control container (pill toggle group). `label` names the group
 *  for assistive tech. Keyboard-operable via its focusable child buttons. */
function segGroup(label: string): HTMLElement {
  const g = document.createElement("div");
  g.className = "agent-seg";
  g.setAttribute("role", "group");
  g.setAttribute("aria-label", label);
  return g;
}

/** A segmented-control button (a focusable pill toggle). Defaults to the
 *  unpressed state; callers toggle `.active` + `aria-pressed`. */
function segButton(text: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "agent-seg-btn";
  b.textContent = text;
  b.setAttribute("aria-pressed", "false");
  return b;
}

/** A greyed/disabled option row with an explanatory hover tooltip (future
 *  phases: MCP/Tools, Multi-step, Tags/Labels). */
function disabledRow(label: string, tooltip: string): HTMLElement {
  const row = optionRow(label);
  row.classList.add("disabled");
  row.title = tooltip;
  const tag = document.createElement("span");
  tag.className = "agent-option-soon micro";
  tag.textContent = "Soon";
  row.append(tag);
  return row;
}

/** The recurring sub-modes the calendar-style picker handles. */
type RecurringMode = "interval" | "daily" | "weekly";

/** Format `hh:mm` as a zero-padded 24h `HH:MM` string (for the time inputs). */
function hhmm(hh: number, mm: number): string {
  return `${String(hh).padStart(2, "0")}:${String(mm).padStart(2, "0")}`;
}

/** Decompose an interval in ms back to the largest whole `<N><unit>` that
 *  reproduces it (days, then hours, then minutes), for prefilling the picker. */
function intervalParts(ms: number): [number, "m" | "h" | "d"] {
  const MIN = 60 * 1000;
  const HOUR = 60 * MIN;
  const DAY = 24 * HOUR;
  if (ms % DAY === 0) return [ms / DAY, "d"];
  if (ms % HOUR === 0) return [ms / HOUR, "h"];
  return [Math.max(1, Math.round(ms / MIN)), "m"];
}

/** Day-of-week chip labels (two letters), aligned to `WEEKDAYS` order.
 *  Two letters disambiguate Tue/Thu and Sat/Sun, which a single-letter label
 *  collapsed (the `title` tooltip still carries the full token). */
const DAY_LABELS: Record<Weekday, string> = {
  mon: "Mo",
  tue: "Tu",
  wed: "We",
  thu: "Th",
  fri: "Fr",
  sat: "Sa",
  sun: "Su",
};

/** Recurrence sub-modes, in the order they appear in the sub-selector, with
 *  Daily first so it is the default-selected mode when Schedule is chosen. */
const RECURRING_MODES: readonly { mode: RecurringMode; label: string }[] = [
  { mode: "daily", label: "Daily" },
  { mode: "weekly", label: "Weekly" },
  { mode: "interval", label: "Interval" },
];

/**
 * Build the Schedule tab's contents (calendar-style, Google-Calendar /
 * Apple-Reminders feel): a recurrence sub-selector (Daily · Weekly · Interval,
 * default Daily) followed by the calendar sub-controls for the chosen sub-mode.
 * Returns the wrapper element plus:
 *  - `show(visible)` — reveal the Schedule contents (and reset the sub-mode to
 *    the default, Daily) or hide them (the One-time / Event triggers).
 *  - `trigger()` — the emitted `trigger:` string for the current selection (via
 *    the shared `buildTrigger`), or `null` if the selection is incomplete.
 *  - `isValid()` — whether `trigger()` would round-trip through `parseSchedule`.
 *
 * Localised time: the tz defaults to the browser zone and is shown as a small
 * "times in <tz>" label; the IANA name is baked into the emitted trigger so the
 * component computes the occurrence in the right zone. Times are entered/shown
 * in local 24h `HH:MM`.
 */
export function buildRecurrencePicker(
  onChange: () => void,
  initial?: Schedule | null,
): {
  el: HTMLElement;
  show: (visible: boolean) => void;
  trigger: () => string | null;
  isValid: () => boolean;
} {
  // When prefilling from an existing trigger, use its timezone so the emitted
  // trigger round-trips unchanged; otherwise default to the browser zone.
  const tz =
    initial && (initial.kind === "daily" || initial.kind === "weekly")
      ? initial.tz
      : browserTz();
  // The default recurrence sub-mode when Schedule is selected, or the prefilled
  // schedule's mode when editing an existing invocation.
  const DEFAULT_MODE: RecurringMode = initial ? initial.kind : "daily";
  let mode: RecurringMode = DEFAULT_MODE;

  const el = document.createElement("div");
  el.className = "agent-recur";

  // ── Recurrence sub-selector (segmented): Daily · Weekly · Interval ──
  const subRow = document.createElement("div");
  subRow.className = "agent-recur-sub";
  const subSeg = segGroup("Recurrence");
  const subBtns: Record<RecurringMode, HTMLButtonElement> = {
    interval: segButton("Interval"),
    daily: segButton("Daily"),
    weekly: segButton("Weekly"),
  };
  for (const { mode: m, label } of RECURRING_MODES) {
    subBtns[m].textContent = label;
    subSeg.append(subBtns[m]);
  }
  subRow.append(subSeg);

  // ── Interval row: "Every [N] [minutes/hours/days]" ──
  const intervalRow = document.createElement("div");
  intervalRow.className = "agent-recur-row";
  const everyLabel = document.createElement("span");
  everyLabel.className = "agent-recur-text micro";
  everyLabel.textContent = "Every";
  const nInput = document.createElement("input");
  nInput.type = "number";
  nInput.min = "1";
  nInput.value = "2";
  nInput.className = "modal-input agent-recur-n";
  // Interval unit as a segmented toggle (no native <select> chrome).
  let unitVal: "m" | "h" | "d" = "h";
  const unitSeg = segGroup("Interval unit");
  const unitBtns: Record<"m" | "h" | "d", HTMLButtonElement> = {
    m: segButton("minutes"),
    h: segButton("hours"),
    d: segButton("days"),
  };
  for (const u of ["m", "h", "d"] as const) {
    unitBtns[u].addEventListener("click", () => {
      unitVal = u;
      applyUnit();
      onChange();
    });
    unitSeg.append(unitBtns[u]);
  }
  function applyUnit() {
    for (const u of ["m", "h", "d"] as const) {
      const active = u === unitVal;
      unitBtns[u].classList.toggle("active", active);
      unitBtns[u].setAttribute("aria-pressed", String(active));
    }
  }
  applyUnit();
  intervalRow.append(everyLabel, nInput, unitSeg);

  // ── Daily row: "Every day at [time]" ──
  const dailyRow = document.createElement("div");
  dailyRow.className = "agent-recur-row";
  const dailyLabel = document.createElement("span");
  dailyLabel.className = "agent-recur-text micro";
  dailyLabel.textContent = "Every day at";
  const dailyTime = document.createElement("input");
  dailyTime.type = "time";
  dailyTime.value = "09:00";
  dailyTime.className = "modal-input agent-recur-time";
  dailyRow.append(dailyLabel, dailyTime);

  // ── Weekly row: day chips + "at [time]" ──
  const weeklyRow = document.createElement("div");
  weeklyRow.className = "agent-recur-row agent-recur-weekly";
  const chips = document.createElement("div");
  chips.className = "agent-day-chips";
  chips.setAttribute("role", "group");
  chips.setAttribute("aria-label", "Days of week");
  const selectedDays = new Set<Weekday>();
  for (const d of WEEKDAYS) {
    const chip = document.createElement("button");
    chip.type = "button";
    chip.className = "agent-day-chip";
    chip.textContent = DAY_LABELS[d];
    chip.title = d;
    chip.setAttribute("aria-label", d);
    chip.setAttribute("aria-pressed", "false");
    chip.addEventListener("click", () => {
      if (selectedDays.has(d)) selectedDays.delete(d);
      else selectedDays.add(d);
      const on = selectedDays.has(d);
      chip.classList.toggle("active", on);
      chip.setAttribute("aria-pressed", String(on));
      onChange();
    });
    chips.append(chip);
  }
  const weeklyAt = document.createElement("span");
  weeklyAt.className = "agent-recur-text micro";
  weeklyAt.textContent = "at";
  const weeklyTime = document.createElement("input");
  weeklyTime.type = "time";
  weeklyTime.value = "09:00";
  weeklyTime.className = "modal-input agent-recur-time";
  weeklyRow.append(chips, weeklyAt, weeklyTime);

  // ── tz hint (shared) ──
  const tzHint = document.createElement("div");
  tzHint.className = "agent-recur-tz micro";
  tzHint.textContent = `times in ${tz}`;

  // Prefill the sub-controls from an existing schedule (the Trigger popup edit
  // flow). The sub-mode itself is set via DEFAULT_MODE above; here we fill the
  // values so Save round-trips an unedited trigger unchanged.
  if (initial) {
    if (initial.kind === "interval") {
      const [n, unit] = intervalParts(initial.ms);
      nInput.value = String(n);
      unitVal = unit;
      applyUnit();
    } else if (initial.kind === "daily") {
      dailyTime.value = hhmm(initial.hh, initial.mm);
    } else {
      weeklyTime.value = hhmm(initial.hh, initial.mm);
      for (const chip of chips.querySelectorAll<HTMLButtonElement>(".agent-day-chip")) {
        const day = chip.title as Weekday;
        if (initial.days.includes(day)) {
          selectedDays.add(day);
          chip.classList.add("active");
          chip.setAttribute("aria-pressed", "true");
        }
      }
    }
  }

  el.append(subRow, intervalRow, dailyRow, weeklyRow, tzHint);

  for (const ctrl of [nInput, dailyTime, weeklyTime]) {
    ctrl.addEventListener("input", onChange);
    ctrl.addEventListener("change", onChange);
  }

  function currentRecurrence(): Recurrence | null {
    if (mode === "interval") {
      const n = Number(nInput.value);
      if (!Number.isInteger(n) || n <= 0) return null;
      return { mode: "interval", n, unit: unitVal };
    }
    if (mode === "daily") {
      if (!/^\d{2}:\d{2}$/.test(dailyTime.value)) return null;
      return { mode: "daily", time: dailyTime.value, tz };
    }
    if (mode === "weekly") {
      if (selectedDays.size === 0) return null;
      if (!/^\d{2}:\d{2}$/.test(weeklyTime.value)) return null;
      const days = WEEKDAYS.filter((d) => selectedDays.has(d));
      return { mode: "weekly", days, time: weeklyTime.value, tz };
    }
    return null;
  }

  function trigger(): string | null {
    const rec = currentRecurrence();
    if (!rec) return null;
    const t = buildTrigger(rec);
    // Round-trip guard: only emit a trigger the component will actually parse.
    return parseSchedule(t) ? t : null;
  }

  // Reflect the chosen sub-mode: highlight its segment + reveal only its
  // calendar sub-controls. The tz hint shows for the time-of-day modes.
  function applyMode() {
    for (const { mode: m } of RECURRING_MODES) {
      const active = m === mode;
      subBtns[m].classList.toggle("active", active);
      subBtns[m].setAttribute("aria-pressed", String(active));
    }
    intervalRow.style.display = mode === "interval" ? "" : "none";
    dailyRow.style.display = mode === "daily" ? "" : "none";
    weeklyRow.style.display = mode === "weekly" ? "" : "none";
    tzHint.style.display = mode === "daily" || mode === "weekly" ? "" : "none";
  }
  for (const { mode: m } of RECURRING_MODES) {
    subBtns[m].addEventListener("click", () => {
      mode = m;
      applyMode();
      onChange();
    });
  }

  // Reveal the Schedule contents (resetting the sub-mode to Daily) or hide.
  function show(visible: boolean) {
    if (visible) {
      mode = DEFAULT_MODE;
      applyMode();
    }
    el.style.display = visible ? "" : "none";
  }

  show(false);
  return { el, show, trigger, isValid: () => trigger() !== null };
}

