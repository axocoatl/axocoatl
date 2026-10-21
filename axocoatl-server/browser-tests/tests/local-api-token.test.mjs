import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';

import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';

// Every context here starts signed out (plain `browser.newContext`): these
// journeys exercise the real sign-in link and cookie.
let runtime;
let browser;
let otherSite;

/**
 * Pages of another website, from a real server on the IPv6 loopback address
 * `[::1]`: a site that is neither `127.0.0.1` nor `localhost`, so a link on
 * it opens the workbench as a link on any other website would
 * (`Sec-Fetch-Site: cross-site`). A real server, not a Playwright route:
 * while a page has a route, Playwright intercepts every request the page
 * makes, the workbench's too, which a journey about a link from another
 * website does not need.
 */
async function startOtherSite() {
  const pages = new Map();
  const server = createServer((request, response) => {
    const body = pages.get(new URL(request.url, 'http://[::1]').pathname);
    response.writeHead(body === undefined ? 404 : 200, {
      'content-type': 'text/html; charset=utf-8',
      'cache-control': 'no-store',
    });
    response.end(body ?? '');
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '::1', resolve);
  });
  const origin = `http://[::1]:${server.address().port}`;
  return {
    /** Serve `body` at `pathname` and return its URL. */
    page(pathname, body) {
      pages.set(pathname, body);
      return `${origin}${pathname}`;
    },
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

before(async () => {
  runtime = await launchTestDaemon();
  otherSite = await startOtherSite();
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
});

after(async () => {
  await browser?.close();
  await otherSite?.close();
  await runtime?.stop();
});

const port = () => new URL(runtime.baseUrl).port;
const workbenchConnected = () => typeof S !== 'undefined' && S.liveConnection === 'connected';
const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

/** The requests `page` has in flight, and when that set last changed. */
function trackRequests(page) {
  const requests = { inFlight: new Set(), changedAt: Date.now() };
  const started = (request) => {
    requests.inFlight.add(request);
    requests.changedAt = Date.now();
  };
  const ended = (request) => {
    requests.inFlight.delete(request);
    requests.changedAt = Date.now();
  };
  page.on('request', started);
  page.on('requestfinished', ended);
  page.on('requestfailed', ended);
  return requests;
}

/**
 * Wait until the workbench is connected and its page has had no request in
 * flight for half a second; a journey here does this before it reloads the
 * workbench or leaves it. On Linux, WebKit's network process now and then
 * crashed when a reload or a navigation cancelled the requests a workbench
 * that had just connected was still making, and the crash signs every
 * context out (README.md, "WebKit (Safari)"). A person reloads or leaves a
 * workbench that has loaded; these journeys test signing in, not that crash.
 */
async function waitForSettledWorkbench(page, requests) {
  await page.waitForFunction(workbenchConnected);
  const deadline = Date.now() + 30_000;
  while (requests.inFlight.size > 0 || Date.now() - requests.changedAt < 500) {
    if (Date.now() > deadline) {
      const urls = [...requests.inFlight].map((request) => request.url());
      throw new Error(`The workbench still had requests in flight after 30 s: ${urls.join(', ')}`);
    }
    await delay(50);
  }
}

/**
 * Click the sign-in page's Continue link and wait for the navigation it
 * starts: the page is already at `/`, so waiting for that URL would not wait
 * for anything. The browser must still hold the sign-in cookie, and `/` must
 * then serve the workbench; either failing names what went wrong at once
 * instead of leaving the wait for a live connection to time out.
 */
async function clickContinue(page, label) {
  assert.ok(await signInCookie(page.context()), `${label}: the browser still holds the sign-in cookie`);
  const workbench = `http://localhost:${port()}/`;
  const [response] = await Promise.all([
    page.waitForResponse((candidate) => candidate.url() === workbench
      && candidate.request().isNavigationRequest()),
    page.click('a[href="/"]'),
  ]);
  assert.equal(response.status(), 200, `${label}: Continue opens the workbench`);
  await page.waitForFunction(workbenchConnected);
}

async function signInCookie(context) {
  return (await context.cookies()).find((cookie) => cookie.name === runtime.cookieName);
}

async function signedInCookie(context) {
  return (await context.cookies()).find((cookie) => cookie.name === `${runtime.cookieName}-signed-in`);
}

/**
 * The page landed on the workbench at `/` on localhost without the token.
 * The workbench may already have written the Session it restored into the
 * URL (`?session=`) when `page.goto` returns, so only that parameter may
 * follow.
 */
function assertLandedWithoutToken(page) {
  const landed = new URL(page.url());
  assert.equal(`${landed.origin}${landed.pathname}`, `http://localhost:${port()}/`);
  assert.deepEqual([...landed.searchParams.keys()].filter((name) => name !== 'session'), []);
  assert.ok(!page.url().includes(runtime.token));
}

test('a signed-out browser gets the sign-in page and the API refuses it', async () => {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const response = await page.goto(`http://localhost:${port()}/`);
    assert.equal(response.status(), 401);
    assert.equal(response.headers()['cache-control'], 'no-store');
    assert.match(await page.locator('body').innerText(), /axocoatl url/);
    assert.equal(await page.locator('a[href="/"]').count(), 1);
    assert.equal(await page.locator('ax-rail').count(), 0);
    assert.match(response.headers()['content-security-policy'], /default-src 'none'/);
    const api = await context.request.get(`http://localhost:${port()}/api/sessions`);
    assert.equal(api.status(), 401);
    assert.equal(await signInCookie(context), undefined);
  } finally {
    await context.close();
  }
});

