// Unit tests for the I3 Invocations-table pure helpers in agentsView.ts: the
// row-model builder (flattening the replicated index + host-note lookup), the
// null-last sort, and the substring filter. These are DOM-free so they run
// fast and pin the table's data shape independent of rendering.

import { beforeEach, describe, expect, it } from "vitest";
import {
  type AgentsViewCallbacks,
  buildInvocationRows,
  filterInvocationRows,
  focusInvocationRow,
  renderAgentsView,
  setSubTab,
  sortInvocationRows,
  type InvocationRow,
} from "./agentsView";
import type { AgentDef, AgentIndex } from "./agents";
import type { Invocation } from "./api";

const inv = (over: Partial<Invocation>): Invocation => ({
  id: "i",
  agent: "standup",
  trigger: "2h",
  prompt: "p",
  host_file_id: "f1",
  last_run_ms: null,
  status: "scheduled",
  ...over,
});

describe("buildInvocationRows", () => {
  const now = Date.parse("2026-06-16T08:00:00Z");
  const titles: Record<string, string | null> = { f1: "Daily Standup", gone: null };
  const title = (id: string) => (id in titles ? titles[id] : null);

  it("flattens an invocation into a display row with a human trigger + host title", () => {
    const [row] = buildInvocationRows(
      [inv({ id: "1", trigger: "daily at 09:00 UTC", host_file_id: "f1", last_run_ms: 123 })],
      now,
      title,
    );
    expect(row.id).toBe("1");
    expect(row.trigger).toBe("Daily at 09:00 UTC");
    expect(row.hostNote).toBe("Daily Standup");
    expect(row.hostExists).toBe(true);
    expect(row.lastRunMs).toBe(123);
    expect(row.nextFireMs).toBe(now + 60 * 60 * 1000); // +1h to 09:00Z
  });

  it("marks a missing host note (orphaned handle) and falls back to its id", () => {
    const [row] = buildInvocationRows([inv({ id: "2", host_file_id: "gone" })], now, title);
    expect(row.hostExists).toBe(false);
    expect(row.hostNote).toBe("gone");
  });

  it("leaves next-fire null for a never-run interval (no anchor)", () => {
    const [row] = buildInvocationRows([inv({ trigger: "2h", last_run_ms: null })], now, title);
    expect(row.nextFireMs).toBeNull();
  });
});

const row = (over: Partial<InvocationRow>): InvocationRow => ({
  id: "i",
  agent: "a",
  hostFileId: "f",
  hostNote: "Note",
  hostExists: true,
  trigger: "every 2 hours",
  status: "scheduled",
  lastRunMs: null,
  nextFireMs: null,
  ...over,
});

describe("sortInvocationRows", () => {
  it("sorts a string column asc/desc, case-insensitively", () => {
    const rows = [row({ id: "1", agent: "Zeta" }), row({ id: "2", agent: "alpha" })];
    expect(sortInvocationRows(rows, "agent", true).map((r) => r.id)).toEqual(["2", "1"]);
    expect(sortInvocationRows(rows, "agent", false).map((r) => r.id)).toEqual(["1", "2"]);
  });

  it("sorts a numeric column and pins nulls last in BOTH directions", () => {
    const rows = [
      row({ id: "a", nextFireMs: 300 }),
      row({ id: "b", nextFireMs: null }),
      row({ id: "c", nextFireMs: 100 }),
    ];
    expect(sortInvocationRows(rows, "nextFire", true).map((r) => r.id)).toEqual(["c", "a", "b"]);
    // Desc reverses the populated rows but nulls stay last (not first).
    expect(sortInvocationRows(rows, "nextFire", false).map((r) => r.id)).toEqual(["a", "c", "b"]);
  });

  it("is stable for equal keys (preserves index order)", () => {
    const rows = [row({ id: "x", status: "scheduled" }), row({ id: "y", status: "scheduled" })];
    expect(sortInvocationRows(rows, "status", true).map((r) => r.id)).toEqual(["x", "y"]);
  });
});

describe("filterInvocationRows", () => {
  const rows = [
    row({ id: "1", agent: "standup", trigger: "Daily at 09:00 UTC", status: "scheduled" }),
    row({ id: "2", agent: "digest", trigger: "every 1 day", status: "error" }),
  ];

  it("matches a bare term across agent/trigger/host/status (case-insensitive)", () => {
    expect(filterInvocationRows(rows, "STANDUP").map((r) => r.id)).toEqual(["1"]);
    expect(filterInvocationRows(rows, "error").map((r) => r.id)).toEqual(["2"]);
    expect(filterInvocationRows(rows, "daily").map((r) => r.id)).toEqual(["1"]);
  });

  it("AND-combines multiple terms and returns all rows for an empty query", () => {
    expect(filterInvocationRows(rows, "digest day").map((r) => r.id)).toEqual(["2"]);
    expect(filterInvocationRows(rows, "digest standup")).toHaveLength(0);
    expect(filterInvocationRows(rows, "   ")).toHaveLength(2);
  });
});

// ── DOM: the Agents | Triggers sub-tabs + the Open-in-Agents deep-link ────────
// These mount the real view under jsdom and assert the sub-tab structure (which
// panel is visible), the user-facing "Triggers" relabel, and that the trigger
// deep-link activates the Triggers tab + targets the matching row.

