// Axocoatl browser driver: one stateless headless Chromium check.
//
// Axocoatl runs it as `node --input-type=module -e <this file>` inside the
// browser container, with one JSON document on stdin (schema
// axocoatl.browser-input/1). It prints one JSON document (schema
// axocoatl.browser/1) on stdout. Page failures are results (exit 0, ok:false);
// only a driver failure exits non-zero. The pure helpers are exported for
// `node --test driver.test.mjs`.

export const SCHEMA_IN = 'axocoatl.browser-input/1';
export const SCHEMA_OUT = 'axocoatl.browser/1';
export const PLAYWRIGHT_VERSION = '1.60.0';
export const PLAYWRIGHT_DIR = '/opt/axocoatl/playwright/';
export const ACTIONS = [
  'goto', 'click', 'fill', 'select', 'check', 'uncheck', 'press', 'wait_for', 'expect_text', 'reload', 'back',
];
export const MAX_STEPS = 40;
export const MAX_STRING = 4096;
export const PROXY_SERVER = 'http://127.0.0.1:3128';
export const PROXY_BYPASS = 'localhost,127.0.0.1,[::1]';
export const CHROMIUM_ARGS = [
  '--no-sandbox', '--disable-dev-shm-usage', '--disable-quic', '--no-first-run',
  '--disable-background-networking', '--disable-component-update', '--disable-sync',
  '--disable-domain-reliability', '--metrics-recording-only',
];
const TARGET_KEYS = ['role', 'label', 'text', 'testid', 'css'];
const STEP_KEYS = ['action', 'target', 'url', 'value', 'key', 'text', 'timeout_ms'];
const PROXY_REFUSED = ['net::ERR_TUNNEL_CONNECTION_FAILED', 'net::ERR_PROXY_CONNECTION_FAILED'];

const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

function checkString(value, field) {
  if (typeof value !== 'string') return `${field} must be a string`;
  if (Buffer.byteLength(value) > MAX_STRING) return `${field} exceeds ${MAX_STRING} bytes`;
  return null;
}

/** Why `value` is not an http(s) URL Axocoatl will open, or null. */
export function checkUrl(value, field = 'url') {
  const bad = checkString(value, field);
  if (bad) return bad;
  let url;
  try {
    url = new URL(value);
  } catch {
    return `${field} is not an absolute URL`;
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    return `${field} must use http or https, not ${url.protocol.replace(/:$/, '')}`;
  }
  if (url.username || url.password) return `${field} must not contain a user name or password`;
  return null;
}

/** Why `target` is not a usable element target, or null. */
export function checkTarget(target, field = 'target') {
  if (!isObject(target)) return `${field} must be an object`;
  for (const key of Object.keys(target)) {
    if (![...TARGET_KEYS, 'name', 'nth'].includes(key)) return `${field}.${key} is not a target field`;
  }
  const kinds = TARGET_KEYS.filter((key) => target[key] !== undefined);
  if (kinds.length !== 1) return `${field} needs exactly one of ${TARGET_KEYS.join(', ')}`;
  for (const key of [...TARGET_KEYS, 'name']) {
    if (target[key] !== undefined) {
      const bad = checkString(target[key], `${field}.${key}`);
      if (bad) return bad;
    }
  }
  if (target.name !== undefined && kinds[0] !== 'role') return `${field}.name is only for role targets`;
  if (target.nth !== undefined && !(Number.isInteger(target.nth) && target.nth >= 0)) {
    return `${field}.nth must be a whole number from 0`;
  }
  return null;
}