test('the sign-in link sets an HttpOnly cookie, drops the token from the URL and opens the workbench', async () => {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const redirects = [];
    page.on('response', (response) => {
      if (response.status() === 303) redirects.push(response.headers());
    });
    await page.goto(runtime.signInUrl);
    assertLandedWithoutToken(page);
    assert.equal(redirects.length, 1);
    assert.equal(redirects[0].location, '/');
    assert.equal(redirects[0]['referrer-policy'], 'no-referrer');
    assert.equal(redirects[0]['cache-control'], 'no-store');

    const cookie = await signInCookie(context);
    assert.ok(cookie, 'the sign-in cookie is stored');
    assert.equal(cookie.value, runtime.token);
    assert.equal(cookie.domain, 'localhost');
    assert.equal(cookie.path, '/');
    assert.equal(cookie.httpOnly, true);
    assert.equal(cookie.sameSite, 'Strict');
    // Beside it, a cookie that holds no secret says this browser signed in.
    const signedIn = await signedInCookie(context);
    assert.ok(signedIn, 'the signed-in cookie is stored');
    assert.equal(signedIn.value, '1');
    assert.equal(signedIn.domain, 'localhost');
    assert.equal(signedIn.httpOnly, true);
    assert.equal(signedIn.sameSite, 'Lax');
    assert.equal(await page.evaluate(() => document.cookie.includes('axocoatl-token')), false);

    await page.waitForFunction(workbenchConnected);
    assert.equal(await page.locator('ax-rail').count(), 1);
    assert.equal(await page.evaluate(async () => (await fetch('/api/sessions')).status), 200);
    assert.ok(!page.url().includes(runtime.token));
  } finally {
    await context.close();
  }
});

