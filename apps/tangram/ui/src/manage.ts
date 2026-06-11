// App management for the shell sidebar (Phase S2c, Decision E): the registry's
// standalone fleet-management UI, folded into the APPS section.
//
// The shell does NOT add a new host API for this — it talks to the registry
// app's bearer-gated actions over RELATIVE cross-app paths (the registry is a
// sibling app under the same host, mounted at `/registry/`, so from the
// shell's `/tangram/` mount `../registry/api/actions/*` resolves to it). The
// bearer token is the same `tangram_auth_token` localStorage slot the
// standalone registry + marketplace UIs use, so a token set in one surface is
// shared. Mutating actions are bearer-gated host-side, so an unauthenticated
// call just fails cleanly (401) and we surface the message.

const TOKEN_KEY = "tangram_auth_token";

/** The bearer token the user supplied (shared with the registry/marketplace UIs). */
export function authToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? "";
}

export function setAuthToken(value: string): void {
  if (value) localStorage.setItem(TOKEN_KEY, value);
  else localStorage.removeItem(TOKEN_KEY);
}

// POST a registry action. Path is relative: `../registry/...` from the shell's
// `/tangram/` mount. The token, when present, rides as a bearer header.
async function registryAction(name: string, args: unknown): Promise<unknown> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  const token = authToken();
  if (token) headers["Authorization"] = `Bearer ${token}`;
  const res = await fetch(`../registry/api/actions/${name}`, {
    method: "POST",
    headers,
    body: JSON.stringify(args ?? {}),
  });
  const data = (await res.json().catch(() => ({}))) as {
    result?: unknown;
    error?: string;
  };
  if (res.status === 401) {
    throw new Error("unauthorized — set the auth token first");
  }
  if (!res.ok) {
    throw new Error(data.error ?? `action ${name} failed (${res.status})`);
  }
  return data.result;
}

export const registry = {
  setEnabled: (name: string, enabled: boolean) =>
    registryAction("set_enabled", { name, enabled }) as Promise<null>,
  removeApp: (name: string) =>
    registryAction("remove_app", { name }) as Promise<null>,
};
