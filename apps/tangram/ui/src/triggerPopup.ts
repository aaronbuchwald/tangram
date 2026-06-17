// The Trigger popup (the scheduled-invocation redesign): opened by clicking an
// inline `[⚡ <agent>](agent://<id>)` link in the editor. It stays on the file,
// loads the invocation from the replicated index by id, and shows its trigger in
// the same popup-v2 Schedule form (pre-filled) plus its prompt. Buttons:
//
//   - Save           → `update_invocation(id, trigger, prompt)`
//   - Open in Agents → deep-link into the Agents view's Triggers sub-tab,
//                      switching to it and scrolling to + briefly highlighting
//                      this trigger's row (I3; `onOpenAgents` → `tabs.openAgents(id)`)
//   - Exit           → close with no change
//
// It mirrors modal.ts / agentPopup.ts's overlay language (single-instance
// overlay at the shell root, backdrop-click / Esc to dismiss) and reuses
// agentPopup.ts's `buildRecurrencePicker` so the picker behaviour is identical.

import { buildRecurrencePicker } from "./agentPopup";
import { parseSchedule } from "./invocations";
import type { Invocation } from "./api";

/** Side effects the popup needs from the shell. */
export interface TriggerPopupCallbacks {
  /** Persist the edited trigger + prompt (`update_invocation`). */
  onSave: (trigger: string, prompt: string) => void;
  /** "Open in Agents" — deep-link into the Agents view's Triggers sub-tab,
   *  switching to it and scrolling to + highlighting this trigger's row (I3). */
  onOpenAgents: () => void;
  /** Delete the invocation (and let the caller strip the inline link). */
  onDelete: () => void;
  /** Close with no change (Exit / dismiss). */
  onClose: () => void;
}

let current: { dismiss: () => void } | null = null;

/** True while the Trigger popup is open (used by the editor's click guard). */
export function isTriggerPopupOpen(): boolean {
  return current !== null;
}

/**
 * Open the Trigger popup for an existing scheduled invocation, pre-filled from
 * its index record. Single-instance: any open popup is dismissed first.
 */
export function openTriggerPopup(
  inv: Invocation,
  callbacks: TriggerPopupCallbacks,
): void {
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
  overlay.addEventListener("mousedown", (e) => {
    if (e.target === overlay) dismiss();
  });

  const title = document.createElement("div");
  title.className = "modal-title";
  title.textContent = `Trigger: ${inv.agent}`;

  // Prompt editor (the invocation's user prompt).
  const promptInput = document.createElement("textarea");
  promptInput.className = "modal-input agent-input";
  promptInput.rows = 3;
  promptInput.placeholder = `What should ${inv.agent} do?`;
  promptInput.value = inv.prompt;
  promptInput.spellcheck = false;

  // The Schedule picker, pre-filled from the existing trigger. An unparseable
  // trigger (shouldn't happen for a stored scheduled invocation) falls back to
  // the picker default.
  const opts = document.createElement("div");
  opts.className = "agent-options";
  const initial = parseSchedule(inv.trigger);
  const picker = buildRecurrencePicker(() => refresh(), initial);
  picker.show(true);
  opts.append(picker.el);

  const actions = document.createElement("div");
  actions.className = "modal-actions";
  const deleteBtn = document.createElement("button");
  deleteBtn.type = "button";
  deleteBtn.className = "modal-btn danger";
  deleteBtn.textContent = "Delete";
  const agentsBtn = document.createElement("button");
  agentsBtn.type = "button";
  agentsBtn.className = "modal-btn";
  agentsBtn.textContent = "Open in Agents";
  const exitBtn = document.createElement("button");
  exitBtn.type = "button";
  exitBtn.className = "modal-btn";
  exitBtn.textContent = "Exit";
  const saveBtn = document.createElement("button");
  saveBtn.type = "button";
  saveBtn.className = "modal-btn primary";
  saveBtn.textContent = "Save";
  actions.append(deleteBtn, agentsBtn, exitBtn, saveBtn);

  dialog.append(title, promptInput, opts, actions);

  function refresh() {
    const hasPrompt = promptInput.value.trim().length > 0;
    saveBtn.disabled = !(hasPrompt && picker.isValid());
  }
  promptInput.addEventListener("input", refresh);

  saveBtn.addEventListener("click", () => {
    const trigger = picker.trigger();
    const prompt = promptInput.value.trim();
    if (!trigger || !prompt) return;
    teardown();
    callbacks.onSave(trigger, prompt);
  });
  agentsBtn.addEventListener("click", () => {
    teardown();
    callbacks.onOpenAgents();
  });
  exitBtn.addEventListener("click", () => dismiss());
  deleteBtn.addEventListener("click", () => {
    teardown();
    callbacks.onDelete();
  });

  document.body.appendChild(overlay);
  refresh();
  promptInput.focus();
}
