// The OAuth sign-in flow (`/auth/*`) and the account surface (`/account*`)
// — RUNTIME_PLAN Phase 6, ADR-0003.
//
// The Worker is an OAuth CLIENT of GitHub (authorization-code web flow,
// hand-rolled: ~3 requests, no library). The three upstream endpoints are
// env-overridable so the miniflare e2e can swap in a stub IdP
// (scripts/e2e-cloudflare-identity.sh); unset, they default to real GitHub.
//
//   browser → GET  /auth/login      → 302 to GitHub authorize (+ state cookie)
//   GitHub  → GET  /auth/callback   → code→token exchange, GET user,
//                                      accounts.signIn() → session cookie,
//                                      302 /account
//   browser → GET  /account         → the account page (mint/revoke PATs)
//   page    → GET  /account/api/me, GET|POST /account/api/pats,
//             DELETE /account/api/pats/<id>   (session-cookie gated)
//
// CSRF: the OAuth state round-trips through a short-lived SameSite=Lax
// cookie; the account API is cookie-authed and relies on SameSite=Lax (no
// cross-site POST/DELETE carries the cookie) — noted in ADR-0003.

import {
  SESSION_COOKIE,
  TangramAccounts,
  bearerToken,
  getCookie,
  randomToken,
} from "./auth";
import type { Env } from "./components";

import accountHtml from "./account.html";

const GITHUB_AUTHORIZE_URL = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN_URL = "https://github.com/login/oauth/access_token";
const GITHUB_USER_URL = "https://api.github.com/user";

const STATE_COOKIE = "tangram_oauth_state";
const JSON_TYPE = { "Content-Type": "application/json" };

function accounts(env: Env): DurableObjectStub<TangramAccounts> {
  return env.TANGRAM_ACCOUNTS.get(env.TANGRAM_ACCOUNTS.idFromName("accounts"));
}

function sessionCookie(url: URL, value: string, maxAgeSeconds: number): string {
  const secure = url.protocol === "https:" ? "; Secure" : "";
  return (
    `${SESSION_COOKIE}=${value}; Path=/; HttpOnly; SameSite=Lax; ` +
    `Max-Age=${maxAgeSeconds}${secure}`
  );
}

function redirect(location: string, setCookies: string[]): Response {
  const headers = new Headers({ Location: location });
  for (const cookie of setCookies) headers.append("Set-Cookie", cookie);
  return new Response(null, { status: 302, headers });
}

// ── /auth/* — the OAuth web flow ─────────────────────────────────────────────

export async function handleAuth(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  switch (url.pathname) {
    case "/auth/login":
      return login(url, env);
    case "/auth/callback":
      return callback(request, url, env);
    case "/auth/logout": {
      const session = getCookie(request, SESSION_COOKIE);
      if (session) await accounts(env).logout(session);
      return redirect("/", [sessionCookie(url, "", 0)]);
    }
    default:
      return new Response("not found", { status: 404 });
  }
}

function login(url: URL, env: Env): Response {
  if (!env.GITHUB_CLIENT_ID || !env.GITHUB_CLIENT_SECRET) {
    return new Response(
      "sign-in is not configured: set the GITHUB_CLIENT_ID and " +
        "GITHUB_CLIENT_SECRET secrets (see cloud/cloudflare/README.md)",
      { status: 503 },
    );
  }
  const state = randomToken("");
  const authorize = new URL(env.OAUTH_AUTHORIZE_URL ?? GITHUB_AUTHORIZE_URL);
  authorize.searchParams.set("client_id", env.GITHUB_CLIENT_ID);
  authorize.searchParams.set("redirect_uri", `${url.origin}/auth/callback`);
  authorize.searchParams.set("state", state);
  authorize.searchParams.set("scope", "read:user");
  const secure = url.protocol === "https:" ? "; Secure" : "";
  return redirect(authorize.toString(), [
    `${STATE_COOKIE}=${state}; Path=/auth; HttpOnly; SameSite=Lax; Max-Age=600${secure}`,
  ]);
}

