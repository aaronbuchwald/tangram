// Backend client for the tangram shell.
//
// All paths are RELATIVE — the shell is prefix-mounted under `/tangram/`
// (and could be served under a tenant namespace), so absolute paths would
// break embedding. The vault's own surface is at `api/...`; the host's
// fleet endpoint lives one level up at `../api/fleet`.

export interface MdFile {
  id: string;
  path: string;
  body: string;
  created_at_ms: number;
  updated_at_ms: number | null;
}

/** Tools/MCP T1: the user's recorded decision on one `kind: agent`
 *  definition's `mcp_servers:` access request. Mirrors `McpGrant` in
 *  `apps/tangram/src/lib.rs`; carried on the vault state frame (the SSE
 *  `state` event serializes the full model). `status` is `"pending" |
 *  "approved" | "denied"` as stored; the UI derives a `"stale"` view when the
 *  def's request hash no longer matches `requested_hash`. */
export interface McpGrant {
  agent: string;
  requested: string[];
  requested_hash: string;
  approved: string[];
  status: string;
  updated_at_ms: number;
}

/** One SCHEDULED agent invocation from the replicated index. Mirrors
 *  `Invocation` in `apps/tangram/src/lib.rs`; carried on the vault state frame.
 *  The inline `[⚡ <agent>](agent://<id>)` link in a note is just the handle
 *  (`{id, agent}`); this record owns trigger/prompt/last-run/status. */
export interface Invocation {
  id: string;
  agent: string;
  trigger: string;
  prompt: string;
  host_file_id: string;
  last_run_ms: number | null;
  status: string;
  /** Run-scoped mounted files (embedded-runs R4): vault file PATHS whose
   *  contents are injected into the agent's context at run time. Absent on docs
   *  written by older binaries (treat as []). A Run-scoped, additive field. */
  files?: string[] | null;
}

/** One execution of a Run (embedded-runs R3) — the append-only executions log
 *  entry. Mirrors `Execution` in `apps/tangram/src/lib.rs`; carried on the vault
 *  state frame. `config_hash` is the sha256 of the resolved effective config
 *  (Agent ⊕ Run overrides) at run time; `output_block_id` is the callout's
 *  block id for deep-linking. */
export interface Execution {
  execution_id: string;
  run_id: string;
  agent: string;
  ts: number;
  status: string;
  model: string;
  output_block_id: string;
  config_hash: string;
}

/** One typed graph edge on a SmartObject (Smart Objects SO1). Mirrors `ObjLink`
 *  in `apps/tangram/src/lib.rs`. `target` is an object id; `url` is an optional
 *  external href. */
export interface ObjLink {
  rel: string;
  target: string;
  url?: string | null;
}

/** The derived-role descriptor on a SmartObject (Smart Objects SO2). Mirrors
 *  `DeriveSpec` in `apps/tangram/src/lib.rs`. When present, the object's `data`
 *  is COMPUTED by the reactivity engine from its `deps` (dependency object ids),
 *  not written; `kind` selects the per-type computation (e.g. `rollup`). */
export interface DeriveSpec {
  kind: string;
  deps: string[];
  /** Optional opaque params for the kind (e.g. rollup's
   *  `{"op":"sum","field":"qty"}`). */
  params?: string | null;
}

/** One smart object from the replicated object store. The inline
 *  `[<label>](obj://<id>)` chip in a note is just the handle (`{id}`); this
 *  record owns `type`/`data`/`links`/`render`. Mirrors `SmartObject` in
 *  `apps/tangram/src/lib.rs`; carried on the vault state frame. SO2 adds the
 *  optional `derive` (the derived-role wiring) + `derive_error` (the cached
 *  cycle/error state); a plain object carries neither. The Rust field is
 *  `obj_type`, serialized as `type` on the wire. */
export interface SmartObject {
  id: string;
  type: string;
  data: string;
  links: ObjLink[];
  render: string;
  /** SO2: the derived-role wiring. Absent/null ⇒ a plain object (inert data). */
  derive?: DeriveSpec | null;
  /** SO2: a cached error (dependency cycle / unknown kind) the engine set on a
   *  broken derived object; absent/null ⇒ no error. */
  derive_error?: string | null;
}

/** One entry in the smart-object type registry (Smart Objects SO1) — a type the
 *  `@` picker offers. Mirrors `ObjectType` in `apps/tangram/src/lib.rs`. */
export interface ObjectType {
  name: string;
  label: string;
  render: string;
}

export interface VaultState {
  files: MdFile[];
  /** Present on documents written by this binary or newer; absent (treat as
   *  []) on older docs. */
  mcp_grants?: McpGrant[] | null;
  /** The replicated scheduled-invocation index (the redesign). Absent on older
   *  docs (treat as []). */
  invocations?: Invocation[] | null;
  /** The replicated append-only executions log (embedded-runs R3). Absent on
   *  older docs (treat as []). */
  executions?: Execution[] | null;
  /** The replicated smart-object store (Smart Objects SO1). Absent on older docs
   *  (treat as []). */
  objects?: SmartObject[] | null;
}

