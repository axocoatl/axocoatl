// Unit tests for the check runner's pure helpers.
// Run: node --test crates/axocoatl-tools/assets/browser/check.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  boundText, checkInput, checkRelativePath, configSource, escapeRegExp, relativize, stripAnsi, summarize,
} from './check.mjs';

const input = (fields = {}) => ({
  schema: 'axocoatl.browser-check-input/1',
  entry: 'qa/findings/B07.spec.ts',
  files: [{ path: 'qa/findings/B07.spec.ts', content: "import { test } from '@playwright/test';" }],
  base_url: 'http://localhost:8765',
  ...fields,
});

test('files must be safe repository paths', () => {
  assert.equal(checkRelativePath('qa/findings/B07.spec.ts'), null);
  for (const path of ['', '/abs.spec.ts', '../x.spec.ts', 'qa/../x.ts', 'qa/./x.ts', 'qa//x.ts', 'qa/.env',
    'node_modules/@playwright/test/index.js', 'qa\\x.ts', 'qa/x y.ts']) {
    assert.notEqual(checkRelativePath(path), null, path);
  }
  assert.equal(checkInput(input()), null);
  assert.match(checkInput(input({ entry: 'qa/notes.md' })), /entry must be/);
  assert.match(checkInput(input({ entry: 'qa/other.spec.ts' })), /entry must be one of files/);
  assert.match(checkInput(input({ files: [] })), /1-32 files/);
  assert.match(checkInput(input({ base_url: 'file:///tmp' })), /http or https/);
  assert.match(checkInput(input({ grep: '' })), /grep/);
  const twice = input().files[0];
  assert.match(checkInput(input({ files: [twice, twice] })), /listed twice/);
});

test('the configuration runs exactly the entry with one worker and keeps the credential out of files', () => {
  const source = configSource({
    testDir: '/tmp/axo-check-1/work', entry: 'qa/findings/B07.spec.ts', baseURL: 'http://localhost:8765',
    testTimeoutMs: 30000, globalTimeoutMs: 100000, reportFile: '/tmp/axo-check-1/report.json', outputDir: '/tmp/axo-check-1/results',
  });
  assert.match(source, /retries: 0/);
  assert.match(source, /workers: 1/);
  assert.match(source, /process\.env\.AXO_PROXY_PASSWORD/);
  assert.doesNotMatch(source, /axe_/);
  const matcher = JSON.parse(/new RegExp\(("[^"]+")\)/.exec(source)[1]);
  const pattern = new RegExp(matcher);
  assert.ok(pattern.test('/tmp/axo-check-1/work/qa/findings/B07.spec.ts'));
  assert.ok(!pattern.test('/tmp/axo-check-1/work/qa/findings/B07xspec.ts'));
  assert.ok(!pattern.test('/tmp/axo-check-1/work/qa/findings/B07.spec.ts.bak'));
  assert.equal(escapeRegExp('a.b+c'), 'a\\.b\\+c');
});

test('a Playwright JSON report is summarized with repository paths', () => {
  const root = '/tmp/axo-check-1/work';
  const report = {
    suites: [{
      title: 'qa/findings/B07.spec.ts', file: 'qa/findings/B07.spec.ts',
      specs: [{
        title: 'a coupon never makes the total negative', file: 'qa/findings/B07.spec.ts', line: 3,
        tests: [{ results: [{
          status: 'failed', duration: 5127,
          error: {
            message: '\u001b[31mError: expect(locator).not.toContainText(expected) failed\u001b[39m',
            location: { file: `${root}/qa/findings/B07.spec.ts`, line: 6, column: 47 },
            snippet: `> 6 | await expect(...)\n    at ${root}/qa/findings/B07.spec.ts:6:47`,
          },
          attachments: [{ name: 'screenshot', contentType: 'image/png', path: '/tmp/axo-check-1/results/x/test-failed-1.png' }],
        }] }],
      }],
      suites: [{
        title: 'cart', file: 'qa/findings/B07.spec.ts',
        specs: [{ title: 'shows ORD-2051', file: 'qa/findings/B07.spec.ts', line: 9, tests: [{ results: [{ status: 'passed', duration: 66 }] }] }],
      }],
    }],
    errors: [],
  };
  const summary = summarize(report, root);
  assert.equal(summary.status, 'failed');
  assert.deepEqual(summary.counts, { total: 2, passed: 1, failed: 1, timed_out: 0, skipped: 0, interrupted: 0 });
  assert.equal(summary.tests[0].error.location.file, 'qa/findings/B07.spec.ts');
  assert.equal(summary.tests[0].error.message, 'Error: expect(locator).not.toContainText(expected) failed');
  assert.ok(!summary.tests[0].error.snippet.includes(root));
  assert.equal(summary.tests[1].title, 'cart > shows ORD-2051');
  assert.equal(summary.screenshot, '/tmp/axo-check-1/results/x/test-failed-1.png');

  assert.equal(summarize({ suites: [], errors: [{ message: 'SyntaxError: Unexpected token' }] }, root).status, 'error');
  assert.equal(summarize({ suites: [], errors: [] }, root).status, 'no_tests');
  assert.equal(summarize(null, root).status, 'no_tests');
  const passing = { suites: [{ title: 'a.spec.ts', file: 'a.spec.ts', specs: [{ title: 't', file: 'a.spec.ts', line: 1, tests: [{ results: [{ status: 'passed', duration: 1 }] }] }] }] };
  assert.equal(summarize(passing, root).status, 'passed');
});

test('logs are cut from the start and stripped of colour', () => {
  assert.equal(stripAnsi('\u001b[2mdim\u001b[22m'), 'dim');
  assert.deepEqual(boundText('abcdef', 3, true), { text: 'def', truncated: true });
  assert.deepEqual(boundText('abcdef', 3), { text: 'abc', truncated: true });
  assert.equal(relativize('/r/w/qa/a.ts:1', '/r/w'), 'qa/a.ts:1');
});
