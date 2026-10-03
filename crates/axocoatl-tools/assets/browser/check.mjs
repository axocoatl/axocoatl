// Axocoatl check runner: runs one Playwright test file against the app.
//
// Axocoatl runs it as `node --input-type=module -e <this file>` inside the
// browser container, with one JSON document on stdin (schema
// axocoatl.browser-check-input/1) that carries the test file and the files it
// imports. It writes them under a fresh /tmp directory, runs them with the
// image's @playwright/test 1.60.0 (one worker, no retries) and prints one
// JSON document (schema axocoatl.browser-check/1). A failing test is a result
// (exit 0); only a runner failure exits non-zero. The pure helpers are
// exported for `node --test check.test.mjs`.

export const SCHEMA_IN = 'axocoatl.browser-check-input/1';
export const SCHEMA_OUT = 'axocoatl.browser-check/1';
export const PLAYWRIGHT_VERSION = '1.60.0';
export const PLAYWRIGHT_MODULES = '/opt/axocoatl/playwright/node_modules';
export const MAX_FILES = 32;
export const MAX_FILE_BYTES = 256 * 1024;
export const MAX_TOTAL_BYTES = 1024 * 1024;
export const PROXY_SERVER = 'http://127.0.0.1:3128';
export const PROXY_BYPASS = 'localhost,127.0.0.1,[::1]';
export const CHROMIUM_ARGS = [
  '--no-sandbox', '--disable-dev-shm-usage', '--disable-quic', '--no-first-run',
  '--disable-background-networking', '--disable-component-update', '--disable-sync',
  '--disable-domain-reliability', '--metrics-recording-only',
];
const SEGMENT = /^[A-Za-z0-9_@+][A-Za-z0-9._@+-]*$/;
const TEST_FILE = /\.(?:[cm]?[jt]sx?)$/;

const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

/** Why `path` is not a safe relative file path, or null. */
export function checkRelativePath(path, field = 'path') {
  if (typeof path !== 'string' || path.length === 0) return `${field} must be a non-empty string`;
  if (Buffer.byteLength(path) > 512) return `${field} exceeds 512 bytes`;
  const segments = path.split('/');
  for (const segment of segments) {
    if (!SEGMENT.test(segment) || segment === '.' || segment === '..') {
      return `${field} must be a relative path of letters, digits, '.', '_', '-', '@' and '+' without '..'`;
    }
  }
  if (segments[0] === 'node_modules') return `${field} must not be under node_modules`;
  return null;
}

/** Validate the runner input. Returns the first problem, or null. */
export function checkInput(input) {
  if (!isObject(input)) return 'input must be an object';
  if (input.schema !== SCHEMA_IN) return `input schema must be ${SCHEMA_IN}`;
  const bad = checkRelativePath(input.entry, 'entry');
  if (bad) return bad;
  if (!TEST_FILE.test(input.entry)) return 'entry must be a .ts, .js, .mjs, .cjs, .mts, .cts, .tsx or .jsx file';
  if (!Array.isArray(input.files) || input.files.length === 0 || input.files.length > MAX_FILES) {
    return `files must list 1-${MAX_FILES} files`;
  }
  let total = 0;
  const seen = new Set();
  for (const [index, file] of input.files.entries()) {
    if (!isObject(file)) return `files[${index}] must be an object`;
    const problem = checkRelativePath(file.path, `files[${index}].path`);
    if (problem) return problem;
    if (seen.has(file.path)) return `files[${index}].path is listed twice`;
    seen.add(file.path);
    if (typeof file.content !== 'string') return `files[${index}].content must be a string`;
    const bytes = Buffer.byteLength(file.content);
    if (bytes > MAX_FILE_BYTES) return `files[${index}] exceeds ${MAX_FILE_BYTES} bytes`;
    total += bytes;
  }
  if (total > MAX_TOTAL_BYTES) return `files exceed ${MAX_TOTAL_BYTES} bytes together`;
  if (!seen.has(input.entry)) return 'entry must be one of files';
  if (input.grep !== undefined && input.grep !== null
    && (typeof input.grep !== 'string' || input.grep.length === 0 || Buffer.byteLength(input.grep) > 512)) {
    return 'grep must be 1-512 bytes';
  }
  if (typeof input.base_url !== 'string') return 'base_url must be a string';
  let url;
  try {
    url = new URL(input.base_url);
  } catch {
    return 'base_url is not an absolute URL';
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return 'base_url must use http or https';
  return null;
}

/** Regular-expression source matching exactly `text`. */
export function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\/]/g, '\\$&');
}