const agentDef = (over: Partial<AgentDef> = {}): AgentDef => ({
  kind: "agent",
  name: "standup",
  model: "deepseek",
  labels: [],
  meta: {},
  version: null,
  mcpServers: [],
  instructions: "",
  fileId: "agent-file",
  path: "Agents/standup.md",
  ...over,
});

const agentIndex = (defs: AgentDef[]): AgentIndex => {
  const byName = new Map(defs.map((d) => [d.name.toLowerCase(), d]));
  return {
    all: defs,
    findAgent: (name) => byName.get(name.trim().toLowerCase()) ?? null,
    has: (name) => byName.has(name.trim().toLowerCase()),
  };
};

const callbacks = (invocations: Invocation[]): AgentsViewCallbacks => ({
  openNote: () => {},
  fileById: () => undefined,
  newAgent: () => {},
  mcpGrants: () => [],
  fleetApps: () => [],
  invocations: () => ({
    all: invocations,
    byId: (id) => invocations.find((i) => i.id === id) ?? null,
    forAgent: () => [],
  }),
  hostNoteTitle: (fileId) => (fileId === "missing" ? null : `Note ${fileId}`),
  agentByName: () => null,
});

const sampleInvocations: Invocation[] = [
  inv({ id: "inv-1", agent: "standup", host_file_id: "f1" }),
  inv({ id: "inv-2", agent: "digest", host_file_id: "f2", trigger: "daily at 09:00 UTC" }),
];

function mount(invocations = sampleInvocations) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  renderAgentsView(host, agentIndex([agentDef()]), callbacks(invocations));
  return host;
}

const panels = (host: HTMLElement) => ({
  agents: host.querySelector<HTMLElement>(".agents-panel-agents")!,
  triggers: host.querySelector<HTMLElement>(".agents-panel-triggers")!,
});

const subBtn = (host: HTMLElement, tab: "agents" | "triggers") =>
  host.querySelector<HTMLButtonElement>(`.agents-subtab-btn[data-subtab="${tab}"]`)!;

describe("Agents view sub-tabs", () => {
  beforeEach(() => {
    // Reset module-local sub-tab state + clear any persisted choice so each test
    // starts from the documented default (Agents).
    localStorage.clear();
    setSubTab("agents");
    document.body.replaceChildren();
  });

  it("renders both sub-tabs and defaults to the Agents tab", () => {
    const host = mount();
    expect(subBtn(host, "agents").textContent).toBe("Agents");
    expect(subBtn(host, "triggers").textContent).toBe("Triggers");
    expect(subBtn(host, "agents").classList.contains("active")).toBe(true);
    expect(subBtn(host, "triggers").classList.contains("active")).toBe(false);

    const { agents, triggers } = panels(host);
    expect(agents.hidden).toBe(false);
    expect(triggers.hidden).toBe(true);
  });

  it("uses the 'Triggers' label (not 'Invocations') for the section + count", () => {
    setSubTab("triggers");
    const host = mount();
    expect(host.querySelector(".invocations-title")?.textContent).toBe("Triggers");
    expect(host.textContent).not.toContain("Invocations");
    // Count noun is "triggers".
    expect(host.querySelector(".invocations-view .agents-count")?.textContent).toContain(
      "triggers",
    );
  });

  it("shows the renamed empty state when there are no triggers", () => {
    setSubTab("triggers");
    const host = mount([]);
    expect(host.querySelector(".invocations-view .agents-empty")?.textContent).toBe(
      "No scheduled triggers yet",
    );
  });

  it("switches panel visibility when a sub-tab is clicked", () => {
    const host = mount();
    subBtn(host, "triggers").click();
    // The click re-renders the view in-place; re-query the fresh nodes.
    const { agents, triggers } = panels(host);
    expect(triggers.hidden).toBe(false);
    expect(agents.hidden).toBe(true);
    expect(subBtn(host, "triggers").classList.contains("active")).toBe(true);

    subBtn(host, "agents").click();
    const after = panels(host);
    expect(after.agents.hidden).toBe(false);
    expect(after.triggers.hidden).toBe(true);
  });
});

describe("Open-in-Agents deep-link", () => {
  beforeEach(() => {
    localStorage.clear();
    setSubTab("agents");
    document.body.replaceChildren();
  });

  it("activates the Triggers sub-tab and targets the matching row", () => {
    // jsdom doesn't implement scrollIntoView nor CSS.escape; stub both so the
    // deep-link focus path (scroll the targeted row into view, after a
    // CSS.escape'd attribute query) runs without throwing.
    Element.prototype.scrollIntoView = () => {};
    (globalThis as { CSS?: { escape: (s: string) => string } }).CSS ??= {
      escape: (s: string) => s.replace(/["\\]/g, "\\$&"),
    };
    // Simulate the Trigger popup's deep-link: focusInvocationRow flips the
    // sub-tab; the subsequent render (what tabs.openAgents triggers) honours it.
    focusInvocationRow("inv-2");
    const host = mount();

    const { agents, triggers } = panels(host);
    expect(triggers.hidden).toBe(false);
    expect(agents.hidden).toBe(true);
    expect(subBtn(host, "triggers").classList.contains("active")).toBe(true);

    const target = host.querySelector<HTMLElement>('[data-invocation-id="inv-2"]');
    expect(target).not.toBeNull();
    // The flash class is added on the targeted row (then removed after a timeout).
    expect(target?.classList.contains("invocations-row-flash")).toBe(true);
  });
});
