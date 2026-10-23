// Let a page settle before it reloads or leaves a fixture daemon's page, in
// WebKit on Linux (README, "WebKit (Safari)").
//
// There WebKit loads everything through one network process on libsoup, and
// that process has crashed when a reload or a navigation cancelled requests
// a workbench that had just loaded was still making; the crash takes every
// context's cookies, so the journey goes on signed out. A person reloads or
// leaves a page that has loaded. So under `node --test` every WebKit page on
// Linux (support/browser-crash.mjs watches them all) waits, before
// `reload`, `goto`, `goBack` or `goForward` from a daemon's page, until it has
// had no request in flight for half a second. A request still in flight
// after five seconds, as when a journey holds one on purpose, is cancelled by
// the navigation as before: this only waits, it never fails a journey.
// `waitForNoRequestsInFlight` (support/daemon.mjs) is the strict form a
// journey calls itself, which fails after 30 seconds.

export const QUIET_MS = 500;
export const BOUND_MS = 5_000;

const NAVIGATIONS = ['reload', 'goto', 'goBack', 'goForward'];

/** Whether pages of `engine` on `platform` settle before they navigate. */
export function settlesNavigations(engine, platform = process.platform) {
  return engine === 'webkit' && platform === 'linux';
}

/**
 * Make `page` wait, before each navigation from a page `isDaemonUrl` accepts,
 * until no request of its own has been in flight for `quietMs`, or `boundMs`
 * has passed. Returns the page's requests in flight and when that last
 * changed, for tests.
 */
export function settleNavigations(page, {
  isDaemonUrl,
  quietMs = QUIET_MS,
  boundMs = BOUND_MS,
  now = Date.now,
  sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds)),
}) {
  const requests = { inFlight: new Set(), changedAt: now() };
  const ended = (request) => {
    requests.inFlight.delete(request);
    requests.changedAt = now();
  };
  page.on('request', (request) => {
    requests.inFlight.add(request);
    requests.changedAt = now();
  });
  page.on('requestfinished', ended);
  page.on('requestfailed', ended);
  const settle = async () => {
    if (!isDaemonUrl(page.url())) return;
    const deadline = now() + boundMs;
    while ((requests.inFlight.size > 0 || now() - requests.changedAt < quietMs) && now() < deadline) {
      await sleep(25);
    }
  };
  for (const method of NAVIGATIONS) {
    const navigate = page[method].bind(page);
    page[method] = async (...args) => {
      await settle();
      return navigate(...args);
    };
  }
  return requests;
}