/** The Playwright configuration module for one run. The proxy credential is
 * read from the environment so it is never written to a file. */
export function configSource({ testDir, entry, baseURL, testTimeoutMs, globalTimeoutMs, reportFile, outputDir }) {
  const matcher = `^${escapeRegExp(`${testDir}/${entry}`)}$`;
  return `const password = process.env.AXO_PROXY_PASSWORD;
export default {
  testDir: ${JSON.stringify(testDir)},
  testMatch: new RegExp(${JSON.stringify(matcher)}),
  retries: 0,
  workers: 1,
  fullyParallel: false,
  timeout: ${Number(testTimeoutMs)},
  globalTimeout: ${Number(globalTimeoutMs)},
  reporter: [['json', { outputFile: ${JSON.stringify(reportFile)} }]],
  outputDir: ${JSON.stringify(outputDir)},
  preserveOutput: 'failures-only',
  updateSnapshots: 'none',
  use: {
    baseURL: ${JSON.stringify(baseURL)},
    headless: true,
    viewport: { width: 1280, height: 720 },
    proxy: { server: ${JSON.stringify(PROXY_SERVER)}, bypass: ${JSON.stringify(PROXY_BYPASS)}, ...(password ? { username: 'axo', password } : {}) },
    launchOptions: { args: ${JSON.stringify(CHROMIUM_ARGS)} },
    serviceWorkers: 'block',
    acceptDownloads: false,
    screenshot: 'only-on-failure',
    trace: 'off',
    video: 'off',
  },
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
};
`;
}

