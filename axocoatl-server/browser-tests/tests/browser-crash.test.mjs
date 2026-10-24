// How the suite tells a browser crash from a product failure (README,
// "Browser crashes"): which failures run-tests.mjs reruns, and that a crash
// is printed under the test it happened in. The journeys that crash a
// browser run in a child `node --test`, so their lines stay out of this
// run's output and its counts.
import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { test } from 'node:test';
import { pathToFileURL } from 'node:url';

import { guardCreation } from '../support/browser-crash.mjs';
import { CRASH_LINE, judgeRun, rerunCap, rerunPassed } from '../support/crash-rerun.mjs';
import { TEST_ROOT } from '../support/daemon.mjs';

const REPORTER = pathToFileURL(path.join(TEST_ROOT, 'support', 'crash-report.mjs')).href;
// The engine the fixtures' `chromium` is (support/browser-engine.mjs).
const ENGINE = process.env.AXO_BROWSER_ENGINE || 'chromium';

function failure(name, crash = [], extra = {}) {
  return { name, nesting: 0, failureType: 'testCodeFailure', error: `${name} broke`, derived: false, crash, ...extra };
}

function file(failures, counts = {}) {
  return { tests: 3, passed: 3 - failures.length, failed: failures.length, skipped: 0, failures, crashLinesOnPassedTests: 0, ...counts };
}

test('a file is rerun only when each of its failures followed a detected browser crash', () => {
  const crash = [`${CRASH_LINE} the web process of a webkit page crashed`];
  const judged = judgeRun({ code: 1, report: { files: {
    'tests/clean.test.mjs': file([]),
    'tests/crashed.test.mjs': file([failure('a', crash), failure('b', crash)]),
    'tests/mixed.test.mjs': file([failure('c', crash), failure('d')]),
    'tests/product.test.mjs': file([failure('e')]),
    'tests/nested.test.mjs': file([
      failure('child', crash, { nesting: 1 }),
      failure('parent', [], { failureType: 'subtestsFailed', derived: true }),
    ]),
    'tests/hook.test.mjs': file([failure('/abs/tests/hook.test.mjs', [], { failureType: 'hookFailed' })]),
  } } }, 3);
  assert.deepEqual(judged.rerun.map(({ file: name }) => name), ['tests/crashed.test.mjs', 'tests/nested.test.mjs']);
  assert.deepEqual(judged.failed[0].afterCrash.map(({ name }) => name), ['c']);
  assert.deepEqual(judged.failed.map(({ file: name, failures }) => [name, failures.map(({ name: test_ }) => test_)]), [
    ['tests/mixed.test.mjs', ['d']],
    ['tests/product.test.mjs', ['e']],
    ['tests/hook.test.mjs', ['/abs/tests/hook.test.mjs']],
  ]);
  assert.deepEqual(judged.problems, []);
});

test('more crashed files than the cap reruns none and fails; a bare non-zero exit fails', () => {
  const crash = [`${CRASH_LINE} x`];
  const files = Object.fromEntries([1, 2, 3, 4].map((index) => [`tests/f${index}.test.mjs`, file([failure('a', crash)])]));
  const capped = judgeRun({ code: 1, report: { files } }, 3);
  assert.deepEqual(capped.rerun, []);
  assert.equal(capped.crashed.length, 4);
  assert.match(capped.problems.join(), /4 files failed after a browser crash; a suite reruns at most 3/);
  assert.deepEqual(judgeRun({ code: 1, report: { files } }, 4).rerun.length, 4);
  assert.deepEqual(judgeRun({ code: 1, report: { files: { 'tests/a.test.mjs': file([]) } } }, 3).problems,
    ['node --test exited with 1 without reporting a failed test']);
  assert.equal(rerunCap(undefined), 0);
  assert.equal(rerunCap(''), 0);
  assert.equal(rerunCap('0'), 0);
  assert.throws(() => rerunCap('three'), /AXO_BROWSER_CRASH_RERUNS=three/);
  assert.equal(rerunPassed({ code: 0, report: { files: { 'tests/a.test.mjs': file([]) } } }, ['tests/a.test.mjs']), true);
  assert.equal(rerunPassed({ code: 0, report: { files: {} } }, ['tests/a.test.mjs']), false);
  assert.equal(rerunPassed({ code: 1, report: { files: { 'tests/a.test.mjs': file([]) } } }, ['tests/a.test.mjs']), false);
});

