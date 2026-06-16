// The `/agent` CREATE/DEFINE popup (P1). Triggered from the vault editor when
// the caret sits right after the reserved literal `/agent` and the user presses
// Enter (or clicks a highlighted `/agent` token — see slashTrigger.ts). It makes
// a NEW agent/skill: a vault note whose leading YAML frontmatter carries the
// definition (kind/name/model/labels/version) and whose body is the
// instructions (system prompt / task).
//
// A definition is decoupled from its triggers (see agents.ts): once saved, the
// indexer picks the note up on the next vault state, so the new agent is
// immediately invocable inline via `/<name>`.
//
// Visual language mirrors modal.ts (overlay/card, keyboard-first, backdrop /
// Esc to dismiss). Scope guard (P1): create-and-save only — no event/cron
// triggers, no tools/sandbox fields.

import { vault } from "./api";

/** The kind of definition the create popup just saved. */
export type Kind = "agent" | "skill";

/** Details of the agent/skill the create popup just saved (Fix 2): enough for
 *  the caller to swap the `/agent` token for `/<name>` AND build the new def's
 *  run popup immediately — without waiting for the vault-state round-trip that
 *  rebuilds the index. Mirrors the fields `parseAgent` would recover. */
export interface CreatedAgent {
  name: string;
  kind: Kind;
  model: string;
  labels: string[];
  /** The system prompt / task body (trimmed) — the run popup's `instructions`. */
  instructions: string;
  /** Where the def was saved (so the caller can locate/open the note). */
  path: string;
}

/** Constraints/hooks the create popup needs from the caller. */
export interface CreateAgentOptions {
  /** True if `name` is already taken in the index (case-insensitive). */
  isNameTaken: (name: string) => boolean;
  /** Called after a successful create with the new def's name+kind (Fix 2). The
   *  caller replaces the triggering `/agent` token with `/<name>` and opens the
   *  run popup bound to the new def so the user can prompt it right away. */
  onCreated: (created: CreatedAgent) => void;
  /** Close with no document change (Cancel / dismiss); refocus the editor. */
  onClose: () => void;
}

// Only one popup at a time (shared single-instance discipline with the run
// popup is unnecessary — each is its own overlay — but we still close the
// previous create popup if one is somehow open).
let current: { dismiss: () => void } | null = null;

/** True while the create popup is open (used by the editor's auto-open guard). */
export function isCreateAgentPopupOpen(): boolean {
  return current !== null;
}

