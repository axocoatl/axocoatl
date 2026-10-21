import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';

import {
  browserEngines, launchTestDaemon, trackRequests, waitForNoRequestsInFlight,
} from '../support/daemon.mjs';

// What the workbench shell does when its own API answers 401, in every
// engine (Chromium, and WebKit when it is installed).
//
// Usually `/` then serves the sign-in page as well (a cleared or expired
// sign-in cookie, a rotated local API token), and one reload shows it. But
// `/` can go on serving the shell while the API answers 401. Safari on macOS
// does that when a page it has open set, for localhost with Path=/api, 51
// cookies of 4,000 bytes and then smaller ones: WebKit caps the Cookie header
// it sends at about 200 KB, puts the longest-path cookies first and skips
// each one that no longer fits, so the smaller ones fill what the large ones
// leave (the large ones alone still leave room for the token cookie), and
// the token cookie is left off every API request and still sent to `/`. A
// Preview's script can set those cookies when the Preview runs in a tab of
// its own, which in Safari happens only when someone opens its address
// there: the workbench keeps Previews embedded (operate/security.mdx).
// The shell used to reload itself without end there. Its code now reloads
// at most once a minute, and otherwise says this browser is signed out and
// how to sign in again. These journeys pin one automatic reload, then the
// message, and no further automatic reload in the seconds each one waits
// after it; they do not pin the one-minute window itself.
//
// The first and third journeys make the API answer 401 with Playwright, and
// the second clears the browser's cookies. The last two set the cookies from
// a top-level Preview origin, against the real daemon: with Path=/api and
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
 * Open the fixture Session's Preview top-level (as someone does who opens its
 * address in a tab of its own; in Safari the workbench offers no way to),
 * served here by Playwright as the "Preview browser contract" journey serves
 * it, and set from its script, for localhost with `cookiePath`, 51 cookies of
 * 4,000 bytes and then smaller ones down to 8 bytes. WebKit skips a cookie
 * that no longer fits under its cap and goes on with the next, so the
 * smaller ones fill what is left and no room remains for the token cookie,
 * whatever its length.
 */
async function fillCookieHeaderFromTopLevelPreview(context, cookiePath) {
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
      // shows the message again, with no automatic reload in the 3 s this
      // waits (the code keeps the automatic reload used for a minute).
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
      const requests = trackRequests(page);
      await page.goto(runtime.signInUrl);
      await page.waitForFunction(connected);
      // Clear the cookies of a workbench that has finished loading: a request
      // it is still making would get the 401 before this journey's own.
      await waitForNoRequestsInFlight(requests, 'The workbench');
      const loads = documentLoads(page);
      const signInPage = page.waitForResponse((response) => response.request().isNavigationRequest()
        && new URL(response.url()).pathname === '/' && response.status() === 401);
      await context.clearCookies();
      // A request of the shell's own may still get the 401 first and start
      // the reload; that reload is the one this journey asserts.
      await page.evaluate(() => { fetch('/api/agents'); }).catch((error) => {
        if (!/Execution context was destroyed/.test(error.message)) throw error;
      });
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

  test(`${engine.name}: the large and filler cookies a top-level Preview sets for localhost with Path=/api sign Safari out of the API, and the shell says so instead of reloading without end`, { timeout: 180_000 }, async () => {
    const browser = await engine.launch();
    const safariSharesLocalhostCookies = process.platform === 'darwin'
      && browser.browserType().name() === 'webkit';
    try {
      const context = await signedInContext(browser);
      const stored = await fillCookieHeaderFromTopLevelPreview(context, '/api');
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
      // the message. A shell that reloaded on every 401 loaded 167 documents
      // in the 20 s this journey waits for the message (Playwright WebKit 26.4
      // on macOS).
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(5_000);
      assert.deepEqual(loads, [200, 200]);

      // Opening the sign-in link again does not help while the cookies stay,
      // and still does not loop: the shell does not reload in the 3 s this
      // waits.
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

  test(`${engine.name}: the large and filler cookies a top-level Preview sets for localhost with Path=/ws keep Safari's live connection from opening, while the API answers`, { timeout: 180_000 }, async () => {
    const browser = await engine.launch();
    const safariSharesLocalhostCookies = process.platform === 'darwin'
      && browser.browserType().name() === 'webkit';
    try {
      const context = await signedInContext(browser);
      assert.equal(await fillCookieHeaderFromTopLevelPreview(context, '/ws'), safariSharesLocalhostCookies ? 60 : 0);
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
