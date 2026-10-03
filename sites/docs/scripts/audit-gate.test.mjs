// Tests for audit-gate.mjs: node --test scripts/audit-gate.test.mjs
//
// The fixtures in audit-gate-fixtures/ were recorded on 2026-10-02 with npm
// 10.9.8 from copies of this site's package.json and package-lock.json:
//   audit-*.json      `npm audit --json --package-lock-only` output
//     covered         the locked tree as committed
//     new-path        plus `make-fetch-happen` (a second dependent)
//     direct          plus `http-cache-semantics@4.2.0` as a direct dependency
//     alias           plus `hcs@npm:http-cache-semantics@4.2.0`
//     unknown-high    plus `lodash@4.17.20`
//     critical        plus `minimist@1.2.5`
//     moderate        plus `postcss@8.5.20`
//     registry-error  the committed tree audited against an unreachable registry
//     workspace       a separate project, not trimmed: root `docs` with
//                     `workspaces: ["tools/*"]` and no dependencies, and the
//                     workspace `tools/tool` depending on
//                     `http-cache-semantics@4.2.0`
//   lock-*.json       the matching package-lock.json, trimmed to the root and
//                     the packages that can reach http-cache-semantics
//   dist-tags-*.json  `npm view http-cache-semantics dist-tags --json`
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  advisoryIdFromUrl,
  collectAdvisories,
  directDependents,
  EXCEPTIONS_FILE,
  GateError,
  lockGraph,
  parseRange,
  pathAvoiding,
  runGate,
  satisfies,
  untraced,
  utcToday,
  validateExceptions,
} from './audit-gate.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtureDir = path.join(here, 'audit-gate-fixtures');
const fixture = (name) => fs.readFileSync(path.join(fixtureDir, name), 'utf8');
const NAME = 'GHSA-ch52-4w7c-c8xp http-cache-semantics (high)';

// A recorded fixture, changed by `change` and serialized again.
function changed(name, change) {
  const value = JSON.parse(fixture(name));
  change(value);
  return JSON.stringify(value);
}

const EXCEPTION = Object.freeze({
  advisory: 'GHSA-ch52-4w7c-c8xp',
  package: 'http-cache-semantics',
  vulnerable: '<=4.2.0',
  dependents: ['astro'],
  scope: 'build',
  reason: 'astro uses it only to compute cache lifetimes of remote images during a build.',
  reviewed: '2026-10-02',
  expires: '2026-11-01',
});

function gate({
  audit = 'audit-covered.json',
  auditStatus = 1,
  auditStdout,
  lock = 'lock-covered.json',
  lockText,
  latest,
  viewStatus = 0,
  exceptions = [EXCEPTION],
  exceptionsText,
  today = '2026-10-02',
  packageLockOnly = false,
} = {}) {
  const calls = [];
  const lines = [];
  const npm = (args) => {
    calls.push(args);
    if (args[0] === 'audit') {
      return { status: auditStatus, stdout: auditStdout ?? fixture(audit), stderr: '' };
    }
    if (args[0] === 'view' && args[1] === 'http-cache-semantics') {
      const tags = JSON.parse(fixture('dist-tags-http-cache-semantics.json'));
      if (latest !== undefined) tags.latest = latest;
      return viewStatus === 0
        ? { status: 0, stdout: JSON.stringify(tags), stderr: '' }
        : { status: viewStatus, stdout: '{"error":{"code":"E404"}}', stderr: 'npm error 404' };
    }
    throw new Error(`unexpected npm ${args.join(' ')}`);
  };
  const read = (name) => {
    if (name === EXCEPTIONS_FILE) return exceptionsText ?? JSON.stringify(exceptions);
    if (name === 'package-lock.json') return lockText ?? fixture(lock);
    throw new Error(`unexpected read of ${name}`);
  };
  const result = runGate({
    dir: '/nonexistent',
    packageLockOnly,
    today,
    npm,
    read,
    log: (line) => lines.push(line),
  });
  return { ...result, calls, output: lines.join('\n'), failureText: result.failures.join('\n') };
}

test('a covered advisory passes and prints the exception with its expiry', () => {
  const result = gate();
  assert.equal(result.ok, true, result.output);
  assert.deepEqual(result.failures, []);
  assert.match(result.output, /exception applied: GHSA-ch52-4w7c-c8xp http-cache-semantics <=4\.2\.0 via astro \(scope build, reviewed 2026-10-02, expires 2026-11-01\)/);
  assert.match(result.output, /audit-gate: PASS$/);
  assert.deepEqual(result.calls, [
    ['audit', '--json'],
    ['view', 'http-cache-semantics', 'dist-tags', '--json'],
  ]);
});

