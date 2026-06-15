// The app registry: which jco-transpiled components this Worker hosts, with
// their capability grants — the Cloudflare mirror of apps.toml (name →
// component, ui, allow_hosts, env). Loaders and UIs are bundled statically
// (workerd has no dynamic loading); the APPS var still controls which names
// ROUTE, so an APPS entry without a registry entry degrades to the plain
// sync relay surface, exactly the pre-Phase-7 behavior.
//
// Build dist/components first: `npm run build:components`
// (cloud/cloudflare/build-components.sh).

import { load as loadNotes } from "../dist/components/notes/cores.js";
import { load as loadNutrition } from "../dist/components/nutrition/cores.js";

import notesUi from "../../../apps/notes/ui/index.html";
import nutritionUi from "../../../apps/nutrition/ui/index.html";

import { TangramGuest, tangramHostImports, wasiImports } from "./shim";

import type { TangramAccounts } from "./auth";

export interface Env {
  TANGRAM_DOC: DurableObjectNamespace;
  /** The accounts DO (RUNTIME_PLAN Phase 6): OAuth accounts, sessions,
   * PATs — one instance, named "accounts". */
  TANGRAM_ACCOUNTS: DurableObjectNamespace<TangramAccounts>;
  /** Comma-separated app names this Worker serves, e.g. "notes,nutrition". */
  APPS: string;
  /** GitHub OAuth app credentials (Worker secrets) — unset, `/auth/login`
   * answers 503 and the tenant namespace is unreachable (no accounts). */
  GITHUB_CLIENT_ID?: string;
  GITHUB_CLIENT_SECRET?: string;
  /** Upstream IdP endpoint overrides — default to real GitHub; the
   * identity e2e points them at a stub IdP under miniflare. */
  OAUTH_AUTHORIZE_URL?: string;
  OAUTH_TOKEN_URL?: string;
  OAUTH_USER_URL?: string;
  /** Nutrition's strategy config — set CALORIENINJAS_API_KEY as a Worker
   * secret (`wrangler secret put CALORIENINJAS_API_KEY`) to enable
   * description-based meal logging. Manual gram-quantified logging always
   * works; without a key a description-only meal fails with a clear error. */
  NUTRITION_STRATEGY?: string;
  CALORIENINJAS_API_KEY?: string;
  ANTHROPIC_API_KEY?: string;
  ANTHROPIC_AUTH_TOKEN?: string;
}

interface AppDef {
  load: (
    imports: Record<string, Record<string, unknown>>,
  ) => Promise<Record<string, unknown>>;
  ui: string;
  /** The app's outbound-network grant on the Worker runtime — the coarse host
   * fence (tangram-host's `allow_hosts`). Call-level egress (ADR-0008) is a
   * native-host feature and does not apply here. */
  allowHosts: string[];
  /** Env vars granted to the component, from the Worker's vars/secrets. */
  env: (env: Env) => [string, string][];
}

function present(vars: Array<[string, string | undefined]>): [string, string][] {
  return vars.filter((entry): entry is [string, string] => !!entry[1]);
}

const APP_DEFS: Record<string, AppDef> = {
  notes: {
    load: loadNotes,
    ui: notesUi,
    allowHosts: [],
    env: () => [],
  },
  nutrition: {
    load: loadNutrition,
    ui: nutritionUi,
    allowHosts: ["api.calorieninjas.com"],
    env: (env) =>
      present([
        ["NUTRITION_STRATEGY", env.NUTRITION_STRATEGY],
        ["CALORIENINJAS_API_KEY", env.CALORIENINJAS_API_KEY],
        ["ANTHROPIC_API_KEY", env.ANTHROPIC_API_KEY],
        ["ANTHROPIC_AUTH_TOKEN", env.ANTHROPIC_AUTH_TOKEN],
      ]),
  },
};

/** The component's describe() manifest, parsed (tangram-host's `Describe`). */
export interface Describe {
  name: string;
  instructions?: string;
  actions: ActionInfo[];
  capabilities?: unknown;
}

export interface ActionInfo {
  name: string;
  description: string;
  mutates: boolean;
  input_schema: unknown;
}

/** A live app: the instantiated guest plus its parsed manifest. */
export interface AppLogic {
  guest: TangramGuest;
  describe: Describe;
}

/** The static UI for an app, if its component is bundled. */
export function appUi(app: string): string | undefined {
  return APP_DEFS[app]?.ui;
}

export function hasComponent(app: string): boolean {
  return app in APP_DEFS;
}

/** Instantiate the app's component with its grants (once per DO instance;
 * the caller caches). Returns null for apps without a bundled component. */
export async function instantiateApp(app: string, env: Env): Promise<AppLogic | null> {
  const def = APP_DEFS[app];
  if (!def) return null;
  const root = await def.load({
    "tangram:app/host": tangramHostImports(app, def.allowHosts),
    ...wasiImports(def.env(env)),
  });
  const guest = root.guest as unknown as TangramGuest;
  const describe: Describe = JSON.parse(guest.describe());
  return { guest, describe };
}
