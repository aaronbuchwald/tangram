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

export interface VaultState {
  files: MdFile[];
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
