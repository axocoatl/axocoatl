import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';

import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';

let runtime;
let browser;
let provider;
let providerCalls = 0;

async function launchFixtureProvider() {
  const server = createServer((request, response) => {
    if (request.method === 'GET' && request.url === '/api/tags') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ models: [{ name: 'browser-test-model' }] }));
      return;
    }
    if (request.method === 'GET' && request.url === '/v1/models') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ data: [{ id: 'browser-test-model' }] }));
      return;
    }
    if (request.method === 'HEAD') {
      response.writeHead(200);
      response.end();
      return;
    }
    if (request.method !== 'POST' || request.url !== '/v1/chat/completions') {
      response.writeHead(404);
      response.end();
      return;
    }

    let body = '';
    request.setEncoding('utf8');
    request.on('data', (chunk) => { body += chunk; });
    request.on('end', () => {
      const payload = JSON.parse(body || '{}');
      assert.equal(payload.model, 'browser-test-model');
      providerCalls += 1;
      const first = {
        id: `fixture-${providerCalls}`,
        model: 'browser-test-model',
        choices: [{ index: 0, delta: { content: 'Launch Automation recorded the signal.' }, finish_reason: null }],
      };
      const done = {
        id: `fixture-${providerCalls}`,
        model: 'browser-test-model',
        choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
        usage: { prompt_tokens: 11, completion_tokens: 5 },
      };
      response.writeHead(200, {
        'content-type': 'text/event-stream',
        'cache-control': 'no-cache',
        connection: 'keep-alive',
      });
      response.end(`data: ${JSON.stringify(first)}\n\ndata: ${JSON.stringify(done)}\n\ndata: [DONE]\n\n`);
    });
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.equal(typeof address, 'object');
  return {
    server,
    baseUrl: `http://127.0.0.1:${address.port}`,
    close: () => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}

before(async () => {
  provider = await launchFixtureProvider();
  runtime = await launchTestDaemon({
    ollamaBaseUrl: provider.baseUrl,
    skills: [{
      id: 'launch_ready',
      name: 'Launch ready',
      description: 'Publish the typed launch signal.',
      emits: ['LaunchReady'],
      agents: ['browser-test-coder'],
      prompt: 'Publish the launch-ready signal.',
    }],
  });
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
});

after(async () => {
  await browser?.close();
  await runtime?.stop();
  await provider?.close();
});

async function openSession(session) {
  const context = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  const page = await context.newPage();
  const browserErrors = [];
  page.on('pageerror', (error) => browserErrors.push(`pageerror: ${error.message}`));
  await page.goto(`${runtime.baseUrl}/?session=${encodeURIComponent(session.id)}`, { waitUntil: 'domcontentloaded' });
  await page.waitForFunction((sessionId) => {
    const rail = document.querySelector('ax-rail');
    return Boolean(customElements.get('ax-settings')
      && customElements.get('ax-automation-settings')
      && customElements.get('ax-settings-skills')
      && rail?.shadowRoot
      && rail.current === sessionId);
  }, session.id);
  return { context, page, browserErrors };
}

async function openSettingsSection(page, name) {
  const settings = page.locator('ax-settings');
  if ((await settings.getAttribute('open')) === null) {
    await page.getByRole('button', { name: 'Settings' }).click();
  }
  await settings.getByRole('button', { name }).click();
}

