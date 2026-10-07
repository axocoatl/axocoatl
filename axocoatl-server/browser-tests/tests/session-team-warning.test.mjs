// Team and budget shows the same-model reviewer warning beside the reviewer
// picker and in the preview, before Apply, and never blocks Apply.
import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { launchTestDaemon, resolveChromiumExecutable, newAuthorizedContext } from '../support/daemon.mjs';

let runtime, browser;
before(async () => {
  runtime = process.env.AXOCOATL_COMPONENT_BASE_URL ? { baseUrl: process.env.AXOCOATL_COMPONENT_BASE_URL, stop: async () => {} } : await launchTestDaemon();
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
});
after(async () => { await browser?.close(); await runtime?.stop(); });

const WRITER = { provider: 'openrouter', model: 'qwen/qwen3-coder' };
const SAME = "The reviewer runs the writer's model (openrouter:qwen/qwen3-coder). A same-model second look measured no gain; choose a different reviewer model.";
const limits = { activations: 2, invocations: 12, tokens: 32768, cost_microunits: 0 };
const template = (id, name, model, extra = {}) => ({ slot_id: `slot-${id}`, template_id: id, source_slot_id: null, name, ...WRITER, model, instructions: null, max_output_tokens: 256, required: true, reset_history: true, limits: null, expires_at_ms: null, ...extra });

async function fixture({ theme = 'light', serverWarnings = true, reviewer = 'same-reviewer', previewWarnings = null } = {}) {
  const context = await newAuthorizedContext(browser, { viewport: theme === 'dark' ? { width: 390, height: 840 } : { width: 1100, height: 820 }, colorScheme: theme, reducedMotion: 'reduce' });
  const page = await context.newPage(), errors = [], calls = [];
  page.on('pageerror', error => errors.push(error.message));
  const view = {
    history_version: 'execution_v2', configuration_revision: 1, approved: true, dependencies: [], layout: [], required_checks: [['sh', '-c', 'npm test']], suggested_check: null,
    slots: [{ ...template('writer', 'Writer', WRITER.model), template_id: null, writes: null, limits, expires_at_ms: Date.now() + 86400000, reset_history: false }],
    templates: [template('writer', 'Writer', WRITER.model, { writes: null }), template('same-reviewer', 'Same-model reviewer', WRITER.model, { role: 'worker', writes: [] }), template('other-reviewer', 'Other reviewer', 'openai/gpt-oss-120b', { role: 'worker', writes: [] })],
    reviewers: ['same-reviewer', 'other-reviewer'],
    required_review: { template_id: reviewer, max_rounds: 2, limits: { activations: 2, invocations: 6, tokens: 65536, cost_microunits: 0 } },
    ...(serverWarnings ? { warnings: [{ code: 'same_model_reviewer', message: SAME }] } : {}),
  };
  await page.route('**/team-fixture', route => route.fulfill({ contentType: 'text/html', body: `<!doctype html><html data-theme="${theme}"><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-session-team session="fixture-session"></ax-session-team><script type="module" src="/ui/session-team.js"></script></body></html>` }));
  await page.route('**/api/sessions/fixture-session/team**', async route => {
    const suffix = new URL(route.request().url()).pathname.split('/team')[1], body = route.request().method() === 'POST' ? route.request().postDataJSON() : null;
    calls.push({ suffix, body });
    if (!suffix) return route.fulfill({ json: view });
    if (suffix === '/cancel') return route.fulfill({ json: { cancelled: true } });
    if (suffix === '/preview') return route.fulfill({ json: { edit: body, review_digest: 'exact-review', configuration_revision: body.expected_configuration_revision + 1, applies_to: 'future_turns', changes: [], coordinators: [], profiles: [], toolless_slots: [], ...(previewWarnings ? { warnings: previewWarnings } : {}) } });
    if (suffix === '/apply') { view.configuration_revision += 1; view.required_review = body.edit.required_review; return route.fulfill({ json: { configuration_revision: view.configuration_revision } }); }
  });
  await page.goto(`${runtime.baseUrl}/team-fixture`);
  await page.getByRole('button', { name: 'Team and budget', exact: true }).click();
  await page.getByText('Saved Session configuration 1.', { exact: false }).waitFor();
  return { context, page, calls, errors };
}

