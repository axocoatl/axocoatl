import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';

import { browserEngines, launchTestDaemon } from '../support/daemon.mjs';

// What the workbench shell does when its own API answers 401, in every
// engine (Chromium, and WebKit when it is installed).
//
// Usually `/` then serves the sign-in page as well (a cleared or expired
// sign-in cookie, a rotated local API token), and one reload shows it. But
// `/` can go on serving the shell while the API answers 401. Safari on macOS
// does that when a page it has open set about 52 large cookies for localhost
// with Path=/api: WebKit caps the Cookie header it sends at about 200 KB and
// fills it with the longest-path cookies first, so the token cookie is left
// off every API request and still sent to `/`. A full-page Preview's script
// can set those cookies (operate/security.mdx). The shell used to reload
// itself without end there. It now reloads at most once a minute, and
// otherwise says this browser is signed out and how to sign in again.
//
// The first and third journeys make the API answer 401 with Playwright, and
// the second clears the browser's cookies. The last two set the cookies from
// a full-page Preview origin, against the real daemon: with Path=/api and
// with Path=/ws. In WebKit on macOS, which keeps them, they assert what the
// person then sees; in Chromium and WebKit on Linux, that the cookies are
// refused and the workbench works.
const engines = await browserEngines();
let runtime;

before(async () => {
  runtime = await launchTestDaemon();
});

after(async () => {
  await runtime?.stop();
});

const port = () => new URL(runtime.baseUrl).port;
const workbench = () => `http://localhost:${port()}/`;
const connected = () => typeof S !== 'undefined' && S.liveConnection === 'connected';
const SIGNED_OUT = 'This browser is signed out';

/** The statuses of the documents `page` loads at `/` on the workbench. */
function documentLoads(page) {
  const loads = [];
  page.on('response', (response) => {
    const request = response.request();
    const url = new URL(response.url());
    if (request.isNavigationRequest() && request.frame() === page.mainFrame()
        && url.host === `localhost:${port()}` && url.pathname === '/') {
      loads.push(response.status());
    }
  });
  return loads;
}

const signedOutMessage = (page) => page.getByRole('alertdialog', { name: SIGNED_OUT });

/**
 * The signed-out message is shown, modal (the rest of the page is inert),
 * focused, says what to do, and stays after Escape. `loads` (from
 * documentLoads) is reported if it never appears, so a reload loop shows how
 * many documents it loaded.
 */
async function assertSignedOutMessage(page, loads) {
  const message = signedOutMessage(page);
  try {
    await message.waitFor({ state: 'visible', timeout: 20_000 });
  } catch (error) {
    throw new Error(`no signed-out message after 20 s; documents loaded at /: ${loads.length} (${loads.slice(0, 12).join(', ')})`, { cause: error });
  }
  const text = await message.innerText();
  assert.match(text, /axocoatl url/);
  assert.match(text, /axocoatl dev/);
  assert.match(text, /Safari > Settings > Privacy > Manage Website Data/);
  assert.deepEqual(await page.evaluate(() => {
    const message = document.getElementById('signed-out');
    return {
      modal: message.getAttribute('aria-modal'),
      othersInert: [...document.body.children].every((element) => element === message || element.inert),
      focused: message.contains(document.activeElement),
      description: document.getElementById(message.getAttribute('aria-describedby'))?.textContent.includes('axocoatl url'),
      count: document.querySelectorAll('#signed-out').length,
    };
  }), { modal: 'true', othersInert: true, focused: true, description: true, count: 1 });
  await page.getByRole('button', { name: 'Reload' }).waitFor({ state: 'visible' });
  for (let press = 0; press < 2; press += 1) await page.keyboard.press('Escape');
  assert.equal(await message.isVisible(), true, 'Escape hid the signed-out message');
}

/** A new signed-in context, through the real sign-in link. */
async function signedInContext(browser) {
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto(runtime.signInUrl);
  await page.waitForFunction(connected);
  await page.close();
  return context;
}

/**
 * Open the fixture Session's Preview top-level (as Open full preview does),
 * served here by Playwright as the "Preview browser contract" journey serves
 * it, and set from its script, for localhost with `cookiePath`, 51 cookies of
 * 4,000 bytes and then smaller ones down to 8 bytes. WebKit skips a cookie
 * that no longer fits under its cap and goes on with the next, so the
 * smaller ones fill what is left and no room remains for the token cookie,
 * whatever its length.
 */