test('a page the browser never finishes creating fails as a browser crash, and is closed if it comes', async () => {
  const stalls = [];
  let finish;
  let closed = false;
  const create = guardCreation('context.newPage()', 'webkit',
    () => new Promise((resolve) => { finish = resolve; }), { timeoutMs: 50, onStall: (what) => stalls.push(what) });
  await assert.rejects(create(), /^Error: context\.newPage\(\): the webkit browser did not create it in 0\.05 s$/);
  assert.deepEqual(stalls, ['context.newPage() had not returned after 0.05 s: the webkit browser never finished '
    + 'creating it, as when its web process ends while a page is being created']);
  finish({ close: async () => { closed = true; } });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(closed, true);
  const quick = guardCreation('context.newPage()', 'webkit', async (value) => value,
    { timeoutMs: 50, onStall: (what) => stalls.push(what) });
  assert.equal(await quick('page'), 'page');
  await new Promise((resolve) => setTimeout(resolve, 80));
  assert.equal(stalls.length, 1);
});

// `node --test` on files written into a temporary folder of this package (so
// 'playwright' and the engine switch resolve as for the tests), with
// support/crash-report.mjs's report.
async function runFixtures(sources) {
  const directory = await mkdtemp(path.join(TEST_ROOT, '.crash-fixture-'));
  try {
    const files = [];
    for (const [name, source] of Object.entries(sources)) {
      const file_ = path.join(directory, name);
      await writeFile(file_, source);
      files.push(file_);
    }
    const reportFile = path.join(directory, 'report.json');
    const env = { ...process.env };
    delete env.NODE_TEST_CONTEXT;
    const { code, output } = await new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [
        '--test', '--test-force-exit', '--test-concurrency=1',
        '--test-reporter=tap', '--test-reporter-destination=stdout',
        `--test-reporter=${REPORTER}`, `--test-reporter-destination=${reportFile}`,
        // Relative, as run-tests.mjs names them.
        ...files.map((file_) => path.relative(TEST_ROOT, file_)),
      ], { cwd: TEST_ROOT, env, stdio: ['ignore', 'pipe', 'pipe'] });
      let output_ = '';
      child.stdout.on('data', (chunk) => { output_ += chunk; });
      child.stderr.on('data', (chunk) => { output_ += chunk; });
      child.once('error', reject);
      child.once('exit', (status) => resolve({ code: status, output: output_ }));
    });
    const report = JSON.parse(await readFile(reportFile, 'utf8'));
    const byName = Object.fromEntries(Object.entries(report.files)
      .map(([name, entry]) => [path.basename(name), entry]));
    return { code, output, files: byName };
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

const SUPPORT = pathToFileURL(path.join(TEST_ROOT, 'support', 'daemon.mjs')).href;

test('the report keeps each browser crash line with the test it was printed under', async () => {
  const { code, files } = await runFixtures({
    'lines.test.mjs': `
      import { afterEach, test } from 'node:test';
      afterEach((t) => { if (t.name.includes('crash')) t.diagnostic('${CRASH_LINE} under ' + t.name); });
      // A file's output is reported under the name it was given, not its path.
      test('passes', () => { console.log('to stdout'); console.error('to stderr'); });
      test('fails after a crash', () => { throw new Error('Target page, context or browser has been closed'); });
      test('fails alone', () => { throw new Error('assertion'); });
      test('parent', async (t) => {
        await t.test('child with a crash', () => { throw new Error('closed'); });
      });
    `,
  });
  assert.equal(code, 1);
  const entry = files['lines.test.mjs'];
  assert.deepEqual(entry.failures.map(({ name, nesting, derived, crash }) => [name, nesting, derived, crash]), [
    ['fails after a crash', 0, false, [`${CRASH_LINE} under fails after a crash`]],
    ['fails alone', 0, false, []],
    ['child with a crash', 1, false, [`${CRASH_LINE} under child with a crash`]],
    ['parent', 0, true, []],
  ]);
  assert.equal(entry.failures[0].error, 'Target page, context or browser has been closed');
  assert.deepEqual([entry.tests, entry.passed, entry.failed], [5, 1, 4]);
});

// Every process below `pid`, deepest first.
function descendants(pid) {
  return new Promise((resolve, reject) => {
    execFile('ps', ['-A', '-o', 'pid=,ppid='], (error, stdout) => {
      if (error) return reject(error);
      const children = new Map();
      for (const line of stdout.trim().split('\n')) {
        const [child, parent] = line.trim().split(/\s+/).map(Number);
        if (!children.has(parent)) children.set(parent, []);
        children.get(parent).push(child);
      }
      const found = [];
      const visit = (parent) => {
        for (const child of children.get(parent) || []) {
          visit(child);
          found.push(child);
        }
      };
      visit(pid);
      resolve(found);
    });
  });
}

