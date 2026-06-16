// Unit tests for the inline `agent://<id>` link parser + EOF-safe hit-test (the
// scheduled-invocation handle) and the replicated-index builder. These mirror
// the component's `parse_agent_links` (apps/tangram/src/agents.rs) so the UI and
// component agree on the handle format byte-for-byte.

import { describe, expect, it } from "vitest";
import {
  agentLinkAt,
  buildAgentLink,
  buildInvocationIndex,
  formatRelativeTime,
  formatSchedule,
  nextFireMs,
  parseAgentLinks,
} from "./invocations";
import type { Invocation } from "./api";

describe("parseAgentLinks (inline agent:// handles)", () => {
  it("finds every link with its id + range, in order", () => {
    const body = "Run [⚡ standup](agent://abc123) then [⚡ digest](agent://def456).";
    const links = parseAgentLinks(body);
    expect(links.map((l) => l.id)).toEqual(["abc123", "def456"]);
    // The first link's range round-trips to the original token text.
    const first = links[0];
    expect(body.slice(first.from, first.to)).toBe("[⚡ standup](agent://abc123)");
  });

  it("ignores non-agent links and empty ids", () => {
    expect(parseAgentLinks("see agent://abc bare").length).toBe(0);
    expect(parseAgentLinks("[x](agent://)").length).toBe(0);
    expect(parseAgentLinks("[note](other://abc) [[wiki]]").length).toBe(0);
  });

  it("matches the component's id scheme via buildAgentLink", () => {
    const link = buildAgentLink("standup", "uuid-1");
    expect(link).toBe("[⚡ standup](agent://uuid-1)");
    const [parsed] = parseAgentLinks(`x ${link} y`);
    expect(parsed.id).toBe("uuid-1");
  });
});

describe("agentLinkAt (on-link hit test)", () => {
  const body = "[⚡ a](agent://id1)X"; // link occupies [0, to); trailing X
  const to = parseAgentLinks(body)[0].to;

  it("hits inside the token (opening boundary inclusive)", () => {
    expect(agentLinkAt(body, 0)?.id).toBe("id1"); // opening `[`
    expect(agentLinkAt(body, to - 1)?.id).toBe("id1"); // last char `)`
  });

  it("is null exactly at the END boundary (the EOF-click rule)", () => {
    // pos === to is "past the link" (the trailing X / caret-after position).
    expect(agentLinkAt(body, to)).toBeNull();
  });

  it("is null off any token", () => {
    expect(agentLinkAt("no links here", 3)).toBeNull();
  });
});

describe("buildInvocationIndex (replicated index)", () => {
  const inv = (id: string, agent: string, trigger: string): Invocation => ({
    id,
    agent,
    trigger,
    prompt: "p",
    host_file_id: "f",
    last_run_ms: null,
    status: "scheduled",
  });

  it("indexes by id and by agent (case-insensitive)", () => {
    const idx = buildInvocationIndex([
      inv("1", "Standup", "2h"),
      inv("2", "standup", "daily at 09:00 UTC"),
      inv("3", "Digest", "1d"),
    ]);
    expect(idx.all.length).toBe(3);
    expect(idx.byId("2")?.trigger).toBe("daily at 09:00 UTC");
    expect(idx.byId("nope")).toBeNull();
    expect(idx.forAgent("STANDUP").map((i) => i.id).sort()).toEqual(["1", "2"]);
    expect(idx.forAgent("digest").length).toBe(1);
  });

  it("tolerates an empty/absent index", () => {
    expect(buildInvocationIndex([]).all.length).toBe(0);
  });
});

// ── I3 invocations-table display helpers ─────────────────────────────────────

describe("formatSchedule (human-readable trigger summary)", () => {
  it("renders interval shorthands as 'every N <unit>'", () => {
    expect(formatSchedule("2m")).toBe("every 2 minutes");
    expect(formatSchedule("1h")).toBe("every 1 hour");
    expect(formatSchedule("3d")).toBe("every 3 days");
    expect(formatSchedule("every 2h")).toBe("every 2 hours");
    expect(formatSchedule("@hourly")).toBe("every 1 hour");
    expect(formatSchedule("@daily")).toBe("every 1 day");
  });

  it("renders calendar schedules with their time + zone", () => {
    expect(formatSchedule("daily at 09:00 UTC")).toBe("Daily at 09:00 UTC");
    expect(formatSchedule("weekly on mon,wed at 18:00 America/New_York")).toBe(
      "Weekly on Mon, Wed at 18:00 America/New_York",
    );
  });

  it("falls back to the raw string for an unparseable trigger", () => {
    expect(formatSchedule("garbage")).toBe("garbage");
    expect(formatSchedule("")).toBe("—");
  });
});

describe("nextFireMs (display projection, not scheduling)", () => {
  const MIN = 60 * 1000;
  const HOUR = 60 * MIN;

  it("projects interval from last_run (and null when never run)", () => {
    const lastRun = 1_000_000;
    expect(nextFireMs("2h", Date.now(), lastRun)).toBe(lastRun + 2 * HOUR);
    expect(nextFireMs("2h", Date.now(), null)).toBeNull();
  });

  it("projects daily to the next occurrence of the wall-clock time (UTC)", () => {
    // 2026-06-16T08:00:00Z → next "daily at 09:00 UTC" is +1h.
    const now = Date.parse("2026-06-16T08:00:00Z");
    expect(nextFireMs("daily at 09:00 UTC", now, null)).toBe(now + HOUR);
    // After the time has passed today, it rolls to tomorrow (+23h from 10:00Z).
    const after = Date.parse("2026-06-16T10:00:00Z");
    expect(nextFireMs("daily at 09:00 UTC", after, null)).toBe(after + 23 * HOUR);
  });

  it("projects weekly to the next selected day at the time", () => {
    // 2026-06-16 is a Tuesday (08:00Z). Next "weekly on wed at 09:00 UTC" is
    // tomorrow (Wed) → +25h.
    const now = Date.parse("2026-06-16T08:00:00Z");
    expect(nextFireMs("weekly on wed at 09:00 UTC", now, null)).toBe(now + 25 * HOUR);
  });

  it("returns null for an unparseable schedule", () => {
    expect(nextFireMs("nonsense", Date.now(), 123)).toBeNull();
  });
});

describe("formatRelativeTime", () => {
  const now = Date.parse("2026-06-16T12:00:00Z");
  const MIN = 60 * 1000;
  const HOUR = 60 * MIN;
  const DAY = 24 * HOUR;

  it("renders null as an em dash", () => {
    expect(formatRelativeTime(null, now)).toBe("—");
  });

  it("renders recent past as 'just now' / 'Nm ago' / 'Nh ago' / 'Nd ago'", () => {
    expect(formatRelativeTime(now - 5 * 1000, now)).toBe("just now");
    expect(formatRelativeTime(now - 5 * MIN, now)).toBe("5m ago");
    expect(formatRelativeTime(now - 3 * HOUR, now)).toBe("3h ago");
    expect(formatRelativeTime(now - 2 * DAY, now)).toBe("2d ago");
  });

  it("renders the future with an 'in ' prefix", () => {
    expect(formatRelativeTime(now + 2 * HOUR, now)).toBe("in 2h");
  });
});
