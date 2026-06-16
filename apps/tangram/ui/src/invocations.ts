// Agent INVOCATIONS, parsed from ```agent fenced blocks in any vault note (R1).
//
// R1 — the trigger belongs to the INVOCATION, not the definition. A definition
// (agents.ts) is a pure capability (kind/name/model/instructions/labels); the
// thing that decides WHEN and HOW an agent runs is a durable instance — a fenced
// block inside a markdown file — that links to a definition via `use:` and owns
// the `trigger` + `prompt`:
//
//     ```agent
//     use: <definition-name>
//     trigger: cron every 1h          # or "one-time"
//     prompt: <prompt text, may span
//     multiple lines until the fence>
//     ```
//
// The block is the source of truth and is INDEXED (derived from the file text),
// so editing or removing it self-cleans — no stray refs. Each invocation gets a
// stable `invocationId` = a hash of {hostFileId + use + trigger + prompt}; an
// unedited block keeps its id, editing it produces a new id, removing it drops
// it. UI display of invocations is not required for R1 — the component is the
// consumer; this module just provides the parser/index so the UI and the
// component (apps/tangram/src/agents.rs) agree on the format BYTE-FOR-BYTE.

import type { MdFile } from "./api";

/** One parsed ```agent invocation block. */
export interface Invocation {
  /** The host file this block lives in (its stable id). */
  hostFileId: string;
  /** The definition this invocation runs (the `use:` field — a def name). */
  use: string;
  /** The raw `trigger:` text, e.g. `cron every 1h`, `cron @daily`, `one-time`. */
  trigger: string;
  /** The `prompt:` text (may span multiple lines until the closing fence). */
  prompt: string;
  /** Stable hash of {hostFileId + use + trigger + prompt} — stray-ref-safe. */
  invocationId: string;
}

/**
 * A stable id for an invocation: a hex 64-bit FNV-1a hash of
 * `hostFileId\0use\0trigger\0prompt`. This mirrors `invocation_id` in
 * `apps/tangram/src/agents.rs` EXACTLY (same fields, same NUL separator, same
 * FNV-1a constants, same 16-hex-digit zero-padded output) so the UI and the
 * component derive identical ids for the same block.
 */
export function invocationId(
  hostFileId: string,
  use: string,
  trigger: string,
  prompt: string,
): string {
  const key = `${hostFileId}\0${use}\0${trigger}\0${prompt}`;
  return fnv1aHex(key);
}

// 64-bit FNV-1a over the UTF-8 bytes, lowercase hex (16 digits, zero-padded).
// BigInt keeps the full 64-bit width (Number would lose precision past 2^53).
function fnv1aHex(s: string): string {
  const OFFSET = 0xcbf29ce484222325n;
  const PRIME = 0x00000100000001b3n;
  const MASK = 0xffffffffffffffffn;
  const bytes = new TextEncoder().encode(s);
  let hash = OFFSET;
  for (const b of bytes) {
    hash ^= BigInt(b);
    hash = (hash * PRIME) & MASK;
  }
  return hash.toString(16).padStart(16, "0");
}

/**
 * Parse every ```agent invocation block in `body`, in document order. Mirrors
 * `parse_invocations` in `apps/tangram/src/agents.rs`: `use`/`trigger`/`prompt`
 * are flat `key: value` lines at the top of the block; everything after the
 * `prompt:` line (until the closing fence) is part of the prompt (multi-line). A
 * block missing `use` is skipped (it cannot resolve a definition); `trigger`
 * defaults to `one-time`.
 */
export function parseInvocations(hostFileId: string, body: string): Invocation[] {
  const out: Invocation[] = [];
  const lines = (body ?? "").split("\n");
  let i = 0;
  while (i < lines.length) {
    if (lines[i].trim() !== "```agent") {
      i++;
      continue;
    }
    // Collect block lines until the closing fence.
    let j = i + 1;
    const block: string[] = [];
    let closed = false;
    while (j < lines.length) {
      if (lines[j].trim() === "```") {
        closed = true;
        break;
      }
      block.push(lines[j]);
      j++;
    }
    const inv = parseInvocationBlock(hostFileId, block);
    if (inv) out.push(inv);
    i = closed ? j + 1 : j;
  }
  return out;
}

/** Parse the inner lines of one ```agent block; null when `use` is missing. */
function parseInvocationBlock(hostFileId: string, block: string[]): Invocation | null {
  let use: string | null = null;
  let trigger: string | null = null;
  let prompt: string | null = null;

  for (let k = 0; k < block.length; k++) {
    const line = block[k];
    const idx = line.indexOf(":");
    if (idx === -1) continue;
    const key = line.slice(0, idx).trim().toLowerCase();
    const val = line.slice(idx + 1).trim();
    if (key === "use" && use === null) {
      use = val;
    } else if (key === "trigger" && trigger === null) {
      trigger = val;
    } else if (key === "prompt" && prompt === null) {
      // The prompt runs from this line's value to the end of the block.
      const parts = [val, ...block.slice(k + 1)];
      prompt = parts.join("\n").trim();
      break;
    }
  }

  const useTrimmed = (use ?? "").trim();
  if (useTrimmed.length === 0) return null;
  const triggerVal = trigger ?? "one-time";
  const promptVal = prompt ?? "";
  return {
    hostFileId,
    use: useTrimmed,
    trigger: triggerVal,
    prompt: promptVal,
    invocationId: invocationId(hostFileId, useTrimmed, triggerVal, promptVal),
  };
}

/** A read-only index of the vault's agent invocations. */
export interface InvocationIndex {
  /** All parsed invocations, in input (file, then document) order. */
  readonly all: Invocation[];
  /** Look up an invocation by its stable id. */
  byId(id: string): Invocation | null;
  /** Every invocation whose `use:` names the given definition (case-insensitive). */
  forDef(name: string): Invocation[];
}

/**
 * Build the invocation index over the current vault files. Rebuilt on each vault
 * state alongside the agent/link indexes (the single rebuild point in
 * `main.ts`'s `onVaultState`). Because it is derived from each file's body, an
 * edited/removed block self-cleans on the next state.
 */
export function buildInvocationIndex(files: MdFile[]): InvocationIndex {
  const all: Invocation[] = [];
  const byId = new Map<string, Invocation>();
  for (const f of files) {
    for (const inv of parseInvocations(f.id, f.body ?? "")) {
      all.push(inv);
      byId.set(inv.invocationId, inv);
    }
  }
  return {
    all,
    byId: (id) => byId.get(id) ?? null,
    forDef: (name) => {
      const needle = name.trim().toLowerCase();
      return all.filter((inv) => inv.use.trim().toLowerCase() === needle);
    },
  };
}

/**
 * Render a durable ```agent block (the cron-invocation text written into the
 * file when the user picks a non-one-time trigger in the run popup). The shape
 * matches `parseInvocations` above and the component's parser exactly.
 */
export function buildInvocationBlock(
  use: string,
  trigger: string,
  prompt: string,
): string {
  return ["```agent", `use: ${use}`, `trigger: ${trigger}`, `prompt: ${prompt}`, "```"].join(
    "\n",
  );
}
