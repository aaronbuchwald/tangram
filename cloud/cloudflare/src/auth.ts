// Identity on Cloudflare (RUNTIME_PLAN Phase 6, ADR-0003): one accounts
// Durable Object holds every account (account == tenant), its browser
// sessions, and its personal access tokens (PATs). The Worker router asks it
// one question per `/t/<tenant>/...` request — `authorize(tenant, bearer,
// session)` — which is what makes revocation immediate: there is no cache
// between "the PAT row is deleted" and "the next request 401s".
//
// Secrets are never stored: sessions and PATs are persisted as SHA-256
// hashes of the token, so DO storage holds nothing replayable. Tokens are
// shown exactly once (mint response / Set-Cookie).
//
// This mirrors tangram-host's Phase 5 design (crates/tangram-host/src/
// auth.rs): every request under `/t/<tenant>/` resolves to a principal or
// gets ONE uniform 401 — an unknown tenant, a wrong token, another tenant's
// token, and a missing token are indistinguishable (no existence oracle).

import { DurableObject } from "cloudflare:workers";

import type { Env } from "./components";

/** Sessions expire; PATs live until revoked (they're for headless replicas). */
const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;

/** Tenant slugs: what an IdP login is reduced to (collision-safe in signIn). */
const SLUG_MAX = 32;

// ── storage records ──────────────────────────────────────────────────────────

/** `tenant:<slug>` — one account. */
interface AccountRecord {
  tenant: string;
  provider: string;
  providerId: string;
  login: string;
  createdAtMs: number;
}

/** `session:<sha256(token)>` — one browser session. */
interface SessionRecord {
  tenant: string;
  createdAtMs: number;
  expiresAtMs: number;
}

/** `pat:<sha256(token)>` — auth lookup is O(1) by token hash. */
interface PatRecord {
  tenant: string;
  id: string;
  label: string;
  createdAtMs: number;
}

/** `patindex:<tenant>:<id>` — list/revoke without knowing the token. */
interface PatIndexRecord {
  hash: string;
  label: string;
  createdAtMs: number;
}

export interface PatInfo {
  id: string;
  label: string;
  createdAtMs: number;
}

export class TangramAccounts extends DurableObject<Env> {
  /**
   * Sign an IdP identity in: create the account on first sight (tenant slug
   * from the login, collision-safe — a different identity holding `alice`
   * makes this one `alice-2`), then mint a fresh browser session. Returns
   * the session TOKEN (only ever returned here; storage keeps the hash).
   */
  async signIn(
    provider: string,
    providerId: string,
    login: string,
  ): Promise<{ tenant: string; session: string }> {
    const identKey = `ident:${provider}:${providerId}`;
    let tenant = await this.ctx.storage.get<string>(identKey);
    if (!tenant) {
      const base = slugify(login);
      tenant = base;
      for (let n = 2; await this.ctx.storage.get(`tenant:${tenant}`); n++) {
        tenant = `${base}-${n}`;
      }
      const account: AccountRecord = {
        tenant,
        provider,
        providerId,
        login,
        createdAtMs: Date.now(),
      };
      await this.ctx.storage.put(`tenant:${tenant}`, account);
      await this.ctx.storage.put(identKey, tenant);
    }
    const session = randomToken("tgs");
    const record: SessionRecord = {
      tenant,
      createdAtMs: Date.now(),
      expiresAtMs: Date.now() + SESSION_TTL_MS,
    };
    await this.ctx.storage.put(`session:${await sha256Hex(session)}`, record);
    return { tenant, session };
  }

  /**
   * THE per-request question for `/t/<tenant>/...`: does this bearer PAT or
   * session cookie authenticate as exactly this tenant? Every failure mode
   * (unknown tenant, revoked PAT, expired session, someone else's token,
   * nothing presented) is the same `false` — the Worker answers a uniform
   * 401 for all of them.
   */
  async authorize(tenant: string, bearer?: string, session?: string): Promise<boolean> {
    if (bearer) {
      const pat = await this.ctx.storage.get<PatRecord>(`pat:${await sha256Hex(bearer)}`);
      if (pat && pat.tenant === tenant) return true;
    }
    if (session) {
      const record = await this.sessionRecord(session);
      if (record && record.tenant === tenant) return true;
    }
    return false;
  }

  /** The signed-in account behind a session cookie, or null. */
  async me(session: string): Promise<{ tenant: string; login: string; provider: string } | null> {
    const record = await this.sessionRecord(session);
    if (!record) return null;
    const account = await this.ctx.storage.get<AccountRecord>(`tenant:${record.tenant}`);
    if (!account) return null;
    return { tenant: account.tenant, login: account.login, provider: account.provider };
  }

