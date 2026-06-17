// The Run editor (embedded-runs R2): the modal opened by clicking an inline
// `[⚡ <agent>](agent://<id>)` chip in the editor. It loads the Run from the
// replicated `invocations` index by id and presents it as a MODAL WITH FOUR
// TABS (the right sidebar stays the copilot chat). Internally a Run is still the
// scheduled "invocation"; the data model + identifiers are unchanged (the rename
// is user-facing — docs/design/embedded-runs.md §1).
//
//   - Config        — the headline: **visible additive inheritance**. The
//     inherited Agent config (system prompt / model / base MCP servers / tags)
//     renders greyed + read-only; the Run-scoped fields (one-time prompt,
//     schedule, additive grants/tags) render editable + highlighted; any field
//     that REPLACES an inherited value gets an "overrides agent" badge. Resolved
//     from the agent definition by the Run's `agent` name (versioning deferred);
//     a missing Agent shows a clear unresolved state. (runConfig.ts is the pure
//     classification engine.)
//   - Runs          — a **Re-run now** action (`run_agent`) + a read-only
//     **preview of the resolved effective config** (inherited ⊕ Run overrides —
//     the exact config a run would use).
//   - History       — the Run's **Executions**. R2 reads CURRENT data only: the
//     Run's `last_run_ms` + `status` → a single most-recent Execution row; the
//     full append-only executions log lands with R3 (shown as an empty tail).
//   - Observability — per-execution trace surface. R2 shows what's available
//     (last run's status/time) + a pointer to the gateway/Langfuse OTLP
//     observability; full per-execution traces arrive with the executions log
//     (R3). No new telemetry is built here.
//
// Save/Exit semantics are preserved from R1: Save → `update_invocation`;
// "Open in Agents" deep-link; Delete; Exit. The tabs are within the same modal.
// It reuses agentPopup.ts's `buildRecurrencePicker` so the schedule picker
// behaviour is identical, and modal.ts / agentPopup.ts's overlay language.

import { buildRecurrencePicker } from "./agentPopup";
import { parseSchedule } from "./invocations";
import { formatRelativeTime, formatSchedule } from "./invocations";
import {
  effectiveConfig,
  resolveRunConfig,
  type ResolvedField,
  type ResolvedList,
  type ResolvedRunConfig,
} from "./runConfig";
import type { AgentDef } from "./agents";
import type { Execution, Invocation } from "./api";

/** Side effects the Run editor needs from the shell. */
export interface TriggerPopupCallbacks {
  /** Persist the edited schedule + prompt + Run-scoped mounted files
   *  (`update_invocation`; embedded-runs R4 adds `files`). */
  onSave: (trigger: string, prompt: string, files: string[]) => void;
  /** "Open in Agents" — deep-link into the Agents view's Runs sub-tab,
   *  switching to it and scrolling to + highlighting this Run's row (I3). */
  onOpenAgents: () => void;
  /** Delete the invocation (and let the caller strip the inline link). */
  onDelete: () => void;
  /** Close with no change (Exit / dismiss). */
  onClose: () => void;
  /** Resolve the Run's Agent definition by name (case-insensitive) so the
   *  Config tab can render the inherited config. Null ⇒ the Agent is missing
   *  (unresolved inheritance). */
  agentByName: (name: string) => AgentDef | null;
  /** Re-run the Agent now (the Runs tab's "Re-run now" → `run_agent`). Resolves
   *  with the produced output text, or rejects with an error message. */
  onRerun: (agent: string) => Promise<string>;
  /** The Run's executions, newest first (embedded-runs R3 executions log) — the
   *  History tab lists these. Read live off the vault state frame in main.ts. */
  executionsForRun: (runId: string) => Execution[];
  /** The vault's pickable file paths (embedded-runs R4): the source for the
   *  Run-scoped mounted-files multi-select. Excludes the `.keep` folder
   *  sentinels. Read live off the vault state frame in main.ts. */
  vaultFiles: () => string[];
}

