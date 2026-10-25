import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';

import { startCookieEcho } from '../support/cookie-echo.mjs';
import {
  browserEngines, launchTestDaemon, trackRequests, waitForNoRequestsInFlight,
} from '../support/daemon.mjs';

// What the workbench shell does when its own API answers 401, in every
// engine (Chromium, and WebKit when it is installed).
//
// Usually `/` then serves the sign-in page as well (a cleared or expired
// sign-in cookie, a rotated local API token), and one reload shows it. But
// `/` can go on serving the shell while the API answers 401. Safari on macOS
// does that when a page it has open set, for the workbench's host with
// Path=/api, 51 cookies of 4,000 bytes and then smaller ones: WebKit caps the
// Cookie header it sends at about 200 KB, puts the longest-path cookies first
// and skips each one that no longer fits, so the smaller ones fill what the
// large ones leave (the large ones alone still leave room for the token
// cookie), and the token cookie is left off every API request and still sent
// to `/`. The workbench is served at 127.0.0.1, so only a page on another
// 127.0.0.1 port can set those cookies. A page on a `*.localhost` address
// sets them for `Domain=localhost` in Safari, and they never reach 127.0.0.1;
// the first journey after the 401 ones shows that, in every engine.
// The shell used to reload itself without end there. Its code now reloads
// at most once a minute, and otherwise says this browser is signed out and
// how to sign in again. These journeys pin one automatic reload, then the
// message, and no further automatic reload in the seconds each one waits
// after it; a journey with a fake clock pins the one-minute window itself.
//
// The first three journeys and the fifth make the API answer 401 with
// Playwright, and the fourth clears the browser's cookies; one more makes
// every live connection attempt fail and /ws answer 401. The cookie
// journeys set the cookies from real pages: a top-level page on another
// `*.localhost` address, and one on another 127.0.0.1 port, with Path=/api,
// Path=/ and Path=/ws, against the real daemon. In WebKit on macOS, which
// caps the Cookie header, the 127.0.0.1 ones assert what the person then
// sees; Chromium (on macOS and Linux) and WebKit on Linux send every cookie,
// and there they assert that the workbench keeps working. Each journey's
// name says which of the two it asserts in that engine.
const engines = await browserEngines();
// The engine each entry really launches: with AXO_BROWSER_ENGINE=webkit,
// `chromium` launches WebKit (support/browser-engine.mjs).
const { chromium: chromiumEntry } = await import('playwright');
const engineType = (engine) => (engine.name === 'chromium' ? chromiumEntry.name() : engine.name);
let runtime;
let pages;

before(async () => {
  runtime = await launchTestDaemon();
  // Real pages on other addresses: `http://<name>.localhost:<port>/` and
  // `http://127.0.0.1:<port>/` on a port that is not the daemon's.
  pages = await startCookieEcho();
});

// One browser per engine for the whole file, as the other files keep one:
// each journey works in contexts of its own, which share no cookies, and
// closes them. Closing a browser took up to 25 seconds on a loaded Mac, even
// with only a blank page open, which a journey's own time budget should not
// have to absorb.
const launched = new Map();
async function browserFor(engine) {
  if (!launched.has(engine.name)) launched.set(engine.name, engine.launch());
  return launched.get(engine.name);
}
async function closeContexts(browser) {
  await Promise.all(browser.contexts().map((context) => context.close()));
}

after(async () => {
  for (const launching of launched.values()) {
    const browser = await launching.catch(() => null);
    await browser?.close();
  }
  await pages?.close();
  await runtime?.stop();
});

const port = () => new URL(runtime.baseUrl).port;
// The workbench's one address.
const workbenchHost = () => `127.0.0.1:${port()}`;
const workbench = () => `http://${workbenchHost()}/`;
const connected = () => typeof S !== 'undefined' && S.liveConnection === 'connected';
const SIGNED_OUT = 'This browser is signed out';
const RELOADED_AT = 'axocoatl:signed-out-reload';
// What the message and the sign-in page say to do about a cookie flood.
const REMEDY = /another 127\.0\.0\.1 address[\s\S]*Settings > Privacy > Manage Website Data/;

/** The statuses of the documents `page` loads at `/` on the workbench. */
function documentLoads(page) {
  const loads = [];
  page.on('response', (response) => {
    const request = response.request();
    const url = new URL(response.url());
    if (request.isNavigationRequest() && request.frame() === page.mainFrame()
        && url.host === workbenchHost() && url.pathname === '/') {
      loads.push(response.status());
    }
  });
  return loads;
}

