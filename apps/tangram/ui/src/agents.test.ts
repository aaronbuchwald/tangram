// Tools/MCP T1: the `mcp_servers:` request parser + the request-hash helper.
// The hash MUST match `mcp_request_hash` in `apps/tangram/src/agents.rs`
// (same canonicalization, same NUL separator, same FNV-1a) so the user's
// approval (UI) binds to the same hash the component guards on.

import { describe, expect, it } from "vitest";
import { canonicalServers, mcpRequestHash, parseAgent } from "./agents";
import type { MdFile } from "./api";

function file(body: string): MdFile {
  return {
    id: "f1",
    path: "agents/x.md",
    body,
    created_at_ms: 0,
    updated_at_ms: null,
  };
}

describe("mcp_servers request parsing", () => {
  it("kind: agent declares a canonicalized request", () => {
    const def = parseAgent(
      file(
        "---\nkind: agent\nname: planner\nmcp_servers: [Nutrition, notes, nutrition]\n---\nDo it.",
      ),
    );
    expect(def?.mcpServers).toEqual(["notes", "nutrition"]);
  });

  it("kind: skill parses-and-ignores mcp_servers", () => {
    const def = parseAgent(
      file("---\nkind: skill\nname: sum\nmcp_servers: [nutrition]\n---\nb"),
    );
    expect(def?.mcpServers).toEqual([]);
  });

  it("an agent without mcp_servers requests nothing", () => {
    const def = parseAgent(file("---\nkind: agent\nname: plain\n---\nb"));
    expect(def?.mcpServers).toEqual([]);
  });
});

describe("canonicalServers + mcpRequestHash", () => {
  it("canonicalizes (trim/lowercase/dedupe/sort)", () => {
    expect(canonicalServers([" Nutrition ", "notes", "NUTRITION", ""])).toEqual([
      "notes",
      "nutrition",
    ]);
  });

  it("is order-insensitive and set-sensitive", () => {
    const a = mcpRequestHash(["nutrition", "notes"]);
    const b = mcpRequestHash(["NOTES", "Nutrition"]);
    expect(a).toEqual(b);
    expect(mcpRequestHash(["notes", "nutrition", "shell"])).not.toEqual(a);
  });

  it("matches an independent FNV-1a over the canonical NUL-joined set", () => {
    // FNV-1a over the canonical servers joined by NUL: "notes" + NUL +
    // "nutrition". An independent reference (not the helper) pins the wire value
    // so a drift is caught and the Rust/TS hashes stay in lockstep.
    const nul = String.fromCharCode(0);
    expect(mcpRequestHash(["nutrition", "notes"])).toEqual(
      fnv1aRef(`notes${nul}nutrition`),
    );
  });
});

// A reference FNV-1a (independent of the implementation under test) so the
// parity test does not just compare the helper against itself.
function fnv1aRef(s: string): string {
  const OFFSET = 0xcbf29ce484222325n;
  const PRIME = 0x00000100000001b3n;
  const MASK = 0xffffffffffffffffn;
  let h = OFFSET;
  for (const b of new TextEncoder().encode(s)) {
    h ^= BigInt(b);
    h = (h * PRIME) & MASK;
  }
  return h.toString(16).padStart(16, "0");
}
