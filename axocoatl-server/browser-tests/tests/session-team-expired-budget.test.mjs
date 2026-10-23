import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { setTimeout as sleep } from 'node:timers/promises';
import { test } from 'node:test';

import { browserEngines, isWebKitLoadDiagnostic, launchTestDaemon, newAuthorizedContext } from '../support/daemon.mjs';
import { answerContractProbe, OLLAMA_VERSION } from '../support/ollama.mjs';

// A Team whose budget expiry has passed (a day after it was applied, as on
// most upgrades) is applied again through Team and budget's own Edit,
// Preview and Apply, with nothing typed: Edit fills in a new expiry and says
// so, and Preview shows it before Apply authorizes it. When the model was
// pulled again since the Team was reviewed, Preview also says that the
// Agent is observed again, and why. The daemon, its team routes and the
// component are the real ones; Ollama is a stand-in.
const engines = await browserEngines();
const DAY_MS = 24 * 60 * 60 * 1000;
const MODEL = 'browser-test-model:latest';
const DIGESTS = ['a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72', 'b80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72'];

/** An Ollama stand-in whose model's digest is `ollama.digest`. */
async function ollama() {
  const state = { digest: DIGESTS[0] };
  state.server = createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = raw ? JSON.parse(raw) : null;
    if (answerContractProbe(req.url, body, res)) return;
    const responses = {
      '/api/version': { version: OLLAMA_VERSION }, '/api/status': { cloud: { disabled: true } },
      '/api/show': { details: { format: 'gguf' }, capabilities: ['completion'] },
      '/api/tags': { models: [{ name: MODEL, model: MODEL, digest: state.digest }] },
      '/api/ps': { models: [{ name: MODEL, model: MODEL, digest: state.digest, details: { format: 'gguf' }, context_length: 2048, runner: 'llamacpp' }] },
      '/api/generate': { model: MODEL, created_at: '2026-10-23T00:00:00Z', response: '', done: true, done_reason: 'load' },
    };
    res.writeHead(responses[req.url] ? 200 : 404, { 'content-type': 'application/json' });
    res.end(JSON.stringify(responses[req.url] || {}));
  });
  await new Promise((resolve) => state.server.listen(0, '127.0.0.1', resolve));
  state.url = `http://127.0.0.1:${state.server.address().port}`;
  return state;
}

