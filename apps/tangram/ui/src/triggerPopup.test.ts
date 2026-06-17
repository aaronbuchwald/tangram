// DOM tests for the Run editor modal (embedded-runs R2, triggerPopup.ts): the
// four-tab structure, tab switching, the visible additive-inheritance render
// (inherited greyed / run-scoped highlighted), the unresolved-agent state, the
// Runs-tab re-run wiring + resolved preview, the History/Observability current-
// data panels, and the preserved Save/Delete/Open-in-Agents/Exit semantics.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  isTriggerPopupOpen,
  openTriggerPopup,
  type TriggerPopupCallbacks,
} from "./triggerPopup";
import type { AgentDef } from "./agents";
import type { Execution, Invocation } from "./api";

const exec = (over: Partial<Execution> = {}): Execution => ({
  execution_id: "e1",
  run_id: "i1",
  agent: "standup",
  ts: Date.now() - 1000,
  status: "ran",
  model: "deepseek-chat",
  output_block_id: "runout-i1",
  config_hash: "a".repeat(64),
  ...over,
});

const inv = (over: Partial<Invocation> = {}): Invocation => ({
  id: "i1",
  agent: "standup",
  trigger: "daily at 09:00 UTC",
  prompt: "",
  host_file_id: "f1",
  last_run_ms: null,
  status: "scheduled",
  ...over,
});

const def = (over: Partial<AgentDef> = {}): AgentDef => ({
  kind: "agent",
  name: "standup",
  model: "deepseek-chat",
  labels: ["daily"],
  meta: {},
  version: null,
  mcpServers: ["notes"],
  instructions: "You are the standup assistant.",
  fileId: "fd",
  path: "agents/standup.md",
  ...over,
});

const cbs = (over: Partial<TriggerPopupCallbacks> = {}): TriggerPopupCallbacks => ({
  onSave: vi.fn(),
  onOpenAgents: vi.fn(),
  onDelete: vi.fn(),
  onClose: vi.fn(),
  agentByName: () => def(),
  onRerun: () => Promise.resolve("ok"),
  executionsForRun: () => [],
  ...over,
});

const $ = (sel: string) => document.querySelector(sel) as HTMLElement | null;
const $$ = (sel: string) => Array.from(document.querySelectorAll(sel)) as HTMLElement[];
const tabBtn = (label: string) =>
  $$(".run-editor-tab").find((b) => b.textContent === label) as HTMLButtonElement;

afterEach(() => {
  document.body.replaceChildren();
});

describe("Run editor — modal + four tabs", () => {
  beforeEach(() => openTriggerPopup(inv(), cbs()));

  it("opens a modal titled for the Run with all four tabs", () => {
    expect(isTriggerPopupOpen()).toBe(true);
    expect($(".modal-title")!.textContent).toBe("Run: standup");
    const labels = $$(".run-editor-tab").map((b) => b.textContent);
    expect(labels).toEqual(["Config", "Runs", "History", "Observability"]);
  });

  it("shows Config first; clicking a tab switches the visible panel", () => {
    expect($(".run-config")).not.toBeNull();
    expect($(".run-runs")).toBeNull();
    tabBtn("Runs").click();
    expect($(".run-runs")).not.toBeNull();
    expect($(".run-config")).toBeNull();
    tabBtn("History").click();
    expect($(".run-history")).not.toBeNull();
    expect($(".run-executions")).not.toBeNull();
    tabBtn("Observability").click();
    expect($(".run-observability")).not.toBeNull();
    expect($(".run-obs-pointer")).not.toBeNull(); // Langfuse/OTLP pointer
  });
});

describe("Config tab — visible additive inheritance", () => {
  it("renders the inherited Agent config greyed/read-only and run fields scoped", () => {
    openTriggerPopup(inv(), cbs());
    expect($(".run-section-inherited")).not.toBeNull();
    expect($(".run-section-scoped")).not.toBeNull();
    // Inherited instructions + model present in the greyed section.
    expect($(".run-section-inherited")!.textContent).toContain("You are the standup assistant.");
    expect($(".run-section-inherited")!.textContent).toContain("deepseek-chat");
    expect($(".run-section-inherited")!.textContent).toContain("notes"); // base MCP
    // The schedule field carries the "this run" origin tag (purely run-scoped).
    const sched = $(".run-section-scoped")!;
    expect(sched.querySelector(".run-origin-added")).not.toBeNull();
  });

  it("tags a non-empty one-time prompt as a run-scoped addition", () => {
    openTriggerPopup(inv({ prompt: "extra" }), cbs());
    // Two 'added' origin tags now: the prompt and the schedule.
    expect($$(".run-section-scoped .run-origin-added").length).toBe(2);
  });

  it("shows a clear unresolved state when the Agent is missing", () => {
    openTriggerPopup(inv({ agent: "ghost" }), cbs({ agentByName: () => null }));
    expect($(".run-unresolved")).not.toBeNull();
    expect($(".run-unresolved-title")!.textContent).toContain("ghost");
    // The inherited values are absent (no def to resolve).
    expect($(".run-inherited-value")).toBeNull();
  });
});

