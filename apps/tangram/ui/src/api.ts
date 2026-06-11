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
