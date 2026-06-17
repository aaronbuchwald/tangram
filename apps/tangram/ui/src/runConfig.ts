// The Run editor's inheritance engine (embedded-runs R2): the pure computation
// behind the Config tab's **visible additive inheritance** and the Runs tab's
// **resolved effective config** preview.
//
// The two-layer model (docs/design/embedded-runs.md §1, LOCKED): an **Agent** is
// the reusable definition (instructions / model / base MCP servers / tags); a
// **Run** is a bound instance that references the Agent by name and *layers*
// context additively on top — its one-time prompt, its schedule, and (a later
// checkpoint) additive MCP grants / tags scoped to just this Run.
//
// "Additive inheritance, visibly distinguished" means every field of the
// resolved config is classified as one of three origins so the UI can render
// each differently:
//
//   - "inherited" — comes straight from the Agent, the Run does not touch it
//     (greyed + read-only in the editor).
//   - "added"     — a Run-scoped value layered ON TOP of the inheritance, with
//     no inherited counterpart it replaces (highlighted as the Run's own).
//   - "override"  — a Run-scoped value that REPLACES an inherited one (flagged
//     with an "overrides agent" badge so the divergence is explicit).
//
// Versioning is deferred (§4): a Run resolves its Agent purely by NAME. When the
// named Agent is missing from the index the inheritance is UNRESOLVED — the
// editor must surface that rather than silently showing an empty inheritance.
//
// This module is intentionally DOM-free + pure so the merge/override and the
// effective-config preview can be unit-tested directly (tests pin the
// inherited/added/override classification and the resolved preview). The Run
// record's data model is unchanged (no component change) — the only Run-scoped
// fields that exist today are the prompt and the schedule; the additive
// MCP/tags lanes are rendered as inheritance-only with a "scoped additions land
// later" affordance until the data model carries them (R3+).

import { DEFAULT_MODEL, type AgentDef } from "./agents";
import type { Invocation } from "./api";
import { canonicalServers } from "./agents";

/** How a resolved field's value was arrived at, for the visible distinction. */
export type FieldOrigin = "inherited" | "added" | "override";

/** One resolved scalar field (system prompt, model) with its origin + the
 *  inherited value it replaced (only meaningful for an "override"). */
export interface ResolvedField {
  origin: FieldOrigin;
  /** The effective value a run would use. */
  value: string;
  /** The inherited value (the Agent's), kept so an override can show the
   *  before/after. Empty string when the Agent contributes nothing. */
  inherited: string;
}

/** One resolved list field (MCP servers, tags): the inherited base plus the
 *  Run-scoped additions, kept separate so the UI can chip them differently. */
export interface ResolvedList {
  /** The Agent's base set (inherited, read-only). */
  inherited: string[];
  /** The Run-scoped additions layered on top (the `+` items). Empty until the
   *  Run data model carries scoped grants/tags (R3+). */
  added: string[];
  /** The merged effective set (inherited ∪ added), de-duplicated + sorted —
   *  exactly what a run would resolve to. */
  effective: string[];
}

/** The fully resolved Run config: the inherited Agent config layered with the
 *  Run-scoped context, every field tagged with its origin. */
export interface ResolvedRunConfig {
  /** Whether the Run's named Agent was found in the index. When false the
   *  inheritance is UNRESOLVED and every inherited field is empty. */
  resolved: boolean;
  /** The Agent name the Run references (verbatim from the Run record). */
  agentName: string;
  /** The system prompt / instructions (inherited from the Agent; a Run cannot
   *  override it in the current data model, so always "inherited" when present). */
  instructions: ResolvedField;
  /** The model (inherited from the Agent; "inherited" in the current data
   *  model — no per-Run model override field exists yet). */
  model: ResolvedField;
  /** The one-time prompt — a Run-scoped value layered on top of the Agent's
   *  instructions. "added" when non-empty, "inherited" (empty) for pure
   *  inheritance (the Run runs the Agent with no extra prompt). */
  prompt: ResolvedField;
  /** The schedule trigger — purely Run-scoped (an Agent carries no schedule),
   *  so "added" when set. Empty trigger ⇒ unscheduled. */
  schedule: ResolvedField;
  /** Base MCP servers from the Agent + any Run-scoped additions. */
  mcpServers: ResolvedList;
  /** Tags: the Agent's labels + any Run-scoped additions. */
  tags: ResolvedList;
  /** Run-scoped mounted files (embedded-runs R4): vault file paths the Run
   *  mounts. Purely Run-scoped + additive (an Agent carries no mounts), so the
   *  whole set is the Run's own — rendered in the "THIS RUN" section and folded
   *  into the resolved effective config + the component's config hash. */
  mountedFiles: string[];
}

/** De-duplicate (case-insensitively) + sort a string list, dropping blanks —
 *  the same canonicalization the MCP request uses, reused for tags too so the
 *  effective set is stable. */
function canon(items: string[]): string[] {
  return canonicalServers(items);
}

