// Unit tests for the Run editor's inheritance engine (embedded-runs R2,
// runConfig.ts): the additive inherited/added/override classification, the
// inherited∪added list merge, and the resolved effective-config preview. DOM-
// free + pure so they pin the merge semantics independent of rendering.

import { describe, expect, it } from "vitest";
import {
  effectiveConfig,
  mergeList,
  resolveRunConfig,
} from "./runConfig";
import { DEFAULT_MODEL, type AgentDef } from "./agents";
import type { Invocation } from "./api";

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
  labels: ["daily", "team"],
  meta: {},
  version: null,
  mcpServers: ["notes", "nutrition"],
  instructions: "You are the standup assistant.",
  fileId: "fd",
  path: "agents/standup.md",
  ...over,
});

describe("mergeList — inherited ∪ added", () => {
  it("classifies the base as inherited and new items as added", () => {
    const m = mergeList(["notes"], ["nutrition"]);
    expect(m.inherited).toEqual(["notes"]);
    expect(m.added).toEqual(["nutrition"]);
    expect(m.effective).toEqual(["notes", "nutrition"]);
  });

  it("drops an 'addition' already present in the base (not additive)", () => {
    const m = mergeList(["Notes"], ["notes", "extra"]);
    expect(m.inherited).toEqual(["notes"]); // canonicalized (lowercased)
    expect(m.added).toEqual(["extra"]); // 'notes' already inherited
    expect(m.effective).toEqual(["extra", "notes"]); // sorted, de-duped
  });

  it("canonicalizes (trim/lowercase/dedupe/sort) both sides", () => {
    const m = mergeList([" B ", "a", "a"], ["C", "b"]);
    expect(m.inherited).toEqual(["a", "b"]);
    expect(m.added).toEqual(["c"]);
    expect(m.effective).toEqual(["a", "b", "c"]);
  });
});

describe("resolveRunConfig — visible additive inheritance", () => {
  it("marks the Agent's instructions + model as inherited", () => {
    const cfg = resolveRunConfig(inv(), def());
    expect(cfg.resolved).toBe(true);
    expect(cfg.instructions.origin).toBe("inherited");
    expect(cfg.instructions.value).toBe("You are the standup assistant.");
    expect(cfg.model.origin).toBe("inherited");
    expect(cfg.model.value).toBe("deepseek-chat");
  });

  it("marks a non-empty one-time prompt as ADDED (layered on top)", () => {
    const cfg = resolveRunConfig(inv({ prompt: "extra context" }), def());
    expect(cfg.prompt.origin).toBe("added");
    expect(cfg.prompt.value).toBe("extra context");
  });

  it("treats an empty prompt as pure inheritance (not added)", () => {
    const cfg = resolveRunConfig(inv({ prompt: "  " }), def());
    expect(cfg.prompt.origin).toBe("inherited");
    expect(cfg.prompt.value).toBe("");
  });

  it("marks a set schedule as ADDED (purely run-scoped — agent has none)", () => {
    const cfg = resolveRunConfig(inv({ trigger: "2h" }), def());
    expect(cfg.schedule.origin).toBe("added");
    expect(cfg.schedule.value).toBe("2h");
  });

  it("inherits the Agent's base MCP servers + tags (no run additions today)", () => {
    const cfg = resolveRunConfig(inv(), def());
    expect(cfg.mcpServers.inherited).toEqual(["notes", "nutrition"]);
    expect(cfg.mcpServers.added).toEqual([]);
    expect(cfg.tags.inherited).toEqual(["daily", "team"]);
    expect(cfg.tags.added).toEqual([]);
  });

  it("layers explicit run-scoped additions onto the inherited base", () => {
    const cfg = resolveRunConfig(inv(), def(), {
      mcpServers: ["feedback", "notes"],
      tags: ["urgent"],
    });
    expect(cfg.mcpServers.inherited).toEqual(["notes", "nutrition"]);
    expect(cfg.mcpServers.added).toEqual(["feedback"]); // 'notes' already inherited
    expect(cfg.mcpServers.effective).toEqual(["feedback", "notes", "nutrition"]);
    expect(cfg.tags.added).toEqual(["urgent"]);
  });

  it("carries the Run-scoped mounted files (embedded-runs R4), order-preserved + de-duped", () => {
    // Purely Run-scoped + additive; canonicalized (trim, blank-drop, de-dupe)
    // but NOT sorted — the order the user picked is the injection order.
    const cfg = resolveRunConfig(
      inv({ files: [" notes/b.md ", "notes/a.md", "notes/b.md", "  "] }),
      def(),
    );
    expect(cfg.mountedFiles).toEqual(["notes/b.md", "notes/a.md"]);
  });

  it("defaults mounted files to empty when the Run has none", () => {
    expect(resolveRunConfig(inv(), def()).mountedFiles).toEqual([]);
  });

  it("surfaces an UNRESOLVED state when the named Agent is missing", () => {
    const cfg = resolveRunConfig(inv({ agent: "ghost" }), null);
    expect(cfg.resolved).toBe(false);
    expect(cfg.agentName).toBe("ghost");
    expect(cfg.instructions.value).toBe("");
    expect(cfg.model.value).toBe(""); // no default when unresolved
    expect(cfg.mcpServers.inherited).toEqual([]);
  });

  it("falls back to the default model when the Agent omits one", () => {
    const cfg = resolveRunConfig(inv(), def({ model: "" }));
    expect(cfg.model.value).toBe(DEFAULT_MODEL);
  });
});

describe("effectiveConfig — resolved-effective preview (inherited ⊕ overrides)", () => {
  it("projects the exact config a run would use", () => {
    const cfg = resolveRunConfig(
      inv({ prompt: "go", trigger: "2h", files: ["notes/a.md"] }),
      def(),
      { mcpServers: ["feedback"] },
    );
    const eff = effectiveConfig(cfg);
    expect(eff).toEqual({
      resolved: true,
      agentName: "standup",
      model: "deepseek-chat",
      instructions: "You are the standup assistant.",
      prompt: "go",
      schedule: "2h",
      mcpServers: ["feedback", "notes", "nutrition"],
      tags: ["daily", "team"],
      mountedFiles: ["notes/a.md"],
    });
  });

  it("carries the unresolved flag through the preview", () => {
    const eff = effectiveConfig(resolveRunConfig(inv({ agent: "ghost" }), null));
    expect(eff.resolved).toBe(false);
    expect(eff.mcpServers).toEqual([]);
  });
});
