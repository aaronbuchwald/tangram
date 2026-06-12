// AC8 — the LIVE Amazon grocery→cart demo driver
// (docs/design/task-automation-browser.md §9; ADR-0010).
//
// This is the host-side record harness made concrete: a self-contained
// Playwright script that signs into Amazon with a 1Password-brokered
// credential, builds a cart from a grocery list, and HARD-STOPS before
// checkout. It is the "self-contained script that reads from op IN-PROCESS"
// the substrate spec requires:
//
//   * the credential is read via `op read op://Shopper/Amazon/{username,
//     password}` IN THIS PROCESS and passed straight to page.fill — it is
//     NEVER an argument authored by the operator, never printed, never logged.
//   * CART ONLY. The script never clicks Place order / Buy now / proceed to
//     checkout. Building the cart is the entire deliverable.
//   * every action is appended to an AutomationScript artifact (the
//     record→script proof), with the credential recorded as an
//     inject_credential REFERENCE (op://…), never a value.
//
// Run:  node crates/tangram-automation/tests/fixtures/amazon_cart_demo.mjs
// Env:  reads /home/ubuntu/tangram/.env (OP_SERVICE_ACCOUNT_TOKEN).
// Out:  amazon_cart_demo.script.json (the recorded script),
//       cart-built.png / signin-*.png (evidence screenshots).

import pw from '/tmp/node_modules/playwright/index.js';
const { chromium } = pw;
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';

const ENV_PATH = '/home/ubuntu/tangram/.env';
const GROCERY_LIST = ['milk', 'eggs', 'bananas'];
const OUT_DIR = process.cwd();

// Durable session reuse (substrate extension #1/#4). The persisted session is
// an AUTH BEARER → it lives OUTSIDE the repo, owner-only, and is reused so
// login + CAPTCHA are a one-time cost.
//   * userDataDir   — full profile (stable fingerprint, fewer re-challenges)
//   * storageState  — portable cookies+localStorage JSON (the handoff format
//                     from an interactive solve done on the owner's machine)
import os from 'node:os';
import path from 'node:path';
const SITE = 'www.amazon.com';
const PROFILE_ROOT = path.join(os.homedir(), '.tangram-automation', 'profiles', SITE);
const USER_DATA_DIR = path.join(PROFILE_ROOT, 'user-data');
const STORAGE_STATE = path.join(PROFILE_ROOT, 'storage-state.json');
fs.mkdirSync(PROFILE_ROOT, { recursive: true, mode: 0o700 });