// The shell's own app name on the host. The shell is the outer container and
// must never appear in its own selectable APPS list (opening it would nest
// tangram inside tangram). The host's `/api/fleet` payload carries no explicit
// "this is the shell" flag, so the name is the identifier — kept here as the
// single source of truth and reused wherever the self-entry must be excluded.
export const SHELL_APP = "tangram";

export interface FleetApp {
  name: string;
  // "file" = a bootstrap app from apps.toml (host-owned, not managed here);
  // "registry" = installed via the registry app, so the shell can
  // enable/disable/remove it through the registry's bearer-gated actions.
  source: "file" | "registry";
  registry: boolean;
  require_auth: boolean;
  enabled: boolean;
  running: boolean;
  healthy: boolean;
  error: string | null;
}

export interface Fleet {
  apps: FleetApp[];
  gateway: unknown;
}

async function postAction(name: string, args: unknown): Promise<unknown> {
  const res = await fetch(`api/actions/${name}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(args ?? {}),
  });
  const json = (await res.json()) as { result?: unknown; error?: string };
  if (!res.ok) {
    throw new Error(json.error ?? `action ${name} failed (${res.status})`);
  }
  return json.result;
}

export const vault = {
  createFile: (path: string, body: string) =>
    postAction("create_file", { path, body }) as Promise<string>,
  writeFile: (id: string, body: string) =>
    postAction("write_file", { id, body }) as Promise<null>,
  renameFile: (id: string, new_path: string) =>
    postAction("rename_file", { id, new_path }) as Promise<null>,
  deleteFile: (id: string) => postAction("delete_file", { id }) as Promise<null>,
  createFolder: (path: string) =>
    postAction("create_folder", { path }) as Promise<null>,
  renameFolder: (path: string, new_path: string) =>
    postAction("rename_folder", { path, new_path }) as Promise<null>,
  deleteFolder: (path: string) =>
    postAction("delete_folder", { path }) as Promise<null>,
  // Scheduled invocations (the redesign): the replicated index keyed by the
  // UUID embedded in the note's inline `agent://<id>` link. The UI mints the id,
  // inserts the link, and creates the entry; the Trigger popup edits/deletes it.
  createInvocation: (
    id: string,
    agent: string,
    trigger: string,
    prompt: string,
    host_file_id: string,
    files: string[] = [],
  ) =>
    postAction("create_invocation", {
      id,
      agent,
      trigger,
      prompt,
      host_file_id,
      files,
    }) as Promise<null>,
  // Run-scoped mounted files (embedded-runs R4): `files` is the vault file path
  // set the Run mounts; the component injects their contents at run time and
  // folds them into the resolved-config hash.
  updateInvocation: (id: string, trigger: string, prompt: string, files: string[] = []) =>
    postAction("update_invocation", { id, trigger, prompt, files }) as Promise<null>,
  deleteInvocation: (id: string) =>
    postAction("delete_invocation", { id }) as Promise<null>,
  // Re-run an agent now (embedded-runs R2 — the Run editor's Runs tab "Re-run
  // now"). Component-side `run_agent` resolves the def by name + runs it once,
  // appending its output; returns the produced text. Not bound to the Run's
  // schedule — a manual one-off using the Agent's instructions.
  runAgent: (name: string) => postAction("run_agent", { name }) as Promise<string>,
  // Smart objects SO1: the replicated object store keyed by the UUID embedded
  // in the note's inline `obj://<id>` chip. The UI mints the id, inserts the
  // chip via the `@` type-picker, and creates the entry; the object popup
  // edits/deletes it. Mirrors the invocation API. The action arg key is
  // `obj_type` (the Rust parameter name); the wire model field is `type`.
  createObject: (
    id: string,
    obj_type: string,
    data: string,
    links: ObjLink[] = [],
    render = "",
    derive: DeriveSpec | null = null,
  ) =>
    postAction("create_object", {
      id,
      obj_type,
      data,
      links,
      render,
      derive,
    }) as Promise<null>,
  updateObject: (
    id: string,
    obj_type: string,
    data: string,
    links: ObjLink[] = [],
    render = "",
    derive: DeriveSpec | null = null,
  ) =>
    postAction("update_object", {
      id,
      obj_type,
      data,
      links,
      render,
      derive,
    }) as Promise<null>,
  deleteObject: (id: string) => postAction("delete_object", { id }) as Promise<null>,
  listObjects: () => postAction("list_objects", {}) as Promise<SmartObject[]>,
  // Smart objects SO4: ingest a recipe URL into a normalized `recipe` object.
  // The host fetches the page (gated egress), the component extracts schema.org
  // JSON-LD + LLM-normalizes the ingredients, creates the recipe object keyed by
  // `object_id` (the UUID the UI minted for the inline chip), and caches by
  // URL+JSON-LD hash (re-import is free). Returns the created/cached object id.
  ingestRecipe: (url: string, object_id: string) =>
    postAction("ingest_recipe", { url, object_id }) as Promise<string>,
  objectTypes: () => postAction("object_types", {}) as Promise<ObjectType[]>,
  // SO3: toggle a recipe in/out of a derived grocery-list's included set (drives
  // the live recompute of the grocery-list + downstream cart-preview).
  toggleRecipeInPlan: (grocery_list_id: string, recipe_id: string, include: boolean) =>
    postAction("toggle_recipe_in_plan", {
      grocery_list_id,
      recipe_id,
      include,
    }) as Promise<null>,
  // Tools/MCP T1: the user-approval actions on a `kind: agent`'s `mcp_servers`
  // request. `approve_mcp` binds to the hash the user saw (a stale hash is
  // refused by the component).
  approveMcp: (agent: string, requested_hash: string) =>
    postAction("approve_mcp", { agent, requested_hash }) as Promise<null>,
  denyMcp: (agent: string) => postAction("deny_mcp", { agent }) as Promise<null>,
  revokeMcp: (agent: string) =>
    postAction("revoke_mcp", { agent }) as Promise<null>,
};

/** Subscribe to the vault's live state over SSE. Returns an unsubscribe fn. */
export function subscribeVault(onState: (state: VaultState) => void): () => void {
  const source = new EventSource("api/events");
  source.addEventListener("state", (ev) => {
    try {
      onState(JSON.parse((ev as MessageEvent).data) as VaultState);
    } catch {
      // ignore malformed frames
    }
  });
  return () => source.close();
}

/** Fetch the host's live fleet (the apps on this host). */
export async function fetchFleet(): Promise<Fleet> {
  const res = await fetch("../api/fleet");
  if (!res.ok) throw new Error(`fleet fetch failed (${res.status})`);
  return (await res.json()) as Fleet;
}

// ── auth (multi-tenant session + PAT API, auth.md §9 C5) ─────────────────────
//
// All paths are RELATIVE to the shell's `/tangram/` mount, so `../api/auth…`
// resolves to the host root. In self-hosted mode the host reports
// mode="self-hosted" and the UI shows no auth chrome (the loopback-trusted
// default is unchanged).

export type AuthMode = "self-hosted" | "multi-tenant";

export interface AuthPrincipal {
  user_id: string;
  email: string;
  groups: string[];
  scopes: string[];
}

export interface AuthState {
  mode: AuthMode;
  principal: AuthPrincipal | null;
  /** Whether OAuth/OIDC sign-in is available (the "Sign in with GitHub" button). */
  oauth?: boolean;
}

export interface PatInfo {
  id: string;
  label: string;
  scopes: string[];
  created_ms: number;
  expires_ms: number | null;
}

export interface MintedPat extends PatInfo {
  token: string; // shown ONCE
}

/** The host's auth state: {mode, principal}. The shell branches on this. */
export async function fetchAuth(): Promise<AuthState> {
  const res = await fetch("../api/auth", { credentials: "same-origin" });
  if (!res.ok) throw new Error(`auth state fetch failed (${res.status})`);
  return (await res.json()) as AuthState;
}

/** Exchange a pasted PAT for an HttpOnly session cookie. Throws on a bad PAT. */
export async function login(token: string): Promise<void> {
  const res = await fetch("../api/auth/login", {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token }),
  });
  if (!res.ok) {
    throw new Error("That access key was not accepted (check it and try again).");
  }
}