/** The four tabs of the Run editor, in display order. */
type RunTab = "config" | "runs" | "history" | "observability";

const TAB_ORDER: readonly { id: RunTab; label: string }[] = [
  { id: "config", label: "Config" },
  { id: "runs", label: "Runs" },
  { id: "history", label: "History" },
  { id: "observability", label: "Observability" },
];

let current: { dismiss: () => void } | null = null;

/** True while the Run editor is open (used by the editor's click guard). */
export function isTriggerPopupOpen(): boolean {
  return current !== null;
}

/**
 * Open the Run editor for an existing scheduled Run, pre-filled from its index
 * record. Single-instance: any open popup is dismissed first. The editor is a
 * modal with four tabs (Config / Runs / History / Observability); Config is
 * shown first.
 */
export function openTriggerPopup(
  inv: Invocation,
  callbacks: TriggerPopupCallbacks,
): void {
  current?.dismiss();

  const overlay = document.createElement("div");
  overlay.className = "modal-overlay";
  const dialog = document.createElement("div");
  dialog.className = "modal agent-popup run-editor";
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-label", `Edit run for ${inv.agent}`);
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

  // Resolve the inherited Agent config once (versioning deferred — by name).
  const def = callbacks.agentByName(inv.agent);

  // ── Title ────────────────────────────────────────────────────────────────
  const title = document.createElement("div");
  title.className = "modal-title";
  title.textContent = `Run: ${inv.agent}`;

  // ── Config tab's Run-scoped editable controls (the source of Save) ─────────
  // The one-time prompt (Run-scoped, layered on top — empty = pure inheritance).
  const promptInput = document.createElement("textarea");
  promptInput.className = "modal-input agent-input run-prompt-input";
  promptInput.rows = 3;
  promptInput.placeholder = `Optional one-time prompt — empty runs ${inv.agent} as defined`;
  promptInput.value = inv.prompt;
  promptInput.spellcheck = false;

  // The Schedule picker, pre-filled from the existing trigger.
  const initial = parseSchedule(inv.trigger);
  const picker = buildRecurrencePicker(() => refreshSave(), initial);
  picker.show(true);

  // Run-scoped mounted files (embedded-runs R4): the live, editable set this
  // editor mutates. Seeded from the Run's stored `files` (de-duped, order kept),
  // mutated by the picker, read on Save. The Config panel is re-rendered when it
  // changes so the resolved preview + chip list stay in sync.
  const mountedFiles: string[] = dedupePaths(inv.files ?? []);
  function refreshConfigPanel() {
    if (currentTab === "config") panel.replaceChildren(renderPanel("config"));
  }

  // ── Tab bar + panels ───────────────────────────────────────────────────────
  const tabBar = document.createElement("div");
  tabBar.className = "agent-seg run-editor-tabs";
  tabBar.setAttribute("role", "tablist");
  tabBar.setAttribute("aria-label", "Run editor sections");

  const panel = document.createElement("div");
  panel.className = "run-editor-panel";

  const tabBtns: Partial<Record<RunTab, HTMLButtonElement>> = {};
  for (const { id, label } of TAB_ORDER) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "agent-seg-btn run-editor-tab";
    b.textContent = label;
    b.dataset.tab = id;
    b.setAttribute("role", "tab");
    b.addEventListener("click", () => selectTab(id));
    tabBtns[id] = b;
    tabBar.appendChild(b);
  }

  let currentTab: RunTab = "config";
  function selectTab(tab: RunTab) {
    currentTab = tab;
    for (const { id } of TAB_ORDER) {
      const btn = tabBtns[id]!;
      const on = id === tab;
      btn.classList.toggle("active", on);
      btn.setAttribute("aria-pressed", String(on));
      btn.setAttribute("aria-selected", on ? "true" : "false");
    }
    panel.replaceChildren(renderPanel(tab));
  }

  function renderPanel(tab: RunTab): HTMLElement {
    switch (tab) {
      case "config":
        return renderConfigPanel(inv, def, promptInput, picker.el, {
          mountedFiles,
          vaultFiles: callbacks.vaultFiles(),
          onMountsChanged: () => {
            refreshConfigPanel();
          },
        });
      case "runs":
        return renderRunsPanel(inv, def, callbacks, mountedFiles);
      case "history":
        return renderHistoryPanel(callbacks.executionsForRun(inv.id));
      case "observability":
        return renderObservabilityPanel(inv);
    }
  }

  // ── Actions (preserved from R1: Delete / Open in Agents / Exit / Save) ─────
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

  dialog.append(title, tabBar, panel, actions);

  // Save validity tracks the Config tab's schedule picker; the prompt is now
  // OPTIONAL (empty = pure inheritance), so it no longer gates Save.
  function refreshSave() {
    saveBtn.disabled = !picker.isValid();
  }
  promptInput.addEventListener("input", refreshSave);

  saveBtn.addEventListener("click", () => {
    const trigger = picker.trigger();
    if (!trigger) return;
    const prompt = promptInput.value.trim();
    teardown();
    callbacks.onSave(trigger, prompt, dedupePaths(mountedFiles));
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
  selectTab("config");
  refreshSave();
  promptInput.focus();
}

// ── small DOM helpers (local to the Run editor) ──────────────────────────────

function el(tag: string, cls?: string, text?: string): HTMLElement {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
}

/** Trim, drop blanks, and de-duplicate a path list while preserving first-seen
 *  order — mirrors the component's `canonical_mounted_files` (lib.rs) so the
 *  editor and the stored set agree (order matters for mounts: it is the
 *  injection order + part of the config hash). NOT sorted. */
function dedupePaths(paths: string[]): string[] {
  const out: string[] = [];
  for (const raw of paths) {
    const p = raw.trim();
    if (p.length > 0 && !out.includes(p)) out.push(p);
  }
  return out;
}

/** An "overrides agent" badge — flags a Run-scoped value that REPLACES an
 *  inherited one (the explicit divergence call-out). */
function overrideBadge(): HTMLElement {
  const b = el("span", "run-override-badge", "overrides agent");
  b.title = "This Run replaces the Agent's value";
  return b;
}

/** A small origin tag (inherited / added) for a field's header. */
function originTag(origin: "inherited" | "added"): HTMLElement {
  return el(
    "span",
    `run-origin-tag run-origin-${origin}`,
    origin === "inherited" ? "from agent" : "this run",
  );
}

/** Build the header for a Run-scoped field: a label, the origin tag, and — when
 *  the field REPLACES an inherited value — the explicit "overrides agent" badge.
 *  An "override" origin maps the tag to "this run"; "inherited" (empty
 *  Run-scoped value) reads "from agent". */
function scopedFieldHead(label: string, field: ResolvedField): HTMLElement {
  const head = el("div", "run-field-head");
  head.append(el("span", "run-field-label micro", label));
  head.append(originTag(field.origin === "inherited" ? "inherited" : "added"));
  if (field.origin === "override") head.append(overrideBadge());
  return head;
}

// ── Config tab — visible additive inheritance ────────────────────────────────

/** A read-only inherited scalar block (system prompt / model), greyed. */
function inheritedScalar(label: string, value: string): HTMLElement {
  const wrap = el("div", "run-field run-field-inherited");
  const head = el("div", "run-field-head");
  head.append(el("span", "run-field-label micro", label), originTag("inherited"));
  wrap.appendChild(head);
  const body =
    value.trim().length > 0
      ? el("div", "run-field-value run-inherited-value", value)
      : el("div", "run-field-empty micro", "— not set on the agent —");
  wrap.appendChild(body);
  return wrap;
}

/** A read-only inherited / additive list block (MCP servers, tags). Inherited
 *  chips are greyed; Run-scoped additions (if any) are highlighted; an
 *  "additions land later" hint shows when the data model has no scoped lane. */
function inheritedList(label: string, list: ResolvedList): HTMLElement {
  const wrap = el("div", "run-field run-field-inherited");
  const head = el("div", "run-field-head");
  head.append(el("span", "run-field-label micro", label), originTag("inherited"));
  wrap.appendChild(head);

  const chips = el("div", "run-chips");
  if (list.inherited.length === 0 && list.added.length === 0) {
    chips.appendChild(el("span", "run-field-empty micro", "— none —"));
  } else {
    for (const s of list.inherited) chips.appendChild(el("span", "run-chip run-chip-inherited", s));
    for (const s of list.added) {
      const c = el("span", "run-chip run-chip-added", `+ ${s}`);
      chips.appendChild(c);
    }
  }
  wrap.appendChild(chips);
  // A `+` add affordance, inert: the Run data model has no scoped grants/tags
  // lane yet (R3+). Shown so the additive surface is discoverable.
  const add = el("button", "run-add-inert", "+ add (scoped to this run)") as HTMLButtonElement;
  add.type = "button";
  add.disabled = true;
  add.title = "Run-scoped additions to MCP grants / tags land with a later checkpoint";
  wrap.appendChild(add);
  return wrap;
}

/** Render the Config panel: the inherited-from-Agent config (greyed,
 *  read-only) above the Run-scoped editable fields (highlighted), with explicit
 *  override call-outs. Unresolved when the Agent name doesn't match a def. */
function renderConfigPanel(
  inv: Invocation,
  def: AgentDef | null,
  promptInput: HTMLElement,
  pickerEl: HTMLElement,
  mounts: {
    mountedFiles: string[];
    vaultFiles: string[];
    onMountsChanged: () => void;
  },
): HTMLElement {
  // The Config panel reads the LIVE editable mounted set (mutated in place by
  // the picker below), so the resolved preview reflects the current selection.
  const cfg = resolveRunConfig(
    { ...inv, files: mounts.mountedFiles },
    def,
  );
  const panel = el("div", "run-config");

  // ── Inherited Agent config (greyed, read-only) ──
  const inheritedSec = el("section", "run-section run-section-inherited");
  const ih = el("div", "run-section-head");
  ih.append(
    el("h3", "run-section-title", `Inherited from agent: ${inv.agent}`),
    el("span", "run-section-sub micro", "read-only · the Agent definition"),
  );
  inheritedSec.appendChild(ih);

  if (!cfg.resolved) {
    // Unresolved inheritance — the named Agent isn't in the vault index.
    const warn = el("div", "run-unresolved");
    warn.append(
      el("div", "run-unresolved-title", `Agent "${inv.agent}" not found`),
      el(
        "div",
        "run-unresolved-hint micro",
        "This Run references an agent by name that isn't indexed in the vault. " +
          "Its inherited config can't be resolved; create or rename an agent to match, " +
          "or edit the Run below. (Runs reference agents by name — versioning is deferred.)",
      ),
    );
    inheritedSec.appendChild(warn);
  } else {
    inheritedSec.appendChild(inheritedScalar("System prompt / instructions", cfg.instructions.value));
    inheritedSec.appendChild(inheritedScalar("Model", cfg.model.value));
    inheritedSec.appendChild(inheritedList("Base MCP servers / tools", cfg.mcpServers));
    inheritedSec.appendChild(inheritedList("Tags", cfg.tags));
  }
  panel.appendChild(inheritedSec);

  // ── Run-scoped fields (editable, highlighted) ──
  const runSec = el("section", "run-section run-section-scoped");
  const rh = el("div", "run-section-head");
  rh.append(
    el("h3", "run-section-title", "This run (layered on top)"),
    el("span", "run-section-sub micro", "editable · additive over the agent"),
  );
  runSec.appendChild(rh);

  // One-time prompt (Run-scoped, additive over the Agent's instructions).
  const promptField = el("div", "run-field run-field-scoped");
  promptField.appendChild(scopedFieldHead("One-time prompt", cfg.prompt));
  promptField.appendChild(
    el(
      "div",
      "run-field-hint micro",
      "Layered on top of the agent's instructions. Empty = pure inheritance (run the agent as defined).",
    ),
  );
  promptField.appendChild(promptInput);
  runSec.appendChild(promptField);

  // Schedule (purely Run-scoped — an Agent carries no schedule).
  const schedField = el("div", "run-field run-field-scoped");
  schedField.appendChild(scopedFieldHead("Schedule", cfg.schedule));
  schedField.appendChild(pickerEl);
  runSec.appendChild(schedField);

  // Mounted files (Run-scoped, additive — embedded-runs R4). A multi-select of
  // existing vault files; the chosen set is injected into the agent at run time.
  runSec.appendChild(renderMountedFilesField(mounts));

  panel.appendChild(runSec);
  return panel;
}

/** The Run-scoped **mounted files** field (embedded-runs R4): the current
 *  mounted set as removable chips + an "add" picker over the vault's files. It
 *  mutates the shared `mountedFiles` array in place and calls `onMountsChanged`
 *  so the Config panel (resolved preview) re-renders. Purely Run-scoped +
 *  additive (the Agent carries no mounts), so it lives in the "THIS RUN"
 *  section and reads as the Run's own ("this run"). */
function renderMountedFilesField(mounts: {
  mountedFiles: string[];
  vaultFiles: string[];
  onMountsChanged: () => void;
}): HTMLElement {
  const wrap = el("div", "run-field run-field-scoped run-mounts-field");
  const head = el("div", "run-field-head");
  head.append(
    el("span", "run-field-label micro", "Mounted files"),
    originTag("added"),
  );
  wrap.appendChild(head);
  wrap.appendChild(
    el(
      "div",
      "run-field-hint micro",
      "Vault files whose contents are injected into the agent at run time (a " +
        '"Mounted files:" preamble), and reachable via the vault read_file tool ' +
        "when the agent has that grant. Run-scoped — only this run sees them.",
    ),
  );

  // The current mounted set as removable chips.
  const chips = el("div", "run-chips run-mounts-chips");
  if (mounts.mountedFiles.length === 0) {
    chips.appendChild(el("span", "run-field-empty micro", "— no files mounted —"));
  } else {
    for (const path of mounts.mountedFiles) {
      const chip = el("span", "run-chip run-chip-added run-mount-chip", path);
      const x = el("button", "run-mount-remove", "×") as HTMLButtonElement;
      x.type = "button";
      x.title = `Unmount ${path}`;
      x.setAttribute("aria-label", `Unmount ${path}`);
      x.addEventListener("click", () => {
        const i = mounts.mountedFiles.indexOf(path);
        if (i >= 0) mounts.mountedFiles.splice(i, 1);
        mounts.onMountsChanged();
      });
      chip.appendChild(x);
      chips.appendChild(chip);
    }
  }
  wrap.appendChild(chips);

  // The "add a vault file" picker: every vault file not already mounted.
  const remaining = mounts.vaultFiles.filter((f) => !mounts.mountedFiles.includes(f));
  const picker = document.createElement("select");
  picker.className = "modal-input agent-input run-mounts-picker";
  const placeholder = document.createElement("option");
  placeholder.value = "";
  placeholder.textContent =
    remaining.length > 0 ? "+ mount a vault file…" : "— all vault files mounted —";
  placeholder.disabled = true;
  placeholder.selected = true;
  picker.appendChild(placeholder);
  for (const path of remaining) {
    const opt = document.createElement("option");
    opt.value = path;
    opt.textContent = path;
    picker.appendChild(opt);
  }
  picker.disabled = remaining.length === 0;
  picker.addEventListener("change", () => {
    const path = picker.value;
    if (path && !mounts.mountedFiles.includes(path)) {
      mounts.mountedFiles.push(path);
      mounts.onMountsChanged();
    }
  });
  wrap.appendChild(picker);
  return wrap;
}

// ── Runs tab — re-run now + resolved effective config preview ─────────────────

/** A resolved effective scalar row in the preview. */
function previewScalar(label: string, value: string): HTMLElement {
  const row = el("div", "run-preview-row");
  row.append(
    el("span", "run-preview-label micro", label),
    el("span", "run-preview-value", value.trim().length > 0 ? value : "—"),
  );
  return row;
}

/** A resolved effective list row (chips) in the preview. */
function previewList(label: string, items: string[]): HTMLElement {
  const row = el("div", "run-preview-row");
  row.appendChild(el("span", "run-preview-label micro", label));
  const chips = el("div", "run-chips run-preview-chips");
  if (items.length === 0) chips.appendChild(el("span", "run-preview-value", "—"));
  else for (const s of items) chips.appendChild(el("span", "run-chip run-chip-effective", s));
  row.appendChild(chips);
  return row;
}

function renderRunsPanel(
  inv: Invocation,
  def: AgentDef | null,
  callbacks: TriggerPopupCallbacks,
  mountedFiles: string[],
): HTMLElement {
  // Resolve against the LIVE mounted set so the effective-config preview reflects
  // the current selection (embedded-runs R4).
  const cfg = resolveRunConfig({ ...inv, files: mountedFiles }, def);
  const eff = effectiveConfig(cfg);
  const panel = el("div", "run-runs");

  // ── Re-run now ──
  const actionRow = el("div", "run-rerun-row");
  const rerunBtn = el("button", "modal-btn primary run-rerun-btn", "Re-run now") as HTMLButtonElement;
  rerunBtn.type = "button";
  rerunBtn.disabled = !cfg.resolved;
  if (!cfg.resolved) rerunBtn.title = `Agent "${inv.agent}" not found`;
  const status = el("div", "run-rerun-status micro");
  actionRow.append(rerunBtn, status);
  panel.appendChild(actionRow);
  panel.appendChild(
    el(
      "div",
      "run-rerun-hint micro",
      "Runs the agent once now (run_agent), independent of the schedule.",
    ),
  );

  rerunBtn.addEventListener("click", () => {
    rerunBtn.disabled = true;
    status.textContent = "running…";
    status.className = "run-rerun-status micro run-rerun-running";
    callbacks
      .onRerun(inv.agent)
      .then(() => {
        status.textContent = "ran — output appended";
        status.className = "run-rerun-status micro run-rerun-ok";
        rerunBtn.disabled = !cfg.resolved;
      })
      .catch((e: unknown) => {
        status.textContent = `failed: ${e instanceof Error ? e.message : String(e)}`;
        status.className = "run-rerun-status micro run-rerun-err";
        rerunBtn.disabled = !cfg.resolved;
      });
  });

  // ── Resolved effective config preview (read-only) ──
  const previewSec = el("section", "run-section run-preview");
  const head = el("div", "run-section-head");
  head.append(
    el("h3", "run-section-title", "Resolved effective config"),
    el("span", "run-section-sub micro", "inherited ⊕ run overrides — what a run would use"),
  );
  previewSec.appendChild(head);

  if (!eff.resolved) {
    previewSec.appendChild(
      el("div", "run-field-empty micro", `Unresolved — agent "${inv.agent}" not found.`),
    );
  } else {
    previewSec.appendChild(previewScalar("Agent", eff.agentName));
    previewSec.appendChild(previewScalar("Model", eff.model));
    previewSec.appendChild(
      previewScalar("Schedule", eff.schedule ? formatSchedule(eff.schedule) : "unscheduled"),
    );
    previewSec.appendChild(
      previewScalar("Prompt", eff.prompt || "(none — runs the agent as defined)"),
    );
    previewSec.appendChild(previewList("MCP servers", eff.mcpServers));
    previewSec.appendChild(previewList("Tags", eff.tags));
    // Run-scoped mounted files (embedded-runs R4) — part of the resolved
    // effective config (and the component's config hash).
    previewSec.appendChild(previewList("Mounted files", eff.mountedFiles));
    previewSec.appendChild(previewScalar("Instructions", eff.instructions || "(none)"));
  }
  panel.appendChild(previewSec);
  return panel;
}

// ── History tab — Executions (the append-only executions log, R3) ─────────────

/** Render the History tab: the Run's Executions from the append-only log
 *  (embedded-runs R3), newest first. Each row shows the outcome, when it ran,
 *  the model, and the resolved-config hash (the reproducibility snapshot). */
function renderHistoryPanel(executions: Execution[]): HTMLElement {
  const panel = el("div", "run-history");
  panel.appendChild(el("h3", "run-section-title", "Executions"));

  const now = Date.now();
  const list = el("div", "run-executions");
  if (executions.length === 0) {
    list.appendChild(
      el("div", "run-field-empty micro", "No executions yet — this Run hasn't fired."),
    );
  } else {
    executions.forEach((e, i) => {
      const row = el("div", "run-execution-row");
      const dot = el(
        "span",
        `run-exec-dot run-exec-${e.status.toLowerCase().replace(/[^a-z0-9]+/g, "-")}`,
      );
      const when = el("span", "run-exec-when", formatRelativeTime(e.ts, now));
      when.title = new Date(e.ts).toLocaleString();
      const st = el("span", "run-exec-status", e.status);
      const model = el("span", "run-exec-model micro", e.model);
      // The resolved-config hash (short) — the per-execution reproducibility
      // snapshot. Hover shows the full sha256.
      const hash = el("span", "run-exec-hash micro", `cfg ${e.config_hash.slice(0, 8)}`);
      hash.title = `Resolved-config hash (sha256): ${e.config_hash}`;
      row.append(dot, when, st, model, hash);
      if (i === 0) row.append(el("span", "run-exec-tag micro", "most recent"));
      list.appendChild(row);
    });
  }
  panel.appendChild(list);

  // Note the deep per-execution trace stays the O-series stub.
  panel.appendChild(
    el(
      "div",
      "run-deferred-tail micro",
      "Each row is an Execution from the append-only log, with the resolved-config " +
        "hash that produced it. Deep per-execution traces live in the Observability tab " +
        "(host-side OTLP / Langfuse).",
    ),
  );
  return panel;
}

// ── Observability tab — per-execution trace surface (current data; full R3) ───

function renderObservabilityPanel(inv: Invocation): HTMLElement {
  const panel = el("div", "run-observability");
  panel.appendChild(el("h3", "run-section-title", "Observability"));

  const now = Date.now();
  const avail = el("div", "run-obs-available");
  if (inv.last_run_ms === null) {
    avail.appendChild(
      el("div", "run-field-empty micro", "No execution to trace yet — this Run hasn't fired."),
    );
  } else {
    avail.appendChild(previewScalar("Last execution", formatRelativeTime(inv.last_run_ms, now)));
    avail.appendChild(previewScalar("Status", inv.status));
  }
  panel.appendChild(avail);

  // Pointer to the host-side gateway/Langfuse OTLP observability (O1/O2). The
  // shell UI builds no new telemetry; traces are emitted host-side at the LLM
  // egress boundary into the Langfuse stack (deploy/observability/).
  const ptr = el("div", "run-obs-pointer");
  ptr.append(
    el("div", "run-obs-pointer-title micro", "Traces"),
    el(
      "div",
      "run-obs-pointer-hint micro",
      "Per-call LLM/gateway traces are emitted host-side into the Langfuse / OTLP " +
        "observability stack (the gateway egress boundary). Full per-execution traces " +
        "surfaced here arrive with the executions log (R3).",
    ),
  );
  panel.appendChild(ptr);
  return panel;
}

// Re-exported for the focused unit tests (the panel renderers stay private; the
// classification engine they consume is tested directly via runConfig.ts).
export type { ResolvedRunConfig, ResolvedField };
