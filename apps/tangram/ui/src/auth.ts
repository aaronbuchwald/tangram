// Shell auth UI (docs/design/auth.md §9 C5, §14 shape B/C). Multi-tenant only:
// the host's `GET /api/auth` reports the mode, and the shell branches —
//
//   self-hosted  → no auth chrome at all (loopback-trusted; unchanged)
//   multi-tenant, unauthenticated → a full-screen login view (paste a PAT now;
//                                   the OAuth button lands in C6)
//   multi-tenant, authenticated   → a principal chip in the topbar + a
//                                   "Devices & Keys" view (mint/list/revoke
//                                   PATs) + sign-out
//
// The session is an HttpOnly cookie set by the host on login — never read or
// written by JS (the `localStorage["tangram_auth_token"]` slot the shell used
// to carry was removed when the token box went away; there is no token in JS).

import {
  fetchAuth,
  listPats,
  login,
  logout,
  mintPat,
  revokePat,
  type AuthPrincipal,
  type AuthState,
  type PatInfo,
} from "./api";
import { confirmAction } from "./modal";

/** Render the full-screen login view into `host` and resolve once the user has
 * successfully exchanged a PAT for a session (the caller then re-boots the
 * shell). The OAuth "Sign in" button is C6 — present but disabled here. */
export function renderLogin(host: HTMLElement, onAuthenticated: () => void): void {
  host.replaceChildren();
  const wrap = document.createElement("div");
  wrap.className = "login-view";
  wrap.innerHTML = `
    <div class="login-card">
      <h1>Tangram</h1>
      <p class="login-sub">This host requires sign-in.</p>
      <label class="login-label" for="login-key">Access key</label>
      <input class="login-input" id="login-key" type="password" autocomplete="off"
             spellcheck="false" placeholder="tgp_…" />
      <div class="login-error" id="login-error"></div>
      <button class="login-btn primary" id="login-submit" type="button">Continue</button>
      <button class="login-btn" id="login-oauth" type="button" disabled
              title="OAuth sign-in arrives in a later update">Sign in with GitHub</button>
      <p class="login-hint">Paste a personal access key (shown once when minted).
        Replicas and tools use the same keys.</p>
    </div>
  `;
  host.appendChild(wrap);

  const input = wrap.querySelector<HTMLInputElement>("#login-key")!;
  const errorEl = wrap.querySelector<HTMLDivElement>("#login-error")!;
  const submit = wrap.querySelector<HTMLButtonElement>("#login-submit")!;

  async function attempt() {
    const token = input.value.trim();
    if (!token) return;
    submit.disabled = true;
    errorEl.textContent = "";
    try {
      await login(token);
      onAuthenticated();
    } catch (e) {
      errorEl.textContent = e instanceof Error ? e.message : String(e);
      submit.disabled = false;
      input.select();
    }
  }

  submit.addEventListener("click", () => void attempt());
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      void attempt();
    }
  });
  input.focus();
}

/** The display name for a principal (its email's local part, else user_id). */
function principalLabel(p: AuthPrincipal): string {
  const at = p.email.indexOf("@");
  return at > 0 ? p.email.slice(0, at) : p.user_id;
}

/** Render the topbar principal chip + sign-out into `host`. Clicking the chip
 * opens the Devices & Keys view. */
export function renderPrincipalChip(host: HTMLElement, principal: AuthPrincipal): void {
  host.replaceChildren();
  const chip = document.createElement("button");
  chip.className = "principal-chip";
  chip.type = "button";
  chip.title = `${principal.email} — manage devices & keys`;
  chip.innerHTML = `<span class="principal-dot"></span><span class="principal-name"></span>`;
  chip.querySelector<HTMLSpanElement>(".principal-name")!.textContent =
    principalLabel(principal);
  chip.addEventListener("click", () => void openDevicesAndKeys(principal));
  host.appendChild(chip);
}

/** The Devices & Keys modal: mint / list / revoke the caller's own PATs, and
 * sign out. The minted token is shown ONCE, with a copy affordance. */