async function callback(request: Request, url: URL, env: Env): Promise<Response> {
  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state");
  const expectedState = getCookie(request, STATE_COOKIE);
  if (!code || !state || !expectedState || state !== expectedState) {
    return new Response("invalid oauth callback (missing code or state mismatch)", {
      status: 400,
    });
  }

  // code → access token (GitHub returns JSON when asked).
  const tokenResponse = await fetch(env.OAUTH_TOKEN_URL ?? GITHUB_TOKEN_URL, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: "application/json" },
    body: new URLSearchParams({
      client_id: env.GITHUB_CLIENT_ID ?? "",
      client_secret: env.GITHUB_CLIENT_SECRET ?? "",
      code,
      redirect_uri: `${url.origin}/auth/callback`,
    }),
  });
  const token = (await tokenResponse.json().catch(() => ({}))) as { access_token?: string };
  if (!tokenResponse.ok || !token.access_token) {
    return new Response("oauth code exchange failed", { status: 502 });
  }

  // access token → identity. GitHub's API requires a User-Agent.
  const userResponse = await fetch(env.OAUTH_USER_URL ?? GITHUB_USER_URL, {
    headers: {
      Authorization: `Bearer ${token.access_token}`,
      Accept: "application/vnd.github+json",
      "User-Agent": "tangram",
    },
  });
  const user = (await userResponse.json().catch(() => ({}))) as {
    id?: number | string;
    login?: string;
  };
  if (!userResponse.ok || user.id === undefined || !user.login) {
    return new Response("oauth identity lookup failed", { status: 502 });
  }

  const { session } = await accounts(env).signIn("github", String(user.id), user.login);
  const secure = url.protocol === "https:" ? "; Secure" : "";
  return redirect("/account", [
    sessionCookie(url, session, 30 * 24 * 60 * 60),
    `${STATE_COOKIE}=; Path=/auth; HttpOnly; SameSite=Lax; Max-Age=0${secure}`,
  ]);
}

// ── /account — page + PAT API (session-cookie gated) ────────────────────────

export async function handleAccount(request: Request, env: Env, apps: string[]): Promise<Response> {
  const url = new URL(request.url);
  const session = getCookie(request, SESSION_COOKIE);
  const me = session ? await accounts(env).me(session) : null;
  if (!me || !session) {
    if (url.pathname === "/account") return redirect("/auth/login", []);
    return new Response(JSON.stringify({ error: "not signed in" }), {
      status: 401,
      headers: JSON_TYPE,
    });
  }

  if (url.pathname === "/account" && request.method === "GET") {
    return new Response(accountHtml, {
      headers: { "Content-Type": "text/html; charset=utf-8" },
    });
  }
  if (url.pathname === "/account/api/me" && request.method === "GET") {
    return Response.json({ ...me, apps });
  }
  if (url.pathname === "/account/api/pats" && request.method === "GET") {
    const listed = await accounts(env).listPats(session);
    return Response.json({ pats: listed?.pats ?? [] });
  }
  if (url.pathname === "/account/api/pats" && request.method === "POST") {
    const body = (await request.json().catch(() => ({}))) as { label?: string };
    const label = (body.label ?? "").trim() || "token";
    const minted = await accounts(env).mintPat(session, label);
    if (!minted) return new Response(JSON.stringify({ error: "not signed in" }), { status: 401 });
    // The one and only time the token is visible.
    return Response.json({ id: minted.id, label, token: minted.token });
  }
  const patPath = "/account/api/pats/";
  if (url.pathname.startsWith(patPath) && request.method === "DELETE") {
    const id = url.pathname.slice(patPath.length);
    const revoked = await accounts(env).revokePat(session, id);
    if (revoked === null)
      return new Response(JSON.stringify({ error: "not signed in" }), { status: 401 });
    if (!revoked)
      return new Response(JSON.stringify({ error: "no such token" }), {
        status: 404,
        headers: JSON_TYPE,
      });
    return new Response(null, { status: 204 });
  }
  return new Response("not found", { status: 404 });
}

// ── /t/<tenant>/... — principal resolution for the tenant namespace ─────────

/** Resolve the request's credentials against ONE tenant: a PAT bearer
 * (replicas, MCP clients, curl) or the browser session cookie (the UI's
 * relative fetches carry it automatically). One accounts-DO round trip. */
export async function authorizeTenant(
  request: Request,
  env: Env,
  tenant: string,
): Promise<boolean> {
  const bearer = bearerToken(request);
  const session = getCookie(request, SESSION_COOKIE);
  if (!bearer && !session) return false;
  return accounts(env).authorize(tenant, bearer, session);
}
