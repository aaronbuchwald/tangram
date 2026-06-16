// Unit tests for the I3 Invocations-table pure helpers in agentsView.ts: the
// row-model builder (flattening the replicated index + host-note lookup), the
// null-last sort, and the substring filter. These are DOM-free so they run
// fast and pin the table's data shape independent of rendering.

import { describe, expect, it } from "vitest";
import {
  buildInvocationRows,
  filterInvocationRows,
  sortInvocationRows,
  type InvocationRow,
} from "./agentsView";
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