// A name must be a path-safe basename: no slashes (the path is derived), no
// path-hostile/control/wildcard chars, no `.`/`..`.
const INVALID_NAME = /[/<>:"\\|?*\x00-\x1f]/;

/** Validate a trimmed candidate name. Returns an error string or null. */
function validateName(name: string, isTaken: (n: string) => boolean): string | null {
  if (name.length === 0) return "Name is required";
  if (name === "." || name === "..") return "Name can't be '.' or '..'";
  if (INVALID_NAME.test(name)) {
    return 'Name can\'t contain / < > : " \\ | ? * or control characters';
  }
  if (name.toLowerCase() === "agent") {
    return '"agent" is reserved (it opens this create popup)';
  }
  if (isTaken(name)) return `"${name}" already exists`;
  return null;
}

/** Quote a YAML scalar only when it could be misread (so simple names stay
 *  bare). We double-quote and escape embedded quotes/backslashes otherwise. */
function yamlScalar(value: string): string {
  if (/^[A-Za-z0-9][A-Za-z0-9 _.\-]*$/.test(value)) return value;
  return `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/** Build the note body: a `---` frontmatter block + a blank line + the
 *  instructions. Mirrors the shape agents.ts parses back. */
export function buildAgentNote(
  kind: Kind,
  name: string,
  model: string,
  labels: string[],
  instructions: string,
): string {
  const fm: string[] = ["---"];
  fm.push(`kind: ${kind}`);
  fm.push(`name: ${yamlScalar(name)}`);
  fm.push(`model: ${yamlScalar(model)}`);
  if (labels.length > 0) {
    fm.push(`labels: [${labels.map(yamlScalar).join(", ")}]`);
  }
  fm.push(`version: "0.1.0"`);
  fm.push("---");
  return `${fm.join("\n")}\n\n${instructions.trim()}\n`;
}

/** The vault path a definition is saved at: agents at `agents/<name>.md`,
 *  skills under `agents/skills/<name>.md`. */
export function agentNotePath(kind: Kind, name: string): string {
  return kind === "skill" ? `agents/skills/${name}.md` : `agents/${name}.md`;
}

/** Open the `/agent` create/define popup. Single-instance. */
export function openCreateAgentPopup(opts: CreateAgentOptions): void {
  current?.dismiss();

  const overlay = document.createElement("div");
  overlay.className = "modal-overlay";

  const dialog = document.createElement("div");
  dialog.className = "modal agent-create";
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
    opts.onClose();
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

  // ── form ─────────────────────────────────────────────────────────────────
  const title = document.createElement("div");
  title.className = "modal-title";
  title.textContent = "New agent or skill";

  const grid = document.createElement("div");
  grid.className = "agent-create-grid";

  // name
  const nameInput = document.createElement("input");
  nameInput.className = "modal-input";
  nameInput.type = "text";
  nameInput.autocomplete = "off";
  nameInput.spellcheck = false;
  nameInput.placeholder = "e.g. summarize";
  grid.append(fieldLabel("Name"), nameInput);

  // kind — a small segmented toggle (no native <select> chrome). Default skill.
  let kind: Kind = "skill";
  const kindSeg = document.createElement("div");
  kindSeg.className = "agent-seg agent-create-kind";
  kindSeg.setAttribute("role", "group");
  kindSeg.setAttribute("aria-label", "Kind");
  const kindBtns: Record<Kind, HTMLButtonElement> = {
    skill: kindSegButton("skill"),
    agent: kindSegButton("agent"),
  };
  function applyKind() {
    for (const k of ["skill", "agent"] as const) {
      const active = k === kind;
      kindBtns[k].classList.toggle("active", active);
      kindBtns[k].setAttribute("aria-pressed", String(active));
    }
  }
  for (const k of ["skill", "agent"] as const) {
    kindBtns[k].addEventListener("click", () => {
      kind = k;
      applyKind();
    });
    kindSeg.append(kindBtns[k]);
  }
  applyKind();
  grid.append(fieldLabel("Kind"), kindSeg);

  // model
  const modelInput = document.createElement("input");
  modelInput.className = "modal-input";
  modelInput.type = "text";
  modelInput.autocomplete = "off";
  modelInput.spellcheck = false;
  modelInput.value = "deepseek-chat";
  grid.append(fieldLabel("Model"), modelInput);

  // labels (optional, comma-separated)
  const labelsInput = document.createElement("input");
  labelsInput.className = "modal-input";
  labelsInput.type = "text";
  labelsInput.autocomplete = "off";
  labelsInput.spellcheck = false;
  labelsInput.placeholder = "optional, comma-separated";
  grid.append(fieldLabel("Labels"), labelsInput);

  // instructions
  const instrInput = document.createElement("textarea");
  instrInput.className = "modal-input agent-input";
  instrInput.rows = 5;
  instrInput.spellcheck = false;
  instrInput.placeholder = "The system prompt / task for this agent…";
  grid.append(fieldLabel("Instructions"), instrInput);

  const hint = document.createElement("div");
  hint.className = "modal-hint";
  hint.textContent =
    "Saved as a vault note; invoke it inline with /name. Esc to cancel.";

  const actions = document.createElement("div");
  actions.className = "modal-actions";
  const cancelBtn = document.createElement("button");
  cancelBtn.type = "button";
  cancelBtn.className = "modal-btn";
  cancelBtn.textContent = "Cancel";
  const createBtn = document.createElement("button");
  createBtn.type = "button";
  createBtn.className = "modal-btn primary";
  createBtn.textContent = "Create";
  actions.append(cancelBtn, createBtn);

  dialog.append(title, grid, hint, actions);

  function refresh(): string | null {
    const name = nameInput.value.trim();
    if (name.length === 0) {
      hint.classList.remove("error");
      hint.textContent =
        "Saved as a vault note; invoke it inline with /name. Esc to cancel.";
      createBtn.disabled = true;
      return null;
    }
    const error = validateName(name, opts.isNameTaken);
    if (error) {
      hint.textContent = error;
      hint.classList.add("error");
      createBtn.disabled = true;
      return null;
    }
    hint.classList.remove("error");
    hint.textContent =
      "Saved as a vault note; invoke it inline with /name. Esc to cancel.";
    createBtn.disabled = false;
    return name;
  }

  async function create() {
    const name = refresh();
    if (!name) return;
    const model = modelInput.value.trim() || "deepseek-chat";
    const labels = labelsInput.value
      .split(",")
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
    const instructions = instrInput.value;
    const body = buildAgentNote(kind, name, model, labels, instructions);
    const path = agentNotePath(kind, name);

    createBtn.disabled = true;
    createBtn.textContent = "Creating…";
    try {
      await vault.createFile(path, body);
    } catch (e) {
      hint.textContent = String(e instanceof Error ? e.message : e);
      hint.classList.add("error");
      createBtn.disabled = false;
      createBtn.textContent = "Create";
      return;
    }
    // Done: hand the new def back so the caller swaps the `/agent` token for
    // `/<name>` and chains into its run popup (Fix 2) — bound to these exact
    // fields, so it works before the index round-trip rebuilds. Close first.
    teardown();
    opts.onCreated({
      name,
      kind,
      model,
      labels,
      instructions: instructions.trim(),
      path,
    });
  }

  nameInput.addEventListener("input", () => refresh());
  cancelBtn.addEventListener("click", () => dismiss());
  createBtn.addEventListener("click", () => void create());
  // Enter in any single-line input creates; the textarea keeps Enter for
  // newlines (use the button there).
  for (const input of [nameInput, modelInput, labelsInput]) {
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void create();
      }
    });
  }

  document.body.appendChild(overlay);
  refresh();
  nameInput.focus();
}

function fieldLabel(text: string): HTMLElement {
  const el = document.createElement("label");
  el.className = "agent-create-label micro";
  el.textContent = text;
  return el;
}

/** A segmented-control button for the kind toggle (a focusable pill). */
function kindSegButton(text: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "agent-seg-btn";
  b.textContent = text;
  b.setAttribute("aria-pressed", "false");
  return b;
}
