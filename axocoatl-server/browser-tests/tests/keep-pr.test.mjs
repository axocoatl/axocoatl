import assert from 'node:assert/strict';
import { before, after, test } from 'node:test';
import { chromium } from 'playwright';
import { launchTestDaemon, resolveChromiumExecutable, newAuthorizedContext } from '../support/daemon.mjs';

// <ax-keep-pr> in a fixture page served beside the embedded UI. The daemon's
// keep-pr endpoint is answered by controlled responses: these tests cover the
// element's states and the exact requests it sends; Rust tests cover Keep.

const SESSION = 'ses-11111111-2222-4333-8444-555555555555';
const RUN = 'run-0f8c1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b';
const COMMIT = '1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b';

let runtime;
let browser;
before(async () => {
  runtime = process.env.AXOCOATL_COMPONENT_BASE_URL
    ? { baseUrl: process.env.AXOCOATL_COMPONENT_BASE_URL, stop: async () => {} }
    : await launchTestDaemon();
  browser = await chromium.launch({ headless: true, executablePath: await resolveChromiumExecutable() });
});
after(async () => {
  await browser?.close();
  await runtime?.stop();
});

async function setup({ verdict = 'pass', answer } = {}) {
  const context = await newAuthorizedContext(browser, { viewport: { width: 390, height: 844 }, colorScheme: 'dark', reducedMotion: 'reduce' });
  const page = await context.newPage();
  const calls = [];
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/keep-pr-fixture', route => route.fulfill({
    contentType: 'text/html',
    body: `<!doctype html><html data-theme="dark"><meta name="viewport" content="width=device-width,initial-scale=1">`
      + `<link rel="stylesheet" href="/ui/tokens.css">`
      + `<ax-keep-pr session-id="${SESSION}" run-id="${RUN}" verdict="${verdict}" loadout="fix"></ax-keep-pr>`
      + `<script type="module">import { AxKeepPr } from '/ui/keep-pr.js'; window.keepPrExport = typeof AxKeepPr;`
      + `window.keepEvents = []; document.addEventListener('keep-pr-result', event => window.keepEvents.push(event.detail));</script></html>`,
  }));
  await page.route(`**/api/sessions/${SESSION}/keep-pr`, async route => {
    const body = route.request().postDataJSON();
    calls.push(body);
    const reply = answer ? answer(body) : { status: 200, json: kept(body) };
    await route.fulfill({ status: reply.status, contentType: 'application/json', body: JSON.stringify(reply.json) });
  });
  await page.goto(`${runtime.baseUrl}/keep-pr-fixture`);
  await page.waitForFunction(() => customElements.get('ax-keep-pr') && window.keepPrExport === 'function');
  return { context, page, calls, errors };
}

function kept(body) {
  const response = {
    branch: body.branch || 'axocoatl/fix-0f8c1a2b',
    commit: COMMIT,
    paths: ['src/pagination.ts', 'test/pagination.test.ts'],
    not_committed: ['notes.txt'],
    warnings: ['Changed in the working tree but not by the run\'s Agents, so not committed: notes.txt'],
  };
  if (body.open_pr) {
    response.pushed_to = `${body.remote || 'origin'}/${response.branch}`;
    response.base = 'main';
    response.pull_request_url = 'https://github.com/acme/widgets/pull/7';
  }
  return response;
}

const keepButton = page => page.getByRole('button', { name: 'Keep as branch', exact: true });
const openButton = page => page.getByRole('button', { name: 'Open pull request…', exact: true });