/** Merge an inherited base list with Run-scoped additions into a classified
 *  {inherited, added, effective}. An "addition" already present in the base is
 *  dropped from `added` (it is not additive — it is already inherited). */
export function mergeList(base: string[], additions: string[]): ResolvedList {
  const inherited = canon(base);
  const have = new Set(inherited.map((s) => s.toLowerCase()));
  const added = canon(additions).filter((s) => !have.has(s.toLowerCase()));
  return {
    inherited,
    added,
    effective: canon([...inherited, ...added]),
  };
}

/** Resolve a Run's full config against its (possibly missing) Agent definition,
 *  classifying every field's origin for the visible-inheritance render and the
 *  effective-config preview.
 *
 *  `def` is the Agent resolved by the Run's `agent` name (null ⇒ unresolved).
 *  The Run's `prompt` is a Run-scoped one-time prompt (additive over the Agent's
 *  instructions, NOT a replacement). The `trigger` is purely Run-scoped. There
 *  are no per-Run MCP/tags additions in the current data model, so those lists
 *  carry only the inherited base (the `additions` args default to empty); the
 *  merge supports additions so a later data-model change slots straight in. */
export function resolveRunConfig(
  inv: Invocation,
  def: AgentDef | null,
  additions: { mcpServers?: string[]; tags?: string[] } = {},
): ResolvedRunConfig {
  const resolved = def !== null;
  const instructions = def?.instructions.trim() ?? "";
  const model = def?.model.trim() || (resolved ? DEFAULT_MODEL : "");
  const prompt = inv.prompt.trim();
  const schedule = inv.trigger.trim();

  return {
    resolved,
    agentName: inv.agent,
    // Instructions: inherited from the Agent, never overridden by the Run today.
    instructions: { origin: "inherited", value: instructions, inherited: instructions },
    // Model: inherited (no per-Run model field exists yet).
    model: { origin: "inherited", value: model, inherited: model },
    // One-time prompt: Run-scoped, layered on top — "added" when present, an
    // empty prompt is pure inheritance (run the Agent with no extra prompt).
    prompt: {
      origin: prompt.length > 0 ? "added" : "inherited",
      value: prompt,
      inherited: "",
    },
    // Schedule: purely Run-scoped (an Agent has no schedule), so "added".
    schedule: {
      origin: schedule.length > 0 ? "added" : "inherited",
      value: schedule,
      inherited: "",
    },
    mcpServers: mergeList(def?.mcpServers ?? [], additions.mcpServers ?? []),
    tags: mergeList(def?.labels ?? [], additions.tags ?? []),
    // Run-scoped mounted files: trim + drop blanks + de-dupe, ORDER PRESERVED
    // (the component injects in the Run's stored order; the hash is order-aware),
    // so this does NOT use the canonicalize-and-sort path.
    mountedFiles: dedupePreserveOrder(inv.files ?? []),
  };
}

/** Trim, drop blanks, and de-duplicate a path list while PRESERVING first-seen
 *  order — the same canonicalization the component applies to a Run's mounted
 *  files (`canonical_mounted_files` in lib.rs). Order matters for mounts (it is
 *  the injection order + part of the config hash), so this is NOT sorted. */
function dedupePreserveOrder(paths: string[]): string[] {
  const out: string[] = [];
  for (const raw of paths) {
    const p = raw.trim();
    if (p.length > 0 && !out.includes(p)) out.push(p);
  }
  return out;
}

/** A flat, display-ready preview of the EFFECTIVE config a run would use
 *  (inherited ⊕ Run overrides) — the Runs tab's read-only resolved preview.
 *  Pure over a `ResolvedRunConfig` so it can be asserted in tests. */
export interface EffectiveConfigPreview {
  resolved: boolean;
  agentName: string;
  model: string;
  /** The effective system prompt sent to the model (the Agent's instructions). */
  instructions: string;
  /** The effective user prompt (the Run's one-time prompt, empty ⇒ a default
   *  "run now" style prompt is used by the component). */
  prompt: string;
  /** The effective schedule trigger (empty ⇒ unscheduled). */
  schedule: string;
  /** The effective MCP servers (inherited ∪ added). */
  mcpServers: string[];
  /** The effective tags (inherited ∪ added). */
  tags: string[];
  /** The Run-scoped mounted files (embedded-runs R4) the component injects. */
  mountedFiles: string[];
}

/** Project a resolved config to the flat effective preview — the exact config a
 *  run would use. */
export function effectiveConfig(cfg: ResolvedRunConfig): EffectiveConfigPreview {
  return {
    resolved: cfg.resolved,
    agentName: cfg.agentName,
    model: cfg.model.value,
    instructions: cfg.instructions.value,
    prompt: cfg.prompt.value,
    schedule: cfg.schedule.value,
    mcpServers: cfg.mcpServers.effective,
    tags: cfg.tags.effective,
    mountedFiles: cfg.mountedFiles,
  };
}
