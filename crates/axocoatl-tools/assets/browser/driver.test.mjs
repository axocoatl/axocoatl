// Unit tests for the browser driver's pure helpers.
// Run: node --test crates/axocoatl-tools/assets/browser/driver.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  boundOutput, boundText, checkInput, checkStep, checkTarget, checkUrl, egressDenial, failureKind,
  locatorCode, quote, redact, run, stepCode, MAX_STEPS,
} from './driver.mjs';

const input = (fields = {}) => ({ schema: 'axocoatl.browser-input/1', url: 'http://localhost:8765/', ...fields });

test('only http and https URLs without credentials are opened', () => {
  for (const url of ['javascript:alert(1)', 'data:text/html,x', 'file:///etc/passwd', 'chrome://version',
    'about:blank', 'ftp://example.com/', 'localhost:3000']) {
    assert.match(checkUrl(url) ?? '', /http or https|absolute URL/, url);
  }
  assert.match(checkUrl('http://user:pw@localhost/'), /user name or password/);
  assert.match(checkUrl(`http://localhost/${'a'.repeat(4096)}`), /4096 bytes/);
  assert.equal(checkUrl('http://localhost:5173/a?b#c'), null);
  assert.equal(checkUrl('https://example.com'), null);
  assert.match(checkInput(input({ url: 'javascript:void(0)' })), /http or https/);
  assert.match(checkInput(input({ steps: [{ action: 'goto', url: 'data:text/html,x' }] })), /steps\[0\]\.url/);
});

test('step validation matches the tool schema', () => {
  assert.equal(checkStep({ action: 'click', target: { role: 'button', name: 'Add' } }, 0), null);
  assert.equal(checkStep({ action: 'press', key: 'Enter' }, 0), null);
  assert.equal(checkStep({ action: 'wait_for', url: 'http://localhost/x' }, 0), null);
  assert.match(checkStep({ action: 'evaluate', text: '1+1' }, 2), /^steps\[2\]\.action must be one of/);
  assert.match(checkStep({ action: 'click' }, 0), /click needs target/);
  assert.match(checkStep({ action: 'fill', target: { label: 'x' } }, 0), /fill needs value/);
  assert.match(checkStep({ action: 'reload', text: 'x' }, 0), /does not take text/);
  assert.match(checkStep({ action: 'wait_for', text: 'a', target: { text: 'b' } }, 0), /exactly one of target, url or text/);
  assert.match(checkStep({ action: 'click', target: { text: 'x' }, timeout_ms: 20000 }, 0), /100-10000/);
  assert.match(checkStep({ action: 'click', target: { text: 'x' }, code: 'x' }, 0), /not a step field/);
  assert.match(checkTarget({ role: 'button', css: 'b' }), /exactly one of/);
  assert.match(checkTarget({ label: 'x', name: 'y' }), /only for role/);
  assert.match(checkTarget({ text: 'x', nth: 1.5 }), /whole number/);
  assert.match(checkTarget({ text: 'x'.repeat(4097) }), /4096 bytes/);
  const steps = Array.from({ length: MAX_STEPS + 1 }, () => ({ action: 'reload' }));
  assert.match(checkInput(input({ steps })), /at most 40 steps/);
  assert.match(checkInput(input({ snapshot: 'html' })), /snapshot/);
  assert.equal(checkInput(input({ steps: [{ action: 'back' }] })), null);
});

test('each target kind renders its Playwright locator', () => {
  assert.equal(locatorCode({ role: 'button', name: 'Place order' }), "page.getByRole('button', { name: 'Place order' })");
  assert.equal(locatorCode({ role: 'heading' }), "page.getByRole('heading')");
  assert.equal(locatorCode({ label: 'Email' }), "page.getByLabel('Email')");
  assert.equal(locatorCode({ text: 'Add to cart' }), "page.getByText('Add to cart')");
  assert.equal(locatorCode({ testid: 'cart-total' }), "page.getByTestId('cart-total')");
  assert.equal(locatorCode({ css: '#total > span' }), "page.locator('#total > span')");
  assert.equal(locatorCode({ text: 'Add', nth: 2 }), "page.getByText('Add').nth(2)");
  assert.equal(quote("it's \\ new\nline"), "'it\\'s \\\\ new\\nline'");
});

test('each action renders the line to paste into a test', () => {
  const cases = [
    [{ action: 'goto', url: 'http://localhost:8765/' }, "await page.goto('http://localhost:8765/');"],
    [{ action: 'click', target: { role: 'button', name: 'Place order' } }, "await page.getByRole('button', { name: 'Place order' }).click();"],
    [{ action: 'fill', target: { label: 'Email' }, value: "o'neil@x.test" }, "await page.getByLabel('Email').fill('o\\'neil@x.test');"],
    [{ action: 'select', target: { label: 'Size' }, value: 'M' }, "await page.getByLabel('Size').selectOption('M');"],
    [{ action: 'check', target: { label: 'Terms' } }, "await page.getByLabel('Terms').check();"],
    [{ action: 'uncheck', target: { label: 'Terms' } }, "await page.getByLabel('Terms').uncheck();"],
    [{ action: 'press', key: 'Enter' }, "await page.keyboard.press('Enter');"],
    [{ action: 'press', key: 'Enter', target: { label: 'Search' } }, "await page.getByLabel('Search').press('Enter');"],
    [{ action: 'wait_for', target: { text: 'Done' } }, "await page.getByText('Done').waitFor();"],
    [{ action: 'wait_for', url: 'http://localhost/ok' }, "await page.waitForURL('http://localhost/ok');"],
    [{ action: 'wait_for', text: 'Saved' }, "await page.getByText('Saved').first().waitFor();"],
    [{ action: 'expect_text', target: { testid: 'total' }, text: '$20.00' }, "await expect(page.getByTestId('total')).toContainText('$20.00');"],
    [{ action: 'expect_text', text: 'Thanks' }, "await expect(page.locator('body')).toContainText('Thanks');"],
    [{ action: 'reload' }, 'await page.reload();'],
    [{ action: 'back' }, 'await page.goBack();'],
  ];
  for (const [step, code] of cases) assert.equal(stepCode(step), code);
});