const team = page => page.locator('ax-session-team');
const warningItems = page => team(page).locator('.review-warnings li');

for (const theme of ['light', 'dark']) test(`Team and budget ${theme}: the same-model warning sits beside the reviewer picker before Apply`, async () => {
  const { page, context, calls, errors } = await fixture({ theme });
  try {
    // The daemon's warning for the applied team is shown once, next to the
    // reviewer picker, and the review setting opens to show it.
    await warningItems(page).first().waitFor();
    assert.deepEqual(await warningItems(page).allTextContents(), [SAME]);
    assert.equal(await warningItems(page).first().getAttribute('data-code'), 'same_model_reviewer');
    assert.equal(await team(page).locator('.review-setting').evaluate(element => element.open), true);
    assert.equal(await team(page).locator('.review-setting summary').textContent(), 'Required review by Same-model reviewer · up to 2 rounds');
    const picker = await team(page).locator('select.reviewer').boundingBox(), shown = await team(page).locator('.review-warnings').boundingBox();
    assert.ok(picker && shown && shown.y >= picker.y && shown.y - (picker.y + picker.height) < 120, 'the warning sits just below the reviewer picker');
    const role = await team(page).locator('.review-warnings').getAttribute('role');
    assert.equal(role, 'status');

    // A reviewer on another model clears it; choosing the writer's model
    // again shows it, computed for the draft before any preview.
    await page.getByRole('button', { name: 'Edit', exact: true }).click();
    await page.getByLabel('Required reviewer', { exact: true }).selectOption('other-reviewer');
    await team(page).locator('.review-warnings').waitFor({ state: 'hidden' });
    await page.getByLabel('Required reviewer', { exact: true }).selectOption('same-reviewer');
    await warningItems(page).first().waitFor();
    assert.deepEqual(await warningItems(page).allTextContents(), [SAME]);

    // The preview lists it before Apply; it warns and never blocks Apply.
    await page.getByRole('button', { name: 'Preview changes', exact: true }).click();
    await page.getByText('Review these changes.', { exact: false }).waitFor();
    assert.equal(await team(page).locator('.review li.warning').first().textContent(), `Warning: ${SAME}`);
    assert.equal(await page.getByRole('button', { name: 'Apply to this Session', exact: true }).isDisabled(), false);
    await page.getByRole('button', { name: 'Apply to this Session', exact: true }).click();
    await page.getByText('Saved Session configuration 2.', { exact: false }).waitFor();
    const apply = calls.find(call => call.suffix === '/apply').body;
    assert.equal(apply.edit.required_review.template_id, 'same-reviewer');
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('Team and budget: no warning for a reviewer on another model, and the preview carries the daemon\'s own', async () => {
  const { page, context, errors } = await fixture({ serverWarnings: false, reviewer: 'other-reviewer', previewWarnings: [{ code: 'other_check', message: 'The reviewer cannot read the tests directory.' }] });
  try {
    assert.equal(await team(page).locator('.review-warnings').isHidden(), true);
    assert.equal(await warningItems(page).count(), 0);
    assert.equal(await team(page).locator('.review-setting').evaluate(element => element.open), false, 'nothing to warn about opens nothing');
    // A writer moved to the reviewer's model is warned about too.
    await page.getByRole('button', { name: 'Edit', exact: true }).click();
    await team(page).locator('ax-node').first().click();
    await page.getByLabel('Model', { exact: true }).fill('openai/gpt-oss-120b');
    await warningItems(page).first().waitFor();
    assert.match(await warningItems(page).first().textContent(), /openrouter:openai\/gpt-oss-120b/);
    await page.getByLabel('Model', { exact: true }).fill(WRITER.model);
    await team(page).locator('.review-warnings').waitFor({ state: 'hidden' });
    // A warning the daemon gives for the exact edit appears with the preview.
    await page.getByRole('button', { name: 'Preview changes', exact: true }).click();
    await page.getByText('Review these changes.', { exact: false }).waitFor();
    assert.deepEqual(await warningItems(page).allTextContents(), ['The reviewer cannot read the tests directory.']);
    assert.equal(await team(page).locator('.review li.warning').count(), 1);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});
