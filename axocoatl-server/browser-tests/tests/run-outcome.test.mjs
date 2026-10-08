import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { launchTestDaemon, newAuthorizedContext, resolveChromiumExecutable } from '../support/daemon.mjs';

let runtime, browser;
before(async () => {
  runtime = process.env.AXOCOATL_COMPONENT_BASE_URL
    ? { baseUrl: process.env.AXOCOATL_COMPONENT_BASE_URL, stop: async () => {} } : await launchTestDaemon();
  browser = await chromium.launch({ headless: true, executablePath: await resolveChromiumExecutable() });
});
after(async () => { await browser?.close(); await runtime?.stop(); });

const RUN = 'run-00000000-0000-4000-8000-0000000000aa';
const outcome = {
  schema: 'axocoatl.run-outcome/1', run_id: RUN, session_id: 'ses-1', workspace_id: 'wsp-1',
  loadout: { id: 'qa', version: 1, kind: 'qa', digest: 'c'.repeat(64), builtin: true },
  task: 'Explore checkout', started_at_ms: 1, finished_at_ms: 2,
  verdict: 'needs_attention', exit_code: 2,
  attention: ['3 areas were not covered', 'The writer did not answer 1 review finding'],
  turns: [{ turn_id: 'turn-1', purpose: 'run', state: 'completed' }],
  checks: [{
    name: 'e2e', argv: ['sh', '-c', 'e2e run'], state: 'failed', timeout_ms: 600000, exit_code: 1,
    stdout_tail: '', stderr_tail: 'expected 10', report: {
      format: 'junit', sha256: 'b'.repeat(64), passed: 1, failed: 1, skipped: 0, errors: 0, truncated: 0,
      tests: [
        { suite: 'checkout', name: 'pays with a saved card', status: 'failed', message: 'expected 10 < 9' },
        { suite: 'checkout', name: 'shows the cart', status: 'passed' },
      ],
    },
  }],
  review: {
    reviewer: { provider: 'openrouter', model: 'qwen/qwen3-coder', runtime: 'native' }, max_rounds: 3,
    rounds: [{ round: 1, verdict: 'changes', passed: false, findings_text: 'F1 off by one\nF2 missing test', findings: [], continued: true }],
    passed: false, state: 'changes', reason: 'The reviewer asked for changes in round 1 of 3.',
  },
  adjudications: [
    { round: 1, finding_id: 'F1', finding: 'off by one', decision: 'accept', reason: 'fixed the bound' },
    { round: 1, finding_id: 'F2', finding: 'missing test', decision: 'missing', reason: '' },
  ],
  findings: [
    { id: 'B1', source: 'explorer', title: 'total ignores coupon', detail: 'coupon not applied', area: 'checkout',
      repro: { path: 'axocoatl-qa/b1.spec.ts', classification: 'confirmed' } },
    { id: 'B2', source: 'explorer', title: 'search flickers', detail: '', area: 'search',
      repro: { path: 'axocoatl-qa/b2.spec.ts', classification: 'fails_on_clean_build' } },
  ],
  not_covered: [
    { area: 'gift cards', class: 'provider_refusal', detail: 'classifier stop' },
    { area: 'checkout', class: 'not_reached', detail: 'Not reached: ran out of steps' },
    { area: 'writer', class: 'stopped', detail: '' },
  ],
  notes: ['worker-ingest listed as not reached: ingest/legacy.py; a note: the host decides coverage from the files its workers read'],
  warnings: [{ code: 'same_model_reviewer', message: 'The reviewer runs the writer’s model (openrouter:qwen/qwen3-coder).' }],
  usage: { input_tokens: 1200, output_tokens: 300, cost_microunits: 12345, complete: false, retries: 1 },
  network: { events: 9, allowed_connections: 4, refused_connections: 1, route_requests: 3, routes: [['openrouter.ai', 3]] },
};