/** Revoke the current session and clear the cookie. */
export async function logout(): Promise<void> {
  await fetch("../api/auth/logout", {
    method: "POST",
    credentials: "same-origin",
  });
}

/** List the caller's own PATs (metadata only — never the secret). */
export async function listPats(): Promise<PatInfo[]> {
  const res = await fetch("../api/auth/pats", { credentials: "same-origin" });
  if (!res.ok) throw new Error(`could not list keys (${res.status})`);
  return ((await res.json()) as { pats: PatInfo[] }).pats ?? [];
}

/** Mint a PAT (token returned ONCE). Requires a session credential. */
export async function mintPat(label: string, scopes?: string[]): Promise<MintedPat> {
  const res = await fetch("../api/auth/pats", {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(scopes ? { label, scopes } : { label }),
  });
  if (!res.ok) throw new Error(`could not mint a key (${res.status})`);
  return (await res.json()) as MintedPat;
}

/** Revoke one of the caller's PATs by id. */
export async function revokePat(id: string): Promise<void> {
  const res = await fetch(`../api/auth/pats/${encodeURIComponent(id)}`, {
    method: "DELETE",
    credentials: "same-origin",
  });
  if (!res.ok && res.status !== 404) {
    throw new Error(`could not revoke the key (${res.status})`);
  }
}