test('Keep stays disabled unless the run passed', async () => {
  const { context, page, calls, errors } = await setup({ verdict: 'needs_attention' });
  try {
    assert.equal(await keepButton(page).isDisabled(), true);
    assert.equal(await openButton(page).isDisabled(), true);
    await page.getByText('Keep is available only for a run that passed. This run\'s verdict is needs attention.', { exact: true }).waitFor();
    for (const verdict of ['checks_failed', 'error', 'interrupted', '']) {
      await page.evaluate(value => document.querySelector('ax-keep-pr').setAttribute('verdict', value), verdict);
      assert.equal(await keepButton(page).isDisabled(), true, verdict);
      assert.equal(await openButton(page).isDisabled(), true, verdict);
    }
    await page.evaluate(() => document.querySelector('ax-keep-pr').setAttribute('verdict', 'pass'));
    assert.equal(await keepButton(page).isDisabled(), false);
    assert.equal(await openButton(page).isDisabled(), false);
    await page.evaluate(() => document.querySelector('ax-keep-pr').removeAttribute('run-id'));
    assert.equal(await keepButton(page).isDisabled(), true);
    assert.deepEqual(calls, []);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('Keep as branch posts the run and shows the branch, commit and paths', async () => {
  const { context, page, calls, errors } = await setup();
  try {
    await keepButton(page).click();
    await page.getByText('Kept as branch axocoatl/fix-0f8c1a2b at 1a2b3c4d5e6f (2 paths).', { exact: true }).waitFor();
    assert.deepEqual(calls, [{ run_id: RUN, open_pr: false }]);
    assert.deepEqual(await page.getByRole('list', { name: 'Committed paths' }).getByRole('listitem').allTextContents(),
      ['src/pagination.ts', 'test/pagination.test.ts']);
    await page.getByText('so not committed: notes.txt', { exact: false }).waitFor();
    assert.equal(await page.getByRole('link').count(), 0);
    const events = await page.evaluate(() => window.keepEvents);
    assert.equal(events.length, 1);
    assert.equal(events[0].runId, RUN);
    assert.equal(events[0].response.commit, COMMIT);
    assert.equal(await keepButton(page).isDisabled(), false);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('Open pull request confirms the remote, branch and base before pushing', async () => {
  const { context, page, calls, errors } = await setup();
  try {
    await page.evaluate(() => { window.escapeLeaks = 0; document.addEventListener('keydown', event => { if (event.key === 'Escape') window.escapeLeaks += 1; }); });
    await openButton(page).click();
    const dialog = page.getByRole('dialog', { name: 'Open a pull request?' });
    await dialog.waitFor();
    assert.equal(await dialog.getByLabel('Branch').inputValue(), 'axocoatl/fix-0f8c1a2b');
    assert.equal(await dialog.getByLabel('Remote').inputValue(), 'origin');
    const what = await dialog.locator('#keep-pr-what').textContent();
    assert.equal(what, 'Axocoatl commits this run\'s changes to branch axocoatl/fix-0f8c1a2b, pushes it to remote origin with your own git credentials, and opens a pull request with gh into the base branch origin\'s default branch.');
    await dialog.getByText('Axocoatl never force-pushes and never pushes to the default branch.', { exact: false }).waitFor();
    // Escape and Cancel close it without a request, and Escape stays inside.
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'hidden' });
    assert.equal(await page.evaluate(() => window.escapeLeaks), 0);
    await openButton(page).click();
    await dialog.getByRole('button', { name: 'Cancel' }).click();
    await dialog.waitFor({ state: 'hidden' });
    assert.deepEqual(calls, []);
    // The named remote and branch are what is sent.
    await openButton(page).click();
    await dialog.getByLabel('Remote').fill('upstream');
    assert.match(await dialog.locator('#keep-pr-what').textContent(), /to remote upstream with/);
    await dialog.getByLabel('Remote').fill('');
    assert.equal(await dialog.getByRole('button', { name: 'Push and open pull request' }).isDisabled(), true);
    await dialog.getByLabel('Remote').fill('upstream');
    await dialog.getByRole('button', { name: 'Push and open pull request' }).click();
    await page.getByText('Pushed to upstream/axocoatl/fix-0f8c1a2b, base main.', { exact: true }).waitFor();
    assert.deepEqual(calls, [{ run_id: RUN, open_pr: true, branch: 'axocoatl/fix-0f8c1a2b', remote: 'upstream' }]);
    const link = page.getByRole('link', { name: 'https://github.com/acme/widgets/pull/7' });
    assert.equal(await link.getAttribute('href'), 'https://github.com/acme/widgets/pull/7');
    assert.equal(await link.getAttribute('rel'), 'noopener noreferrer');
    assert.equal(await link.getAttribute('target'), '_blank');
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('A refusal is shown as such and nothing is reported kept', async () => {
  const { context, page, calls, errors } = await setup({
    answer: body => body.open_pr
      ? { status: 409, json: { error: 'Session conflict: keep as PR: the branch axocoatl/fix-0f8c1a2b already exists on remote origin; Keep as PR never overwrites a remote branch' } }
      : { status: 409, json: { error: 'Session conflict: keep as PR: a path the run changed had uncommitted changes before the run started, so the run\'s work on it cannot be kept apart from them: src/a.ts' } },
  });
  try {
    await keepButton(page).click();
    const alert = page.getByRole('alert');
    await alert.waitFor();
    assert.equal(await alert.textContent(), 'Refused: a path the run changed had uncommitted changes before the run started, so the run\'s work on it cannot be kept apart from them: src/a.ts');
    assert.equal(await page.getByText('Kept as branch', { exact: false }).count(), 0);
    await openButton(page).click();
    await page.getByRole('button', { name: 'Push and open pull request' }).click();
    await page.getByText('Refused: the branch axocoatl/fix-0f8c1a2b already exists on remote origin; Keep as PR never overwrites a remote branch', { exact: true }).waitFor();
    assert.equal(calls.length, 2);
    const events = await page.evaluate(() => window.keepEvents);
    assert.deepEqual(events.map(event => event.status), [409, 409]);
    // A Keep the record already holds is shown when set.
    await page.evaluate(commit => { document.querySelector('ax-keep-pr').result = { branch: 'axocoatl/fix-0f8c1a2b', commit, pull_request_url: 'https://github.com/acme/widgets/pull/7' }; }, COMMIT);
    await page.getByText('Kept as branch axocoatl/fix-0f8c1a2b at 1a2b3c4d5e6f.', { exact: true }).waitFor();
    await page.getByRole('link', { name: 'https://github.com/acme/widgets/pull/7' }).waitFor();
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});