test('a lost sign-in and a browser that died are printed under their tests, and a plain failure is not', async () => {
  const { code, files, output } = await runFixtures({
    'signin.test.mjs': `
      import assert from 'node:assert/strict';
      import { after, before, test } from 'node:test';
      import { chromium } from 'playwright';
      import { launchTestDaemon, newAuthorizedContext, resolveChromiumExecutable } from '${SUPPORT}';
      let runtime;
      let browser;
      before(async () => {
        runtime = await launchTestDaemon();
        const executablePath = await resolveChromiumExecutable();
        browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
      });
      after(async () => { await browser?.close(); await runtime?.stop(); });
      const origin = () => runtime.baseUrl;
      const read = (page) => page.evaluate(async () => (await fetch('/api/workspaces')).status);
      test('a context that loses its sign-in cookie', async () => {
        const context = await newAuthorizedContext(browser);
        const page = await context.newPage();
        await page.goto(origin() + '/api/workspaces');
        assert.equal(await read(page), 200);
        // What a network process crash does: the cookies go, and the journey
        // did not clear them (the harness only sees clearCookies it is asked for).
        await Object.getPrototypeOf(context).clearCookies.call(context);
        // The 401 starts the check; the test fails and ends at once.
        assert.equal(await read(page), 200);
      });
      test('the next test, signed in again, clears its cookies itself', async () => {
        const context = await newAuthorizedContext(browser);
        const page = await context.newPage();
        await page.goto(origin() + '/api/workspaces');
        assert.equal(await read(page), 200);
        await context.clearCookies();
        assert.equal(await read(page), 401);
        await context.close();
      });
    `,
    'killed.test.mjs': `
      import assert from 'node:assert/strict';
      import { execFile } from 'node:child_process';
      import { after, before, test } from 'node:test';
      import { chromium } from 'playwright';
      import { resolveChromiumExecutable } from '${SUPPORT}';
      let browser;
      before(async () => {
        const executablePath = await resolveChromiumExecutable();
        browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
      });
      after(async () => { await browser?.close(); });
      ${descendants.toString()}
      test('a browser whose process is killed', async () => {
        const page = await browser.newPage();
        await page.setContent('<title>alive</title>');
        for (const pid of await descendants(process.pid)) {
          try { process.kill(pid, 'SIGKILL'); } catch {}
        }
        await new Promise((resolve) => browser.isConnected() ? browser.once('disconnected', resolve) : resolve());
        await page.title();
      });
      test('a later test on that browser', async () => {
        await browser.newPage();
      });
      test('a plain failure', () => {
        assert.equal('Sign in', 'Workbench');
      });
    `,
  });
  assert.equal(code, 1, output);
  const signIn = files['signin.test.mjs'];
  assert.deepEqual(signIn.failures.map(({ name }) => name), ['a context that loses its sign-in cookie'], output);
  assert.equal(signIn.failures[0].crash.length, 1, output);
  assert.match(signIn.failures[0].crash[0],
    new RegExp(`^browser crash: \\S+ GET /api/workspaces answered 401, a ${ENGINE} context no longer held `
      + 'the sign-in cookie axocoatl-token-\\d+ for 127\\.0\\.0\\.1'));
  assert.equal(signIn.crashLinesOnPassedTests, 0, output);

  const killed = files['killed.test.mjs'];
  assert.deepEqual(killed.failures.map(({ name, crash }) => [name, crash.length > 0]), [
    ['a browser whose process is killed', true],
    ['a later test on that browser', true],
    ['a plain failure', false],
  ], output);
  assert.match(killed.failures[0].crash.join('\n'), new RegExp(`the ${ENGINE} browser disconnected although the journey did not close it`));
  assert.match(killed.failures[0].crash.join('\n'), /the test failed with "[^"]*Target page, context or browser has been closed/);
  assert.match(killed.failures[1].crash.join('\n'), /on the \w+ browser that disconnected at \S+, earlier in this file/);

  const judged = judgeRun({ code, report: { files: {
    'tests/signin.test.mjs': signIn, 'tests/killed.test.mjs': killed,
  } } }, 3);
  assert.deepEqual(judged.rerun.map(({ file: name }) => name), ['tests/signin.test.mjs']);
  assert.deepEqual(judged.failed.map(({ file: name, failures }) => [name, failures.map(({ name: test_ }) => test_)]),
    [['tests/killed.test.mjs', ['a plain failure']]]);
});