const signedOutMessage = (page) => page.getByRole('alertdialog', { name: SIGNED_OUT });

/**
 * The signed-out message is shown, modal (the rest of the page is inert,
 * and no modal dialog, which would sit above it, is open), focused, says
 * what to do, and stays after Escape.
 * `loads` (from documentLoads) is reported if it never appears, so a reload
 * loop shows how many documents it loaded.
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
  assert.match(text, REMEDY);
  assert.deepEqual(await page.evaluate(() => {
    const message = document.getElementById('signed-out');
    const roots = [document];
    for (let index = 0; index < roots.length; index += 1) {
      for (const element of roots[index].querySelectorAll('*')) {
        if (element.shadowRoot) roots.push(element.shadowRoot);
      }
    }
    return {
      modal: message.getAttribute('aria-modal'),
      othersInert: [...document.body.children].every((element) => element === message || element.inert),
      focused: message.contains(document.activeElement),
      noModalDialog: roots.every((root) => !root.querySelector('dialog[open]:modal')),
      description: document.getElementById(message.getAttribute('aria-describedby'))?.textContent.includes('axocoatl url'),
      count: document.querySelectorAll('#signed-out').length,
    };
  }), { modal: 'true', othersInert: true, focused: true, noModalDialog: true, description: true, count: 1 });
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

/** Make every API request of `context` to the workbench answer 401. */
async function refuseTheApi(context) {
  await context.route((url) => url.host === workbenchHost() && url.pathname.startsWith('/api/'),
    (route) => route.fulfill({ status: 401, contentType: 'application/json', body: '{"error":"unauthorized"}' }));
}

/** How many cookies a flood writes: 51 of 4,000 bytes and 9 smaller ones. */
const FLOOD = 60;

/**
 * Open `origin` top-level, a real page Axocoatl does not serve, and set from
 * its script, with `cookiePath` (and `attributes`), 51 cookies of 4,000
 * bytes and then smaller ones down to 8 bytes. WebKit puts cookies with a
 * longer path first, then, among cookies of one path, those set or changed
 * in a later second first, then by name (Playwright 1.64's WebKit on macOS).
 * It skips a cookie that no longer fits under its cap and goes on with the
 * next, so the smaller ones fill what is left and no room remains for the
 * token cookie, whatever its length. The cookies are written right after a
 * second begins, all in that second, which is later than the sign-in's: a
 * flood that crossed into the next second put some small ones first and
 * left room for the token cookie (1 run in 20).
 *
 * The page stays open until the browser has stored `expected` of them for
 * `storedFor` (or 15 s have passed), and the count is returned. Chromium
 * writes `document.cookie` to its cookie store without waiting, and drops
 * the writes still queued when the page closes. On a loaded Linux machine,
 * closing the page at once left 23 to 37 of the 60 stored in CI, and 4 to 48
 * in 10 of 24 floods in an Ubuntu 24.04 VM with every core busy; reading
 * `document.cookie` after the writes, which waits for them, left all 60 in
 * each of 24 there.
 */
async function fillCookieHeader(context, origin, cookiePath, attributes, storedFor, expected) {
  const storedCount = async () => (await context.cookies(`http://${storedFor}${cookiePath === '/' ? '' : cookiePath}/`))
    .filter((cookie) => cookie.path === cookiePath && /^a-fi(ll|t)-/.test(cookie.name)).length;
  const page = await context.newPage();
  try {
    await page.goto(`${origin}/fill`);
    await page.evaluate(async ({ cookiePath, attributes }) => {
      await new Promise((resolve) => setTimeout(resolve, 1000 - (Date.now() % 1000) + 5));
      const suffix = `${attributes}; Path=${cookiePath}; Max-Age=3600`;
      for (let i = 0; i < 51; i += 1) document.cookie = `a-fill-${i}=${'x'.repeat(4000)}${suffix}`;
      [2048, 1024, 512, 256, 128, 64, 32, 16, 8].forEach((size, i) => {
        document.cookie = `a-fit-${i}=${'y'.repeat(size)}${suffix}`;
      });
      // Flush queued cookie writes before the page can close.
      return document.cookie.length;
    }, { cookiePath, attributes });
    const deadline = Date.now() + 15_000;
    let stored = await storedCount();
    while (stored !== expected && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 100));
      stored = await storedCount();
    }
    return stored;
  } finally {
    await page.close();
  }
}

