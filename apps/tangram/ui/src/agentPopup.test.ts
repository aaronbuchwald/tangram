// DOM tests for the run popup's submit (embedded-runs R3 one-time/scheduled
// unification): One-time emits the `once` trigger and Schedule emits a recurring
// trigger — both via the single `onSubmit(trigger, prompt)` path (the caller
// turns that into a chip + index entry). There is no longer a separate run-now
// path.

import { afterEach, describe, expect, it, vi } from "vitest";
import { openAgentPopup } from "./agentPopup";
import type { AgentDef } from "./agents";

const def = (over: Partial<AgentDef> = {}): AgentDef => ({
  kind: "skill",
  name: "standup",
  model: "deepseek-chat",
  labels: [],
  meta: {},
  version: null,
  mcpServers: [],
  instructions: "Write a status.",
  fileId: "fd",
  path: "agents/standup.md",
  ...over,
});

const $ = (sel: string) => document.querySelector(sel) as HTMLElement | null;
const $$ = (sel: string) => Array.from(document.querySelectorAll(sel)) as HTMLElement[];
const seg = (label: string) =>
  $$(".agent-seg-btn").find((b) => b.textContent === label) as HTMLButtonElement;
const submitBtn = () =>
  $$(".modal-btn").find((b) => b.textContent === "Submit") as HTMLButtonElement;

afterEach(() => {
  for (const o of $$(".modal-overlay")) o.remove();
});

describe("run popup submit — one-time/scheduled unification (R3)", () => {
  it("One-time submit emits the `once` trigger (a chip + index entry, not run-now)", () => {
    const onSubmit = vi.fn();
    openAgentPopup(def(), { onSubmit, onClose: () => {} });
    // One-time is the default mode.
    const input = $(".agent-input") as HTMLTextAreaElement;
    input.value = "Summarize today.";
    input.dispatchEvent(new Event("input"));
    submitBtn().click();
    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit).toHaveBeenCalledWith("once", "Summarize today.");
  });

  it("Schedule submit emits the recurrence picker's trigger", () => {
    const onSubmit = vi.fn();
    openAgentPopup(def(), { onSubmit, onClose: () => {} });
    const input = $(".agent-input") as HTMLTextAreaElement;
    input.value = "Daily digest.";
    input.dispatchEvent(new Event("input"));
    // Switch to Schedule → the picker reveals (defaults to Daily).
    seg("Schedule").click();
    submitBtn().click();
    expect(onSubmit).toHaveBeenCalledTimes(1);
    const [trigger, prompt] = onSubmit.mock.calls[0];
    expect(trigger).toMatch(/^daily at \d{2}:\d{2} /);
    expect(prompt).toBe("Daily digest.");
  });

  it("submit is disabled until a prompt is entered", () => {
    openAgentPopup(def(), { onSubmit: () => {}, onClose: () => {} });
    expect(submitBtn().disabled).toBe(true);
    const input = $(".agent-input") as HTMLTextAreaElement;
    input.value = "go";
    input.dispatchEvent(new Event("input"));
    expect(submitBtn().disabled).toBe(false);
  });
});