async function fillCookieHeaderFromFullPreview(context, cookiePath) {
  const session = runtime.fixtures.alpha.sessions[0];
  const previewOrigin = `http://${session.id}-p3000.localhost:${port()}`;
  const fromPreview = (url) => url.origin === previewOrigin;
  await context.route(fromPreview, (route) => route.fulfill({
    status: 200, contentType: 'text/html; charset=utf-8', body: '<!doctype html><title>Preview</title>',
  }));
  const preview = await context.newPage();
  try {
    await preview.goto(`${previewOrigin}/`);
    await preview.evaluate((cookiePath) => {
      const attributes = `; Domain=localhost; Path=${cookiePath}; Max-Age=3600`;
      for (let i = 0; i < 51; i += 1) document.cookie = `fill-${i}=${'x'.repeat(4000)}${attributes}`;
      [2048, 1024, 512, 256, 128, 64, 32, 16, 8].forEach((size, i) => {
        document.cookie = `fit-${i}=${'y'.repeat(size)}${attributes}`;
      });
    }, cookiePath);
  } finally {
    await preview.close();
    await context.unroute(fromPreview);
  }
  const stored = await context.cookies(`http://localhost:${port()}${cookiePath}/`);
  return stored.filter((cookie) => cookie.name.startsWith('fill-') || cookie.name.startsWith('fit-')).length;
}

/** Whether `context` still holds this daemon's real token cookie (compared, never printed). */
async function holdsTheToken(context) {
  return (await context.cookies(workbench()))
    .some((cookie) => cookie.name === runtime.cookieName && cookie.value === runtime.token);
}

