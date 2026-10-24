// Tell a browser crash from a product failure (README, "Browser crashes").
//
// Under `node --test`, every browser a journey launches (through any
// engine's `launch`) is watched, and each of these is printed under the test
// it happened in as a `# browser crash: <what>` line:
//
// - a page whose web process crashed (Playwright's page `crash` event);
// - a browser that disconnected although the journey did not close it;
// - a context or page that the browser has not created after 30 s (Playwright's
//   WebKit never finishes a page whose web process ends meanwhile);
// - a context that lost the sign-in cookie it was given (`authorizeContext`)
//   or signed in with (`/?token=`), although the journey did not clear it.
//   Axocoatl never removes that cookie; a browser drops every context's
//   cookies when its network process crashes, and goes on without saying so;
// - with one of those in the same test, or a browser of this file that
//   crashed in an earlier test, a failure with "Target page, context or
//   browser has been closed", "Target crashed" or "WebKit encountered an
//   internal error", the errors such a crash gives.
//
// The cookie checks a test starts (on a daemon's 401, on closing a context,
// and when the test ends) finish before the test's lines are printed, so a
// line lands under the test it belongs to. run-tests.mjs reruns a file once
// when each of its failures has such a line, and never otherwise.

import { CRASH_ERRORS, CRASH_LINE } from './crash-rerun.mjs';
import { settleNavigations, settlesNavigations } from './settle-navigation.mjs';

const CHECK_TIMEOUT_MS = 10_000;
// How long a browser may take to create a context or a page, which runs no
// product code. Playwright's WebKit never finishes a page whose web process
// ends while it is being created, and the journey would wait forever.
const CREATE_TIMEOUT_MS = 30_000;

// What was seen since the last test ended, the checks still running, and the
// contexts holding a sign-in.
const crashes = [];
const pending = new Set();
const signIns = new Set();
const watchedContexts = new WeakMap();
const watchedPages = new WeakSet();
let isDaemonUrl = () => false;
// The browser that had crashed when the running test asked it for a context.
let crashedBrowserUsed = null;

/** support/daemon.mjs says which URLs are a fixture daemon's. */
export function configureCrashWatch({ isDaemonUrl: lookup }) {
  isDaemonUrl = lookup;
}

function record(what) {
  crashes.push({ at: new Date(), what });
}

function track(promise) {
  const settled = promise.then(() => {}, () => {});
  pending.add(settled);
  settled.finally(() => pending.delete(settled));
  return settled;
}

function withTimeout(promise, milliseconds) {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('timed out')), milliseconds);
      timer.unref?.();
    }),
  ]).finally(() => clearTimeout(timer));
}

/**
 * `create` (a browser's newContext, a context's newPage), failing with a
 * browser crash when it has not returned after `timeoutMs`. What it made
 * later is closed.
 */
export function guardCreation(what, engine, create, { timeoutMs = CREATE_TIMEOUT_MS, onStall = record } = {}) {
  return (...args) => {
    const created = create(...args);
    let timer;
    const stalled = new Promise((_, reject) => {
      timer = setTimeout(() => {
        const seconds = timeoutMs / 1000;
        onStall(`${what} had not returned after ${seconds} s: the ${engine} browser never finished `
          + 'creating it, as when its web process ends while a page is being created');
        created.then((made) => made?.close?.().catch(() => {}), () => {});
        reject(new Error(`${what}: the ${engine} browser did not create it in ${seconds} s`));
      }, timeoutMs);
    });
    return Promise.race([created, stalled]).finally(() => clearTimeout(timer));
  };
}