  /** Mint a PAT for the session's account; the token is returned exactly
   * once and stored only as its hash. */
  async mintPat(session: string, label: string): Promise<{ id: string; token: string } | null> {
    const record = await this.sessionRecord(session);
    if (!record) return null;
    const token = randomToken("tgp");
    const id = randomToken("").slice(0, 8);
    const hash = await sha256Hex(token);
    const pat: PatRecord = { tenant: record.tenant, id, label, createdAtMs: Date.now() };
    const index: PatIndexRecord = { hash, label, createdAtMs: pat.createdAtMs };
    await this.ctx.storage.put(`pat:${hash}`, pat);
    await this.ctx.storage.put(`patindex:${record.tenant}:${id}`, index);
    return { id, token };
  }

  /** Revoke one PAT by id. Deleting `pat:<hash>` IS the revocation: the
   * next authorize() misses. null = bad session; false = no such PAT. */
  async revokePat(session: string, id: string): Promise<boolean | null> {
    const record = await this.sessionRecord(session);
    if (!record) return null;
    const indexKey = `patindex:${record.tenant}:${id}`;
    const index = await this.ctx.storage.get<PatIndexRecord>(indexKey);
    if (!index) return false;
    await this.ctx.storage.delete(`pat:${index.hash}`);
    await this.ctx.storage.delete(indexKey);
    return true;
  }

  /** The session's PATs (ids and labels only — tokens are unrecoverable). */
  async listPats(session: string): Promise<{ tenant: string; pats: PatInfo[] } | null> {
    const record = await this.sessionRecord(session);
    if (!record) return null;
    const entries = await this.ctx.storage.list<PatIndexRecord>({
      prefix: `patindex:${record.tenant}:`,
    });
    const pats: PatInfo[] = [...entries.entries()].map(([key, value]) => ({
      id: key.split(":")[2],
      label: value.label,
      createdAtMs: value.createdAtMs,
    }));
    pats.sort((a, b) => a.createdAtMs - b.createdAtMs);
    return { tenant: record.tenant, pats };
  }

  /** Drop a browser session (sign-out). */
  async logout(session: string): Promise<void> {
    await this.ctx.storage.delete(`session:${await sha256Hex(session)}`);
  }

  private async sessionRecord(session: string): Promise<SessionRecord | null> {
    const key = `session:${await sha256Hex(session)}`;
    const record = await this.ctx.storage.get<SessionRecord>(key);
    if (!record) return null;
    if (record.expiresAtMs <= Date.now()) {
      await this.ctx.storage.delete(key);
      return null;
    }
    return record;
  }
}

// ── helpers shared with the Worker router ────────────────────────────────────

/** `<prefix>_<40 hex>` from the platform CSPRNG (160 bits). */
export function randomToken(prefix: string): string {
  const bytes = crypto.getRandomValues(new Uint8Array(20));
  const hex = [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
  return prefix ? `${prefix}_${hex}` : hex;
}

export async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Reduce an IdP login to a tenant slug: lowercase `[a-z0-9-]`, trimmed,
 * never empty. Uniqueness is signIn's job, not this function's. */
export function slugify(login: string): string {
  const slug = login
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, SLUG_MAX)
    .replace(/-+$/g, "");
  return slug || "user";
}

/** One cookie's value out of the request's Cookie header. */
export function getCookie(request: Request, name: string): string | undefined {
  const header = request.headers.get("cookie");
  if (!header) return undefined;
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq === -1) continue;
    if (part.slice(0, eq).trim() === name) return part.slice(eq + 1).trim();
  }
  return undefined;
}

/** The bearer token out of `Authorization: Bearer <token>`, if any. */
export function bearerToken(request: Request): string | undefined {
  const header = request.headers.get("authorization");
  if (!header?.startsWith("Bearer ")) return undefined;
  return header.slice("Bearer ".length);
}

export const SESSION_COOKIE = "tangram_session";

/** The ONE 401 for the whole tenant namespace — same status, headers, and
 * body for every failure mode, mirroring tangram-host's
 * `auth::tenant_unauthorized` (no existence oracle). */
export function tenantUnauthorized(): Response {
  return new Response(
    JSON.stringify({
      error:
        "missing or invalid credentials for this tenant namespace " +
        "(send Authorization: Bearer <a PAT from /account>, or sign in)",
    }),
    {
      status: 401,
      headers: { "Content-Type": "application/json", "WWW-Authenticate": "Bearer" },
    },
  );
}
