import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { createServer, request as httpRequest } from 'node:http';
import { mkdir, realpath, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { chromium, devices } from 'playwright';

import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';
import { cookieNames, startCookieEcho } from '../support/cookie-echo.mjs';

// What a Preview page can do with cookies, through the real Preview proxy to
// a real Session app on local Podman. Safari on macOS lets a top-level page on
// <name>.localhost set a Domain=localhost cookie and then sends it to every
// other Preview origin and every other localhost address, but never to the
// workbench, which is served at 127.0.0.1; Chromium and WebKit on Linux
// refuse it (operate/security.mdx). So in Safari the daemon never serves a
// Preview as a page of its own (src/preview_document.rs): a typed address, a
// redirect, a real Command-click in the Preview frame or in the Files view's
// frame, and the legacy /api/sessions/<id>/proxy/<port> path (refused in
// every engine) all end at a refusal page or text, the app is never asked,
// and no cookie is set. The embedded Preview still works, and Chromium opens
// a Preview top-level as before.
//
// The daemon knows Safari by its user agent alone, and Playwright's WebKit
// sends Safari's Mac user agent on Linux too, so there it is refused like
// Safari (safariRefusesTopLevelPreviews) while its cookie jar still refuses
// Domain=localhost cookies (safariSharesLocalhostCookies is macOS only).
// WebKit browsers on Linux (GNOME Web, WebKitGTK, WPE) send WebKit's own
// `X11; Linux` user agent instead, which neither the daemon nor the
// workbench takes for Safari. So in WebKit on Linux every journey runs a
// second time with that user agent ("WebKit on Linux (X11 user agent): ..."):
// Open full preview and the Preview frame's popups open the Preview in a tab,
// and a top-level Preview runs through the proxy with its cookies and its
// WebSocket upgrade, as in Chromium, on the engine of those browsers.
//
// The first journeys set, from a top-level page on a *.localhost address, a
// cookie that is not ASCII, a forged axocoatl-token-<port> and -signed-in
// cookie, and 40 cookies of 1 KB, all for Domain=localhost. In Chromium
// that page is the Preview, opened top-level (page.goto). In WebKit it is a
// page Axocoatl does not serve (the cookie echo at forge.localhost), as any
// other development server on a *.localhost address could be, because the
// daemon refuses the Preview there. They assert, in every engine:
// - none of those cookies reaches the workbench, which stays signed in, and
//   a signed-out browser gets the sign-in page; a page on another 127.0.0.1
//   port, which can set the "signed in" cookie, sends it through the retry
//   page at most once;
// - the browser may send forged axocoatl-token-* cookies to a Preview origin
//   (Safari does), but no Preview's app ever receives one, over HTTP or on a
//   WebSocket upgrade. Outside Safari the proxy strips them and passes the
//   app's other cookies on (on an upgrade, those that are visible ASCII); in
//   Safari the app receives no top-level request at all;
// - an app's Set-Cookie with Domain=localhost stays host-only on its own
//   Preview origin, because the proxy removes Domain.
// What cookies set for 127.0.0.1 by a page on another 127.0.0.1 port can do
// in Safari is in signed-out.test.mjs: 51 large ones and smaller fillers
// with Path=/api sign it out of the API, with Path=/ out of the workbench,
// and with Path=/ws keep the live connection from opening. About 1,000 at
// once, seconds after a sign-in with no workbench page open, sign it out
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
// Latin-1, so a byte outside ASCII survives), and each request's User-Agent,
// and lists them at /__seen.
// /set-cookies answers with Set-Cookie headers that ask for Domain=localhost,
// the way an app's server can. /embed nests a frame at the address its query
// names, and a page under /at-published- sets cookies for its own address
// from script and asks its server for the same path with -fetch appended.
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
    seen.push({ port, path: url.pathname, cookie: cookieOf(request), userAgent: request.headers['user-agent'] || '' });
    const headers = { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' };
    if (url.pathname === '/set-cookies') {
      headers['set-cookie'] = [
        'upstream-domain=from-upstream; Domain=localhost; Path=/',
        'upstream-domain-upper=from-upstream; DOMAIN=LOCALHOST; Path=/; SameSite=Lax',
        'upstream-domain-dot=from-upstream; Path=/; domain=.localhost',
        'upstream-host-only=from-upstream; Path=/; HttpOnly',
      ];
    }
    if (url.pathname === '/redirect-to') {
      response.writeHead(302, { location: url.searchParams.get('to'), 'set-cookie': 'app-redirect=1; Path=/' });
      response.end();
      return;
    }
    if (url.pathname === '/embed') {
      const src = (url.searchParams.get('src') || '').replace(/[&"<>]/g, (c) => '&#' + c.charCodeAt(0) + ';');
      response.writeHead(200, headers);
      response.end('<!doctype html><meta charset="utf-8"><title>embed</title><iframe id="nested" src="' + src + '"></iframe>');
      return;
    }
    if (url.pathname.startsWith('/at-published-') && !url.pathname.endsWith('-fetch')) {
      response.writeHead(200, headers);
      response.end('<!doctype html><meta charset="utf-8"><title>at published</title><script>'
        + 'document.cookie = "published-api=1; Path=/api"; document.cookie = "published-root=1; Path=/";'
        + 'fetch(location.pathname + "-fetch").then(() => { document.title = "fetched"; });</script>');
      return;
    }
    response.writeHead(200, headers);
    // A page under /top- says, from script, that it ran: a host-only cookie
    // and a Domain=localhost one.
    const ran = url.pathname.startsWith('/top-')
      ? '<script>document.cookie = "ran-top-level-host=1; Path=/";'
        + 'document.cookie = "ran-top-level=1; Domain=localhost; Path=/";</script>'
      : '';
    response.end('<!doctype html><meta charset="utf-8"><title>preview ' + port + url.pathname + '</title>' + ran
      + '<a id="self-link" href="/top-cmd-clicked" style="display:block;font-size:40px">Open</a>');
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
// A page on a *.localhost address that Axocoatl does not serve.
let echo;
// WebKit on macOS shares those cookies; WebKit on Linux refuses them.
let safariSharesLocalhostCookies = false;
// In WebKit, which says it is Safari on a Mac on either system, the
// workbench keeps Previews embedded (static/ui/preview-embedding.js) and the
// daemon refuses a Preview page that would run top-level
// (src/preview_document.rs). These two are the running journey's
// (`journey` below): false when it runs with the X11 user agent.
let safariKeepsPreviewsEmbedded = false;
let safariRefusesTopLevelPreviews = false;
// The user agents every journey runs with: the browser's own, which in
// WebKit is Safari's (`safari`, set before the journeys), and in WebKit on
// Linux also WebKit's own Linux one (`linux`).
const webkitOnLinux = chromium.name() === 'webkit' && process.platform === 'linux';
const profiles = [{ name: '', contextOptions: {}, safari: false, linux: false }];
if (webkitOnLinux) {
  profiles.push({ name: 'WebKit on Linux (X11 user agent): ', contextOptions: {}, safari: false, linux: true });
}
const X11_WEBKIT_USER_AGENT = /^Mozilla\/5\.0 \(X11; Linux [^)]+\) AppleWebKit\//;
// Except for a tab WebKit opens on its own, on a Command-click (Control on
// Linux): Playwright gives Safari's Mac user agent only to the pages it
// opens or that a page opens, and on Linux the first request of such a tab
// carries WebKit's own (X11; Linux), as any WebKit on Linux does. The daemon
// serves that request as it serves WebKit on Linux, whose cookie jar refuses
// Domain=localhost cookies. On macOS that request says Safari on a Mac.
let browserOpenedTabsSaySafari = false;

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
  // An HTML file the Files view shows in its own sandboxed frame, with a
  // link to the Preview's address.
  await writeFile(path.join(project, 'links.html'), '<!doctype html><title>links</title>'
    + `<a id="files-link" href="${previewOrigin(3000)}/top-files-clicked" style="display:block;font-size:40px">Preview</a>`);
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
  const webkit = browser.browserType().name() === 'webkit';
  safariSharesLocalhostCookies = process.platform === 'darwin' && webkit;
  profiles[0].safari = webkit;
  browserOpenedTabsSaySafari = safariSharesLocalhostCookies;
  if (webkitOnLinux) {
    // The user agent of a WebKit browser on this machine, as GNOME Web
    // sends it: its platform and WebKit's version.
    const context = await browser.newContext();
    const platform = await (await context.newPage()).evaluate(() => navigator.platform);
    await context.close();
    assert.match(platform, /^Linux /);
    const [major, minor = '0'] = browser.version().split('.');
    profiles[1].contextOptions = {
      userAgent: `Mozilla/5.0 (X11; ${platform}) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/${major}.${minor} Safari/605.1.15`,
    };
  }
  echo = await startCookieEcho();
});

/**
 * Register the journey `name` once for each profile, with that profile's
 * Safari flags while it runs.
 */
function journey(name, options, run) {
  for (const profile of profiles) {
    test(`${profile.name}${name}`, options, async () => {
      safariKeepsPreviewsEmbedded = profile.safari;
      safariRefusesTopLevelPreviews = profile.safari;
      await run(profile);
    });
  }
}

after(async () => {
  await browser?.close();
  await echo?.close();
  await runtime?.stop();
});

/**
 * Open `url` top-level, a page on a *.localhost address, and set from its
 * script: a cookie that is not ASCII, a forged token cookie, a forged "signed
 * in" cookie, a host-only cookie and a flood, all but the host-only one for
 * Domain=localhost. Returns what the page then reads.
 */
async function forgeFromTopLevelPage(context, url) {
  const page = await context.newPage();
  try {
    const response = await page.goto(url);
    assert.equal(response?.status(), 200, url);
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

/**
 * Where the forging page runs: the Preview on port 3000, opened top-level as
 * someone who opens its address in a tab does; in Safari, where the daemon
 * refuses that, a page Axocoatl does not serve on another *.localhost address.
 */
const forgeUrl = (pathname) => (safariRefusesTopLevelPreviews
  ? `http://forge.localhost:${echo.port}${pathname}`
  : `${previewOrigin(3000)}${pathname}`);

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

/**
 * Assert that `page`, opened top-level on a Preview origin, shows the daemon's
 * refusal page and nothing of the app: no script, a link to the workbench.
 */
async function assertRefusalPage(page, response) {
  if (response) {
    assert.equal(response.status(), 403, page.url());
    const headers = response.headers();
    assert.equal(headers['cache-control'], 'no-store');
    assert.match(headers['content-security-policy'], /^default-src 'none';.*; sandbox$/);
  }
  assert.equal(await page.title(), 'Preview opens in the workbench', page.url());
  assert.equal(await page.locator('h1').textContent(), 'This Preview opens only in the workbench');
  assert.equal(await page.locator('script').count(), 0);
  assert.equal(await page.locator('a').getAttribute('href'),
    `http://127.0.0.1:${listenerPort}/?session=${session.id}`);
}

/** In Safari, `url` (a Preview address) opened top-level gets the refusal page. */
async function visitRefused(context, url) {
  const page = await context.newPage();
  try {
    await assertRefusalPage(page, await page.goto(url));
  } finally {
    await page.close();
  }
}

/**
 * The cookies of `context` that a page of a Preview's app set from script
 * (`ran-top-level*`) or that anything set for `Domain=localhost`, as
 * `name@domain`. The workbench's own cookies are host-only on 127.0.0.1.
 */
async function cookiesFromTopLevelApps(context) {
  return (await context.cookies())
    .filter((cookie) => cookie.name.startsWith('ran-top-level') || cookie.domain.startsWith('.'))
    .map((cookie) => `${cookie.name}@${cookie.domain}`)
    .sort();
}

journey('a top-level page\'s non-ASCII, forged and 40 x 1 KB localhost cookies never reach the workbench, and no Preview app receives an axocoatl-token cookie', { timeout: 300_000, skip: imageMissing }, async (profile) => {
  const context = await browser.newContext(profile.contextOptions);
  try {
    // What the app received before this journey (an earlier profile's).
    const earlier = (await appSeen()).length;
    // Sign in with the real link, so the product sets its own cookies.
    const workbench = await context.newPage();
    const signedIn = await workbench.goto(runtime.signInUrl);
    assert.equal(signedIn?.status(), 200);
    assert.equal(new URL(signedIn.url()).search, '', 'the sign-in redirect drops ?token=');
    assert.notEqual(await workbench.title(), 'Sign in to Axocoatl');
    await workbench.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');

    let forged;
    if (safariRefusesTopLevelPreviews) {
      // Safari: the daemon serves no Preview page top-level, so neither the
      // app's Set-Cookie nor its script reaches the browser. The forging
      // page is one Axocoatl does not serve.
      await visitRefused(context, `${previewOrigin(3000)}/set-cookies`);
      await visitRefused(context, `${previewOrigin(3000)}/forge`);
      forged = await forgeFromTopLevelPage(context, forgeUrl('/forge'));
      assert.equal(forged.includes('upstream-'), false, cookieNames(forged));
    } else {
      // The app's server asks for Domain=localhost cookies; the proxy removes
      // Domain, so the browser keeps them host-only on Preview 3000.
      await visit(context, `${previewOrigin(3000)}/set-cookies`);
      forged = await forgeFromTopLevelPage(context, forgeUrl('/forge'));
      assert.equal(forged.includes('upstream-domain=from-upstream'), true, cookieNames(forged));
    }
    assert.equal(forged.includes('preview-host-only=from-a'), true, cookieNames(forged));
    assert.equal(forged.includes(`axocoatl-token-${listenerPort}=forged-by-a-preview`),
      safariSharesLocalhostCookies, cookieNames(forged));

    // What the browser sends, before any proxy: the workbench's own
    // host-only token cookie never leaves 127.0.0.1, and no Domain=localhost
    // cookie reaches 127.0.0.1; in Safari the forged Domain=localhost
    // cookies go to every localhost address, axocoatl-token-* included.
    const pairs = (header) => header.split('; ');
    const realToken = `axocoatl-token-${listenerPort}=${runtime.token}`;
    const forgedToken = `axocoatl-token-${listenerPort}=forged-by-a-preview`;
    const atWorkbench = await echo.cookieAt(context, '127.0.0.1');
    const atLocalhost = await echo.cookieAt(context, 'localhost');
    const atPreview5000 = await echo.cookieAt(context, previewHost(5000));
    assert.deepEqual(pairs(atWorkbench),
      [realToken, `axocoatl-token-${listenerPort}-signed-in=1`], cookieNames(atWorkbench));
    for (const header of [atLocalhost, atPreview5000]) {
      assert.equal(header.includes(runtime.token), false, cookieNames(header));
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

    if (safariRefusesTopLevelPreviews) {
      // Safari sends those cookies with a top-level request for a Preview,
      // and the daemon refuses it before the proxy: the app receives no
      // request at all, so no cookie. (Embedded, Safari sends a Preview no
      // cookies; the last journey shows that.)
      for (const [port, pathname] of [[5000, '/check'], [3000, '/check'], [5000, '/check-page'], [3000, '/check-page']]) {
        await visitRefused(context, `${previewOrigin(port)}${pathname}`);
      }
      const seen = (await appSeen()).slice(earlier);
      assert.deepEqual(seen.map(({ port, path: at }) => `${port}${at}`), []);
    } else {
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
      const seen = (await appSeen()).slice(earlier);
      assert.ok(seen.filter((request) => request.upgrade).length === 2,
        JSON.stringify(seen.map(({ port, path: at }) => `${port}${at}`)));
      assert.ok(seen.length >= 10, JSON.stringify(seen.map(({ port, path: at }) => `${port}${at}`)));
      for (const request of seen) {
        assert.equal(/axocoatl-token-/i.test(request.cookie), false,
          `${request.port}${request.path}: ${cookieNames(request.cookie)}`);
        // The app served WebKit on Linux, by its own user agent.
        if (profile.linux && !request.upgrade) {
          assert.match(request.userAgent, X11_WEBKIT_USER_AGENT, `${request.port}${request.path}`);
        }
      }
      // Where the browser sends the forged non-token cookies (it does not
      // here), the proxy passes them on; a WebSocket upgrade forwards only
      // cookies that are visible ASCII (the handshake to the app cannot
      // carry others).
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
    }

    // The workbench stays signed in with these cookies (not with every set a
    // page can write; see the header): the open page reads the API, a
    // reload and a new page get the workbench, and both connect their
    // WebSocket.
    assert.equal(await workbench.evaluate(async () => (await fetch('/api/agents')).status), 200);
    const reloaded = await workbench.reload();
    assert.equal(reloaded?.status(), 200);
    assert.notEqual(await workbench.title(), 'Sign in to Axocoatl');
    await workbench.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');
    const again = await context.newPage();
    try {
      const response = await again.goto(`http://127.0.0.1:${listenerPort}/`);
      assert.equal(response?.status(), 200);
      assert.notEqual(await again.title(), 'Sign in to Axocoatl');
      await again.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');
      assert.equal(await again.evaluate(async () => (await fetch('/api/agents')).status), 200);
    } finally {
      await again.close();
    }
  } finally {
    await context.close();
  }
});

// The "signed in" cookie holds no secret; with it, a navigation the person
// starts passes the same-origin retry page (auth.rs). A Domain=localhost one
// never reaches the workbench at 127.0.0.1, in any engine. A page on another
// 127.0.0.1 port can set one, host-only, in every engine: then a signed-out
// browser passes the retry page once, which clears it, and is never signed in.
journey('a forged signed-in cookie sends a signed-out browser through the retry page at most once', { timeout: 300_000, skip: imageMissing }, async (profile) => {
  // A browser that never signed in.
  const context = await browser.newContext(profile.contextOptions);
  try {
    const page = await context.newPage();
    const statuses = [];
    page.on('response', (response) => {
      const url = new URL(response.url());
      if (url.host === `127.0.0.1:${listenerPort}` && url.pathname === '/') statuses.push(response.status());
    });
    const navigate = async () => {
      statuses.length = 0;
      const signInPage = page.waitForResponse((response) =>
        new URL(response.url()).pathname === '/' && response.status() === 401);
      await page.goto(`http://127.0.0.1:${listenerPort}/`);
      await signInPage;
      await page.waitForLoadState('load');
      await page.waitForTimeout(1_000);
      assert.equal(await page.title(), 'Sign in to Axocoatl');
      return [...statuses];
    };

    await forgeFromTopLevelPage(context, forgeUrl('/forge-signed-out'));
    // Each navigation the person starts gets the sign-in page at once.
    assert.deepEqual(await navigate(), [401]);
    assert.deepEqual(await navigate(), [401]);

    const forger = await context.newPage();
    try {
      await forger.goto(`http://127.0.0.1:${echo.port}/forge-signed-in`);
      await forger.evaluate((listener) => {
        document.cookie = `axocoatl-token-${listener}-signed-in=1; Path=/`;
        document.cookie = `axocoatl-token-${listener}=forged-on-127; Path=/`;
      }, listenerPort);
    } finally {
      await forger.close();
    }
    // The retry page (200), whose own navigation gets the sign-in page
    // (401); it cleared the forged cookie, so the next navigation gets the
    // sign-in page at once. Never a loop, never signed in.
    assert.deepEqual(await navigate(), [200, 401]);
    assert.deepEqual(await navigate(), [401]);
    // The sign-in page's CSP allows no fetch; ask from a public page.
    await page.goto(`http://127.0.0.1:${listenerPort}/health`);
    assert.equal(await page.evaluate(async () => (await fetch('/api/agents')).status), 401);
  } finally {
    await context.close();
  }
});

journey('the workbench shows the real Preview embedded; in Safari it offers no full page, the Preview cannot open itself in a window, and a real Command-click in the Preview or Files frame gets the refusal page; elsewhere Open full preview and the Preview\'s own window.open open it in a tab', { timeout: 300_000, skip: imageMissing }, async (profile) => {
  const context = await browser.newContext(profile.contextOptions);
  try {
    const earlier = (await appSeen()).length;
    const workbench = await context.newPage();
    const signedIn = await workbench.goto(runtime.signInUrl);
    assert.equal(signedIn?.status(), 200);
    await workbench.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');
    await workbench.goto(`http://127.0.0.1:${listenerPort}/?session=${encodeURIComponent(session.id)}`);
    await workbench.waitForFunction((sessionId) => typeof S !== 'undefined'
      && S.session?.id === sessionId && sessionRuntimeSurfaceReady(S.session), session.id,
    { timeout: 120_000 });
    await workbench.locator('[data-session-view="browser"]').click();
    await workbench.evaluate(() => browserGo('http://localhost:3000/embedded'));
    const iframe = (await workbench.locator('#browser')
      .evaluateHandle((element) => element.frame)).asElement();
    assert.ok(iframe, 'the Preview must render in the workbench\'s frame');
    assert.equal(await iframe.getAttribute('src'), `${previewOrigin(3000)}/embedded`);
    const frame = await iframe.contentFrame();
    await frame.waitForFunction(() => document.title === 'preview 3000/embedded');
    const openFull = workbench.locator('ax-browser [data-act="open-full"]');
    if (safariKeepsPreviewsEmbedded) {
      assert.equal(await iframe.getAttribute('sandbox'),
        'allow-scripts allow-same-origin allow-forms allow-modals');
      assert.equal(await openFull.getAttribute('href'), null);
      assert.equal(await openFull.getAttribute('aria-disabled'), 'true');
      assert.match(await openFull.getAttribute('title'), /^In Safari, a Preview in its own tab could set cookies/);
      assert.equal(await workbench.locator('ax-browser .embedded-note').isVisible(), true);
      // The Preview's own code, on a real click in its frame, cannot open its
      // address in a window: window.open returns null and no page opens.
      await frame.evaluate(() => {
        document.body.insertAdjacentHTML('beforeend', '<button id="open-self" type="button">Open myself</button>');
        document.querySelector('#open-self').addEventListener('click', () => {
          globalThis.__opened = window.open(location.href, '_blank') === null ? 'null' : 'a window';
        });
      });
      const before = new Set(context.pages());
      await openFull.click({ force: true });
      await frame.locator('#open-self').click();
      await new Promise((resolve) => setTimeout(resolve, 1_500));
      assert.deepEqual(context.pages().filter((page) => !before.has(page)).map((page) => page.url()), []);
      assert.equal(await frame.evaluate(() => globalThis.__opened), 'null');
    } else {
      assert.equal(await iframe.getAttribute('sandbox'),
        'allow-scripts allow-same-origin allow-forms allow-popups allow-modals');
      assert.equal(await openFull.getAttribute('href'), `${previewOrigin(3000)}/embedded`);
      assert.equal(await openFull.getAttribute('aria-disabled'), 'false');
      // Open full preview opens the Preview's address in a tab of its own,
      // with no opener, served by the app through the proxy.
      const full = context.waitForEvent('page', { timeout: 15_000 });
      await openFull.click();
      const fullTab = await full;
      await fullTab.waitForLoadState('load');
      assert.equal(fullTab.url(), `${previewOrigin(3000)}/embedded`);
      assert.equal(await fullTab.title(), 'preview 3000/embedded');
      assert.equal(await fullTab.evaluate(() => window.opener), null);
      await fullTab.close();
      // The frame may open windows: the Preview's own window.open of its
      // address, on a real click in its frame, opens a tab.
      await frame.evaluate(() => {
        document.body.insertAdjacentHTML('beforeend', '<button id="open-self" type="button">Open myself</button>');
        document.querySelector('#open-self').addEventListener('click', () => {
          globalThis.__opened = window.open(`${location.href}?opened`, '_blank') === null ? 'null' : 'a window';
        });
      });
      const popup = context.waitForEvent('page', { timeout: 15_000 });
      await frame.locator('#open-self').click();
      const popupTab = await popup;
      await popupTab.waitForLoadState('load');
      assert.equal(popupTab.url(), `${previewOrigin(3000)}/embedded?opened`);
      assert.equal(await popupTab.title(), 'preview 3000/embedded');
      assert.equal(await frame.evaluate(() => globalThis.__opened), 'a window');
      await popupTab.close();
      // The frame's load, Open full preview's tab and the popup.
      const opens = (await appSeen()).slice(earlier)
        .filter((request) => request.port === 3000 && request.path === '/embedded');
      assert.ok(opens.length >= 3, JSON.stringify(opens.map(({ userAgent }) => userAgent)));
      if (profile.linux) {
        for (const request of opens) assert.match(request.userAgent, X11_WEBKIT_USER_AGENT);
      }
    }
    // A real Command-click (Control on Linux) on a link inside the Preview
    // frame opens a new tab in every engine, whatever the frame's sandbox
    // says (Safari's too, without allow-popups). In Safari the daemon answers
    // that tab with its refusal page and never asks the app, so nothing of
    // the app runs top-level and no cookie is set. Elsewhere, WebKit on Linux
    // included (browserOpenedTabsSaySafari), the tab shows the app, whose
    // script runs and sets a host-only cookie (the browser refuses its
    // Domain=localhost one).
    // (Playwright's WebKit reports no request or response for a tab the
    // browser opened this way, so the page itself, and what the app received,
    // are asserted.)
    const cmdClicked = `${previewOrigin(3000)}/top-cmd-clicked`;
    const opened = context.waitForEvent('page', { timeout: 15_000 });
    await frame.locator('#self-link').click({ modifiers: [process.platform === 'darwin' ? 'Meta' : 'Control'] });
    const tab = await opened;
    await tab.waitForLoadState('load');
    assert.equal(tab.url(), cmdClicked);
    const cmdClickAtApp = (await appSeen()).slice(earlier)
      .find((request) => request.port === 3000 && request.path === '/top-cmd-clicked');
    if (browserOpenedTabsSaySafari) {
      await assertRefusalPage(tab);
      assert.equal(cmdClickAtApp, undefined);
      assert.deepEqual(await cookiesFromTopLevelApps(context), []);
    } else {
      assert.equal(await tab.title(), 'preview 3000/top-cmd-clicked');
      assert.ok(cmdClickAtApp, 'the Command-click did not reach the app');
      if (webkitOnLinux) assert.match(cmdClickAtApp.userAgent, X11_WEBKIT_USER_AGENT);
      assert.deepEqual(await cookiesFromTopLevelApps(context), [`ran-top-level-host@${previewHost(3000)}`]);
    }
    await tab.close();

    // The same Command-click in the Files view's own frame (an HTML file's
    // live preview: sandbox="allow-scripts", no popups), which the
    // workbench's link guard does not see. WebKit and Chromium on macOS both
    // open a tab from it (Playwright 1.64.1), and in Safari the daemon
    // answers that tab with its refusal page. Whatever opens is that address.
    // (On Linux, WebKit's tab says WebKit on Linux, as above.)
    const filesClicked = `${previewOrigin(3000)}/top-files-clicked`;
    await workbench.evaluate(() => openFile('links.html'));
    const filesFrame = workbench.frameLocator('iframe.file-preview');
    await filesFrame.locator('#files-link').waitFor();
    const pagesBeforeFiles = new Set(context.pages());
    await filesFrame.locator('#files-link').click({ modifiers: [process.platform === 'darwin' ? 'Meta' : 'Control'] });
    await new Promise((resolve) => setTimeout(resolve, 1_500));
    const filesTabs = context.pages().filter((candidate) => !pagesBeforeFiles.has(candidate));
    assert.ok(filesTabs.length <= 1, filesTabs.map((filesTab) => filesTab.url()).join(', '));
    for (const filesTab of filesTabs) {
      await filesTab.waitForLoadState('load');
      assert.equal(filesTab.url(), filesClicked);
    }
    if (browserOpenedTabsSaySafari) {
      assert.equal(filesTabs.length, 1, 'WebKit opens a tab for a Command-click in a sandboxed frame');
      await assertRefusalPage(filesTabs[0]);
      assert.equal((await appSeen()).some((request) => request.path === '/top-files-clicked'), false);
      assert.deepEqual(await cookiesFromTopLevelApps(context), []);
    }
    for (const filesTab of filesTabs) await filesTab.close();

    // The app served the embedded Preview, and no request it received carried
    // the workbench's token cookie; the workbench is still signed in.
    const seen = await appSeen();
    assert.ok(seen.some((request) => request.port === 3000 && request.path === '/embedded'),
      JSON.stringify(seen.map(({ port, path: at }) => `${port}${at}`)));
    for (const request of seen) {
      assert.equal(request.cookie.includes(runtime.token), false, `${request.port}${request.path}`);
      assert.equal(/axocoatl-token-/i.test(request.cookie), false,
        `${request.port}${request.path}: ${cookieNames(request.cookie)}`);
    }
    assert.equal(await workbench.evaluate(async () => (await fetch('/api/agents')).status), 200);
    assert.notEqual(await workbench.title(), 'Sign in to Axocoatl');
  } finally {
    await context.close();
  }
});

// Podman publishes a Session's ports on a loopback address other than the
// workbench's 127.0.0.1 (axocoatl-isolation's session_sandbox.rs): [::1] on
// macOS, 127.0.0.2 on Linux. Cookies ignore ports, so on 127.0.0.1 a Session's
// app would receive the workbench's cookies and could set cookies the
// workbench receives, with no click: its Preview's code could navigate its
// own frame to that address, which Chromium and WebKit then sent the
// workbench's SameSite=Strict token cookie, or nest a frame there, which
// WebKit sent it whatever frames lie between (measured in Playwright's
// Chromium and WebKit on macOS while Session ports were published on
// 127.0.0.1; the WebKit run of this journey then fails). Here neither frame
// gets any of the workbench's cookies, in every engine.
test('a Session\'s app is never published at the workbench\'s address, and a Preview that loads the app\'s published port in a frame gets none of the workbench\'s cookies', { timeout: 300_000, skip: imageMissing }, async () => {
  const published = await new Promise((resolve, reject) => {
    execFile('podman', ['port', `axo-ses-${session.id}`], { timeout: 60_000 },
      (error, stdout) => (error ? reject(error) : resolve(stdout.trim())));
  });
  const expected = process.platform === 'darwin' ? '::1' : '127.0.0.2';
  const mappings = Object.fromEntries(published.split('\n').map((line) => {
    const [container, host] = line.split(' -> ');
    return [container, { address: host.slice(0, host.lastIndexOf(':')), port: Number(host.slice(host.lastIndexOf(':') + 1)) }];
  }));
  assert.deepEqual(Object.keys(mappings).sort(), ['3000/tcp', '5000/tcp'], published);
  for (const mapping of Object.values(mappings)) assert.equal(mapping.address, expected, published);
  const appPort = mappings['3000/tcp'].port;
  // Nothing of the Session answers at the workbench's address.
  await assert.rejects(fetch(`http://127.0.0.1:${appPort}/__seen`, { signal: AbortSignal.timeout(5_000) }));
  const appAddress = expected.includes(':') ? `[${expected}]:${appPort}` : `${expected}:${appPort}`;

  const context = await browser.newContext();
  try {
    const workbench = await context.newPage();
    assert.equal((await workbench.goto(runtime.signInUrl))?.status(), 200);
    await workbench.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');
    // What this needs is the Session's readiness below, not every
    // subresource of the page (its load event took over 30 seconds once on
    // a loaded Mac).
    await workbench.goto(`http://127.0.0.1:${listenerPort}/?session=${encodeURIComponent(session.id)}`,
      { waitUntil: 'domcontentloaded' });
    await workbench.waitForFunction((sessionId) => typeof S !== 'undefined'
      && S.session?.id === sessionId && sessionRuntimeSurfaceReady(S.session), session.id,
    { timeout: 120_000 });
    await workbench.locator('[data-session-view="browser"]').click();
    // A frame inside the Preview, at the app's published address.
    await workbench.evaluate((src) => browserGo(`http://localhost:3000/embed?src=${encodeURIComponent(src)}`),
      `http://${appAddress}/at-published-nested`);
    const iframe = (await workbench.locator('#browser')
      .evaluateHandle((element) => element.frame)).asElement();
    const preview = await iframe.contentFrame();
    await preview.waitForFunction(() => document.title === 'embed');
    const nested = await (await preview.waitForSelector('#nested')).contentFrame();
    await nested.waitForFunction(() => document.title === 'fetched', null, { timeout: 30_000 });
    assert.equal(nested.url(), `http://${appAddress}/at-published-nested`);
    // The Preview's own frame, navigated there by its code.
    await preview.evaluate((target) => { location.href = target; }, `http://${appAddress}/at-published-self`);
    await iframe.contentFrame().then((frame) => frame.waitForFunction(() => document.title === 'fetched'
      && location.pathname === '/at-published-self', null, { timeout: 30_000 }));

    const paths = ['/at-published-nested', '/at-published-nested-fetch',
      '/at-published-self', '/at-published-self-fetch'];
    const atApp = (await appSeen()).filter((request) => paths.includes(request.path));
    assert.deepEqual(atApp.map((request) => request.path).sort(), [...paths].sort());
    for (const request of atApp) {
      assert.equal(request.cookie.includes(runtime.token), false, request.path);
      assert.equal(/axocoatl-token-/i.test(request.cookie), false, `${request.path}: ${cookieNames(request.cookie)}`);
    }
    // The cookies its script set stayed off the workbench's address.
    assert.deepEqual((await context.cookies(`http://127.0.0.1:${listenerPort}/api/agents`))
      .map((cookie) => cookie.name).sort(), [`axocoatl-token-${listenerPort}`, `axocoatl-token-${listenerPort}-signed-in`]);
    assert.equal(await workbench.evaluate(async () => (await fetch('/api/agents')).status), 200);
  } finally {
    await context.close();
  }
});

// Every browser on iPhone and iPad uses Safari's engine, so the daemon takes
// their user agents for Safari (src/preview_document.rs), and so does the
// workbench where the engine is WebKit (static/ui/preview-embedding.js).
// Playwright's iPhone and iPad descriptors, through the real Preview proxy:
// a typed Preview address gets the refusal page and the app is not asked,
// while the same address in a frame is served; in WebKit the workbench's own
// rule keeps Previews embedded. (iPadOS also sends a Mac user agent, which
// the other journeys cover.)
test('with iPhone and iPad user agents a typed Preview gets the refusal page, a framed one is served, and WebKit keeps Previews embedded', { timeout: 300_000, skip: imageMissing }, async () => {
  for (const name of ['iPhone 15', 'iPad Pro 11']) {
    const { userAgent, viewport } = devices[name];
    const context = await browser.newContext({ userAgent, viewport });
    try {
      const typed = await context.newPage();
      const typedPath = `/top-typed-${name.split(' ')[0].toLowerCase()}`;
      const response = await typed.goto(`${previewOrigin(5000)}${typedPath}`);
      await assertRefusalPage(typed, response);
      assert.equal((await appSeen()).some((request) => request.path === typedPath), false, name);

      const page = await context.newPage();
      assert.equal((await page.goto(`http://127.0.0.1:${listenerPort}/health`))?.status(), 200);
      if (safariKeepsPreviewsEmbedded) {
        assert.equal(await page.evaluate(async () => (await import('/ui/preview-embedding.js'))
          .KEEPS_PREVIEWS_EMBEDDED), true, name);
      }
      const framedPath = `/framed-${name.split(' ')[0].toLowerCase()}`;
      await page.evaluate((src) => {
        const frame = document.createElement('iframe');
        frame.src = src;
        document.body.append(frame);
      }, `${previewOrigin(5000)}${framedPath}`);
      const frame = await (await page.waitForSelector('iframe')).contentFrame();
      await frame.waitForFunction((title) => document.title === title, `preview 5000${framedPath}`);
      const framed = (await appSeen()).find((request) => request.path === framedPath);
      assert.ok(framed, `${name}: the framed Preview did not reach the app`);
      assert.equal(framed.userAgent, userAgent);
    } finally {
      await context.close();
    }
  }
});

journey('in Safari no Preview page runs top-level, typed, redirected or through the legacy path, which refuses every browser page in every engine', { timeout: 300_000, skip: imageMissing }, async (profile) => {
  const context = await browser.newContext(profile.contextOptions);
  // A page Axocoatl does not serve that redirects (302) to the address its
  // query names, as any other site could.
  const redirector = createServer((request, response) => {
    response.writeHead(302, { location: new URL(request.url, 'http://redirector').searchParams.get('to') });
    response.end();
  });
  await new Promise((resolve, reject) => {
    redirector.once('error', reject);
    redirector.listen(0, '127.0.0.1', resolve);
  });
  let earlier = 0;
  const reached = async (port, pathname) => (await appSeen()).slice(earlier)
    .some((request) => request.port === port && request.path === pathname);
  try {
    earlier = (await appSeen()).length;
    const workbench = await context.newPage();
    const signedIn = await workbench.goto(runtime.signInUrl);
    assert.equal(signedIn?.status(), 200);
    await workbench.waitForFunction(() => typeof S !== 'undefined' && S.liveConnection === 'connected');

    // A Preview address typed or pasted into a tab.
    const typed = await context.newPage();
    const typedResponse = await typed.goto(`${previewOrigin(5000)}/top-typed`);
    if (safariRefusesTopLevelPreviews) {
      await assertRefusalPage(typed, typedResponse);
      assert.equal(await reached(5000, '/top-typed'), false);
      // Its one link opens the workbench's own origin.
      await Promise.all([typed.waitForURL(`http://127.0.0.1:${listenerPort}/**`), typed.locator('a').click()]);
    } else {
      assert.equal(typedResponse?.status(), 200);
      assert.equal(await typed.title(), 'preview 5000/top-typed');
      assert.equal(await reached(5000, '/top-typed'), true);
    }
    await typed.close();

    // A redirect from another site to a Preview address.
    const redirected = await context.newPage();
    const target = `${previewOrigin(3000)}/top-redirected`;
    const redirectedResponse = await redirected.goto(
      `http://127.0.0.1:${redirector.address().port}/?to=${encodeURIComponent(target)}`);
    assert.equal(redirected.url(), target);
    if (safariRefusesTopLevelPreviews) {
      await assertRefusalPage(redirected, redirectedResponse);
      assert.equal(await reached(3000, '/top-redirected'), false);
    } else {
      assert.equal(redirectedResponse?.status(), 200);
      assert.equal(await redirected.title(), 'preview 3000/top-redirected');
      assert.equal(await reached(3000, '/top-redirected'), true);
    }
    await redirected.close();

    // The legacy path, on the workbench's own origin, signed in: a link to
    // it (the workbench does not refuse one, it is not a *.localhost
    // address) whose app would answer with a redirect to the Preview and a
    // cookie for the workbench's host. Every engine gets the refusal text,
    // and the app is never asked.
    const legacy = `http://127.0.0.1:${listenerPort}/api/sessions/${session.id}/proxy/3000`;
    const legacyTarget = `${previewOrigin(3000)}/top-legacy-landed`;
    const legacyPage = await context.newPage();
    const legacyResponse = await legacyPage.goto(`${legacy}/redirect-to?to=${encodeURIComponent(legacyTarget)}`);
    assert.equal(legacyResponse?.status(), 403);
    assert.equal(legacyResponse.headers()['content-type'], 'text/plain; charset=utf-8');
    assert.equal(legacyResponse.headers()['content-security-policy'], "sandbox; default-src 'none'");
    assert.match(await legacyPage.locator('body').textContent(),
      /^The legacy Preview path serves a Session's app only to data requests such as fetch\(\) and XMLHttpRequest/);
    assert.equal(legacyPage.url(), `${legacy}/redirect-to?to=${encodeURIComponent(legacyTarget)}`);
    await legacyPage.close();
    assert.equal(await reached(3000, '/redirect-to'), false);
    assert.equal(await reached(3000, '/top-legacy-landed'), false);
    // The workbench's own page may still fetch() it, and the app answers;
    // its Set-Cookie for the workbench's host is not passed on.
    assert.deepEqual(await workbench.evaluate(async (url) => {
      const response = await fetch(url, { redirect: 'manual' });
      return { type: response.type, status: response.status };
    }, `${legacy}/redirect-to?to=${encodeURIComponent(legacyTarget)}`), { type: 'opaqueredirect', status: 0 });
    assert.equal(await reached(3000, '/redirect-to'), true);
    // A program that calls the API gets the app's redirect as it is, and no
    // cookie (Node's fetch sends no Sec-Fetch-Dest).
    const program = await fetch(`http://127.0.0.1:${listenerPort}/api/sessions/${session.id}/proxy/3000/redirect-to?to=${encodeURIComponent(legacyTarget)}`, {
      headers: { authorization: `Bearer ${runtime.token}` }, redirect: 'manual',
    });
    assert.equal(program.status, 302);
    assert.equal(program.headers.get('location'), legacyTarget);
    assert.equal(program.headers.get('set-cookie'), null);
    assert.match(program.headers.get('content-security-policy'), /^sandbox /);

    // No page of the app ran top-level in Safari, so it set no cookie, and
    // nothing set one for Domain=localhost in any engine. Elsewhere the
    // typed and redirected Previews ran and set their host-only cookies.
    const fromApps = await cookiesFromTopLevelApps(context);
    assert.deepEqual(fromApps, safariRefusesTopLevelPreviews ? [] : [
      `ran-top-level-host@${previewHost(3000)}`,
      `ran-top-level-host@${previewHost(5000)}`,
    ]);
    for (const host of ['127.0.0.1', 'localhost']) {
      assert.equal((await context.cookies(`http://${host}:${listenerPort}/`))
        .some((cookie) => cookie.name === 'app-redirect'), false, host);
    }
    assert.equal(await workbench.evaluate(async () => (await fetch('/api/agents')).status), 200);
  } finally {
    await new Promise((resolve) => redirector.close(resolve));
    await context.close();
  }
});