test('a Session-authorized Skill triggers a UI-created Automation whose result survives restart', { timeout: 90_000 }, async () => {
  const { context, page, browserErrors } = await openSession(runtime.fixtures.alpha.sessions[0]);
  try {
    await page.getByRole('button', { name: 'New session in Alpha Workspace' }).click();
    const sessionDialog = page.getByRole('dialog', { name: 'New session in Alpha Workspace' });
    await sessionDialog.locator('[data-field="session-name"]').fill('Skill-authorized launch');
    await sessionDialog.locator('[data-field="skill"][value="launch_ready"]').check();
    await sessionDialog.locator('[data-field="ports"]').fill('');
    await sessionDialog.getByRole('button', { name: 'Create session' }).click();
    await sessionDialog.waitFor({ state: 'hidden' });

    const createdSession = (await runtime.listSessions())
      .find((session) => session.name === 'Skill-authorized launch');
    assert.ok(createdSession, 'the Session created through the UI must persist');
    assert.deepEqual(createdSession.enabled_skills, ['launch_ready']);
    assert.equal(createdSession.environment.state, 'awaiting_approval');
    await page.waitForFunction((sessionId) => {
      const rail = document.querySelector('ax-rail');
      return rail?.current === sessionId
        && new URL(location.href).searchParams.get('session') === sessionId;
    }, createdSession.id);
    assert.equal(new URL(page.url()).searchParams.get('session'), createdSession.id);

    await openSettingsSection(page, 'Automations Work that starts itself');
    const automations = page.locator('ax-automation-settings');
    await automations.locator('.shell:not(.loading)').waitFor({ state: 'visible' });
    await automations.getByRole('button', { name: '+ Automation' }).first().click();
    const createDialog = automations.getByRole('dialog', { name: 'New Automation' });
    const nameField = createDialog.getByPlaceholder('Review release readiness');
    const idField = createDialog.getByPlaceholder('review-release-readiness');
    const descriptionField = createDialog.getByPlaceholder('Optional: what this Automation does.');
    await nameField.fill('Launch proof recorder');
    await idField.fill('launch-proof-recorder');
    await descriptionField.fill('Records the Session-authorized launch Skill signal.');
    await createDialog.locator('select:has(option[value="on_skill"])').selectOption('on_skill');
    await createDialog.locator('select:has(option[value="launch_ready"])').selectOption('launch_ready');
    const instructionText = 'Reply with exactly: Launch Automation recorded the signal.';
    const instruction = createDialog.getByPlaceholder('Explain what the Agent should do whenever this trigger fires.');
    await instruction.waitFor({ state: 'visible' });
    await instruction.evaluate(() => new Promise((resolve) => {
      requestAnimationFrame(() => requestAnimationFrame(resolve));
    }));
    await instruction.fill(instructionText);
    assert.equal(await nameField.inputValue(), 'Launch proof recorder');
    assert.equal(await idField.inputValue(), 'launch-proof-recorder');
    assert.equal(await descriptionField.inputValue(), 'Records the Session-authorized launch Skill signal.');
    assert.equal(await instruction.inputValue(), instructionText);
    const createResponse = page.waitForResponse((response) => {
      const request = response.request();
      return request.method() === 'POST' && new URL(response.url()).pathname === '/api/automations';
    });
    await createDialog.getByRole('button', { name: 'Create Automation' }).click();
    try {
      const response = await createResponse;
      assert.equal(response.status(), 200, `Automation create returned ${response.status()}: ${await response.text()}`);
      await createDialog.waitFor({ state: 'hidden' });
    } catch (error) {
      const dialogText = await createDialog.innerText().catch(() => '<dialog unavailable>');
      throw new Error(`Automation creation dialog did not close.\nDialog:\n${dialogText}\n\n${runtime.logs()}\n\n${error}`);
    }
    await automations.getByText('id: launch-proof-recorder').waitFor({ state: 'visible' });

    await openSettingsSection(page, 'Skills What agents may call');
    const skills = page.locator('ax-settings-skills');
    await skills.getByRole('button', { name: /Launch ready Publish the typed launch signal/ }).click();
    await skills.getByRole('button', { name: '◆ Fire this Skill' }).click();
    await page.getByText("Fired 'Launch ready'").waitFor({ state: 'visible' });

    await openSettingsSection(page, 'Automations Work that starts itself');
    await automations.getByRole('button', { name: '⟲ Runs' }).click();
    const completed = automations.locator('.run-status.completed');
    await completed.waitFor({ state: 'visible', timeout: 30_000 });
    assert.ok(providerCalls >= 1, 'the Skill trigger must execute the Automation Agent');
    const run = automations.locator('.run-list .run').first();
    const runId = await run.getAttribute('data-run-id');
    assert.ok(runId);
    await run.locator('.run-head').click();
    assert.equal(await run.locator('.run-result-content').innerText(), 'Launch Automation recorded the signal.');
    assert.deepEqual(await run.locator('.run-step').allTextContents(), [
      '✓input · node completed',
      '✓agent · node completed',
    ]);

    await runtime.restart();
    const durableAutomations = await runtime.listAutomations();
    assert.ok(
      durableAutomations.some((automation) => automation.id === 'launch-proof-recorder'),
      `the restarted daemon must reload the persisted Automation: ${JSON.stringify(durableAutomations)}\n${runtime.logs()}`,
    );
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForFunction((sessionId) => {
      const rail = document.querySelector('ax-rail');
      return Boolean(customElements.get('ax-settings')
        && customElements.get('ax-automation-settings')
        && customElements.get('ax-settings-skills')
        && document.querySelector('ax-settings')?._wired
        && rail?.shadowRoot
        && rail.current === sessionId);
    }, createdSession.id);
    assert.equal(new URL(page.url()).searchParams.get('session'), createdSession.id);
    await openSettingsSection(page, 'Automations Work that starts itself');
    const restoredAutomations = page.locator('ax-automation-settings');
    await restoredAutomations.evaluate((element) => element.refresh());
    await restoredAutomations.locator('.shell:not(.loading)').waitFor({ state: 'visible' });
    const restoredState = await restoredAutomations.evaluate((element) => ({
      automations: element.automations,
      errors: Array.from(element.shadowRoot.querySelectorAll('.error .message'), (node) => node.textContent),
    }));
    assert.ok(
      restoredState.automations.some((automation) => automation.id === 'launch-proof-recorder'),
      `the Automation UI must hydrate the persisted catalog: ${JSON.stringify(restoredState)}\n${runtime.logs()}`,
    );
    const restoredAutomation = restoredAutomations.locator('[data-automation-id="launch-proof-recorder"]');
    await restoredAutomation.waitFor({ state: 'visible' });
    await restoredAutomation.click();
    await restoredAutomations.getByRole('button', { name: '⟲ Runs' }).click();
    const restoredRun = restoredAutomations.locator(`[data-run-id="${runId}"]`);
    await restoredRun.waitFor({ state: 'visible' });
    await restoredRun.locator('.run-head').click();
    await restoredRun.locator('.run-status.completed').waitFor({ state: 'visible' });
    assert.equal(await restoredRun.locator('.run-result-content').innerText(), 'Launch Automation recorded the signal.');
    assert.deepEqual(browserErrors, [], browserErrors.join('\n'));
  } finally {
    await context.close();
  }
});
