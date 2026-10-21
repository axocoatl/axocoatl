import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { request as httpRequest } from 'node:http';
import { mkdir, realpath, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';

import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';
import { cookieNames, startCookieEcho } from '../support/cookie-echo.mjs';

// What a Preview page can do with cookies, through the real Preview proxy to
// a real Session app on local Podman. Safari on macOS lets a top-level page on
// <name>.localhost set a Domain=localhost cookie and then sends it to the
// workbench and to every other Preview origin; Chromium and WebKit on Linux
// refuse it (operate/security.mdx). From a full-page Preview the journey sets
// a cookie that is not ASCII, a forged axocoatl-token-<port> and -signed-in
// cookie, and 40 cookies of 1 KB. It asserts, in every engine:
// - with those cookies the workbench stays signed in, and a signed-out
//   browser passes the retry page at most once per navigation;
// - the browser may send forged axocoatl-token-* cookies to a Preview origin
//   (Safari does), but no Preview's app ever receives one, over HTTP or on a
//   WebSocket upgrade: the proxy strips them, and passes the app's other
//   cookies on (on an upgrade, those that are visible ASCII);
// - an app's Set-Cookie with Domain=localhost stays host-only on its own
//   Preview origin, because the proxy removes Domain.
// Other cookies such a page can set do harm in Safari (signed-out.test.mjs):
// about 52 large ones with Path=/api sign it out of the API, and with
// Path=/ws keep the live connection from opening. About 1,000 at once,
// seconds after a sign-in with no workbench page open, sign it out
// (operate/security.mdx).
// It needs Podman and the curated node:20-slim image, and is skipped when
// that image is not on the machine, rather than pulling it. CI pulls the
// image and sets AXOCOATL_E2E_REQUIRE_NODE_IMAGE=1, which makes a missing
// image fail this journey instead.
const IMAGE = 'docker.io/library/node:20-slim';
const FLOOD = 40;
const imageMissing = await new Promise((resolve) => {
  execFile('podman', ['image', 'exists', IMAGE], { timeout: 60_000 }, (error) => resolve(error
    ? `${IMAGE} is not on this machine (or Podman is not running); pull it to run this journey`
    : false));
});
if (imageMissing && process.env.AXOCOATL_E2E_REQUIRE_NODE_IMAGE === '1') {
  throw new Error(`AXOCOATL_E2E_REQUIRE_NODE_IMAGE=1, but ${imageMissing}`);
}

// The Session's app: one Node server on ports 3000 and 5000 that records the
// Cookie headers of every request and WebSocket upgrade it receives (as
// Latin-1, so a byte outside ASCII survives) and lists them at /__seen.
// /set-cookies answers with Set-Cookie headers that ask for Domain=localhost,
// the way an app's server can.
const APP = String.raw`
const crypto = require('node:crypto');
const http = require('node:http');
const seen = [];
const cookieOf = (request) => {
  const cookies = [];
  for (let i = 0; i < request.rawHeaders.length; i += 2) {
    if (request.rawHeaders[i].toLowerCase() === 'cookie') cookies.push(request.rawHeaders[i + 1]);
  }
  return cookies.join('; ');
};
for (const port of [3000, 5000]) {
  const server = http.createServer({ maxHeaderSize: 1024 * 1024 }, (request, response) => {
    const url = new URL(request.url, 'http://preview');
    if (url.pathname === '/__seen') {
      response.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' });
      response.end(JSON.stringify(seen));
      return;
    }
    seen.push({ port, path: url.pathname, cookie: cookieOf(request) });
    const headers = { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' };
    if (url.pathname === '/set-cookies') {
      headers['set-cookie'] = [
        'upstream-domain=from-upstream; Domain=localhost; Path=/',
        'upstream-domain-upper=from-upstream; DOMAIN=LOCALHOST; Path=/; SameSite=Lax',
        'upstream-domain-dot=from-upstream; Path=/; domain=.localhost',
        'upstream-host-only=from-upstream; Path=/; HttpOnly',
      ];
    }
    response.writeHead(200, headers);
    response.end('<!doctype html><meta charset="utf-8"><title>preview ' + port + url.pathname + '</title>');
  });
  server.on('upgrade', (request, socket) => {
    seen.push({ port, path: new URL(request.url, 'http://preview').pathname, cookie: cookieOf(request), upgrade: true });
    const accept = crypto.createHash('sha1')
      .update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11')
      .digest('base64');
    socket.on('error', () => {});
    socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n');
  });
  server.listen(port, '0.0.0.0');
}
`;

let runtime;
let browser;
let session;
let listenerPort;
let safariSharesLocalhostCookies = false;

const previewHost = (port) => `${session.id}-p${port}.localhost`;
const previewOrigin = (port) => `http://${previewHost(port)}:${listenerPort}`;

/** GET a Preview path through the daemon's real Preview proxy, from Node. */
function previewGet(port, pathname) {
  return new Promise((resolve, reject) => {
    const request = httpRequest({
      host: '127.0.0.1',
      port: listenerPort,
      path: pathname,
      headers: { host: `${previewHost(port)}:${listenerPort}` },
      timeout: 10_000,
    }, (response) => {
      let body = '';
      response.setEncoding('utf8');
      response.on('data', (chunk) => { body += chunk; });
      response.on('end', () => resolve({ status: response.statusCode, body }));
    });
    request.on('timeout', () => request.destroy(new Error('Preview request timed out')));
    request.on('error', reject);
    request.end();
  });
}

/** Every request the Session app has received, on either port. */
async function appSeen() {
  const response = await previewGet(3000, '/__seen');
  assert.equal(response.status, 200, response.body);
  return JSON.parse(response.body);
}

async function api(method, pathname, body) {
  const response = await fetch(`${runtime.baseUrl}${pathname}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch { json = text; }
  return { status: response.status, json };
}

before(async () => {
  if (imageMissing) return;
  runtime = await launchTestDaemon({ network: 'bridge' });
  listenerPort = Number(new URL(runtime.baseUrl).port);
  const project = path.join(runtime.projectsDirectory, 'preview-cookies');
  await mkdir(project);
  await writeFile(path.join(project, 'preview-cookie-app.cjs'), APP);
  const workspace = await api('POST', '/api/workspaces', {
    path: await realpath(project), name: 'Preview cookies',
  });
  assert.equal(workspace.status, 200, JSON.stringify(workspace.json));
  const created = await api('POST', `/api/workspaces/${workspace.json.id}/sessions`, {
    name: 'Preview cookies', mode: { kind: 'single_agent', agent_id: 'browser-test-coder' },
    enabled_skills: [], exposed_ports: [3000, 5000], image: IMAGE,
    setup_command: null, setup_approved: false, setup_reviewed: true,
  });
  assert.equal(created.status, 200, JSON.stringify(created.json));
  session = created.json;
  assert.equal(session.environment.state, 'ready', JSON.stringify(session.environment));
  const task = await api('POST', `/api/sessions/${session.id}/tasks`, {
    command: 'node preview-cookie-app.cjs',
  });
  assert.equal(task.status, 200, JSON.stringify(task.json));
  let last = null;
  for (let attempt = 0; attempt < 150; attempt += 1) {
    try {
      const [three, five] = [await previewGet(3000, '/__seen'), await previewGet(5000, '/__seen')];
      if (three.status === 200 && five.status === 200) { last = null; break; }
      last = `${three.status} ${three.body.slice(0, 200)} / ${five.status} ${five.body.slice(0, 200)}`;
    } catch (error) {
      last = error.message;
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  if (last !== null) {
    const tasks = await api('GET', `/api/sessions/${session.id}/tasks`);
    throw new Error(`the Session app did not answer through the Preview proxy: ${last}\ntasks: ${JSON.stringify(tasks.json)}`);
  }
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
  safariSharesLocalhostCookies = process.platform === 'darwin'
    && browser.browserType().name() === 'webkit';
});

after(async () => {
  await browser?.close();
  await runtime?.stop();
});

/**
 * Open `pathname` on Preview `port` top-level, as Open full preview does, and
 * set from its script: a cookie that is not ASCII, a forged token cookie, a
 * forged "signed in" cookie, a host-only cookie and a flood, all but the
 * host-only one for Domain=localhost. Returns what the page then reads.
 */
async function forgeFromFullPreview(context, port, pathname = '/forge') {
  const page = await context.newPage();
  try {
    const response = await page.goto(`${previewOrigin(port)}${pathname}`);
    assert.equal(response?.status(), 200);
    return await page.evaluate(({ listener, flood }) => {
      document.cookie = 'preview-host-only=from-a; Path=/';
      document.cookie = 'junk=é; Domain=localhost; Path=/';
      document.cookie = `axocoatl-token-${listener}=forged-by-a-preview; Domain=localhost; Path=/`;
      document.cookie = `axocoatl-token-${listener}-signed-in=1; Domain=localhost; Path=/`;
      for (let i = 0; i < flood; i += 1) {
        document.cookie = `flood-${i}=${'x'.repeat(1000)}; Domain=localhost; Path=/`;
      }
      return document.cookie;
    }, { listener: listenerPort, flood: FLOOD });
  } finally {
    await page.close();
  }
}

/** Load `url` top-level in a new page of `context`. */
async function visit(context, url) {
  const page = await context.newPage();
  try {
    const response = await page.goto(url);
    assert.equal(response?.status(), 200, url);
  } finally {
    await page.close();
  }
}

test('a full-page Preview\'s non-ASCII, forged and 40 x 1 KB cookies leave the workbench signed in, and no Preview app receives an axocoatl-token cookie', { timeout: 300_000, skip: imageMissing }, async () => {
  const context = await browser.newContext();
  const echo = await startCookieEcho();
  try {
    // Sign in with the real link, so the product sets its own cookies.
    const workbench = await context.newPage();
    const signedIn = await workbench.goto(runtime.signInUrl);
    assert.equal(signedIn?.status(), 200);
    assert.equal(new URL(signedIn.url()).search, '', 'the sign-in redirect drops ?token=');
    assert.notEqual(await workbench.title(), 'Sign in to Axocoatl');
    await workbench.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');

    // The app's server asks for Domain=localhost cookies; the proxy removes
    // Domain, so the browser keeps them host-only on Preview 3000.
    await visit(context, `${previewOrigin(3000)}/set-cookies`);
    const forged = await forgeFromFullPreview(context, 3000);
    assert.equal(forged.includes('upstream-domain=from-upstream'), true, cookieNames(forged));
    assert.equal(forged.includes('preview-host-only=from-a'), true, cookieNames(forged));
    assert.equal(forged.includes(`axocoatl-token-${listenerPort}=forged-by-a-preview`),
      safariSharesLocalhostCookies, cookieNames(forged));

    // What the browser sends, before any proxy: the workbench's own
    // host-only token cookie never leaves localhost; in Safari the forged
    // Domain=localhost cookies go everywhere, axocoatl-token-* included.
    const pairs = (header) => header.split('; ');
    const realToken = `axocoatl-token-${listenerPort}=${runtime.token}`;
    const forgedToken = `axocoatl-token-${listenerPort}=forged-by-a-preview`;
    const atWorkbench = await echo.cookieAt(context, 'localhost');
    const atPreview5000 = await echo.cookieAt(context, previewHost(5000));
    assert.equal(pairs(atWorkbench).includes(realToken), true, cookieNames(atWorkbench));
    assert.equal(atPreview5000.includes(runtime.token), false, cookieNames(atPreview5000));
    for (const header of [atWorkbench, atPreview5000]) {
      const sent = pairs(header);
      assert.equal(sent.includes(forgedToken), safariSharesLocalhostCookies, cookieNames(header));
      assert.equal(sent.some((pair) => /^junk=[^\x00-\x7f]/.test(pair)), safariSharesLocalhostCookies,
        cookieNames(header));
      assert.equal(sent.filter((pair) => pair.startsWith('flood-')).length,
        safariSharesLocalhostCookies ? FLOOD : 0, cookieNames(header));
      // The app's server could not widen its cookies to localhost.
      assert.equal(header.includes('upstream-'), false, cookieNames(header));
      assert.equal(header.includes('preview-host-only='), false, cookieNames(header));
    }
    assert.equal(pairs(atPreview5000).includes(`axocoatl-token-${listenerPort}-signed-in=1`),
      safariSharesLocalhostCookies, cookieNames(atPreview5000));

    // What the Session app receives through the real proxy: top-level, from
    // a fetch on its own origin, and on a WebSocket upgrade.
    await visit(context, `${previewOrigin(5000)}/check`);
    await visit(context, `${previewOrigin(3000)}/check`);
    for (const port of [3000, 5000]) {
      const page = await context.newPage();
      try {
        await page.goto(`${previewOrigin(port)}/check-page`);
        assert.equal(await page.evaluate(async () => (await fetch('/check-fetch')).status), 200);
        assert.equal(await page.evaluate(() => new Promise((resolve) => {
          const socket = new WebSocket(`ws://${location.host}/check-socket`);
          const timer = setTimeout(() => resolve('timed out'), 10_000);
          socket.onopen = () => { clearTimeout(timer); socket.close(); resolve('open'); };
          socket.onerror = () => { clearTimeout(timer); resolve('error'); };
        })), 'open', `WebSocket through the Preview ${port} proxy`);
      } finally {
        await page.close();
      }
    }
    const seen = await appSeen();
    assert.ok(seen.filter((request) => request.upgrade).length === 2,
      JSON.stringify(seen.map(({ port, path: at }) => `${port}${at}`)));
    assert.ok(seen.length >= 10, JSON.stringify(seen.map(({ port, path: at }) => `${port}${at}`)));
    for (const request of seen) {
      assert.equal(/axocoatl-token-/i.test(request.cookie), false,
        `${request.port}${request.path}: ${cookieNames(request.cookie)}`);
    }
    // In Safari the forged non-token cookies reach the app on both ports.
    // A WebSocket upgrade forwards only cookies that are visible ASCII (the
    // handshake to the app cannot carry others), so there the non-ASCII one
    // is left out and the rest still arrive.
    for (const request of seen.filter(({ path: at }) => at.startsWith('/check'))) {
      const label = `${request.port}${request.path}: ${cookieNames(request.cookie)}`;
      assert.equal(request.cookie.includes('junk='), safariSharesLocalhostCookies && !request.upgrade, label);
      assert.equal(request.cookie.includes('flood-39='), safariSharesLocalhostCookies, label);
    }
    const at = (port, pathname) => {
      const found = seen.find((request) => request.port === port && request.path === pathname);
      assert.ok(found, `the app on ${port} received no ${pathname}`);
      return found.cookie.split('; ');
    };
    const check5000 = at(5000, '/check');
    const check3000 = at(3000, '/check');
    // The proxy passes the app every other cookie: in Safari, the forged
    // non-token ones, which shows the browser sent the forged set there.
    assert.equal(check5000.some((pair) => pair.startsWith('junk=')), safariSharesLocalhostCookies,
      cookieNames(check5000.join('; ')));
    assert.equal(check5000.filter((pair) => pair.startsWith('flood-')).length,
      safariSharesLocalhostCookies ? FLOOD : 0);
    assert.equal(check5000.some((pair) => pair.startsWith('upstream-')), false,
      cookieNames(check5000.join('; ')));
    for (const cookie of ['upstream-domain=from-upstream', 'upstream-domain-upper=from-upstream',
      'upstream-domain-dot=from-upstream', 'upstream-host-only=from-upstream', 'preview-host-only=from-a']) {
      assert.equal(check3000.includes(cookie), true, `${cookie}: ${cookieNames(check3000.join('; '))}`);
    }
    assert.equal(at(3000, '/check-fetch').includes('upstream-domain=from-upstream'), true);

    // The workbench stays signed in with these cookies (not with every set a
    // Preview can write; see the header): the open page reads the API, a
    // reload and a new page get the workbench, and both connect their
    // WebSocket.
    assert.equal(await workbench.evaluate(async () => (await fetch('/api/agents')).status), 200);
    const reloaded = await workbench.reload();
    assert.equal(reloaded?.status(), 200);
    assert.notEqual(await workbench.title(), 'Sign in to Axocoatl');
    await workbench.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');
    const again = await context.newPage();
    try {
      const response = await again.goto(`http://localhost:${listenerPort}/`);
      assert.equal(response?.status(), 200);
      assert.notEqual(await again.title(), 'Sign in to Axocoatl');
      await again.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');
      assert.equal(await again.evaluate(async () => (await fetch('/api/agents')).status), 200);
    } finally {
      await again.close();
    }
  } finally {
    await context.close();
    await echo.close();
  }
});