/** Why step `index` is invalid, or null. */
export function checkStep(step, index) {
  const field = `steps[${index}]`;
  if (!isObject(step)) return `${field} must be an object`;
  for (const key of Object.keys(step)) {
    if (!STEP_KEYS.includes(key)) return `${field}.${key} is not a step field`;
  }
  if (!ACTIONS.includes(step.action)) return `${field}.action must be one of ${ACTIONS.join(', ')}`;
  for (const key of ['value', 'key', 'text']) {
    if (step[key] !== undefined) {
      const bad = checkString(step[key], `${field}.${key}`);
      if (bad) return bad;
    }
  }
  if (step.url !== undefined) {
    const bad = checkUrl(step.url, `${field}.url`);
    if (bad) return bad;
  }
  if (step.target !== undefined) {
    const bad = checkTarget(step.target, `${field}.target`);
    if (bad) return bad;
  }
  if (step.timeout_ms !== undefined
    && !(Number.isInteger(step.timeout_ms) && step.timeout_ms >= 100 && step.timeout_ms <= 10000)) {
    return `${field}.timeout_ms must be 100-10000`;
  }
  const has = (key) => step[key] !== undefined;
  const need = (keys, allowed) => {
    for (const key of keys) if (!has(key)) return `${field}: ${step.action} needs ${key}`;
    for (const key of ['target', 'url', 'value', 'key', 'text']) {
      if (has(key) && !keys.includes(key) && !allowed.includes(key)) {
        return `${field}: ${step.action} does not take ${key}`;
      }
    }
    return null;
  };
  switch (step.action) {
    case 'goto': return need(['url'], []);
    case 'click': case 'check': case 'uncheck': return need(['target'], []);
    case 'fill': case 'select': return need(['target', 'value'], []);
    case 'press': return need(['key'], ['target']);
    case 'expect_text': return need(['text'], ['target']);
    case 'reload': case 'back': return need([], []);
    case 'wait_for': {
      const given = ['target', 'url', 'text'].filter(has);
      if (given.length !== 1) return `${field}: wait_for needs exactly one of target, url or text`;
      return need([], ['target', 'url', 'text']);
    }
    default: return `${field}.action is unknown`;
  }
}

/** Validate the whole driver input. Returns the first problem, or null. */
export function checkInput(input) {
  if (!isObject(input)) return 'input must be an object';
  if (input.schema !== SCHEMA_IN) return `input schema must be ${SCHEMA_IN}`;
  const bad = checkUrl(input.url);
  if (bad) return bad;
  const steps = input.steps ?? [];
  if (!Array.isArray(steps)) return 'steps must be an array';
  if (steps.length > MAX_STEPS) return `at most ${MAX_STEPS} steps`;
  for (let index = 0; index < steps.length; index += 1) {
    const problem = checkStep(steps[index], index);
    if (problem) return problem;
  }
  if (!['aria', 'text', 'none'].includes(input.snapshot ?? 'aria')) return 'snapshot must be aria, text or none';
  return null;
}

/** A JavaScript single-quoted string literal. */
export function quote(value) {
  return `'${String(value)
    .replace(/\\/g, '\\\\')
    .replace(/'/g, "\\'")
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r')
    .replace(/\u2028/g, '\\u2028')
    .replace(/\u2029/g, '\\u2029')}'`;
}

/** The Playwright locator expression for a target. */
export function locatorCode(target) {
  let code;
  if (target.role !== undefined) {
    code = target.name !== undefined
      ? `page.getByRole(${quote(target.role)}, { name: ${quote(target.name)} })`
      : `page.getByRole(${quote(target.role)})`;
  } else if (target.label !== undefined) code = `page.getByLabel(${quote(target.label)})`;
  else if (target.text !== undefined) code = `page.getByText(${quote(target.text)})`;
  else if (target.testid !== undefined) code = `page.getByTestId(${quote(target.testid)})`;
  else code = `page.locator(${quote(target.css)})`;
  if (target.nth !== undefined) code += `.nth(${target.nth})`;
  return code;
}

/** The Playwright test line equivalent to one step. */
export function stepCode(step) {
  const target = step.target ? locatorCode(step.target) : null;
  switch (step.action) {
    case 'goto': return `await page.goto(${quote(step.url)});`;
    case 'click': return `await ${target}.click();`;
    case 'fill': return `await ${target}.fill(${quote(step.value)});`;
    case 'select': return `await ${target}.selectOption(${quote(step.value)});`;
    case 'check': return `await ${target}.check();`;
    case 'uncheck': return `await ${target}.uncheck();`;
    case 'press': return target
      ? `await ${target}.press(${quote(step.key)});`
      : `await page.keyboard.press(${quote(step.key)});`;
    case 'wait_for':
      if (target) return `await ${target}.waitFor();`;
      if (step.url !== undefined) return `await page.waitForURL(${quote(step.url)});`;
      return `await page.getByText(${quote(step.text)}).first().waitFor();`;
    case 'expect_text':
      return `await expect(${target ?? "page.locator('body')"}).toContainText(${quote(step.text)});`;
    case 'reload': return 'await page.reload();';
    case 'back': return 'await page.goBack();';
    default: return '';
  }
}