describe("Runs tab — re-run now + resolved preview", () => {
  it("previews the resolved effective config", () => {
    openTriggerPopup(inv({ prompt: "go" }), cbs());
    tabBtn("Runs").click();
    const preview = $(".run-preview")!;
    expect(preview.textContent).toContain("deepseek-chat");
    expect(preview.textContent).toContain("go"); // effective prompt
    expect(preview.textContent).toContain("notes"); // effective MCP
  });

  it("re-runs via onRerun and reports success", async () => {
    const onRerun = vi.fn().mockResolvedValue("output");
    openTriggerPopup(inv(), cbs({ onRerun }));
    tabBtn("Runs").click();
    ($(".run-rerun-btn") as HTMLButtonElement).click();
    expect(onRerun).toHaveBeenCalledWith("standup");
    await Promise.resolve();
    await Promise.resolve();
    expect($(".run-rerun-status")!.textContent).toContain("ran");
  });

  it("disables re-run when the Agent is unresolved", () => {
    openTriggerPopup(inv({ agent: "ghost" }), cbs({ agentByName: () => null }));
    tabBtn("Runs").click();
    expect(($(".run-rerun-btn") as HTMLButtonElement).disabled).toBe(true);
  });
});

describe("History tab — Executions (the append-only executions log, R3)", () => {
  it("shows an empty Executions state when there are no executions", () => {
    openTriggerPopup(inv(), cbs({ executionsForRun: () => [] }));
    tabBtn("History").click();
    expect($(".run-executions")!.textContent).toContain("No executions yet");
  });

  it("reads the executions log: a row per Execution with its config hash", () => {
    const executionsForRun = vi.fn(() => [
      exec({ execution_id: "e2", ts: Date.now() - 500, config_hash: "b".repeat(64) }),
      exec({ execution_id: "e1", ts: Date.now() - 5000 }),
    ]);
    openTriggerPopup(inv(), cbs({ executionsForRun }));
    tabBtn("History").click();
    // The callback was asked for THIS Run's executions.
    expect(executionsForRun).toHaveBeenCalledWith("i1");
    const rows = $$(".run-execution-row");
    expect(rows.length).toBe(2);
    // Newest first carries the "most recent" tag + a short config-hash chip.
    expect(rows[0].querySelector(".run-exec-tag")!.textContent).toBe("most recent");
    expect(rows[0].querySelector(".run-exec-hash")!.textContent).toContain("cfg bbbbbbbb");
    expect(rows[0].textContent).toContain("ran");
  });
});

describe("Save / Delete / Open-in-Agents / Exit semantics (preserved from R1)", () => {
  it("Save calls onSave with the picker trigger + prompt", () => {
    const onSave = vi.fn();
    openTriggerPopup(inv({ prompt: "p" }), cbs({ onSave }));
    ($$(".modal-btn.primary").find((b) => b.textContent === "Save") as HTMLButtonElement).click();
    expect(onSave).toHaveBeenCalledTimes(1);
    const [trigger, prompt] = onSave.mock.calls[0];
    expect(trigger).toBe("daily at 09:00 UTC");
    expect(prompt).toBe("p");
  });

  it("Delete and Open-in-Agents and Exit fire their callbacks", () => {
    const onDelete = vi.fn();
    const onOpenAgents = vi.fn();
    const onClose = vi.fn();
    openTriggerPopup(inv(), cbs({ onDelete, onOpenAgents, onClose }));
    ($$(".modal-btn").find((b) => b.textContent === "Open in Agents") as HTMLButtonElement).click();
    expect(onOpenAgents).toHaveBeenCalledTimes(1);

    openTriggerPopup(inv(), cbs({ onDelete }));
    ($(".modal-btn.danger") as HTMLButtonElement).click();
    expect(onDelete).toHaveBeenCalledTimes(1);

    openTriggerPopup(inv(), cbs({ onClose }));
    ($$(".modal-btn").find((b) => b.textContent === "Exit") as HTMLButtonElement).click();
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