test('--package-lock-only is passed to npm audit', () => {
  const result = gate({ packageLockOnly: true });
  assert.equal(result.ok, true, result.output);
  assert.deepEqual(result.calls[0], ['audit', '--json', '--package-lock-only']);
});

test('an exception still holds on its expiry day (UTC)', () => {
  assert.equal(gate({ today: '2026-11-01' }).ok, true);
});

test('an expired exception fails', () => {
  const result = gate({ today: '2026-11-02' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /GHSA-ch52-4w7c-c8xp http-cache-semantics \(high\): the exception expired on 2026-11-01/);
  assert.doesNotMatch(result.output, /exception applied/);
});

test('an unknown high advisory fails', () => {
  const result = gate({ audit: 'audit-unknown-high.json' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /GHSA-35jh-r3h4-6jhm lodash \(high\): no exception in audit-exceptions\.json/);
  assert.match(result.failureText, /GHSA-r5fr-rjxr-66jc lodash \(high\): no exception/);
  assert.doesNotMatch(result.failureText, /GHSA-29mw-wpgm-hmr9|GHSA-f23m-r3pf-42rh|GHSA-xxjr-mmjv-4gpg/);
  assert.equal(result.failures.length, 2);
  assert.match(result.output, /exception applied: GHSA-ch52-4w7c-c8xp/);
});

test('an unknown critical advisory fails', () => {
  const result = gate({ audit: 'audit-critical.json' });
  assert.equal(result.ok, false);
  assert.deepEqual(result.failures.length, 1);
  assert.match(result.failureText, /GHSA-xvch-5gv4-984h minimist \(critical\): no exception/);
});

test('a published fix voids the exception', () => {
  const result = gate({ latest: '4.2.1' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /a fixed version exists: latest http-cache-semantics is 4\.2\.1, outside the advisory range "<=4\.2\.0"; upgrade instead/);
});

test('a new release outside the recorded range needs a new review', () => {
  const result = gate({ exceptions: [{ ...EXCEPTION, vulnerable: '<=4.1.1' }] });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /latest http-cache-semantics is 4\.2\.0, outside the reviewed range "<=4\.1\.1"; review the advisory again/);
  assert.doesNotMatch(result.failureText, /a fixed version exists/);
});

test('a latest version that cannot be read fails', () => {
  const result = gate({ viewStatus: 1 });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /could not read the latest published version of http-cache-semantics/);
});

test('a new dependent path fails', () => {
  const result = gate({ audit: 'audit-new-path.json', lock: 'lock-new-path.json' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /dependency path docs > make-fetch-happen > http-cache-semantics does not go through astro/);
  assert.match(result.failureText, /make-fetch-happen depends on http-cache-semantics directly but is not in dependents/);
});

test('the new path passes once its dependent is reviewed and listed', () => {
  const result = gate({
    audit: 'audit-new-path.json',
    lock: 'lock-new-path.json',
    exceptions: [{ ...EXCEPTION, dependents: ['astro', 'make-fetch-happen'] }],
  });
  assert.equal(result.ok, true, result.output);
});

test('a direct dependency on the package fails', () => {
  const result = gate({ audit: 'audit-direct.json', lock: 'lock-direct.json' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /dependency path docs > http-cache-semantics does not go through astro/);
  assert.match(result.failureText, /docs depends on http-cache-semantics directly but is not in dependents/);
});

test('an aliased install of the package fails', () => {
  const result = gate({ audit: 'audit-alias.json', lock: 'lock-alias.json' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /dependency path docs > hcs \(npm:http-cache-semantics\) does not go through astro/);
  assert.match(result.failureText, /docs depends on http-cache-semantics directly but is not in dependents/);
});

test('a new dependent inside a listed dependent\'s dependencies fails', () => {
  // astro > make-fetch-happen > http-cache-semantics: every path still goes
  // through astro, but make-fetch-happen was never reviewed.
  const lockText = changed('lock-covered.json', ({ packages }) => {
    const astro = packages['node_modules/astro'];
    delete astro.dependencies['http-cache-semantics'];
    astro.dependencies['make-fetch-happen'] = '^16.0.1';
    packages['node_modules/make-fetch-happen'] = JSON.parse(fixture('lock-new-path.json'))
      .packages['node_modules/make-fetch-happen'];
  });
  const result = gate({ lockText });
  assert.equal(result.ok, false);
  assert.deepEqual(result.failures, [
    `${NAME}: make-fetch-happen depends on http-cache-semantics directly but is not in dependents`,
  ]);
  const listed = gate({ lockText, exceptions: [{ ...EXCEPTION, dependents: ['astro', 'make-fetch-happen'] }] });
  assert.equal(listed.ok, true, listed.output);
});

test('a workspace that depends on the package fails', () => {
  // npm lists workspaces under the root's `workspaces`, not its dependencies.
  const result = gate({ audit: 'audit-workspace.json', lock: 'lock-workspace.json' });
  assert.equal(result.ok, false);
  assert.deepEqual(result.failures, [
    `${NAME}: dependency path tool > http-cache-semantics does not go through astro`,
    `${NAME}: tool depends on http-cache-semantics directly but is not in dependents`,
  ]);
  assert.doesNotMatch(result.output, /exception applied/);
});

test('an installed copy that no dependency path reaches fails closed', () => {
  const stale = 'node_modules/astro-expressive-code/node_modules/http-cache-semantics';
  const lockText = changed('lock-covered.json', ({ packages }) => {
    packages[stale] = { version: '4.1.1', extraneous: true };
  });
  const auditStdout = changed('audit-covered.json', ({ vulnerabilities }) => {
    vulnerabilities['http-cache-semantics'].nodes.push(stale);
  });
  const result = gate({ lockText, auditStdout });
  assert.equal(result.ok, false);
  assert.deepEqual(result.failures, [
    `${NAME}: cannot trace ${stale} to the root, a workspace or a linked folder in package-lock.json`,
  ]);
  const unreported = gate({ lockText });
  assert.equal(unreported.ok, false);
  assert.match(unreported.failureText, /cannot trace node_modules\/astro-expressive-code\/node_modules\/http-cache-semantics/);
});

test('a missing package-lock.json fails closed', () => {
  const result = gate({ lock: 'missing-lock.json' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /cannot read package-lock\.json: ENOENT/);
});

test('moderate advisories are reported but not gated', () => {
  const result = gate({ audit: 'audit-moderate.json' });
  assert.equal(result.ok, true, result.output);
  assert.match(result.output, /not gated: GHSA-fxqj-rqcc-2cmp postcss \(moderate\)/);
  assert.match(result.output, /1 high or critical advisory, 1 below the gate level/);
});

test('an npm audit error fails closed', () => {
  const result = gate({ audit: 'audit-registry-error.json' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /npm audit failed: request to http:\/\/127\.0\.0\.1:1\/-\/npm\/v1\/security\/audits\/quick failed/);
});

test('output that is not JSON fails closed', () => {
  const result = gate({ auditStdout: 'npm ERR! something' });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /could not parse JSON from npm audit --json/);
});

test('an unexpected npm audit exit status fails closed', () => {
  const result = gate({ auditStatus: 254 });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /exited with status 254/);
});

test('an exception for another package does not cover the advisory', () => {
  const result = gate({ exceptions: [{ ...EXCEPTION, package: 'astro', dependents: ['@astrojs/starlight'] }] });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /the exception for GHSA-ch52-4w7c-c8xp names astro, not http-cache-semantics/);
  assert.deepEqual(result.warnings, []);
});

test('an advisory without a GitHub advisory URL cannot be covered', () => {
  const report = JSON.parse(fixture('audit-covered.json'));
  report.vulnerabilities['http-cache-semantics'].via[0].url = 'https://example.com/advisories/GHSA-ch52-4w7c-c8xp';
  const result = gate({ auditStdout: JSON.stringify(report) });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /https:\/\/example\.com\/advisories\/GHSA-ch52-4w7c-c8xp http-cache-semantics \(high\): no exception/);
});

test('an invalid exceptions file fails the gate', () => {
  const result = gate({ exceptions: [{ ...EXCEPTION, expires: '2027-06-01' }] });
  assert.equal(result.ok, false);
  assert.match(result.failureText, /more than 90 days after reviewed/);
  const broken = gate({ exceptionsText: '{not json' });
  assert.equal(broken.ok, false);
  assert.match(broken.failureText, /could not parse JSON from audit-exceptions\.json/);
});

test('an exception that matches no advisory is reported for removal', () => {
  const result = gate({ auditStdout: JSON.stringify({ auditReportVersion: 2, vulnerabilities: {}, metadata: {} }) });
  assert.equal(result.ok, true);
  assert.match(result.warnings.join('\n'), /exception GHSA-ch52-4w7c-c8xp http-cache-semantics matched no high or critical advisory/);
});

test('a high entry that no advisory explains fails closed', () => {
  const report = {
    auditReportVersion: 2,
    vulnerabilities: {
      astro: { name: 'astro', severity: 'high', via: ['missing-package'], effects: [], nodes: ['node_modules/astro'] },
    },
  };
  assert.throws(() => collectAdvisories(report), (error) => error instanceof GateError
    && /no high or critical advisory explains it/.test(error.message));
});

test('the transitive entries in a recorded report resolve to one advisory', () => {
  const advisories = collectAdvisories(JSON.parse(fixture('audit-covered.json')));
  assert.equal(advisories.length, 1);
  assert.equal(advisories[0].id, 'GHSA-ch52-4w7c-c8xp');
  assert.deepEqual([...advisories[0].nodes], ['node_modules/http-cache-semantics']);
});

test('linked folders are followed, and a link to an unlisted folder fails closed', () => {
  const lock = {
    packages: {
      '': { name: 'docs', dependencies: { tool: 'file:tools/tool', astro: '^7.3.2' } },
      'node_modules/astro': { version: '7.3.2', dependencies: { 'http-cache-semantics': '^4.2.0' } },
      'node_modules/tool': { resolved: 'tools/tool', link: true },
      'tools/tool': { name: 'tool', dependencies: { 'http-cache-semantics': '^4.2.0' } },
      'node_modules/http-cache-semantics': { version: '4.2.0' },
    },
  };
  const graph = lockGraph(lock);
  const targets = new Set(graph.locationsOf('http-cache-semantics'));
  assert.deepEqual(pathAvoiding(graph, targets, ['astro']), ['docs', 'tool', 'http-cache-semantics']);
  assert.equal(pathAvoiding(graph, targets, ['astro', 'tool']), null);
  delete lock.packages['tools/tool'];
  assert.throws(() => lockGraph(lock).edges(''), /links node_modules\/tool to a folder it does not list/);
});

test('workspaces are searched although the root does not depend on them', () => {
  const graph = lockGraph(JSON.parse(fixture('lock-workspace.json')));
  assert.deepEqual(graph.starts, ['', 'tools/tool']);
  assert.equal(graph.nameOf('tools/tool'), 'tool');
  const targets = new Set(graph.locationsOf('http-cache-semantics'));
  assert.deepEqual([...targets], ['node_modules/http-cache-semantics']);
  assert.deepEqual(pathAvoiding(graph, targets, ['astro']), ['tool', 'http-cache-semantics']);
  assert.equal(pathAvoiding(graph, targets, ['tool']), null);
  assert.deepEqual(untraced(graph, targets), []);
  assert.deepEqual(directDependents(graph, targets), ['tools/tool']);
  const scoped = lockGraph({ packages: { '': { name: 'docs' }, 'packages/@acme/tool': { version: '1.0.0' } } });
  assert.equal(scoped.nameOf('packages/@acme/tool'), '@acme/tool');
});

test('exception validation', () => {
  const check = (entry, today = '2026-10-02') => validateExceptions([entry], today).errors.join('\n');
  assert.equal(check(EXCEPTION), '');
  assert.match(check({ ...EXCEPTION, note: 'x' }), /unknown field "note"/);
  const { scope: _scope, ...withoutScope } = EXCEPTION;
  assert.match(check(withoutScope), /missing field "scope"/);
  assert.match(check({ ...EXCEPTION, advisory: 'GHSA-CH52-4W7C-C8XP' }), /advisory must be a lowercase GitHub advisory id/);
  assert.match(check({ ...EXCEPTION, advisory: 'CVE-2026-93748' }), /advisory must be/);
  assert.match(check({ ...EXCEPTION, vulnerable: '^4.2.0' }), /vulnerable must be a range/);
  assert.match(check({ ...EXCEPTION, dependents: [] }), /dependents must be/);
  assert.match(check({ ...EXCEPTION, dependents: ['http-cache-semantics'] }), /dependents must be/);
  assert.match(check({ ...EXCEPTION, dependents: ['astro', 'astro'] }), /dependents must be/);
  assert.match(check({ ...EXCEPTION, scope: 'runtime' }), /scope must be one of build/);
  assert.match(check({ ...EXCEPTION, reason: 'not used' }), /reason must explain/);
  assert.match(check({ ...EXCEPTION, expires: '2026-02-30' }), /expires must be a date/);
  assert.match(check({ ...EXCEPTION, expires: '2026-10-01' }), /expires \(2026-10-01\) is before reviewed/);
  assert.equal(check({ ...EXCEPTION, expires: '2026-12-31' }), '');
  assert.match(check({ ...EXCEPTION, expires: '2027-01-01' }), /more than 90 days after reviewed/);
  assert.match(check(EXCEPTION, '2026-10-01'), /reviewed \(2026-10-02\) is after today/);
  assert.match(validateExceptions([EXCEPTION, EXCEPTION], '2026-10-02').errors.join('\n'), /listed more than once for http-cache-semantics/);
  assert.deepEqual(validateExceptions([EXCEPTION, { ...EXCEPTION, package: 'other-package', dependents: ['astro'] }], '2026-10-02').errors, []);
  assert.match(validateExceptions({}, '2026-10-02').errors.join('\n'), /must hold a JSON array/);
});

test('the committed exceptions file is valid', () => {
  const committed = JSON.parse(fs.readFileSync(path.join(here, '..', EXCEPTIONS_FILE), 'utf8'));
  assert.deepEqual(validateExceptions(committed, utcToday()).errors, []);
});

test('advisory ids come only from GitHub advisory URLs', () => {
  assert.equal(advisoryIdFromUrl('https://github.com/advisories/GHSA-ch52-4w7c-c8xp'), 'GHSA-ch52-4w7c-c8xp');
  assert.equal(advisoryIdFromUrl('https://github.com/advisories/GHSA-CH52-4W7C-C8XP'), 'GHSA-ch52-4w7c-c8xp');
  assert.equal(advisoryIdFromUrl('https://github.com/advisories/GHSA-ch52-4w7c-c8xp/x'), null);
  assert.equal(advisoryIdFromUrl('http://github.com/advisories/GHSA-ch52-4w7c-c8xp'), null);
  assert.equal(advisoryIdFromUrl('https://github.com.example/advisories/GHSA-ch52-4w7c-c8xp'), null);
  assert.equal(advisoryIdFromUrl(undefined), null);
});

test('version ranges', () => {
  assert.equal(satisfies('4.2.0', '<=4.2.0'), true);
  assert.equal(satisfies('4.2.0-beta.2', '<=4.2.0'), true);
  assert.equal(satisfies('4.2.1', '<=4.2.0'), false);
  assert.equal(satisfies('4.2.1-beta.1', '<=4.2.0'), false);
  assert.equal(satisfies('4.17.20', '>=4.0.0 <4.17.21'), true);
  assert.equal(satisfies('4.17.21', '>=4.0.0 <4.17.21'), false);
  assert.equal(satisfies('1.2.5', '>= 1.0.0, < 1.2.6'), true);
  assert.equal(satisfies('0.9.0', '>= 1.0.0, < 1.2.6'), false);
  const astroRange = '<=0.0.0-head-body-content-20240329190922 || >=2.10.10';
  assert.equal(satisfies('7.3.2', astroRange), true);
  assert.equal(satisfies('2.10.9', astroRange), false);
  assert.equal(satisfies('1.0.0-beta.10', '>1.0.0-beta.9'), true);
  assert.equal(satisfies('1.0.0-alpha', '<1.0.0-alpha.1'), true);
  assert.equal(satisfies('3.0.0', '*'), true);
  assert.equal(satisfies('3.0.0', '=3.0.0'), true);
  assert.equal(parseRange('^4.2.0'), null);
  assert.equal(parseRange('<=4.2'), null);
  assert.equal(parseRange('1.0.0 - 2.0.0'), null);
  assert.equal(parseRange(''), null);
  assert.equal(satisfies('4.2', '<=4.2.0'), null);
});

test('the command line refuses unknown arguments', () => {
  const run = spawnSync(process.execPath, [path.join(here, 'audit-gate.mjs'), '--audit-level=moderate'], { encoding: 'utf8' });
  assert.equal(run.status, 2);
  assert.match(run.stderr, /Usage: node scripts\/audit-gate\.mjs \[--package-lock-only\]/);
});