for (const engine of engines) {
  test(`${engine.name}: while / serves the shell and every API request answers 401, the shell reloads once and then says it is signed out`, { timeout: 120_000 }, async () => {
    const browser = await engine.launch();
    try {
      const context = await signedInContext(browser);
      await context.route((url) => url.host === `localhost:${port()}` && url.pathname.startsWith('/api/'),
        (route) => route.fulfill({ status: 401, contentType: 'application/json', body: '{"error":"unauthorized"}' }));
      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench(), { waitUntil: 'commit' });
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(3_000);
      // The first load and one reload; never a third.
      assert.deepEqual(loads, [200, 200]);

      // Reload, the person's own choice, loads the shell once more and
      // shows the message again: the automatic reload stays used for the
      // minute.
      await Promise.all([
        page.waitForResponse((response) => response.request().isNavigationRequest()
          && new URL(response.url()).pathname === '/'),
        page.getByRole('button', { name: 'Reload' }).click(),
      ]);
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(3_000);
      assert.deepEqual(loads, [200, 200, 200]);
    } finally {
      await browser.close();
    }
  });

  test(`${engine.name}: a browser whose sign-in cookie is gone reloads once into the sign-in page`, { timeout: 120_000 }, async () => {
    const browser = await engine.launch();
    try {
      const context = await browser.newContext();
      const page = await context.newPage();
      await page.goto(runtime.signInUrl);
      await page.waitForFunction(connected);
      const loads = documentLoads(page);
      await context.clearCookies();
      const signInPage = page.waitForResponse((response) => response.request().isNavigationRequest()
        && new URL(response.url()).pathname === '/' && response.status() === 401);
      await page.evaluate(() => { fetch('/api/agents'); });
      await signInPage;
      await page.waitForLoadState('load');
      await page.waitForTimeout(2_000);
      assert.deepEqual(loads, [401]);
      assert.equal(await page.title(), 'Sign in to Axocoatl');
      assert.equal(await signedOutMessage(page).count(), 0);
    } finally {
      await browser.close();
    }
  });

  test(`${engine.name}: without sessionStorage the shell never reloads on a 401 and says it is signed out`, { timeout: 120_000 }, async () => {
    const browser = await engine.launch();
    try {
      const context = await signedInContext(browser);
      await context.addInitScript(() => {
        Object.defineProperty(window, 'sessionStorage', {
          configurable: true,
          get() { throw new DOMException('The operation is insecure.', 'SecurityError'); },
        });
      });
      await context.route((url) => url.host === `localhost:${port()}` && url.pathname.startsWith('/api/'),
        (route) => route.fulfill({ status: 401, contentType: 'application/json', body: '{"error":"unauthorized"}' }));
      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench(), { waitUntil: 'commit' });
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(3_000);
      assert.deepEqual(loads, [200]);
    } finally {
      await browser.close();
    }
  });

  test(`${engine.name}: about 52 large cookies a full-page Preview sets for localhost with Path=/api sign Safari out of the API, and the shell says so instead of reloading without end`, { timeout: 180_000 }, async () => {
    const browser = await engine.launch();
    const safariSharesLocalhostCookies = process.platform === 'darwin'
      && browser.browserType().name() === 'webkit';
    try {
      const context = await signedInContext(browser);
      const stored = await fillCookieHeaderFromFullPreview(context, '/api');
      // Safari on macOS keeps the 60 cookies for localhost; Chromium and
      // WebKit on Linux refuse every one.
      assert.equal(stored, safariSharesLocalhostCookies ? 60 : 0);
      assert.equal(await holdsTheToken(context), true);
      // The token cookie is still stored and still sent to `/`, but not to
      // the API: a navigation to an API path gets 401 in Safari.
      const probe = await context.newPage();
      const api = await probe.goto(`http://localhost:${port()}/api/agents`);
      assert.equal(api?.status(), safariSharesLocalhostCookies ? 401 : 200);
      await probe.close();

      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench(), { waitUntil: 'commit' });
      if (!safariSharesLocalhostCookies) {
        await page.waitForFunction(connected);
        assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 200);
        await page.waitForTimeout(2_000);
        assert.deepEqual(loads, [200]);
        assert.equal(await signedOutMessage(page).count(), 0);
        return;
      }
      // Safari: `/` serves the shell, its API answers 401, one reload, then
      // the message. Before, this page reloaded without end (43 loads in 8 s).
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(5_000);
      assert.deepEqual(loads, [200, 200]);

      // Opening the sign-in link again does not help while the cookies stay,
      // and still does not loop: within the minute the shell does not reload.
      loads.length = 0;
      await page.goto(runtime.signInUrl, { waitUntil: 'commit' });
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(3_000);
      assert.deepEqual(loads, [303, 200]);

      // What the message says to do: remove localhost's cookies (Manage
      // Website Data removes them all), then open the sign-in link again.
      await context.clearCookies();
      loads.length = 0;
      await page.goto(runtime.signInUrl);
      await page.waitForFunction(connected);
      assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 200);
      assert.equal(await signedOutMessage(page).count(), 0);
      assert.deepEqual(loads, [303, 200]);
    } finally {
      await browser.close();
    }
  });

  test(`${engine.name}: about 52 large cookies a full-page Preview sets for localhost with Path=/ws keep Safari's live connection from opening, while the API answers`, { timeout: 180_000 }, async () => {
    const browser = await engine.launch();
    const safariSharesLocalhostCookies = process.platform === 'darwin'
      && browser.browserType().name() === 'webkit';
    try {
      const context = await signedInContext(browser);
      assert.equal(await fillCookieHeaderFromFullPreview(context, '/ws'), safariSharesLocalhostCookies ? 60 : 0);
      const page = await context.newPage();
      const loads = documentLoads(page);
      const sockets = [];
      page.on('websocket', (socket) => {
        if (new URL(socket.url()).pathname === '/ws') sockets.push(socket);
      });
      await page.goto(workbench());
      await page.waitForFunction(() => typeof S !== 'undefined');
      assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 200);
      if (!safariSharesLocalhostCookies) {
        await page.waitForFunction(connected);
        assert.deepEqual(loads, [200]);
        return;
      }
      // The live connection's handshake gets 401 and the shell keeps
      // retrying; the page itself and its API are unaffected, and no
      // signed-out message is shown (a WebSocket cannot see the 401).
      await page.waitForTimeout(10_000);
      assert.equal(await page.evaluate(() => S.liveConnection), 'reconnecting');
      assert.match(await page.locator('#status-pearls').innerText(), /live connection reconnecting/);
      assert.ok(sockets.length >= 2, `live connection attempts: ${sockets.length}`);
      assert.deepEqual(loads, [200]);
      assert.equal(await signedOutMessage(page).count(), 0);

      // Removing localhost's cookies and signing in again restores it.
      await context.clearCookies();
      await page.goto(runtime.signInUrl);
      await page.waitForFunction(connected);
    } finally {
      await browser.close();
    }
  });
}