async function call(runtime, session, suffix = '', body) {
  const response = await fetch(`${runtime.baseUrl}/api/sessions/${session}/team${suffix}`, {
    method: body ? 'POST' : 'GET', headers: body ? { 'content-type': 'application/json' } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const value = await response.json();
  assert.equal(response.status, 200, JSON.stringify(value));
  return value;
}

/** Apply `session`'s Team through the API with a budget that expires in `ms`. */
async function applyExpiringIn(runtime, session, ms) {
  const view = await call(runtime, session);
  const edit = {
    command_id: `expiring-${session}`, expected_configuration_revision: view.configuration_revision,
    slots: view.slots.map((slot) => ({ ...slot, max_output_tokens: 128, limits: { activations: 2, invocations: 8, tokens: 32768, cost_microunits: 0 }, expires_at_ms: Date.now() + ms })),
    dependencies: view.dependencies, layout: view.layout,
  };
  const preview = await call(runtime, session, '/preview', edit);
  await call(runtime, session, '/apply', { edit, review_digest: preview.review_digest });
  return edit.slots[0].expires_at_ms;
}

/** The epoch milliseconds a `datetime-local` value names, in this time zone. */
const localMs = (value) => new Date(value).getTime();

/** Open Team and budget for `session` in a page served from the daemon's origin. */
async function openTeam(browser, runtime, session) {
  const context = await newAuthorizedContext(browser, { viewport: { width: 1100, height: 820 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => { if (!isWebKitLoadDiagnostic(error)) errors.push(error.message); });
  await page.route('**/team-expired-fixture', (route) => route.fulfill({
    contentType: 'text/html',
    body: `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-session-team session="${session}"></ax-session-team><script type="module" src="/ui/session-team.js"></script></body></html>`,
  }));
  await page.goto(`${runtime.baseUrl}/team-expired-fixture`);
  await page.getByRole('button', { name: 'Team and budget', exact: true }).click();
  return { page, errors, close: () => context.close() };
}

/** Edit, Preview and Apply the Team as Edit fills it in; returns the Preview's lines. */
async function reapply(page, expiredAt) {
  const status = page.locator('ax-session-team .status');
  await page.getByText('expired at', { exact: false }).waitFor();
  assert.match(await status.textContent(), /^Saved Session configuration 1\. The budget of .+ expired at .+\. Select Edit: it fills in a new expiry, 24 hours from then, for you to review\. Then Preview and Apply\.$/);
  // View shows the approved expiry, which has passed.
  const expiry = page.getByLabel('Budget expires (your local time)', { exact: true });
  assert.ok(Math.abs(localMs(await expiry.inputValue()) - expiredAt) < 60000);
  await page.getByRole('button', { name: 'Edit', exact: true }).click();
  await page.getByText('A new budget expiry is filled in for', { exact: false }).waitFor();
  assert.match(await status.textContent(), /, 24 hours from now, since the approved one has passed\. Review it, then Preview and Apply\.$/);
  const renewed = localMs(await expiry.inputValue());
  assert.ok(Math.abs(renewed - (Date.now() + DAY_MS)) < 5 * 60000, `${await expiry.inputValue()}`);
  await page.getByRole('button', { name: 'Preview changes', exact: true }).click();
  await page.getByText('Review these changes.', { exact: false }).waitFor();
  const lines = await page.locator('ax-session-team .review li').allTextContents();
  assert.ok(lines.some((line) => line.includes('; a new expiry, since the approved one, ') && line.endsWith(', has passed.')), lines.join('\n'));
  await page.getByRole('button', { name: 'Apply to this Session', exact: true }).click();
  await page.getByText('Saved Session configuration 2. Select Edit to make changes.', { exact: true }).waitFor();
  return lines;
}

for (const engine of engines) {
  test(`${engine.name}: a Team whose budget expired is applied again through Edit, Preview and Apply`, async () => {
    const model = await ollama();
    let runtime, browser;
    try {
      runtime = await launchTestDaemon({ nativeDataRoot: true, ollamaBaseUrl: model.url });
      const [kept, pulled] = runtime.fixtures.alpha.sessions.map((session) => session.id);
      // Each Team is approved with a budget that has expired by the time
      // Team and budget opens (long enough for its Preview and Apply).
      const expiredAt = await applyExpiringIn(runtime, kept, 10000);
      const pulledExpiredAt = await applyExpiringIn(runtime, pulled, 10000);
      await sleep(Math.max(expiredAt, pulledExpiredAt) - Date.now() + 500);
      browser = await engine.launch();

      // Nothing else changed: the Agent keeps its conversation.
      let opened = await openTeam(browser, runtime, kept);
      try {
        const lines = await reapply(opened.page, expiredAt);
        assert.ok(lines.includes('Browser Test Coder: retained; preserved.'), lines.join('\n'));
        assert.ok(!lines.some((line) => line.includes('observed again')), lines.join('\n'));
        assert.deepEqual(opened.errors, []);
      } finally { await opened.close(); }
      const saved = await call(runtime, kept);
      assert.equal(saved.configuration_revision, 2);
      assert.ok(saved.slots[0].expires_at_ms > Date.now() + DAY_MS - 5 * 60000);

      // The model was pulled again since: Preview says the Agent is
      // observed again, and why, and that it starts a new conversation.
      model.digest = DIGESTS[1];
      opened = await openTeam(browser, runtime, pulled);
      try {
        const lines = await reapply(opened.page, pulledExpiredAt);
        assert.ok(lines.includes('Browser Test Coder: replaced; new conversation.'), lines.join('\n'));
        assert.ok(lines.includes('Browser Test Coder is observed again, so it starts a new conversation: browser-test-model was pulled again since this Team was reviewed (digest a80c4f17acd5, now b80c4f17acd5).'), lines.join('\n'));
        assert.deepEqual(opened.errors, []);
      } finally { await opened.close(); }
      assert.equal((await call(runtime, pulled)).configuration_revision, 2);
    } finally {
      await browser?.close();
      await runtime?.stop();
      await new Promise((resolve) => model.server.close(resolve));
    }
  });
}