export async function openDevicesAndKeys(principal: AuthPrincipal): Promise<void> {
  const overlay = document.createElement("div");
  overlay.className = "modal-overlay";
  const dialog = document.createElement("div");
  dialog.className = "modal keys-modal";
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.innerHTML = `
    <div class="modal-title">Devices &amp; Keys</div>
    <div class="keys-who"></div>
    <div class="keys-mint">
      <input class="modal-input keys-label" type="text" autocomplete="off"
             spellcheck="false" placeholder="Label (e.g. laptop replica)" />
      <button class="modal-btn primary keys-mint-btn" type="button">Mint key</button>
    </div>
    <div class="keys-minted" hidden></div>
    <div class="keys-list"></div>
    <div class="modal-actions">
      <button class="modal-btn keys-signout" type="button">Sign out</button>
      <button class="modal-btn primary keys-close" type="button">Done</button>
    </div>
  `;
  overlay.appendChild(dialog);
  document.body.appendChild(overlay);

  dialog.querySelector<HTMLDivElement>(".keys-who")!.textContent =
    `Signed in as ${principal.email}`;
  const labelInput = dialog.querySelector<HTMLInputElement>(".keys-label")!;
  const mintBtn = dialog.querySelector<HTMLButtonElement>(".keys-mint-btn")!;
  const mintedEl = dialog.querySelector<HTMLDivElement>(".keys-minted")!;
  const listEl = dialog.querySelector<HTMLDivElement>(".keys-list")!;

  let settled = false;
  function close() {
    if (settled) return;
    settled = true;
    document.removeEventListener("keydown", onKey, true);
    overlay.remove();
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  }
  document.addEventListener("keydown", onKey, true);
  overlay.addEventListener("mousedown", (e) => {
    if (e.target === overlay) close();
  });
  dialog.querySelector<HTMLButtonElement>(".keys-close")!.addEventListener("click", close);
  dialog
    .querySelector<HTMLButtonElement>(".keys-signout")!
    .addEventListener("click", () => {
      void (async () => {
        await logout();
        window.location.reload();
      })();
    });

  function renderList(pats: PatInfo[]) {
    listEl.replaceChildren();
    if (pats.length === 0) {
      const empty = document.createElement("div");
      empty.className = "keys-empty";
      empty.textContent = "No keys yet.";
      listEl.appendChild(empty);
      return;
    }
    for (const p of pats) {
      const row = document.createElement("div");
      row.className = "keys-row";
      const meta = document.createElement("div");
      meta.className = "keys-meta";
      const name = document.createElement("div");
      name.className = "keys-row-label";
      name.textContent = p.label;
      const scopes = document.createElement("div");
      scopes.className = "keys-row-scopes";
      scopes.textContent = p.scopes.join(", ") || "(no scopes)";
      meta.append(name, scopes);
      row.appendChild(meta);
      const revoke = document.createElement("button");
      revoke.className = "modal-btn danger keys-revoke";
      revoke.type = "button";
      revoke.textContent = "Revoke";
      revoke.addEventListener("click", () => {
        void (async () => {
          const ok = await confirmAction({
            title: "Revoke key",
            message: `Revoke "${p.label}"? Any device using it loses access immediately.`,
            confirmLabel: "Revoke",
          });
          if (!ok) return;
          await revokePat(p.id);
          await refresh();
        })();
      });
      row.appendChild(revoke);
      listEl.appendChild(row);
    }
  }

  async function refresh() {
    try {
      renderList(await listPats());
    } catch (e) {
      listEl.textContent = e instanceof Error ? e.message : String(e);
    }
  }

  mintBtn.addEventListener("click", () => {
    void (async () => {
      mintBtn.disabled = true;
      try {
        const label = labelInput.value.trim() || "device";
        const minted = await mintPat(label);
        labelInput.value = "";
        mintedEl.hidden = false;
        mintedEl.innerHTML = `
          <div class="keys-minted-note">New key — copy it now, it is shown only once:</div>
          <code class="keys-minted-token"></code>
          <button class="modal-btn keys-copy" type="button">Copy</button>
        `;
        mintedEl.querySelector<HTMLElement>(".keys-minted-token")!.textContent = minted.token;
        mintedEl.querySelector<HTMLButtonElement>(".keys-copy")!.addEventListener("click", () => {
          void navigator.clipboard?.writeText(minted.token);
        });
        await refresh();
      } catch (e) {
        window.alert(e instanceof Error ? e.message : String(e));
      } finally {
        mintBtn.disabled = false;
      }
    })();
  });

  await refresh();
  labelInput.focus();
}

/** Fetch the auth state, with a self-hosted fallback if the call fails (so a
 * transient error never locks the user out of the loopback default). */
export async function loadAuthState(): Promise<AuthState> {
  try {
    return await fetchAuth();
  } catch {
    return { mode: "self-hosted", principal: null };
  }
}