// Safari (WebKit) leaves the SameSite=Strict cookie off a navigation that the
// redirect from 127.0.0.1 carried to localhost, a different site, and off its
// reloads; the workbench then opens the address again from its own origin.
// In every engine the person ends up in the workbench, and stays there on
// reload.
test('a loopback IP sign-in link and a loopback IP bookmark both reach the workbench', async () => {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const requests = trackRequests(page);
    await page.goto(`http://127.0.0.1:${port()}/?token=${runtime.token}`);
    assertLandedWithoutToken(page);
    assert.ok(await signInCookie(context));
    await waitForSettledWorkbench(page, requests);
    await page.reload();
    await waitForSettledWorkbench(page, requests);

    const session = runtime.fixtures.alpha.sessions[0].id;
    await page.goto(`${runtime.baseUrl}/?session=${encodeURIComponent(session)}`);
    assert.equal(page.url(), `http://localhost:${port()}/?session=${encodeURIComponent(session)}`);
    await waitForSettledWorkbench(page, requests);
    await page.reload();
    await page.waitForFunction(workbenchConnected);
    assert.equal(page.url(), `http://localhost:${port()}/?session=${encodeURIComponent(session)}`);
  } finally {
    await context.close();
  }
});

test('once signed in, a link from another site to the loopback IP or localhost still needs one Continue click', async () => {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const requests = trackRequests(page);
    await page.goto(runtime.signInUrl);
    await waitForSettledWorkbench(page, requests);
    assert.ok(await signedInCookie(context));
    const elsewhere = otherSite.page(
      '/workbench-links',
      `<!doctype html><a id="ip" href="http://127.0.0.1:${port()}/">IP</a>`
        + `<a id="name" href="http://localhost:${port()}/">localhost</a>`,
    );
    for (const link of ['#ip', '#name']) {
      await page.goto(elsewhere);
      const workbench = `http://localhost:${port()}/`;
      const [response] = await Promise.all([
        page.waitForResponse((candidate) => candidate.url() === workbench
          && candidate.request().isNavigationRequest()),
        page.click(link),
      ]);
      // The page another site opened is the sign-in page, never the workbench
      // or a page that opens it.
      assert.equal(response.status(), 401, link);
      await page.waitForURL(workbench);
      assert.match(await page.locator('body').innerText(), /axocoatl url/, link);
      assert.equal(await page.locator('ax-rail').count(), 0, link);
      await clickContinue(page, link);
      await waitForSettledWorkbench(page, requests);
    }
  } finally {
    await context.close();
  }
});

test('a wrong sign-in link sets no cookie', async () => {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const response = await page.goto(`http://localhost:${port()}/?token=not-this-daemon`);
    assert.equal(response.status(), 401);
    assert.match(await page.locator('body').innerText(), /not valid/);
    assert.equal(await signInCookie(context), undefined);
  } finally {
    await context.close();
  }
});

test('a sign-in link opened from another site needs one Continue click', async () => {
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    const elsewhere = otherSite.page(
      '/sign-in-link',
      `<!doctype html><a id="sign-in" href="${runtime.signInUrl}">Open Axocoatl</a>`,
    );
    await page.goto(elsewhere);
    await Promise.all([page.waitForURL(`http://localhost:${port()}/`), page.click('#sign-in')]);
    // The cookie is stored, but SameSite=Strict keeps it off the rest of this
    // cross-site navigation, so the sign-in page offers a same-origin link.
    assert.ok(await signInCookie(context));
    assert.match(await page.locator('body').innerText(), /axocoatl url/);
    await clickContinue(page, 'sign-in link from another site');
  } finally {
    await context.close();
  }
});

test('Node clients need the token as a header', async () => {
  for (const pathname of ['/api/sessions', '/.well-known/agent.json']) {
    assert.equal((await runtime.fetchWithoutToken(pathname)).status, 401, pathname);
    const authorized = await runtime.fetchWithoutToken(pathname, {
      headers: { authorization: `Bearer ${runtime.token}` },
    });
    assert.equal(authorized.status, 200, pathname);
  }
  const apiKey = await runtime.fetchWithoutToken('/api/sessions', {
    headers: { 'x-api-key': runtime.token },
  });
  assert.equal(apiKey.status, 200);
  assert.equal((await runtime.fetchWithoutToken('/health/live')).status, 200);
  assert.equal((await runtime.fetchWithoutToken('/ui/tokens.css')).status, 200);
  assert.ok(!runtime.logs().includes(runtime.token), 'the token reached the daemon output');
});