/** The flood from a page on a `*.localhost` address, for `Domain=localhost`. */
const fillFromLocalhostPage = (context, cookiePath, expected) => fillCookieHeader(
  context, `http://another-app.localhost:${pages.port}`, cookiePath, '; Domain=localhost', `localhost:${port()}`, expected,
);

/** The flood from a page on another 127.0.0.1 port, host-only for 127.0.0.1. */
const fillFrom127Page = (context, cookiePath) => fillCookieHeader(
  context, `http://127.0.0.1:${pages.port}`, cookiePath, '', workbenchHost(), FLOOD,
);

/** Whether `context` still holds this daemon's real token cookie (compared, never printed). */
async function holdsTheToken(context) {
  return (await context.cookies(workbench()))
    .some((cookie) => cookie.name === runtime.cookieName && cookie.value === runtime.token);
}

/** The status of a top-level navigation to `path` on the workbench. */
async function navigationStatus(context, path) {
  const probe = await context.newPage();
  try {
    return (await probe.goto(`http://${workbenchHost()}${path}`))?.status();
  } finally {
    await probe.close();
  }
}

/**
 * Whether `type` (an engine's name) caps the Cookie header it sends, so that
 * a flood of cookies for the workbench's host leaves the token cookie off.
 * Measured with Playwright 1.64.1 after the 60-cookie flood: WebKit on macOS
 * sent 54 cookies (204,838 bytes) and left the token cookie off, while
 * Chromium on macOS and Linux and WebKit on Linux (libsoup) sent all 61
 * (208,848 bytes).
 */
const capsTheCookieHeader = (type) => type === 'webkit' && process.platform === 'darwin';

/**
 * Assert that the flood left the token cookie off a top-level request to
 * `path` (it answers 401 then) exactly where `type` caps the Cookie header,
 * and return whether it did.
 */
async function tokenLeftOff(context, type, path) {
  const status = await navigationStatus(context, path);
  const left = status === 401;
  assert.equal(left, capsTheCookieHeader(type), `${path} answered ${status}`);
  return left;
}

/**
 * The Cookie header that a top-level navigation under `cookiePath` on
 * 127.0.0.1 carries, read by the cookie echo on its own port (cookies ignore
 * ports, so the workbench gets the same one): where `type` caps the header
 * it leaves out some flood cookies and the token cookie, otherwise it holds
 * every flood cookie and the token cookie.
 */
async function assertCookieHeaderAt127(context, type, cookiePath) {
  const header = await pages.cookieAt(context, '127.0.0.1', cookiePath === '/' ? '' : cookiePath);
  const names = header.split(/;\s*/).filter(Boolean).map((pair) => pair.split('=')[0]);
  const flood = names.filter((name) => /^a-fi(ll|t)-/.test(name)).length;
  const token = names.includes(runtime.cookieName);
  if (capsTheCookieHeader(type)) {
    assert.equal(token, false, `${cookiePath}: ${flood} flood cookies, ${header.length} bytes`);
    assert.ok(flood < FLOOD, `${cookiePath}: ${flood} flood cookies, ${header.length} bytes`);
  } else {
    assert.deepEqual({ flood, token }, { flood: FLOOD, token: true }, `${cookiePath}: ${header.length} bytes`);
  }
}