test('the run outcome panel shows every part of an Outcome, missing adjudications in red and clean-build failures as such', async () => {
  const context = await newAuthorizedContext(browser), page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.route('**/run-outcome-fixture', (route) => route.fulfill({ contentType: 'text/html', body:
    `<!doctype html><html><head><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-run-outcome run-id="${RUN}"></ax-run-outcome><script type="module" src="/ui/run-outcome.js"></script></body></html>` }));
  await page.route(`**/api/runs/${RUN}`, (route) => route.fulfill({ json: {
    run_id: RUN, session_id: 'ses-1', loadout: 'qa@1', state: 'finished', phase: 'finishing', started_at_ms: 1, outcome,
    keep: { branch: 'axocoatl/qa-00000000', commit: 'a'.repeat(40) } } }));
  try {
    await page.goto(`${runtime.baseUrl}/run-outcome-fixture`);
    const panel = page.locator('ax-run-outcome');
    await panel.locator('.verdict[data-verdict="needs_attention"]').waitFor();
    assert.match(await panel.locator('.verdict').textContent(), /Needs attention.*exit 2/);
    // Checks with their report's test cases.
    const check = panel.locator('details[data-check="e2e"]');
    assert.match(await check.locator('summary').textContent(), /failed.*1 passed, 1 failed/);
    assert.equal(await check.locator('tbody tr').count(), 2);
    // Review rounds and adjudications; the missing one is marked.
    assert.match(await panel.locator('section[data-section="review"]').textContent(), /changes after 1 of 3 rounds/);
    const missing = panel.locator('table.adjudications tr.missing');
    assert.equal(await missing.count(), 1);
    assert.match(await missing.textContent(), /F2.*missing.*did not answer/);
    const color = await missing.locator('td').first().evaluate((cell) => getComputedStyle(cell).color);
    const okColor = await panel.locator('table.adjudications tbody tr:not(.missing) td').first().evaluate((cell) => getComputedStyle(cell).color);
    assert.notEqual(color, okColor, 'a missing adjudication is shown in the error colour');
    // Findings keep "fails on clean build" as its own label.
    assert.match(await panel.locator('li[data-finding="B1"]').textContent(), /confirmed/);
    assert.match(await panel.locator('li[data-finding="B2"]').textContent(), /fails on clean build/);
    // Not covered with its reason, the same-model warning, usage and network.
    // Each line is `NotCovered::reason` as the run summary and JUnit print
    // it: the class once, an empty detail leaving the class alone.
    assert.equal(await panel.locator('li[data-class="provider_refusal"]').textContent(), 'gift cards: provider_refusal: classifier stop');
    assert.equal(await panel.locator('li[data-class="not_reached"]').textContent(), 'checkout: not_reached: ran out of steps');
    assert.equal(await panel.locator('li[data-class="stopped"]').textContent(), 'writer: stopped');
    // The run's notes, after what was not covered.
    assert.deepEqual(await panel.locator('section[data-section="notes"] li.note').allTextContents(), outcome.notes);
    assert.equal(await panel.locator('.warning[data-code="same_model_reviewer"]').count(), 1);
    assert.match(await panel.locator('p.usage').textContent(), /known subtotal/);
    assert.match(await panel.locator('section[data-section="network"]').textContent(), /openrouter\.ai: 3/);
    // Download record and Keep as PR.
    assert.equal(await panel.locator('a.download').getAttribute('href'), `/api/runs/${RUN}/record`);
    const keep = panel.locator('ax-keep-pr');
    assert.equal(await keep.getAttribute('run-id'), RUN);
    assert.equal(await keep.getAttribute('session-id'), 'ses-1');
    assert.equal(await keep.getAttribute('loadout'), 'qa');
    // The run record's latest Keep is shown in the Keep element.
    await keep.locator('.result').filter({ hasText: 'axocoatl/qa-00000000' }).waitFor();
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('the panel renders why something was not covered exactly as NotCovered::reason does', async () => {
  const context = await newAuthorizedContext(browser), page = await context.newPage();
  await page.route('**/not-covered-reason-fixture', (route) => route.fulfill({ contentType: 'text/html', body: '<!doctype html><html><body></body></html>' }));
  try {
    await page.goto(`${runtime.baseUrl}/not-covered-reason-fixture`);
    // The cases of a_not_covered_reason_names_its_class_once in
    // crates/axocoatl-session/src/run_outcome.rs: [class, detail, reason].
    const cases = [
      ['not_reached', 'not_reached: ran out of steps', 'not_reached: ran out of steps'],
      ['not_reached', 'Not reached: no time', 'not_reached: no time'],
      ['blocked', 'blocked: the page did not load', 'blocked: the page did not load'],
      ['provider_failure', 'provider_failure: the explorer did not finish (stream ended early: x: y)',
        'provider_failure: the explorer did not finish (stream ended early: x: y)'],
      ['not_reached', 'not_reached', 'not_reached'],
      ['other', '', 'other'],
      ['other', '  ', 'other'],
      ['other', 'skipped: not a valid status', 'other: skipped: not a valid status'],
      ['provider_refusal', 'classifier stop', 'provider_refusal: classifier stop'],
      ['budget', 'budget:', 'budget'],
    ];
    const rendered = await page.evaluate(async (cases) => {
      const { notCoveredReason } = await import('/ui/run-outcome.js');
      return cases.map(([cls, detail]) => notCoveredReason({ area: 'checkout', class: cls, detail }));
    }, cases);
    assert.deepEqual(rendered, cases.map(([, , reason]) => reason));
    } finally { await context.close(); }
  });

test('the panel shows a cost the run does not know as what was reserved, never as a price', async () => {
  const context = await newAuthorizedContext(browser), page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  const codexRun = 'run-00000000-0000-4000-8000-0000000000cc';
  const codex = {
    ...outcome, run_id: codexRun, verdict: 'pass', exit_code: 0, attention: [], not_covered: [], warnings: [],
    usage: { input_tokens: 600, output_tokens: 18, cost_microunits: 333333, complete: true, cost_known: false, retries: 0 },
  };
  // A Codex writer's cost computed from its reported tokens at its model's
  // list price is shown as a computation.
  const computedRun = 'run-00000000-0000-4000-8000-0000000000cd';
  const computed = {
    ...codex, run_id: computedRun,
    usage: { input_tokens: 4107, output_tokens: 60, cost_microunits: 17835, complete: true, cost_known: true, cost_computed: true, retries: 0 },
  };
  await page.route('**/run-outcome-fixture', (route) => route.fulfill({ contentType: 'text/html', body:
    `<!doctype html><html><head><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-run-outcome run-id="${codexRun}"></ax-run-outcome><ax-run-outcome run-id="${RUN}"></ax-run-outcome><ax-run-outcome run-id="${computedRun}"></ax-run-outcome><script type="module" src="/ui/run-outcome.js"></script></body></html>` }));
  await page.route(`**/api/runs/${codexRun}`, (route) => route.fulfill({ json: {
    run_id: codexRun, session_id: 'ses-1', loadout: 'fix@1', state: 'finished', phase: 'finishing', started_at_ms: 1, outcome: codex } }));
  await page.route(`**/api/runs/${computedRun}`, (route) => route.fulfill({ json: {
    run_id: computedRun, session_id: 'ses-1', loadout: 'fix@1', state: 'finished', phase: 'finishing', started_at_ms: 1, outcome: computed } }));
  await page.route(`**/api/runs/${RUN}`, (route) => route.fulfill({ json: {
    run_id: RUN, session_id: 'ses-1', loadout: 'qa@1', state: 'finished', phase: 'finishing', started_at_ms: 1, outcome } }));
  try {
    await page.goto(`${runtime.baseUrl}/run-outcome-fixture`);
    const unknown = page.locator('ax-run-outcome').first().locator('p.usage');
    await unknown.waitFor();
    assert.equal(await unknown.textContent(), '600 input + 18 output tokens, cost unknown (reserved up to $0.3333)');
    // An Outcome without the field, and one whose cost is known, show the price.
    const known = page.locator('ax-run-outcome').nth(1).locator('p.usage');
    await known.waitFor();
    assert.match(await known.textContent(), /^1200 input \+ 300 output tokens, \$0\.0123, 1 provider retry \(known subtotal/);
    const priced = page.locator('ax-run-outcome').nth(2).locator('p.usage');
    await priced.waitFor();
    assert.equal(await priced.textContent(), '4107 input + 60 output tokens, $0.0178 (includes cost computed from reported tokens at list prices)');
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('the panel says when the run cannot be read and while it is still running', async () => {
  const context = await newAuthorizedContext(browser), page = await context.newPage();
  await page.route('**/run-outcome-fixture', (route) => route.fulfill({ contentType: 'text/html', body:
    '<!doctype html><html><body><ax-run-outcome run-id="run-missing"></ax-run-outcome><ax-run-outcome run-id="run-live"></ax-run-outcome><script type="module" src="/ui/run-outcome.js"></script></body></html>' }));
  await page.route('**/api/runs/run-missing', (route) => route.fulfill({ status: 404, json: { error: 'no run run-missing' } }));
  await page.route('**/api/runs/run-live', (route) => route.fulfill({ json: { run_id: 'run-live', session_id: 'ses-2', loadout: 'fix@1', state: 'running', phase: 'running: the team is working on the task', started_at_ms: 1 } }));
  try {
    await page.goto(`${runtime.baseUrl}/run-outcome-fixture`);
    await page.locator('ax-run-outcome').first().locator('.error').waitFor();
    assert.match(await page.locator('ax-run-outcome').first().locator('.error').textContent(), /no run run-missing/);
    await page.locator('ax-run-outcome').nth(1).locator('.verdict').waitFor();
    assert.match(await page.locator('ax-run-outcome').nth(1).locator('.verdict').textContent(), /Running.*running/);
  } finally { await context.close(); }
});