test('a forged signed-in cookie sends a signed-out browser through the retry page at most once per navigation', { timeout: 300_000, skip: imageMissing }, async () => {
  // A browser that never signed in.
  const context = await browser.newContext();
  try {
    await forgeFromFullPreview(context, 3000, '/forge-signed-out');
    const page = await context.newPage();
    const statuses = [];
    page.on('response', (response) => {
      const url = new URL(response.url());
      if (url.host === `localhost:${listenerPort}` && url.pathname === '/') statuses.push(response.status());
    });
    // Each navigation the person starts: in Safari the retry page (200),
    // whose own navigation gets the sign-in page (401); elsewhere the
    // sign-in page at once. Never a loop, never signed in.
    for (let navigation = 0; navigation < 2; navigation += 1) {
      statuses.length = 0;
      const signInPage = page.waitForResponse((response) =>
        new URL(response.url()).pathname === '/' && response.status() === 401);
      await page.goto(`http://localhost:${listenerPort}/`);
      await signInPage;
      await page.waitForLoadState('load');
      await page.waitForTimeout(1_000);
      assert.deepEqual(statuses, safariSharesLocalhostCookies ? [200, 401] : [401]);
      assert.equal(await page.title(), 'Sign in to Axocoatl');
    }
    // The sign-in page's CSP allows no fetch; ask from a public page.
    await page.goto(`http://localhost:${listenerPort}/health`);
    assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 401);
  } finally {
    await context.close();
  }
});
