// The engine switch (support/browser-engine.mjs): with AXO_BROWSER_ENGINE
// the tests' `chromium` is another engine, its Chromium-only launch options
// are dropped, and without it Playwright is untouched. No browser runs here.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { test } from 'node:test';
import { pathToFileURL } from 'node:url';

import { TEST_ROOT } from '../support/daemon.mjs';

const HOOK = pathToFileURL(path.join(TEST_ROOT, 'support', 'browser-engine.mjs')).href;

function probe(engine, source) {
  const env = { ...process.env };
  delete env.AXO_BROWSER_ENGINE;
  if (engine) env.AXO_BROWSER_ENGINE = engine;
  return spawnSync(process.execPath, ['--import', HOOK, '--input-type=module', '-e', source], {
    cwd: TEST_ROOT, env, encoding: 'utf8',
  });
}

test('AXO_BROWSER_ENGINE=webkit makes the tests\' chromium WebKit; chromium leaves it alone', () => {
  const name = "import { chromium, webkit } from 'playwright'; console.log(chromium.name(), typeof webkit.launch);";
  const webkit = probe('webkit', name);
  assert.equal(webkit.status, 0, webkit.stderr);
  assert.equal(webkit.stdout.trim(), 'webkit function');
  const chromium = probe('chromium', name);
  assert.equal(chromium.status, 0, chromium.stderr);
  assert.equal(chromium.stdout.trim(), 'chromium function');
  const unknown = probe('firefox', name);
  assert.notEqual(unknown.status, 0);
  assert.match(unknown.stderr, /AXO_BROWSER_ENGINE=firefox is not one of chromium, webkit/);
});

test('another engine launches without Chrome\'s executable path or Chromium flags', async () => {
  const { engineLaunchOptions } = await import(pathToFileURL(path.join(TEST_ROOT, 'support', 'playwright-engine.mjs')).href);
  assert.deepEqual(
    engineLaunchOptions({ headless: true, executablePath: '/Applications/Google Chrome.app', args: ['--x'], channel: 'chrome', chromiumSandbox: false, timeout: 5 }),
    { headless: true, timeout: 5 },
  );
  assert.equal(engineLaunchOptions(undefined), undefined);
  // The proxied launch passes the stripped options to WebKit's own launch.
  const launched = probe('webkit', `
    import { chromium, webkit } from 'playwright';
    const real = Object.getPrototypeOf(webkit);
    const seen = [];
    real.launch = async function (options) { seen.push([this.name(), options]); return 'browser'; };
    console.log(await chromium.launch({ headless: true, executablePath: '/chrome' }), JSON.stringify(seen));`);
  assert.equal(launched.status, 0, launched.stderr);
  assert.equal(launched.stdout.trim(), 'browser [["webkit",{"headless":true}]]');
});