export const stripAnsi = (text) => String(text ?? '').replace(/\u001b\[[0-9;]*[A-Za-z]/g, '');

/** Cut `text` to at most `maxBytes` UTF-8 bytes on a character boundary,
 * keeping the start (or the end when `tail`). */
export function boundText(text, maxBytes, tail = false) {
  const buffer = Buffer.from(String(text ?? ''), 'utf8');
  if (buffer.length <= maxBytes) return { text: buffer.toString('utf8'), truncated: false };
  if (tail) {
    let start = buffer.length - maxBytes;
    while (start < buffer.length && (buffer[start] & 0xc0) === 0x80) start += 1;
    return { text: buffer.subarray(start).toString('utf8'), truncated: true };
  }
  let end = maxBytes;
  while (end > 0 && (buffer[end] & 0xc0) === 0x80) end -= 1;
  return { text: buffer.subarray(0, end).toString('utf8'), truncated: true };
}

/** Rewrite absolute paths under `root` as repository paths. */
export function relativize(text, root) {
  return root ? String(text ?? '').split(`${root}/`).join('') : String(text ?? '');
}

function describeError(error, root) {
  if (!error) return undefined;
  const clean = (text, max) => boundText(relativize(stripAnsi(text), root), max).text;
  const result = { message: clean(error.message ?? error.value ?? '', 2000) };
  const location = error.location ?? null;
  if (location && typeof location.file === 'string') {
    result.location = { file: relativize(location.file, root), line: location.line, column: location.column };
  }
  if (error.snippet) result.snippet = clean(error.snippet, 1500);
  return result;
}

/** Summarize a Playwright JSON report. `root` is the directory the files
 * were written to; paths under it are reported relative to it. */
export function summarize(report, root, maxTests = 50) {
  const tests = [];
  const counts = { total: 0, passed: 0, failed: 0, timed_out: 0, skipped: 0, interrupted: 0 };
  let truncated = false;
  let screenshot = null;
  const visit = (suite, titles) => {
    const path = suite.title && !suite.file?.endsWith(suite.title) ? [...titles, suite.title] : titles;
    for (const spec of suite.specs ?? []) {
      for (const test of spec.tests ?? []) {
        const result = (test.results ?? []).at(-1) ?? { status: 'skipped', duration: 0 };
        counts.total += 1;
        const status = result.status ?? 'skipped';
        if (status === 'passed') counts.passed += 1;
        else if (status === 'failed') counts.failed += 1;
        else if (status === 'timedOut') counts.timed_out += 1;
        else if (status === 'interrupted') counts.interrupted += 1;
        else counts.skipped += 1;
        if (!screenshot && status !== 'passed') {
          const image = (result.attachments ?? []).find((item) => item.contentType === 'image/png' && item.path);
          if (image) screenshot = image.path;
        }
        if (tests.length >= maxTests) {
          truncated = true;
          continue;
        }
        tests.push({
          title: [...path, spec.title].filter(Boolean).join(' > '),
          file: relativize(spec.file ?? suite.file ?? '', root),
          line: spec.line,
          status,
          ms: result.duration ?? 0,
          ...(result.error ? { error: describeError(result.error, root) } : {}),
        });
      }
    }
    for (const child of suite.suites ?? []) visit(child, path);
  };
  for (const suite of report?.suites ?? []) visit(suite, []);
  const errors = (report?.errors ?? []).slice(0, 10).map((error) => describeError(error, root));
  let status;
  if (counts.total === 0) status = errors.length > 0 ? 'error' : 'no_tests';
  else if (counts.failed + counts.timed_out + counts.interrupted > 0) status = 'failed';
  else if (counts.passed === 0) status = 'skipped';
  else status = errors.length > 0 ? 'error' : 'passed';
  return { status, counts, tests, errors, truncated, screenshot };
}

async function readStdin(limit) {
  const chunks = [];
  let total = 0;
  for await (const chunk of process.stdin) {
    total += chunk.length;
    if (total > limit) throw new Error('runner input exceeds its bound');
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString('utf8');
}

function emit(document, secret, code) {
  let text = JSON.stringify(document);
  if (secret) text = text.split(secret).join('<redacted>');
  process.stdout.write(`${text}\n`, () => process.exit(code));
}

/** One run. Returns `[document, exit code]` after removing everything the
 * run wrote, so nothing of it outlives the call. */
async function runCheck(fs, spawn, holder) {
  const started = Date.now();
  let root = null;
  try {
    const input = JSON.parse(await readStdin(4 * 1024 * 1024));
    holder.secret = input?.proxy?.password ?? null;
    const secret = holder.secret;
    const problem = checkInput(input);
    if (problem) {
      return [{ schema: SCHEMA_OUT, ok: false, status: 'error', error: `invalid check input: ${problem}` }, 2];
    }
    const limits = {
      total_timeout_ms: 115000, test_timeout_ms: 30000, output_max_bytes: 65536,
      log_max_bytes: 8192, screenshot_max_bytes: 1048576, ...(input.limits ?? {}),
    };
    const cli = `${PLAYWRIGHT_MODULES}/@playwright/test/cli.js`;
    try {
      await fs.access(cli);
    } catch {
      return [{
        schema: SCHEMA_OUT, ok: false, status: 'error',
        error: `this image has no @playwright/test ${PLAYWRIGHT_VERSION} at ${PLAYWRIGHT_MODULES}; run axocoatl browser install`,
      }, 3];
    }
    root = await fs.mkdtemp('/tmp/axo-check-');
    const work = `${root}/work`;
    for (const file of input.files) {
      const target = `${work}/${file.path}`;
      await fs.mkdir(target.slice(0, target.lastIndexOf('/')), { recursive: true, mode: 0o700 });
      await fs.writeFile(target, file.content, { mode: 0o600, flag: 'wx' });
    }
    await fs.symlink(PLAYWRIGHT_MODULES, `${work}/node_modules`);
    const reportFile = `${root}/report.json`;
    const config = `${root}/axocoatl.check.config.mjs`;
    const budget = Math.max(5000, limits.total_timeout_ms - 3000);
    await fs.writeFile(config, configSource({
      testDir: work, entry: input.entry, baseURL: input.base_url, testTimeoutMs: limits.test_timeout_ms,
      globalTimeoutMs: budget, reportFile, outputDir: `${root}/results`,
    }), { mode: 0o600 });
    const args = [cli, 'test', '--config', config, '--workers', '1', '--retries', '0'];
    if (input.grep) args.push('--grep', input.grep);
    // Home, caches and Playwright's transform cache live under the run's
    // directory, so the files of one run never reach the next.
    await fs.mkdir(`${root}/home`, { mode: 0o700 });
    const env = {
      PATH: process.env.PATH ?? '/usr/local/bin:/usr/bin:/bin', HOME: `${root}/home`, TMPDIR: '/tmp',
      XDG_CACHE_HOME: `${root}/home/.cache`, PWTEST_CACHE_DIR: `${root}/transform-cache`,
      PLAYWRIGHT_BROWSERS_PATH: process.env.PLAYWRIGHT_BROWSERS_PATH ?? '/ms-playwright',
      CI: '1', FORCE_COLOR: '0', PW_TEST_HTML_REPORT_OPEN: 'never',
      BASE_URL: input.base_url, AXOCOATL_APP_URL: input.base_url,
      ...(secret ? { AXO_PROXY_PASSWORD: secret } : {}),
    };
    const child = spawn(process.execPath, args, { cwd: work, env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
    const logs = { stdout: [], stderr: [] };
    for (const stream of ['stdout', 'stderr']) {
      let kept = 0;
      child[stream].on('data', (chunk) => {
        logs[stream].push(chunk);
        kept += chunk.length;
        while (kept > limits.log_max_bytes * 4 && logs[stream].length > 1) kept -= logs[stream].shift().length;
      });
    }
    let timedOut = false;
    const exit = await new Promise((resolve) => {
      const timer = setTimeout(() => {
        timedOut = true;
        try { process.kill(-child.pid, 'SIGKILL'); } catch { child.kill('SIGKILL'); }
      }, budget + 1500);
      child.on('close', (code, signal) => { clearTimeout(timer); resolve({ code, signal }); });
      child.on('error', (error) => { clearTimeout(timer); resolve({ code: null, signal: null, error }); });
    });
    let report = null;
    try {
      report = JSON.parse(await fs.readFile(reportFile, 'utf8'));
    } catch {
      report = null;
    }
    const summary = summarize(report, work);
    const log = (stream) => boundText(relativize(stripAnsi(Buffer.concat(logs[stream]).toString('utf8')), work), limits.log_max_bytes, true);
    const stdout = log('stdout');
    const stderr = log('stderr');
    const out = {
      schema: SCHEMA_OUT, ok: summary.status === 'passed' && !timedOut, status: timedOut ? 'timed_out' : summary.status,
      entry: input.entry, counts: summary.counts, tests: summary.tests, errors: summary.errors,
      exit_code: exit.code, ms: Date.now() - started, playwright: PLAYWRIGHT_VERSION,
      stdout: stdout.text, stderr: stderr.text,
      truncated: { tests: summary.truncated, stdout: stdout.truncated, stderr: stderr.truncated, output: false },
    };
    if (!report && !timedOut) {
      out.status = 'error';
      out.error = exit.error ? String(exit.error.message) : 'Playwright wrote no report; see stderr';
    }
    while (Buffer.byteLength(JSON.stringify(out)) > limits.output_max_bytes) {
      out.truncated.output = true;
      if (out.stdout) out.stdout = '';
      else if (out.stderr) out.stderr = '';
      else if (out.tests.length > 1) out.tests.pop();
      else if (out.errors.length > 0) out.errors.pop();
      else break;
    }
    if (summary.screenshot) {
      try {
        const image = await fs.readFile(summary.screenshot);
        out.screenshot = image.length <= limits.screenshot_max_bytes
          ? { type: 'png', base64: image.toString('base64'), bytes: image.length }
          : { type: 'png', dropped: 'too_large', bytes: image.length };
      } catch {
        out.screenshot = { type: 'png', dropped: 'unreadable' };
      }
    }
    return [out, 0];
  } catch (error) {
    return [{ schema: SCHEMA_OUT, ok: false, status: 'error', error: `the check runner failed: ${String(error?.message ?? error)}` }, 1];
  } finally {
    if (root) await fs.rm(root, { recursive: true, force: true }).catch(() => {});
  }
}

async function main() {
  const fs = await import('node:fs/promises');
  const { spawn } = await import('node:child_process');
  const holder = { secret: null };
  const [document, code] = await runCheck(fs, spawn, holder);
  emit(document, holder.secret, code);
}

if (/\[eval\d*\]$/.test(import.meta.url)) await main();