/** Cut `text` to at most `maxBytes` UTF-8 bytes on a character boundary. */
export function boundText(text, maxBytes) {
  const buffer = Buffer.from(String(text ?? ''), 'utf8');
  if (buffer.length <= maxBytes) return { text: buffer.toString('utf8'), bytes: buffer.length, truncated: false };
  let end = maxBytes;
  while (end > 0 && (buffer[end] & 0xc0) === 0x80) end -= 1;
  return { text: buffer.subarray(0, end).toString('utf8'), bytes: buffer.length, truncated: true };
}

/** Classify a failed request: blocked by the egress path, or failed. */
export function failureKind(errorText) {
  return PROXY_REFUSED.includes(errorText) ? 'blocked' : 'failed';
}

/** The reason in an `X-Axocoatl-Egress: denied; reason=<r>` header, or null. */
export function egressDenial(header) {
  if (typeof header !== 'string' || !header.startsWith('denied')) return null;
  const match = /reason=([a-z_]+)/.exec(header);
  return match ? match[1] : 'denied';
}

/** Replace every occurrence of `secret` in a JSON text. */
export function redact(text, secret) {
  return secret ? text.split(secret).join('<redacted>') : text;
}

/** Keep the result within `maxBytes` of JSON by shrinking, in order, the
 * snapshot, then the console, network and dialog lists. */
export function boundOutput(out, maxBytes) {
  const size = () => Buffer.byteLength(JSON.stringify(out));
  if (size() <= maxBytes) return out;
  out.truncated.output = true;
  while (size() > maxBytes && out.snapshot && out.snapshot.text.length > 0) {
    const excess = size() - maxBytes;
    const keep = Math.max(0, Buffer.byteLength(out.snapshot.text) - excess - 64);
    out.snapshot.text = boundText(out.snapshot.text, keep).text;
    out.snapshot.truncated = true;
    if (keep === 0) break;
  }
  const lists = [out.console, out.page_errors, out.network.failed, out.network.http_errors,
    out.network.blocked, out.dialogs, out.steps];
  for (const list of lists) {
    while (size() > maxBytes && list.length > 0) list.pop();
  }
  return out;
}

function locate(page, target) {
  let locator;
  if (target.role !== undefined) {
    locator = page.getByRole(target.role, target.name !== undefined ? { name: target.name } : {});
  } else if (target.label !== undefined) locator = page.getByLabel(target.label);
  else if (target.text !== undefined) locator = page.getByText(target.text);
  else if (target.testid !== undefined) locator = page.getByTestId(target.testid);
  else locator = page.locator(target.css);
  return target.nth !== undefined ? locator.nth(target.nth) : locator;
}

async function expectText(page, step, timeout) {
  const deadline = Date.now() + timeout;
  let seen = '';
  for (;;) {
    const texts = step.target
      ? await locate(page, step.target).allInnerTexts()
      : [await page.locator('body').innerText({ timeout })];
    seen = texts.join(' | ');
    if (texts.some((text) => text.includes(step.text))) return;
    if (Date.now() >= deadline) {
      throw new Error(`expected text ${JSON.stringify(step.text)} was not found; the target shows ${JSON.stringify(boundText(seen, 300).text)}`);
    }
    await page.waitForTimeout(100);
  }
}

async function runStep(page, step, timeout) {
  const options = { timeout };
  switch (step.action) {
    case 'goto': return page.goto(step.url, { ...options, waitUntil: 'load' });
    case 'click': return locate(page, step.target).click(options);
    case 'fill': return locate(page, step.target).fill(step.value, options);
    case 'select': return locate(page, step.target).selectOption(step.value, options);
    case 'check': return locate(page, step.target).check(options);
    case 'uncheck': return locate(page, step.target).uncheck(options);
    case 'press': return step.target
      ? locate(page, step.target).press(step.key, options)
      : page.keyboard.press(step.key);
    case 'wait_for':
      if (step.target) return locate(page, step.target).waitFor(options);
      if (step.url !== undefined) return page.waitForURL(step.url, options);
      return page.getByText(step.text).first().waitFor(options);
    case 'expect_text': return expectText(page, step, timeout);
    case 'reload': return page.reload({ ...options, waitUntil: 'load' });
    case 'back': return page.goBack({ ...options, waitUntil: 'load' });
    default: throw new Error(`unknown action ${step.action}`);
  }
}

