// In WebKit on Linux a page settles before it reloads or leaves a daemon's
// page (support/settle-navigation.mjs). A fake page and clock show when a
// navigation waits, how long, and that it always goes on; the last test only
// checks, in each installed engine, which real pages the watcher gives it.
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { test } from 'node:test';

import { browserEngines } from '../support/daemon.mjs';
import { BOUND_MS, QUIET_MS, settleNavigations, settlesNavigations } from '../support/settle-navigation.mjs';

function fakePage(url) {
  const page = new EventEmitter();
  page.current = url;
  page.navigations = [];
  page.url = () => page.current;
  for (const method of ['reload', 'goto', 'goBack', 'goForward']) {
    page[method] = async (...args) => { page.navigations.push([method, clock.now(), ...args]); };
  }
  return page;
}

const clock = {
  time: 0,
  now: () => clock.time,
  // Each wait advances the clock; `during` runs page events at given times.
  during: [],
  async sleep(milliseconds) {
    clock.time += milliseconds;
    while (clock.during.length && clock.during[0].at <= clock.time) clock.during.shift().run();
  },
};

function setup(url = 'http://localhost:4242/') {
  clock.time = 0;
  clock.during = [];
  const page = fakePage(url);
  settleNavigations(page, {
    isDaemonUrl: (candidate) => new URL(candidate).host === 'localhost:4242',
    now: clock.now,
    sleep: clock.sleep,
  });
  return page;
}

test('only WebKit on Linux settles before a navigation', () => {
  assert.equal(settlesNavigations('webkit', 'linux'), true);
  assert.equal(settlesNavigations('webkit', 'darwin'), false);
  assert.equal(settlesNavigations('chromium', 'linux'), false);
  assert.equal(settlesNavigations('chromium', 'darwin'), false);
});

test('a reload waits until no request has been in flight for half a second', async () => {
  const page = setup();
  const request = {};
  page.emit('request', request);
  clock.during.push({ at: 300, run: () => page.emit('requestfinished', request) });
  await page.reload({ waitUntil: 'load' });
  assert.deepEqual(page.navigations, [['reload', 300 + QUIET_MS, { waitUntil: 'load' }]]);
});

test('a failed request ends too, and every navigation method waits', async () => {
  const page = setup();
  for (const method of ['goto', 'goBack', 'goForward']) {
    const request = {};
    const started = clock.time;
    page.emit('request', request);
    clock.during.push({ at: started + 100, run: () => page.emit('requestfailed', request) });
    await page[method]('http://localhost:4242/next');
    assert.equal(page.navigations.at(-1)[0], method);
    assert.equal(page.navigations.at(-1)[1], started + 100 + QUIET_MS);
  }
});

test('a request held past the bound does not stop the navigation', async () => {
  const page = setup();
  page.emit('request', {});
  await page.reload();
  assert.deepEqual(page.navigations, [['reload', BOUND_MS]]);
});

test('a page that is not a daemon\'s navigates at once', async () => {
  const page = setup('http://forge.localhost:9/');
  page.emit('request', {});
  await page.goto('http://localhost:4242/');
  assert.deepEqual(page.navigations, [['goto', 0, 'http://localhost:4242/']]);
});

test('every page of WebKit on Linux settles, and no other', async () => {
  for (const engine of await browserEngines()) {
    const browser = await engine.launch();
    try {
      // Under AXO_BROWSER_ENGINE=webkit the `chromium` engine is WebKit too.
      const name = browser.browserType().name();
      const page = await browser.newPage();
      assert.equal(Object.hasOwn(page, 'reload'), settlesNavigations(name), `${engine.name} (${name})`);
      assert.equal(Object.hasOwn(page, 'goto'), settlesNavigations(name), `${engine.name} (${name})`);
    } finally {
      await browser.close();
    }
  }
});