test('text, output and secrets are bounded', () => {
  assert.deepEqual(boundText('héllo', 2), { text: 'h', bytes: 6, truncated: true });
  assert.deepEqual(boundText('abc', 10), { text: 'abc', bytes: 3, truncated: false });
  assert.equal(redact('{"p":"axe_secret"}', 'axe_secret'), '{"p":"<redacted>"}');
  assert.equal(failureKind('net::ERR_TUNNEL_CONNECTION_FAILED'), 'blocked');
  assert.equal(failureKind('net::ERR_PROXY_CONNECTION_FAILED'), 'blocked');
  assert.equal(failureKind('net::ERR_CONNECTION_REFUSED'), 'failed');
  assert.equal(egressDenial('denied; reason=not_allowed'), 'not_allowed');
  assert.equal(egressDenial('unavailable'), null);
  const out = {
    snapshot: { text: 'x'.repeat(10000), truncated: false }, console: Array(100).fill({ type: 'error', text: 'y'.repeat(100) }),
    page_errors: [], network: { failed: [], http_errors: [], blocked: [] }, dialogs: [], steps: [],
    truncated: { output: false },
  };
  boundOutput(out, 4000);
  assert.ok(Buffer.byteLength(JSON.stringify(out)) <= 4000);
  assert.equal(out.truncated.output, true);
  assert.equal(out.snapshot.truncated, true);
});

test('a run against a fake Chromium records steps, failures and blocked hosts', async () => {
  const handlers = {};
  const calls = [];
  const locator = (name) => ({
    click: async () => calls.push(`click ${name}`),
    fill: async (value) => calls.push(`fill ${name} ${value}`),
    allInnerTexts: async () => ['Total: -$20.00'],
    innerText: async () => 'Total: -$20.00',
    ariaSnapshot: async () => '- paragraph: "Total: -$20.00"',
    nth: () => locator(name),
    first: () => locator(name),
    waitFor: async () => {},
  });
  const page = {
    on: (event, handler) => { handlers[event] = handler; },
    goto: async (url) => {
      calls.push(`goto ${url}`);
      handlers.requestfailed?.({ failure: () => ({ errorText: 'net::ERR_PROXY_CONNECTION_FAILED' }), url: () => 'http://cdn.example/x.js', method: () => 'GET' });
      handlers.requestfailed?.({ failure: () => ({ errorText: 'net::ERR_CONNECTION_REFUSED' }), url: () => 'http://localhost:9999/', method: () => 'GET' });
    },
    getByRole: (role, options) => locator(`${role}:${options?.name}`),
    getByLabel: (label) => locator(label),
    getByText: (text) => locator(text),
    getByTestId: (id) => locator(id),
    locator: (css) => locator(css),
    url: () => 'http://localhost:8765/',
    title: async () => 'Shop',
    mainFrame: () => null,
    waitForTimeout: async () => {},
    screenshot: async () => Buffer.from([0xff, 0xd8, 0xff, 0x00]),
  };
  const chromium = {
    launch: async (options) => {
      calls.push(`proxy ${options.proxy.server} ${options.proxy.password ?? '-'}`);
      return { newContext: async () => ({ newPage: async () => page, close: async () => {} }), close: async () => {} };
    },
  };
  const out = await run(input({
    steps: [
      { action: 'click', target: { role: 'button', name: 'Add coupon' } },
      { action: 'expect_text', target: { testid: 'total' }, text: '$99', timeout_ms: 100 },
      { action: 'reload' },
    ],
    proxy: null,
  }), chromium);
  assert.equal(out.ok, false);
  assert.equal(out.navigation.ok, true);
  assert.equal(out.steps.length, 2);
  assert.equal(out.steps[0].ok, true);
  assert.equal(out.steps[1].ok, false);
  assert.match(out.steps[1].error, /\$99/);
  assert.equal(out.steps_skipped, 1);
  assert.deepEqual(out.network.blocked, [{ url: 'http://cdn.example/x.js', reason: 'not_allowed' }]);
  assert.equal(out.network.failed[0].error, 'net::ERR_CONNECTION_REFUSED');
  assert.equal(out.snapshot.text, '- paragraph: "Total: -$20.00"');
  assert.equal(out.screenshot.type, 'jpeg');
  assert.equal(out.title, 'Shop');
  assert.ok(calls.includes('proxy http://127.0.0.1:3128 -'));
});