const message = (error) => boundText(String(error?.message ?? error).replace(/\u001b\[[0-9;]*m/g, ''), 1000).text;

function within(promise, ms, what) {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${what} ran out of time`)), Math.max(1, ms)); }),
  ]).finally(() => clearTimeout(timer));
}

async function readStdin(limit) {
  const chunks = [];
  let total = 0;
  for await (const chunk of process.stdin) {
    total += chunk.length;
    if (total > limit) throw new Error('driver input exceeds its bound');
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}

function emit(document, secret, code) {
  process.stdout.write(`${redact(JSON.stringify(document), secret)}\n`, () => process.exit(code));
}

/** Run one check. Exported for the live test; Axocoatl runs it through main(). */
export async function run(input, chromium) {
  const started = Date.now();
  const limits = {
    snapshot_max_bytes: 16384, step_timeout_ms: 10000, total_timeout_ms: 115000, console_max: 50,
    network_max: 50, output_max_bytes: 65536, screenshot_max_bytes: 1048576, ...(input.limits ?? {}),
  };
  const deadline = started + limits.total_timeout_ms;
  const left = () => deadline - Date.now();
  const out = {
    schema: SCHEMA_OUT, ok: false, url: input.url, final_url: null, title: null, status: null, ms: 0,
    playwright: PLAYWRIGHT_VERSION, navigation: null, steps: [], steps_skipped: 0,
    snapshot: null, console: [], page_errors: [],
    network: { failed: [], http_errors: [], blocked: [] }, dialogs: [],
    truncated: { console: false, network: false, output: false },
  };
  const push = (list, item, flag, max) => {
    if (list.length < max) list.push(item); else out.truncated[flag] = true;
  };
  const proxy = { server: PROXY_SERVER, bypass: PROXY_BYPASS };
  if (input.proxy) Object.assign(proxy, { username: input.proxy.username, password: input.proxy.password });
  const browser = await chromium.launch({ headless: true, proxy, args: CHROMIUM_ARGS });
  let screenshot = null;
  try {
    const viewport = { width: 1280, height: 720, ...(input.viewport ?? {}) };
    const context = await browser.newContext({ acceptDownloads: false, serviceWorkers: 'block', viewport });
    const page = await context.newPage();
    page.on('console', (entry) => {
      if (entry.type() === 'error' || entry.type() === 'warning') {
        push(out.console, { type: entry.type(), text: boundText(entry.text(), 1000).text }, 'console', limits.console_max);
      }
    });
    page.on('pageerror', (error) => push(out.page_errors, message(error), 'console', limits.console_max));
    page.on('dialog', (dialog) => {
      push(out.dialogs, { type: dialog.type(), message: boundText(dialog.message(), 1000).text }, 'console', limits.console_max);
      dialog.dismiss().catch(() => {});
    });
    page.on('requestfailed', (request) => {
      const error = request.failure()?.errorText ?? 'failed';
      const url = boundText(request.url(), 1000).text;
      if (failureKind(error) === 'blocked') {
        const reason = error === 'net::ERR_PROXY_CONNECTION_FAILED' && !input.proxy ? 'not_allowed' : 'proxy_refused';
        push(out.network.blocked, { url, reason }, 'network', limits.network_max);
      } else {
        push(out.network.failed, { url, method: request.method(), error }, 'network', limits.network_max);
      }
    });
    page.on('response', (response) => {
      const denial = egressDenial(response.headers()['x-axocoatl-egress']);
      const url = boundText(response.url(), 1000).text;
      if (denial) push(out.network.blocked, { url, reason: denial }, 'network', limits.network_max);
      else if (response.status() >= 400) {
        push(out.network.http_errors, { url, status: response.status() }, 'network', limits.network_max);
      }
      if (response.request().isNavigationRequest() && response.frame() === page.mainFrame()) {
        out.status = response.status();
      }
    });

    const navigation = { ok: false, ms: 0, code: stepCode({ action: 'goto', url: input.url }) };
    out.navigation = navigation;
    const navigationStarted = Date.now();
    try {
      const timeout = Math.min(Math.max(limits.step_timeout_ms, 15000), left());
      const response = await within(page.goto(input.url, { waitUntil: 'load', timeout }), left(), 'the page load');
      const denial = egressDenial(response?.headers()['x-axocoatl-egress']);
      if (denial) throw new Error(`blocked by Axocoatl's egress proxy (${denial})`);
      navigation.ok = true;
    } catch (error) {
      navigation.error = message(error);
    }
    navigation.ms = Date.now() - navigationStarted;

    const steps = input.steps ?? [];
    let failed = !navigation.ok;
    for (let index = 0; index < steps.length; index += 1) {
      const step = steps[index];
      if (failed || left() <= 0) {
        out.steps_skipped = steps.length - index;
        break;
      }
      const entry = { i: index, action: step.action, ok: false, ms: 0, code: stepCode(step) };
      const stepStarted = Date.now();
      try {
        const timeout = Math.min(step.timeout_ms ?? limits.step_timeout_ms, Math.max(1, left()));
        await within(runStep(page, step, timeout), timeout + 1000, `step ${index}`);
        entry.ok = true;
      } catch (error) {
        entry.error = message(error);
        failed = true;
      }
      entry.ms = Date.now() - stepStarted;
      out.steps.push(entry);
    }
    out.ok = !failed;

    out.final_url = page.url();
    out.title = await within(page.title(), 3000, 'reading the title').catch(() => null);
    const kind = input.snapshot ?? 'aria';
    if (kind !== 'none') {
      try {
        const body = page.locator('body');
        const text = kind === 'aria'
          ? await within(body.ariaSnapshot({ timeout: 5000 }), 6000, 'the snapshot')
          : await within(body.innerText({ timeout: 5000 }), 6000, 'the snapshot');
        const bounded = boundText(text, limits.snapshot_max_bytes);
        out.snapshot = { kind, text: bounded.text, bytes: bounded.bytes, truncated: bounded.truncated };
      } catch (error) {
        out.snapshot = { kind, text: '', bytes: 0, truncated: false, error: message(error) };
      }
    }
    if (input.screenshot !== false) {
      try {
        const image = await within(
          page.screenshot({ type: 'jpeg', quality: 70, timeout: 5000, animations: 'disabled', caret: 'hide' }),
          6000, 'the screenshot');
        screenshot = image.length <= limits.screenshot_max_bytes
          ? { type: 'jpeg', base64: image.toString('base64'), bytes: image.length }
          : { type: 'jpeg', dropped: 'too_large', bytes: image.length };
      } catch (error) {
        screenshot = { type: 'jpeg', dropped: message(error) };
      }
    }
    await context.close().catch(() => {});
  } finally {
    await browser.close().catch(() => {});
  }
  out.ms = Date.now() - started;
  boundOutput(out, limits.output_max_bytes);
  if (screenshot) out.screenshot = screenshot;
  return out;
}

async function main() {
  let secret = null;
  try {
    const input = JSON.parse(await readStdin(1024 * 1024));
    secret = input?.proxy?.password ?? null;
    const problem = checkInput(input);
    if (problem) {
      emit({ schema: SCHEMA_OUT, ok: false, error: `invalid browser input: ${problem}` }, secret, 2);
      return;
    }
    let chromium;
    try {
      const { createRequire } = await import('node:module');
      ({ chromium } = createRequire(PLAYWRIGHT_DIR)('playwright-core'));
    } catch (error) {
      emit({
        schema: SCHEMA_OUT, ok: false,
        error: `this image has no Playwright ${PLAYWRIGHT_VERSION} at ${PLAYWRIGHT_DIR} (${message(error)}); run axocoatl browser install`,
      }, secret, 3);
      return;
    }
    emit(await run(input, chromium), secret, 0);
  } catch (error) {
    emit({ schema: SCHEMA_OUT, ok: false, error: `the browser driver failed: ${message(error)}` }, secret, 1);
  }
}

if (/\[eval\d*\]$/.test(import.meta.url)) await main();