// A URL without its query, which can hold the sign-in token.
function shown(url) {
  try {
    const { origin, pathname } = new URL(url);
    return origin === 'null' ? url.split(/[?#]/)[0] : `${origin}${pathname}`;
  } catch {
    return String(url).split(/[?#]/)[0];
  }
}

function firstLine(error) {
  const messages = [];
  for (let current = error, depth = 0; current && depth < 4; current = current.cause, depth += 1) {
    if (current.message) messages.push(String(current.message));
  }
  const crashLike = messages.find((message) => CRASH_ERRORS.test(message));
  return (crashLike || messages[0] || String(error)).split('\n')[0].slice(0, 300);
}

function watchBrowser(browser, engine) {
  const state = { engine, closing: false, crashedAt: null };
  const close = browser.close.bind(browser);
  browser.close = (...args) => {
    state.closing = true;
    return close(...args);
  };
  // browser.newPage() opens its context and page through these too.
  const newContext = guardCreation('browser.newContext()', engine, browser.newContext.bind(browser));
  browser.newContext = (...args) => {
    if (state.crashedAt) crashedBrowserUsed = state;
    return newContext(...args);
  };
  browser.on('disconnected', () => {
    if (state.closing) return;
    state.crashedAt = new Date();
    record(`the ${engine} browser disconnected although the journey did not close it: its process ended`);
  });
  browser.on('context', (context) => watchContext(context, state));
  for (const context of browser.contexts()) watchContext(context, state);
}

// A page that crashes while the journey closes it, its context or its
// browser is not reported: nothing can fail from it. In WebKit on Linux a
// page also settles before it reloads or leaves a daemon's page, which keeps
// its network process from crashing (support/settle-navigation.mjs).
function watchPage(page, browserState, contextWatch) {
  if (watchedPages.has(page)) return;
  watchedPages.add(page);
  if (settlesNavigations(browserState.engine)) settleNavigations(page, { isDaemonUrl: (url) => isDaemonUrl(url) });
  let closing = false;
  const close = page.close.bind(page);
  page.close = (...args) => {
    closing = true;
    return close(...args);
  };
  page.on('crash', () => {
    if (closing || contextWatch.closing || browserState.closing) return;
    record(`the web process of a ${browserState.engine} page crashed (page 'crash' event) at ${shown(page.url())}`);
  });
}

function watchContext(context, browserState) {
  let watch = watchedContexts.get(context);
  if (watch) return watch;
  watch = {
    context,
    engine: browserState.engine,
    expected: new Map(),
    signingIn: new Set(),
    cleared: false,
    reported: false,
    closing: false,
  };
  watchedContexts.set(context, watch);
  context.newPage = guardCreation('context.newPage()', browserState.engine, context.newPage.bind(context));
  context.on('page', (page) => watchPage(page, browserState, watch));
  for (const page of context.pages()) watchPage(page, browserState, watch);
  const clearCookies = context.clearCookies.bind(context);
  context.clearCookies = async (...args) => {
    watch.cleared = true;
    return clearCookies(...args);
  };
  const close = context.close.bind(context);
  context.close = async (...args) => {
    watch.closing = true;
    await track(check(watch, 'when the journey closed it'));
    signIns.delete(watch);
    return close(...args);
  };
  context.on('close', () => signIns.delete(watch));
  context.on('response', (response) => {
    const url = response.url();
    if (!isDaemonUrl(url)) return;
    const { host, hostname, pathname, searchParams } = new URL(url);
    if (response.status() === 303 && searchParams.has('token')) {
      // The sign-in link stores the cookie with this redirect; the response
      // to the request it redirects to comes after the cookie is stored.
      watch.signingIn.add(host);
      return;
    }
    if (watch.signingIn.delete(host)) {
      track(adoptSignIn(watch, url, hostname));
      return;
    }
    if (response.status() === 401) {
      track(check(watch, `${response.request().method()} ${pathname} answered 401`));
    }
  });
  return watch;
}

function addExpected(watch, name, host) {
  watch.expected.set(`${name}@${host}`, { name, host });
  signIns.add(watch);
}

// The harness gave `context` these sign-in cookies (support/daemon.mjs).
export function expectSignIn(context, tokenCookies) {
  const watch = watchedContexts.get(context)
    || watchContext(context, { engine: context.browser()?.browserType().name() || 'browser' });
  for (const { name, url } of tokenCookies) addExpected(watch, name, new URL(url).hostname);
}

// A context that opened the sign-in link holds the token cookie from then on.
async function adoptSignIn(watch, url, hostname) {
  let held;
  try {
    held = await withTimeout(watch.context.cookies(url), CHECK_TIMEOUT_MS);
  } catch {
    return;
  }
  const token = held.find((cookie) => /^axocoatl-token-\d+$/.test(cookie.name)
    && cookie.domain.replace(/^\./, '') === hostname);
  if (!token) return;
  watch.cleared = false;
  watch.reported = false;
  addExpected(watch, token.name, hostname);
}

async function check(watch, when) {
  if (watch.cleared || watch.reported || watch.expected.size === 0) return;
  let held;
  try {
    held = await withTimeout(watch.context.cookies(), CHECK_TIMEOUT_MS);
  } catch {
    return; // The context or its browser has closed; that is reported on its own.
  }
  const missing = [...watch.expected.values()].filter(({ name, host }) => !held.some((cookie) =>
    cookie.name === name && cookie.domain.replace(/^\./, '') === host));
  if (watch.cleared || watch.reported || missing.length === 0) return;
  watch.reported = true;
  record(`${when}, a ${watch.engine} context no longer held the sign-in cookie `
    + `${missing.map(({ name, host }) => `${name} for ${host}`).join(' and ')}, `
    + 'which the journey did not clear and Axocoatl never removes, as when the browser\'s '
    + 'network process crashes and takes every context\'s cookies with it');
}

/** Wait (bounded) for every check started so far. */
async function settleChecks() {
  const deadline = Date.now() + 2 * CHECK_TIMEOUT_MS;
  while (pending.size > 0 && Date.now() < deadline) {
    await withTimeout(Promise.all([...pending]), deadline - Date.now()).catch(() => {});
  }
}

if (process.env.NODE_TEST_CONTEXT) {
  // Watch every browser any engine launches: the tests' `chromium` is the
  // engine AXO_BROWSER_ENGINE names (support/playwright-engine.mjs), and
  // browserEngines() launches `webkit` too. All share one prototype.
  const playwright = await import('playwright');
  const browserType = Object.getPrototypeOf(playwright.webkit);
  const launch = browserType.launch;
  browserType.launch = async function launchWatched(options) {
    const browser = await launch.call(this, options);
    watchBrowser(browser, this.name());
    return browser;
  };

  const { afterEach, beforeEach } = await import('node:test');
  beforeEach(() => {
    crashedBrowserUsed = null;
  });
  afterEach(async (t) => {
    for (const watch of signIns) {
      if (!watch.closing) track(check(watch, 'when the test ended'));
    }
    await settleChecks();
    const found = crashes.splice(0);
    for (const { at, what } of found) t.diagnostic(`${CRASH_LINE} ${at.toISOString()} ${what}`);
    if (t.passed || !t.error) return;
    const error = firstLine(t.error);
    if (!CRASH_ERRORS.test(error)) return;
    if (found.length > 0) {
      t.diagnostic(`${CRASH_LINE} the test failed with "${error}", which that crash gives`);
      return;
    }
    if (crashedBrowserUsed) {
      const { engine, crashedAt } = crashedBrowserUsed;
      t.diagnostic(`${CRASH_LINE} the test failed with "${error}" on the ${engine} browser that `
        + `disconnected at ${crashedAt.toISOString()}, earlier in this file`);
    }
  });
}
