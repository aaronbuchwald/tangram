// A small, Obsidian-style naming modal: a centered input with a title and a
// hint line over a subtle backdrop, rendered once at the shell root (an overlay
// appended to <body>, never per-row). It replaces the browser's native
// prompt() for the vault create/rename flows — jarring chrome that doesn't match
// the shell's dark theme — with a calm, keyboard-driven dialog.
//
// The dialog is keyboard-first: the input autofocuses, Enter confirms (only
// when the value is valid), and Esc cancels. A live hint line shows the active
// naming constraint and turns red when the current value violates one, so the
// user is corrected inline rather than after a failed action round-trip.

/** A name the modal rejects before the value is ever submitted. */
export interface NameValidation {
  /**
   * Validate a trimmed candidate name. Return an error string to block
   * confirmation (shown in the hint line), or null when the value is allowed.
   * Empty input is always blocked by the modal itself (confirm stays disabled),
   * so validators only see non-empty trimmed candidates.
   */
  validate?: (value: string) => string | null;
}

export interface NamePromptOptions extends NameValidation {
  /** Dialog title, e.g. "New note in projects/". */
  title: string;
  /** The resting hint line (naming constraints). Turns into the error on a
   *  invalid value, then returns to this text when the value is valid again. */
  hint?: string;
  /** Prefilled value (e.g. the current path for a rename). */
  value?: string;
  /** Placeholder shown when the input is empty. */
  placeholder?: string;
  /** Confirm-button label. Defaults to "Create". */
  confirmLabel?: string;
  /** If set, the input selects this sub-range on open instead of all text —
   *  used by rename so the basename is selected but the folder prefix is kept. */
  selection?: { start: number; end: number };
}

/**
 * Open the naming modal and resolve with the trimmed name the user confirmed,
 * or null if they cancelled (Esc, backdrop click, or the cancel button). Only
 * one modal is shown at a time; the promise settles exactly once.
 */
export function promptName(opts: NamePromptOptions): Promise<string | null> {
  return new Promise((resolve) => {
    const restingHint = opts.hint ?? "";

    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";

    const dialog = document.createElement("div");
    dialog.className = "modal";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");

    const titleEl = document.createElement("div");
    titleEl.className = "modal-title";
    titleEl.textContent = opts.title;

    const input = document.createElement("input");
    input.className = "modal-input";
    input.type = "text";
    input.autocomplete = "off";
    input.spellcheck = false;
    input.value = opts.value ?? "";
    if (opts.placeholder) input.placeholder = opts.placeholder;

    const hintEl = document.createElement("div");
    hintEl.className = "modal-hint";
    hintEl.textContent = restingHint;

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.className = "modal-btn";
    cancelBtn.textContent = "Cancel";
    const confirmBtn = document.createElement("button");
    confirmBtn.type = "button";
    confirmBtn.className = "modal-btn primary";
    confirmBtn.textContent = opts.confirmLabel ?? "Create";
    actions.append(cancelBtn, confirmBtn);

    dialog.append(titleEl, input, hintEl, actions);
    overlay.appendChild(dialog);

    let settled = false;
    function close(result: string | null) {
      if (settled) return;
      settled = true;
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      resolve(result);
    }

    // Recompute validity: blank disables confirm silently; a non-empty value
    // runs the caller's validator, surfacing any error in (a reddened) hint
    // line and disabling confirm until it clears.
    function refresh(): string | null {
      const trimmed = input.value.trim();
      if (!trimmed) {
        hintEl.textContent = restingHint;
        hintEl.classList.remove("error");
        confirmBtn.disabled = true;
        return null;
      }
      const error = opts.validate?.(trimmed) ?? null;
      if (error) {
        hintEl.textContent = error;
        hintEl.classList.add("error");
        confirmBtn.disabled = true;
        return null;
      }
      hintEl.textContent = restingHint;
      hintEl.classList.remove("error");
      confirmBtn.disabled = false;
      return trimmed;
    }

    function submit() {
      const valid = refresh();
      if (valid) close(valid);
    }

    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close(null);
      } else if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        submit();
      }
    }

    input.addEventListener("input", () => refresh());
    cancelBtn.addEventListener("click", () => close(null));
    confirmBtn.addEventListener("click", () => submit());
    // Backdrop click (outside the dialog) cancels, like a native dialog.
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) close(null);
    });
    document.addEventListener("keydown", onKey, true);

    refresh();
    document.body.appendChild(overlay);
    input.focus();
    if (opts.selection) {
      input.setSelectionRange(opts.selection.start, opts.selection.end);
    } else {
      input.select();
    }
  });
}

export interface ConfirmOptions {
  /** Dialog title, e.g. "Delete note". */
  title: string;
  /** Optional body line describing what will happen. */
  message?: string;
  /** Confirm-button label. Defaults to "Delete". */
  confirmLabel?: string;
  /** Style the confirm button as destructive (red). Defaults to true. */
  danger?: boolean;
}

/**
 * Open a confirmation dialog in the shell's own UI (not window.confirm).
 * Resolves true if the user confirms, false if they cancel (Esc, backdrop
 * click, or Cancel). Keyboard-first: the confirm button autofocuses, Enter
 * confirms, Esc cancels. Only settles once.
 */
export function confirmAction(opts: ConfirmOptions): Promise<boolean> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";

    const dialog = document.createElement("div");
    dialog.className = "modal";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");

    const titleEl = document.createElement("div");
    titleEl.className = "modal-title";
    titleEl.textContent = opts.title;

    const msgEl = document.createElement("div");
    msgEl.className = "modal-message";
    msgEl.textContent = opts.message ?? "";

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.className = "modal-btn";
    cancelBtn.textContent = "Cancel";
    const confirmBtn = document.createElement("button");
    confirmBtn.type = "button";
    confirmBtn.className = (opts.danger ?? true) ? "modal-btn danger" : "modal-btn primary";
    confirmBtn.textContent = opts.confirmLabel ?? "Delete";
    actions.append(cancelBtn, confirmBtn);

    dialog.append(titleEl, msgEl, actions);
    overlay.appendChild(dialog);

    let settled = false;
    function close(result: boolean) {
      if (settled) return;
      settled = true;
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      resolve(result);
    }
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close(false);
      } else if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        close(true);
      }
    }
    cancelBtn.addEventListener("click", () => close(false));
    confirmBtn.addEventListener("click", () => close(true));
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) close(false);
    });
    document.addEventListener("keydown", onKey, true);

    document.body.appendChild(overlay);
    confirmBtn.focus();
  });
}
