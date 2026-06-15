// App management for the shell sidebar (Phase S2c, Decision E): the registry's
// standalone fleet-management UI, folded into the APPS section.
//
// The shell does NOT add a new host API for this — it talks to the registry
// app's actions over RELATIVE cross-app paths (the registry is a sibling app
// under the same host, mounted at `/registry/`, so from the shell's
// `/tangram/` mount `../registry/api/actions/*` resolves to it). In the
// self-hosted, loopback-trusted default (docs/design/auth.md) no credential is
// needed — local connections are authorized. If the host requires credentials
// (exposed / multi-tenant), a mutating call fails cleanly (401) and we surface
// the message; the session-cookie / paste-a-key login UX lands in C5.

// POST a registry action. Path is relative: `../registry/...` from the shell's
// `/tangram/` mount. Over loopback self-hosted this is authorized with no
// credential; the C5 session flow will attach one when the host requires it.
async function registryAction(name: string, args: unknown): Promise<unknown> {
  const res = await fetch(`../registry/api/actions/${name}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(args ?? {}),
  });
  const data = (await res.json().catch(() => ({}))) as {
    result?: unknown;
    error?: string;
  };
  if (res.status === 401) {
    throw new Error(
      "Unauthorized (this host requires credentials — see docs/design/auth.md)",
    );
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
