import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';

import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';

let runtime;
let browser;

before(async () => {
  runtime = await launchTestDaemon();
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({
    headless: true,
    ...(executablePath ? { executablePath } : {}),
  });
});

after(async () => {
  await browser?.close();
  await runtime?.stop();
});

test('restored multi-agent Session rebuilds its visible dependency route after Agent metadata arrives', async () => {
  const source = runtime.fixtures.alpha.sessions[0];
  const session = structuredClone(source);
  session.mode = {
    kind: 'custom',
    agents: ['browser-test-coder', 'browser-test-reviewer'],
  };
  const agents = [
    {
      id: 'browser-test-coder',
      name: 'Browser Test Coder',
      provider: 'ollama',
      model: 'browser-test-model',
      depends_on: [],
    },
    {
      id: 'browser-test-reviewer',
      name: 'Browser Test Reviewer',
      provider: 'ollama',
      model: 'browser-test-model',
      depends_on: ['browser-test-coder'],
    },
  ];
  const failedTurn = {
    id: 'turn-blank-reviewer-fixture',
    session_id: session.id,
    user_input: 'Propose and review a cache invalidation design',
    agent_id: null,
    model: null,
    context: [],
    status: 'failed',
    partial_output: 'Invalidate catalog entries immediately after each mutation.',
    final_output: null,
    error: "agent 'browser-test-reviewer' completed without a user-visible output; the multi-agent Session cannot claim a completed collaboration",
    created_at: 1_000,
    updated_at: 1_200,
    completed_at: 1_200,
    idempotency_key: 'blank-reviewer-fixture',
    metadata: { mode: 'custom' },
    execution_events: [],
    agent_outputs: [
      {
        agent_id: 'browser-test-coder',
        model: 'browser-test-model',
        output: 'Invalidate catalog entries immediately after each mutation.',
        recorded_at: 1_100,
      },
      {
        agent_id: 'browser-test-reviewer',
        model: 'browser-test-model',
        output: '',
        recorded_at: 1_150,
      },
    ],
    superseded: false,
  };

  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const errors = [];
  const failedResponses = [];
  page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`));
  page.on('response', (response) => {
    if (response.status() >= 400) failedResponses.push(`${response.status()} ${response.url()}`);
  });
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`console: ${message.text()}`);
  });
  await page.route('**/api/agents', async (route) => {
    // The Session home restores independently while shell configuration is
    // loading. Keep this response slow enough to deterministically exercise
    // the graph's pre-metadata build followed by its authoritative rebuild.
    await new Promise((resolve) => setTimeout(resolve, 600));
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(agents),
    });
  });
  await page.route('**/api/agents/browser-test-reviewer/status', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ agent_id: 'browser-test-reviewer', status: 'Idle' }),
  }));
  await page.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: '[]',
  }));
  await page.route(`**/api/sessions/${session.id}/turns`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify([failedTurn]),
  }));
  await page.route('**/api/sessions', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify([session]),
  }));
  await page.route(
    `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
    (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify([session]),
    }),
  );

  try {
    await page.goto(`${runtime.baseUrl}/?session=${encodeURIComponent(session.id)}`, {
      waitUntil: 'domcontentloaded',
    });
    await page.waitForFunction((sessionId) => {
      const graph = document.querySelector('#session-lattice-host ax-lattice');
      return document.querySelector('ax-rail')?.current === sessionId
        && graph?.getAttribute('aria-label')?.includes('1 configured dependency');
    }, session.id);

    await page.locator('#panes-menu-btn').click();
    await page.getByRole('menuitem', { name: 'Agent graph' }).click();
    const graph = page.locator('#session-lattice-host ax-lattice');
    await graph.waitFor({ state: 'visible' });
    assert.equal(
      await graph.getAttribute('aria-label'),
      `Agent dependency graph for ${session.name}: 2 agents and 1 configured dependency`,
    );
    assert.equal(await page.locator('#session-lattice-host ax-edge').count(), 1);
    assert.equal(
      await page.locator('#session-lattice-host ax-edge').getAttribute('aria-label'),
      'browser-test-reviewer depends on browser-test-coder',
    );
    assert.equal(
      await page.locator('#session-lattice-host #sl-browser-test-reviewer').getAttribute('aria-label'),
      'Agent browser-test-reviewer; depends on browser-test-coder',
    );
    await page.getByRole('button', { name: '← Conversation' }).click();
    const transcript = page.locator('#session-msgs');
    assert.match(await transcript.textContent(), /Invalidate catalog entries immediately/);
    assert.match(
      await transcript.textContent(),
      /Failed: agent 'browser-test-reviewer' completed without a user-visible output/,
    );
    assert.equal(
      await transcript.locator('[data-agent-id="browser-test-coder"]').count(),
      1,
      'the prior non-empty Agent output remains visible as evidence',
    );
    assert.equal(
      await transcript.locator('[data-agent-id="browser-test-reviewer"]').count(),
      0,
      'a blank downstream output is not rendered as a successful answer',
    );
    assert.deepEqual(
      [...errors, ...failedResponses],
      [],
      [...errors, ...failedResponses].join('\n'),
    );
  } finally {
    await context.close();
  }
});