for (const engine of engines) {
  const type = engineType(engine);
  const caps = capsTheCookieHeader(type);
  test(`${engine.name}: while / serves the shell and every API request answers 401, the shell reloads once and then says it is signed out`, { timeout: 120_000 }, async () => {
    const browser = await browserFor(engine);
    try {
      const context = await signedInContext(browser);
      await refuseTheApi(context);
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
      await closeContexts(browser);
    }
  });

  // The automatic reload is kept for one minute from the last one: a 401
  // 59 s after it shows the message, one 61 s after it reloads once more.
  // Date.now() is fixed by Playwright's clock; the page's timers run as usual.
  test(`${engine.name}: the shell reloads on a 401 at most once a minute`, { timeout: 120_000 }, async () => {
    const browser = await browserFor(engine);
    try {
      const context = await signedInContext(browser);
      const start = Date.parse('2026-10-01T12:00:00Z');
      await context.clock.setFixedTime(start);
      await refuseTheApi(context);
      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench(), { waitUntil: 'commit' });
      await assertSignedOutMessage(page, loads);
      assert.deepEqual(loads, [200, 200]);
      assert.equal(await page.evaluate((key) => sessionStorage.getItem(key), RELOADED_AT), String(start));

      const reloadByHand = () => Promise.all([
        page.waitForResponse((response) => response.request().isNavigationRequest()
          && new URL(response.url()).pathname === '/'),
        page.getByRole('button', { name: 'Reload' }).click(),
      ]);
      await context.clock.setFixedTime(start + 59_000);
      await reloadByHand();
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(2_000);
      assert.deepEqual(loads, [200, 200, 200], '59 s after the automatic reload');

      await context.clock.setFixedTime(start + 61_000);
      await reloadByHand();
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(2_000);
      assert.deepEqual(loads, [200, 200, 200, 200, 200], '61 s after the automatic reload');
      assert.equal(await page.evaluate((key) => sessionStorage.getItem(key), RELOADED_AT), String(start + 61_000));
    } finally {
      await closeContexts(browser);
    }
  });

  // A modal <dialog> sits in the top layer, above the message however that is
  // styled; it used to hide the message until Escape.
  test(`${engine.name}: the signed-out message closes an open modal dialog and stays on top`, { timeout: 120_000 }, async () => {
    const browser = await browserFor(engine);
    try {
      const context = await signedInContext(browser);
      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench());
      await page.waitForFunction(connected);
      // The automatic reload was used a moment ago, so the next 401 shows
      // the message on this page.
      await page.evaluate((key) => sessionStorage.setItem(key, String(Date.now())), RELOADED_AT);
      const sessionId = runtime.fixtures.alpha.sessions[0].id;
      const modalOpen = () => page.evaluate(() => Boolean(
        document.querySelector('#ways-history')?.shadowRoot?.querySelector('dialog')?.open));
      // Ways decisions opens as a modal dialog, in a shadow root.
      await page.evaluate((id) => { void document.querySelector('#ways-history').show({ sessionId: id }); }, sessionId);
      await page.waitForFunction(() => document.querySelector('#ways-history')?.shadowRoot?.querySelector('dialog')?.open);
      await refuseTheApi(context);
      await page.evaluate(() => { void fetch('/api/agents'); });
      await assertSignedOutMessage(page, loads);
      assert.equal(await modalOpen(), false);
      // Nothing on the page can open another one over the message.
      await page.evaluate((id) => {
        const dialog = document.querySelector('#ways-history').shadowRoot.querySelector('dialog');
        dialog.showModal();
        void document.querySelector('#ways-history').show({ sessionId: id });
      }, sessionId);
      assert.equal(await modalOpen(), false);
      await assertSignedOutMessage(page, loads);
      assert.deepEqual(loads, [200]);
    } finally {
      await closeContexts(browser);
    }
  });

  test(`${engine.name}: a browser whose sign-in cookie is gone reloads once into the sign-in page`, { timeout: 120_000 }, async () => {
    const browser = await browserFor(engine);
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
      await closeContexts(browser);
    }
  });

  test(`${engine.name}: without sessionStorage the shell never reloads on a 401 and says it is signed out`, { timeout: 120_000 }, async () => {
    const browser = await browserFor(engine);
    try {
      const context = await signedInContext(browser);
      await context.addInitScript(() => {
        Object.defineProperty(window, 'sessionStorage', {
          configurable: true,
          get() { throw new DOMException('The operation is insecure.', 'SecurityError'); },
        });
      });
      await refuseTheApi(context);
      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench(), { waitUntil: 'commit' });
      await assertSignedOutMessage(page, loads);
      await page.waitForTimeout(3_000);
      assert.deepEqual(loads, [200]);
    } finally {
      await closeContexts(browser);
    }
  });

  // Safari on macOS keeps Domain=localhost cookies that a top-level page on
  // another *.localhost address sets, and sends them to localhost on every
  // port. They never reach 127.0.0.1, the workbench's address: with Path=/api,
  // Path=/ and Path=/ws, the workbench, its API and its live connection keep
  // working in every engine. (Before 1.5.0 the workbench was at localhost,
  // and these cookies signed Safari out of it.)
  test(`${engine.name}: cookies a top-level *.localhost page sets for Domain=localhost never reach the workbench`, { timeout: 180_000 }, async () => {
    const browser = await browserFor(engine);
    const keepsDomainLocalhost = process.platform === 'darwin' && type === 'webkit';
    try {
      const context = await signedInContext(browser);
      for (const cookiePath of ['/api', '/', '/ws']) {
        // Safari on macOS keeps the 60 cookies for localhost; Chromium and
        // WebKit on Linux refuse every one.
        const expected = keepsDomainLocalhost ? FLOOD : 0;
        assert.equal(await fillFromLocalhostPage(context, cookiePath, expected), expected, cookiePath);
      }
      assert.equal(await holdsTheToken(context), true);
      const atWorkbench = (await context.cookies(workbench()))
        .filter((cookie) => /^a-fi(ll|t)-/.test(cookie.name));
      assert.deepEqual(atWorkbench, []);
      assert.equal(await navigationStatus(context, '/api/agents'), 200);

      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench());
      await page.waitForFunction(connected);
      assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 200);
      await page.waitForTimeout(2_000);
      assert.deepEqual(loads, [200]);
      assert.equal(await signedOutMessage(page).count(), 0);
    } finally {
      await closeContexts(browser);
    }
  });

  test(caps
    ? `${engine.name}: the cookies a page on another 127.0.0.1 port sets with Path=/api sign WebKit on macOS out of the API, and the shell says so instead of reloading without end`
    : `${engine.name}: the cookies a page on another 127.0.0.1 port sets with Path=/api all reach the workbench in ${type} on ${process.platform}, which stays signed in`, { timeout: 180_000 }, async () => {
    const browser = await browserFor(engine);
    try {
      const context = await signedInContext(browser);
      assert.equal(await fillFrom127Page(context, '/api'), FLOOD);
      await assertCookieHeaderAt127(context, type, '/api');
      assert.equal(await holdsTheToken(context), true);
      // The token cookie is still stored and still sent to `/`, but in WebKit
      // on macOS not to the API: a navigation to an API path gets 401 there.
      const capped = await tokenLeftOff(context, type, '/api/agents');
      assert.equal(await navigationStatus(context, '/'), 200);

      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench(), { waitUntil: 'commit' });
      if (!capped) {
        await page.waitForFunction(connected);
        assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 200);
        await page.waitForTimeout(2_000);
        assert.deepEqual(loads, [200]);
        assert.equal(await signedOutMessage(page).count(), 0);
        return;
      }
      // WebKit on macOS: `/` serves the shell, its API answers 401, one
      // reload, then the message. A shell that reloaded on every 401 loaded 167 documents
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

      // What the message says to do: remove this browser's data for
      // 127.0.0.1, then open the sign-in link again.
      await context.clearCookies({ domain: '127.0.0.1' });
      loads.length = 0;
      await page.goto(runtime.signInUrl);
      await page.waitForFunction(connected);
      assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 200);
      assert.equal(await signedOutMessage(page).count(), 0);
      assert.deepEqual(loads, [303, 200]);
    } finally {
      await closeContexts(browser);
    }
  });

  // With Path=/ the token cookie stays off `/` too, so the browser only ever
  // sees the daemon's sign-in page: that page says what to do.
  test(caps
    ? `${engine.name}: the cookies a page on another 127.0.0.1 port sets with Path=/ sign WebKit on macOS out entirely, and the sign-in page says what to do`
    : `${engine.name}: the cookies a page on another 127.0.0.1 port sets with Path=/ all reach the workbench in ${type} on ${process.platform}, which stays signed in`, { timeout: 180_000 }, async () => {
    const browser = await browserFor(engine);
    try {
      const context = await signedInContext(browser);
      assert.equal(await fillFrom127Page(context, '/'), FLOOD);
      await assertCookieHeaderAt127(context, type, '/');
      assert.equal(await holdsTheToken(context), true);
      // In WebKit on macOS every request then goes without the token
      // cookie, `/` too.
      const capped = await tokenLeftOff(context, type, '/api/agents');
      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench(), { waitUntil: 'commit' });
      if (!capped) {
        await page.waitForFunction(connected);
        assert.deepEqual(loads, [200]);
        return;
      }
      await page.waitForLoadState('load');
      assert.deepEqual(loads, [401]);
      assert.equal(await page.title(), 'Sign in to Axocoatl');
      assert.match(await page.locator('body').innerText(), REMEDY);
      // Opening the sign-in link again a second or more after the flood
      // sets the token cookie again, now the newest of its path, so WebKit
      // sends it first and the browser is signed in until the page writes
      // its cookies again.
      await page.waitForTimeout(1_100);
      loads.length = 0;
      await page.goto(runtime.signInUrl);
      await page.waitForFunction(connected);
      assert.deepEqual(loads, [303, 200]);
      await page.close();

      // What it says to do restores the workbench.
      await context.clearCookies({ domain: '127.0.0.1' });
      const restored = await context.newPage();
      const restoredLoads = documentLoads(restored);
      await restored.goto(runtime.signInUrl);
      await restored.waitForFunction(connected);
      assert.deepEqual(restoredLoads, [303, 200]);
    } finally {
      await closeContexts(browser);
    }
  });

  // A WebSocket never sees the 401 of a refused handshake. After a few
  // failed attempts the shell asks /ws with fetch, which carries the same
  // cookies, and a 401 there shows the signed-out message as one from the
  // API does. It used to say only that the live connection was reconnecting,
  // forever. Here Playwright closes every live connection attempt before it
  // opens and answers the shell's request to /ws with 401, in every engine;
  // the next journey gets there with real cookies, in WebKit on macOS.
  test(`${engine.name}: when the live connection never opens and /ws answers 401, the shell checks /ws, reloads once and says it is signed out`, { timeout: 120_000 }, async () => {
    const browser = await browserFor(engine);
    try {
      const context = await signedInContext(browser);
      let attempts = 0;
      await context.routeWebSocket((url) => url.host === workbenchHost() && url.pathname === '/ws', (socket) => {
        attempts += 1;
        socket.close();
      });
      const checks = [];
      await context.route((url) => url.host === workbenchHost() && url.pathname === '/ws', (route) => {
        checks.push(route.request().method());
        return route.fulfill({ status: 401, contentType: 'application/json', body: '{"error":"unauthorized"}' });
      });
      const page = await context.newPage();
      const loads = documentLoads(page);
      await page.goto(workbench());
      // Three failed attempts, then the check, one reload, three more, then
      // the message.
      await assertSignedOutMessage(page, loads);
      assert.deepEqual(loads, [200, 200]);
      // One check before the reload and one after (a later one may follow
      // three more failed attempts, seconds on).
      assert.deepEqual(checks.slice(0, 2), ['GET', 'GET']);
      assert.ok(attempts >= 6, `live connection attempts: ${attempts}`);
      assert.equal(await page.evaluate(() => S.liveConnection), 'reconnecting');
      // The API still answers; only /ws refused this browser.
      assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 200);
    } finally {
      await closeContexts(browser);
    }
  });

  test(caps
    ? `${engine.name}: the cookies a page on another 127.0.0.1 port sets with Path=/ws keep WebKit on macOS's live connection from opening, and the shell says it is signed out`
    : `${engine.name}: the cookies a page on another 127.0.0.1 port sets with Path=/ws all reach the workbench in ${type} on ${process.platform}, whose live connection opens`, { timeout: 180_000 }, async () => {
    const browser = await browserFor(engine);
    try {
      const context = await signedInContext(browser);
      assert.equal(await fillFrom127Page(context, '/ws'), FLOOD);
      await assertCookieHeaderAt127(context, type, '/ws');
      // An ordinary request to /ws carries the same cookies as the live
      // connection's handshake: without the token cookie it gets 401, with
      // it 400 (it is not a handshake).
      const capped = await tokenLeftOff(context, type, '/ws');
      assert.equal(await navigationStatus(context, '/api/agents'), 200);
      const page = await context.newPage();
      const loads = documentLoads(page);
      const sockets = [];
      page.on('websocket', (socket) => {
        if (new URL(socket.url()).pathname === '/ws') sockets.push(socket);
      });
      await page.goto(workbench());
      await page.waitForFunction(() => typeof S !== 'undefined');
      assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 200);
      if (!capped) {
        await page.waitForFunction(connected);
        // With the sign-in cookie, /ws answers an ordinary request 400.
        assert.equal(await page.evaluate(async () => (await fetch('/ws')).status), 400);
        assert.deepEqual(loads, [200]);
        return;
      }
      // Three failed attempts, then the check, one reload, three more, then
      // the message.
      await assertSignedOutMessage(page, loads);
      assert.deepEqual(loads, [200, 200]);
      assert.ok(sockets.length >= 3, `live connection attempts: ${sockets.length}`);
      assert.equal(await page.evaluate(() => S.liveConnection), 'reconnecting');

      // Removing 127.0.0.1's cookies and signing in again restores it.
      await context.clearCookies({ domain: '127.0.0.1' });
      await page.goto(runtime.signInUrl);
      await page.waitForFunction(connected);
      assert.equal(await signedOutMessage(page).count(), 0);
    } finally {
      await closeContexts(browser);
    }
  });
}