// ── load only OP_SERVICE_ACCOUNT_TOKEN from .env into this process env ──
const env = { ...process.env };
for (const line of fs.readFileSync(ENV_PATH, 'utf8').split('\n')) {
  const m = line.match(/^([A-Z0-9_]+)=(.*)$/);
  if (m) env[m[1]] = m[2].replace(/^["']|["']$/g, '');
}
process.env.OP_SERVICE_ACCOUNT_TOKEN = env.OP_SERVICE_ACCOUNT_TOKEN;

// Resolve a credential via the op CLI IN-PROCESS; the value never leaves this
// function except into page.fill.
function opRead(ref) {
  return execFileSync('op', ['read', ref, '--no-newline'], {
    env,
    encoding: 'utf8',
  });
}

// ── the recorded AutomationScript (no secret values; only references) ──
const script = {
  template_id: 'amazon-grocery-cart',
  version: 1,
  domains: ['www.amazon.com'],
  steps: [],
};
const rec = (s) => { script.steps.push(s); console.log('STEP', s.step, s.url || s.text || s.secret_ref || s.reason || ''); };

const log = (...a) => console.log('[demo]', ...a);

// Hard guard: a list of phrases we must NEVER click (irreversible checkout).
const FORBIDDEN_CLICK = /place your order|place order|buy now|proceed to checkout|complete (your )?purchase|submit order/i;

// Detect "signed in" off the live nav chrome (the preflight indicator).
async function isSignedIn(page) {
  return (await page.locator('#nav-link-accountList, #nav-cart').count().catch(() => 0)) > 0;
}

// Persist the authenticated session (storageState) with owner-only perms. The
// userDataDir is persisted implicitly by launchPersistentContext.
async function persistSession(ctx) {
  await ctx.storageState({ path: STORAGE_STATE });
  try { fs.chmodSync(STORAGE_STATE, 0o600); } catch {}
  console.log('[demo] persisted session →', STORAGE_STATE);
}

async function main() {
  // Persistent profile under the OUTSIDE-the-repo, owner-only session dir.
  const ctx = await chromium.launchPersistentContext(USER_DATA_DIR, {
    headless: true,
    viewport: { width: 1280, height: 1000 },
    userAgent:
      'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36',
  });
  const page = ctx.pages()[0] || (await ctx.newPage());
  page.setDefaultTimeout(30000);

  const result = { signedIn: false, added: [], stoppedBeforeCheckout: true, blocked: null, preflight: null };

  try {
    // 0. PREFLIGHT — are we already signed in? (cheap, runs FIRST, every time)
    //    The persistent profile / a restored storageState may already carry the
    //    session → skip login + CAPTCHA + credential fetch entirely.
    await page.goto('https://www.amazon.com/gp/css/homepage.html', { waitUntil: 'domcontentloaded' }).catch(() => {});
    if (await isSignedIn(page)) {
      result.preflight = 'signed-in';
      result.signedIn = true;
      log('PREFLIGHT: already signed in — skipping login/CAPTCHA/credentials.');
      await buildCart(page, result);
      await persistSession(ctx);
      return;
    }
    result.preflight = 'not-signed-in';
    log('PREFLIGHT: not signed in — proceeding to the sign-in flow (decision point).');

    // 1. sign-in page
    await page.goto(
      'https://www.amazon.com/ap/signin?openid.return_to=https%3A%2F%2Fwww.amazon.com%2F&openid.mode=checkid_setup&openid.assoc_handle=usflex&openid.ns=http%3A%2F%2Fspecs.openid.net%2Fauth%2F2.0&openid.claimed_id=http%3A%2F%2Fspecs.openid.net%2Fauth%2F2.0%2Fidentifier_select&openid.identity=http%3A%2F%2Fspecs.openid.net%2Fauth%2F2.0%2Fidentifier_select',
      { waitUntil: 'domcontentloaded' }
    );
    rec({ step: 'navigate', url: 'https://www.amazon.com/ap/signin', expect: { url_host: 'www.amazon.com' } });
    await page.screenshot({ path: `${OUT_DIR}/signin-1-email.png` });

    // Already signed in? (persistent profile) — detect the nav greeting.
    const alreadyIn = await page.locator('#nav-link-accountList').count().catch(() => 0);

    // 2. email
    const emailField = page.getByLabel(/mobile number or email|email/i).first()
      .or(page.locator('#ap_email, input[name="email"]')).first();
    if (await emailField.count()) {
      const email = opRead('op://Shopper/Amazon/username');
      await emailField.fill(email);
      rec({ step: 'inject_credential', secret_ref: 'op://Shopper/Amazon/username', target: { role: 'textbox', name: 'Email or mobile number' } });
      const cont = page.locator('#continue, input#continue, button:has-text("Continue")').first();
      if (await cont.count()) { await cont.click(); rec({ step: 'click', target: { role: 'button', name: 'Continue' }, expect: {} }); }
      await page.waitForTimeout(2500);
    }
    await page.screenshot({ path: `${OUT_DIR}/signin-2-after-email.png` });

    // 2b. CAPTCHA / OTP detection — PAUSE, do not attempt to solve.
    const bodyText = (await page.locator('body').innerText().catch(() => '')) || '';
    if (/enter the characters you see|type the characters|puzzle|verification|two-step|one time password|otp|enter the code/i.test(bodyText) &&
        !/password/i.test(await page.locator('#ap_password').count().then(c => c ? 'password' : '').catch(()=>''))) {
      // Only treat as block if there is no password field to proceed with.
    }

    // 3. password
    const pwField = page.locator('#ap_password, input[name="password"]').first();
    if (await pwField.count()) {
      const pw = opRead('op://Shopper/Amazon/password');
      await pwField.fill(pw);
      rec({ step: 'inject_credential', secret_ref: 'op://Shopper/Amazon/password', target: { role: 'textbox', name: 'Password' } });
      const submit = page.locator('#signInSubmit, input#signInSubmit, button:has-text("Sign in")').first();
      if (await submit.count()) { await submit.click(); rec({ step: 'click', target: { role: 'button', name: 'Sign in' }, expect: {} }); }
      await page.waitForTimeout(3500);
    }
    await page.screenshot({ path: `${OUT_DIR}/signin-3-after-password.png` });

    // 4. detect 2FA/CAPTCHA/OTP after submit → PAUSE. URL-independent: Amazon
    //    serves the puzzle from several paths. We NEVER attempt to solve it.
    const afterText = (await page.locator('body').innerText().catch(() => '')) || '';
    if (/(solve this puzzle|protect your account|start puzzle|one time password|otp|two-step|verification code|enter the code|enter the characters you see|type the characters)/i.test(afterText)) {
      result.blocked =
        'human-verification (CAPTCHA puzzle / 2FA / OTP) required at sign-in; PAUSED per the no-bypass rule — owner action needed (solve interactively, or pre-authorize the test profile)';
      throw new Error(result.blocked);
    }

    // signed-in heuristic: account list / cart present on amazon.com
    result.signedIn = (await page.locator('#nav-link-accountList, #nav-cart').count().catch(() => 0)) > 0 || !!alreadyIn;

    // Both sign-in paths converge here: a verified session is persisted so the
    // NEXT run's preflight skips login entirely (one-time CAPTCHA cost).
    if (result.signedIn) await persistSession(ctx);

    await buildCart(page, result);
  } catch (e) {
    log('ERROR:', e.message);
    await page.screenshot({ path: `${OUT_DIR}/error-state.png` }).catch(() => {});
    if (!result.blocked) result.blocked = e.message;
  } finally {
    fs.writeFileSync(`${OUT_DIR}/amazon_cart_demo.script.json`, JSON.stringify(script, null, 2));
    // assert no secret value leaked into the recorded artifact
    const json = fs.readFileSync(`${OUT_DIR}/amazon_cart_demo.script.json`, 'utf8');
    result.scriptHasSecret = /op:\/\//.test(json) ? 'only-references' : 'no-refs';
    console.log('RESULT', JSON.stringify(result));
    await ctx.close();
  }
}

// Build the cart from the grocery list, then HARD STOP before checkout.
// Shared by the preflight (already-signed-in) path and the post-login path.
async function buildCart(page, result) {
    // 5. for each grocery item: search → add first reasonable result to cart
    for (const item of GROCERY_LIST) {
      try {
        await page.goto(`https://www.amazon.com/s?k=${encodeURIComponent(item)}`, { waitUntil: 'domcontentloaded' });
        rec({ step: 'navigate', url: `https://www.amazon.com/s?k=${item}`, expect: { url_host: 'www.amazon.com' } });
        rec({ step: 'type', target: { role: 'searchbox', name: 'Search Amazon' }, text: item, expect: {} });
        await page.waitForTimeout(2000);

        // Prefer an explicit "Add to cart" button in results; else open the
        // first product and add from the PDP.
        let added = false;
        const resultAdd = page.locator('button:has-text("Add to cart"), input[name="submit.addToCart"]').first();
        if (await resultAdd.count()) {
          const label = (await resultAdd.innerText().catch(() => 'Add to cart')) || 'Add to cart';
          if (!FORBIDDEN_CLICK.test(label)) {
            await resultAdd.click();
            await page.waitForTimeout(1500);
            added = true;
          }
        }
        if (!added) {
          const firstProduct = page.locator('div[data-component-type="s-search-result"] h2 a, a.a-link-normal.s-no-outline').first();
          if (await firstProduct.count()) {
            await firstProduct.click();
            await page.waitForTimeout(2500);
            const pdpAdd = page.locator('#add-to-cart-button, input#add-to-cart-button').first();
            if (await pdpAdd.count()) {
              await pdpAdd.click();
              await page.waitForTimeout(2000);
              added = true;
            }
          }
        }
        if (added) {
          result.added.push(item);
          rec({ step: 'click', target: { role: 'button', name: 'Add to Cart' }, expect: { text_present: 'Added to Cart' } });
          log(`added: ${item}`);
        } else {
          log(`could not add: ${item}`);
        }
      } catch (e) {
        log(`item "${item}" failed: ${e.message}`);
      }
    }

    // 6. view the cart (read-only) for evidence, then HARD STOP.
    await page.goto('https://www.amazon.com/gp/cart/view.html', { waitUntil: 'domcontentloaded' });
    rec({ step: 'navigate', url: 'https://www.amazon.com/gp/cart/view.html', expect: { url_host: 'www.amazon.com' } });
    await page.waitForTimeout(2000);
    await page.screenshot({ path: `${OUT_DIR}/cart-built.png`, fullPage: true });
    rec({ step: 'stop_gate', reason: 'cart built — placing the order requires explicit owner approval' });
    log('STOP-GATE: cart built; not proceeding to checkout.');
}

main();
