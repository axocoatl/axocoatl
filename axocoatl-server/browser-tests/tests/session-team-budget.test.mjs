import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';

import { browserEngines, launchTestDaemon, newAuthorizedContext } from '../support/daemon.mjs';

// A new Session's Team and budget opens with the daemon's suggested limits
// filled in, applies without typing, and shows Preview's refusal of a limit
// a turn cannot use with its numbers. The team routes are fixtures; the
// component is the real one served by the daemon.
const engines = await browserEngines();
let runtime;

before(async () => {
  runtime = process.env.AXOCOATL_COMPONENT_BASE_URL
    ? { baseUrl: process.env.AXOCOATL_COMPONENT_BASE_URL, stop: async () => {} }
    : await launchTestDaemon();
});

after(async () => {
  await runtime?.stop();
});

const DAY_MS = 24 * 60 * 60 * 1000;
const FLOOR_REFUSAL = "Lead's token limit of 10,000 is too small: Lead reserves 36,864 tokens per model call (32,768 context + 4,096 output), and a turn with tools needs at least 73,728, for a call and the one after it. Raise its token limit to at least 73,728.";

function teamView({ approved = false } = {}) {
  const lead = {
    slot_id: 'slot-lead', template_id: 'lead', name: 'Lead', provider: 'ollama', model: 'qwen3-coder:30b',
    instructions: null, max_output_tokens: 4096, writes: null, required: true, reset_history: true,
    limits: null, expires_at_ms: null,
  };
  return {
    history_version: 'execution_v2', configuration_revision: approved ? 1 : 0,
    slots: [approved ? { ...lead, template_id: null, reset_history: false, limits: { activations: 4, invocations: 400, tokens: 4000000, cost_microunits: 0 }, expires_at_ms: Date.now() + DAY_MS } : lead],
    dependencies: [], layout: [], templates: [structuredClone(lead)], approved,
    required_checks: [], reviewers: [], suggested_check: null,
    suggested_budget: {
      from: 'fix',
      agent: { activations: 4, invocations: 400, tokens: 4000000, cost_microunits: 10000000 },
      helper: { activations: 3, invocations: 120, tokens: 1500000, cost_microunits: 3000000 },
      free_providers: ['ollama'],
      expires_at_ms: Date.now() + DAY_MS,
    },
  };
}

async function fixture(engine) {
  const browser = await engine.launch();
  const context = await newAuthorizedContext(browser, { viewport: { width: 1100, height: 820 } });
  const page = await context.newPage();
  const errors = [];
  const calls = [];
  page.on('pageerror', (error) => errors.push(error.message));
  let view = teamView();
  await page.route('**/team-budget-fixture', (route) => route.fulfill({
    contentType: 'text/html',
    body: '<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-session-team session="fixture-session"></ax-session-team><script type="module" src="/ui/session-team.js"></script></body></html>',
  }));
  await page.route('**/api/sessions/fixture-session/team**', async (route) => {
    const suffix = new URL(route.request().url()).pathname.split('/team')[1];
    const body = route.request().method() === 'POST' ? route.request().postDataJSON() : null;
    calls.push({ suffix, body });
    if (!suffix) return route.fulfill({ json: view });
    if (suffix === '/preview') {
      // The daemon's floor: a call and the one after it.
      if (body.slots.some((slot) => slot.limits.tokens < 73728)) {
        return route.fulfill({ status: 422, json: { error: FLOOR_REFUSAL } });
      }
      return route.fulfill({ json: {
        edit: body, review_digest: 'exact-review', configuration_revision: 1, applies_to: 'future_turns',
        changes: body.slots.map((slot) => ({ slot_id: slot.slot_id, kind: 'changed', history: 'new conversation' })),
        coordinators: [], profiles: body.slots.map((slot) => ({ definition: slot.slot_id, provider: slot.provider, model: slot.model, isolation: 'in-process', tools: ['read_file', 'edit_file', 'bash'] })),
        toolless_slots: [],
      } });
    }
    if (suffix === '/apply') {
      view = teamView({ approved: true });
      return route.fulfill({ json: { configuration_revision: 1 } });
    }
    return route.fulfill({ status: 404, json: { error: `unexpected ${suffix}` } });
  });
  await page.goto(`${runtime.baseUrl}/team-budget-fixture`);
  await page.getByRole('button', { name: 'Team and budget', exact: true }).click();
  return {
    page, errors, calls,
    async close() { await context.close(); await browser.close(); },
  };
}

for (const engine of engines) {
  test(`${engine.name}: a new Session's limits come filled in and apply without typing`, async () => {
    const { page, errors, calls, close } = await fixture(engine);
    try {
      await page.getByText('Suggested limits are filled in from the built-in fix loadout', { exact: false }).waitFor();
      assert.equal(await page.getByLabel('Activation limit', { exact: true }).inputValue(), '4');
      assert.equal(await page.getByLabel('Provider and tool invocation limit', { exact: true }).inputValue(), '400');
      assert.equal(await page.getByLabel('Total token limit', { exact: true }).inputValue(), '4000000');
      // A local Ollama model costs nothing.
      assert.equal(await page.getByLabel('Cost limit (USD)', { exact: true }).inputValue(), '0');
      const expiry = await page.getByLabel('Budget expires (your local time)', { exact: true }).inputValue();
      assert.match(expiry, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/);
      await page.getByRole('button', { name: 'Edit', exact: true }).click();
      await page.getByRole('button', { name: 'Preview changes', exact: true }).click();
      await page.getByText('Review these changes.', { exact: false }).waitFor();
      const preview = calls.find((call) => call.suffix === '/preview').body;
      assert.deepEqual(preview.slots[0].limits, { activations: 4, invocations: 400, tokens: 4000000, cost_microunits: 0 });
      assert.ok(preview.slots[0].expires_at_ms > Date.now() + DAY_MS - 120000);
      await page.getByRole('button', { name: 'Apply to this Session', exact: true }).click();
      await page.getByText('Saved Session configuration 1.', { exact: false }).waitFor();
      assert.deepEqual(calls.find((call) => call.suffix === '/apply').body.edit, preview);
      assert.deepEqual(errors, []);
    } finally { await close(); }
  });

  test(`${engine.name}: Preview names the minimum a turn needs, and an expiry past a year is refused`, async () => {
    const { page, errors, calls, close } = await fixture(engine);
    try {
      await page.getByText('Suggested limits are filled in', { exact: false }).waitFor();
      await page.getByRole('button', { name: 'Edit', exact: true }).click();
      await page.getByLabel('Total token limit', { exact: true }).fill('10000');
      await page.getByRole('button', { name: 'Preview changes', exact: true }).click();
      await page.getByText(FLOOR_REFUSAL, { exact: false }).waitFor();
      assert.equal(await page.getByRole('button', { name: 'Apply to this Session', exact: true }).isDisabled(), true);

      await page.getByLabel('Total token limit', { exact: true }).fill('4000000');
      const max = await page.getByLabel('Budget expires (your local time)', { exact: true }).getAttribute('max');
      assert.match(max, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/);
      const previews = calls.filter((call) => call.suffix === '/preview').length;
      await page.getByLabel('Budget expires (your local time)', { exact: true }).fill('2099-10-10T10:00');
      await page.getByRole('button', { name: 'Preview changes', exact: true }).click();
      await page.getByText("Lead's budget expiry is more than a year away; choose one within a year.", { exact: false }).waitFor();
      assert.equal(calls.filter((call) => call.suffix === '/preview').length, previews, 'refused before the daemon');
      assert.deepEqual(errors, []);
    } finally { await close(); }
  });
}
