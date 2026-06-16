// Unit tests for the inline `agent://<id>` link parser + EOF-safe hit-test (the
// scheduled-invocation handle) and the replicated-index builder. These mirror
// the component's `parse_agent_links` (apps/tangram/src/agents.rs) so the UI and
// component agree on the handle format byte-for-byte.

import { describe, expect, it } from "vitest";
import {
  agentLinkAt,
  buildAgentLink,
  buildInvocationIndex,
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
