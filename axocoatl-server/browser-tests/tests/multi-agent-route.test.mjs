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
  const completedTurn = {
    id: 'turn-historical-research-fixture',
    session_id: session.id,
    user_input: 'Research and summarize the earlier storage decision',
    agent_id: null,
    model: null,
    context: [],
    status: 'completed',
    partial_output: 'Use append-only storage for durable evidence.',
    final_output: 'Use append-only storage for durable evidence.',
    error: null,
    created_at: 500,
    updated_at: 900,
    completed_at: 900,
    idempotency_key: 'historical-research-fixture',
    metadata: { mode: 'custom' },
    execution_events: [
      {
        operation_id: 'coordination:turn-historical-research-fixture:0:planned',
        recorded_at: 525,
        kind: 'coordination_planned',
        execution_id: 'turn-historical-research-fixture',
        metadata: {
          agents: [
            { id: 'historical-researcher', name: 'Historical Researcher', depends_on: [] },
            { id: 'historical-writer', name: 'Historical Writer', depends_on: ['historical-researcher'] },
          ],
          roots: ['historical-researcher'],
          sinks: ['historical-writer'],
          max_generations: 2,
        },
      },
      {
        operation_id: 'coordination:turn-historical-research-fixture:1:agent-activated',
        recorded_at: 550,
        kind: 'coordination_agent_activated',
        execution_id: 'turn-historical-research-fixture',
        metadata: {
          agent_id: 'historical-researcher', generation: 1,
          cause_signal_ids: [], parents: [],
        },
      },
      {
        operation_id: 'coordination:turn-historical-research-fixture:2:agent-completed',
        recorded_at: 650,
        kind: 'coordination_agent_completed',
        execution_id: 'turn-historical-research-fixture',
        metadata: {
          agent_id: 'historical-researcher', generation: 1,
          signal_id: 'coordination-signal:turn-historical-research-fixture:historical-researcher:g1:completed',
          cause_signal_ids: [], summary: 'Append-only storage preserves the evidence chain.',
          usage: { input_tokens: 5, output_tokens: 3, reasoning_tokens: 0, total_tokens: 8, known: true },
        },
      },
      {
        operation_id: 'coordination:turn-historical-research-fixture:3:agent-activated',
        recorded_at: 700,
        kind: 'coordination_agent_activated',
        execution_id: 'turn-historical-research-fixture',
        metadata: {
          agent_id: 'historical-writer', generation: 1,
          cause_signal_ids: ['coordination-signal:turn-historical-research-fixture:historical-researcher:g1:completed'],
          parents: ['historical-researcher'],
        },
      },
      {
        operation_id: 'coordination:turn-historical-research-fixture:4:agent-completed',
        recorded_at: 800,
        kind: 'coordination_agent_completed',
        execution_id: 'turn-historical-research-fixture',
        metadata: {
          agent_id: 'historical-writer', generation: 1,
          signal_id: 'coordination-signal:turn-historical-research-fixture:historical-writer:g1:completed',
          cause_signal_ids: ['coordination-signal:turn-historical-research-fixture:historical-researcher:g1:completed'],
          summary: 'Use append-only storage for durable evidence.',
          usage: { input_tokens: 6, output_tokens: 4, reasoning_tokens: 0, total_tokens: 10, known: true },
        },
      },
      {
        operation_id: 'coordination:turn-historical-research-fixture:5:completed',
        recorded_at: 850,
        kind: 'coordination_completed',
        execution_id: 'turn-historical-research-fixture',
        metadata: {
          status: 'completed',
          agents: [
            { agent_id: 'historical-researcher', state: 'completed', generation: 1 },
            { agent_id: 'historical-writer', state: 'completed', generation: 1 },
          ],
          sinks: ['historical-writer'],
          usage: { input_tokens: 11, output_tokens: 7, reasoning_tokens: 0, total_tokens: 18, known: true },
        },
      },
    ],
    agent_outputs: [
      {
        operation_id: 'coordination-output:turn-historical-research-fixture:historical-researcher:g1',
        agent_id: 'historical-researcher', model: 'browser-test-model',
        output: 'Append-only storage preserves the evidence chain.',
        activation_generation: 1, disposition: 'completed',
        causal_signal_id: 'coordination-signal:turn-historical-research-fixture:historical-researcher:g1:completed',
        superseded: false, recorded_at: 650,
      },
      {
        operation_id: 'coordination-output:turn-historical-research-fixture:historical-writer:g1',
        agent_id: 'historical-writer', model: 'browser-test-model',
        output: 'Use append-only storage for durable evidence.',
        activation_generation: 1, disposition: 'completed',
        causal_signal_id: 'coordination-signal:turn-historical-research-fixture:historical-writer:g1:completed',
        superseded: false, recorded_at: 800,
      },
    ],
    superseded: false,
  };
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
    execution_events: [
      {
        operation_id: 'coordination:turn-blank-reviewer-fixture:0:planned', recorded_at: 1_025,
        kind: 'coordination_planned', execution_id: 'turn-blank-reviewer-fixture',
        metadata: {
          agents: [
            { id: 'browser-test-coder', name: 'Browser Test Coder', depends_on: [] },
            { id: 'browser-test-reviewer', name: 'Browser Test Reviewer', depends_on: ['browser-test-coder'] },
          ],
          roots: ['browser-test-coder'],
          sinks: ['browser-test-reviewer'],
          max_generations: 2,
        },
      },
      {
        operation_id: 'coordination:turn-blank-reviewer-fixture:1:agent-activated', recorded_at: 1_050,
        kind: 'coordination_agent_activated', execution_id: 'turn-blank-reviewer-fixture',
        metadata: {
          agent_id: 'browser-test-coder', generation: 1, cause_signal_ids: [], parents: [],
        },
      },
      {
        operation_id: 'coordination:turn-blank-reviewer-fixture:2:agent-completed', recorded_at: 1_100,
        kind: 'coordination_agent_completed', execution_id: 'turn-blank-reviewer-fixture',
        metadata: {
          agent_id: 'browser-test-coder', generation: 1,
          signal_id: 'coordination-signal:turn-blank-reviewer-fixture:browser-test-coder:g1:completed',
          cause_signal_ids: [],
          summary: 'Invalidate catalog entries immediately after each mutation.',
          usage: { input_tokens: 9, output_tokens: 5, reasoning_tokens: 0, total_tokens: 14, known: true },
        },
      },
      {
        operation_id: 'coordination:turn-blank-reviewer-fixture:3:agent-activated', recorded_at: 1_125,
        kind: 'coordination_agent_activated', execution_id: 'turn-blank-reviewer-fixture',
        metadata: {
          agent_id: 'browser-test-reviewer', generation: 1,
          cause_signal_ids: ['coordination-signal:turn-blank-reviewer-fixture:browser-test-coder:g1:completed'],
          parents: ['browser-test-coder'],
        },
      },
      {
        operation_id: 'coordination:turn-blank-reviewer-fixture:4:coordination_agent_failed', recorded_at: 1_150,
        kind: 'coordination_agent_failed', execution_id: 'turn-blank-reviewer-fixture',
        metadata: {
          agent_id: 'browser-test-reviewer', generation: 1,
          cause_signal_ids: ['coordination-signal:turn-blank-reviewer-fixture:browser-test-coder:g1:completed'],
          signal_id: 'coordination-signal:turn-blank-reviewer-fixture:browser-test-reviewer:g1:failed',
          summary: 'Agent completed without a user-visible output.',
          usage: { input_tokens: 8, output_tokens: 0, reasoning_tokens: 0, total_tokens: 8, known: true },
        },
      },
      {
        operation_id: 'coordination:turn-blank-reviewer-fixture:5:completed', recorded_at: 1_175,
        kind: 'coordination_completed', execution_id: 'turn-blank-reviewer-fixture',
        metadata: {
          status: 'failed',
          agents: [
            { agent_id: 'browser-test-coder', state: 'completed', generation: 1 },
            { agent_id: 'browser-test-reviewer', state: 'failed', generation: 1 },
          ],
          sinks: ['browser-test-reviewer'],
          usage: { input_tokens: 17, output_tokens: 5, reasoning_tokens: 0, total_tokens: 22, known: true },
        },
      },
    ],
    agent_outputs: [
      {
        operation_id: 'coordination-output:turn-blank-reviewer-fixture:browser-test-coder:g1',
        agent_id: 'browser-test-coder',
        model: 'browser-test-model',
        output: 'Invalidate catalog entries immediately after each mutation.',
        activation_generation: 1,
        disposition: 'completed',
        causal_signal_id: 'coordination-signal:turn-blank-reviewer-fixture:browser-test-coder:g1:completed',
        superseded: false,
        recorded_at: 1_100,
      },
      {
        operation_id: 'coordination-output:turn-blank-reviewer-fixture:browser-test-reviewer:g1',
        agent_id: 'browser-test-reviewer',
        model: 'browser-test-model',
        output: '',
        activation_generation: 1,
        disposition: 'failed',
        causal_signal_id: 'coordination-signal:turn-blank-reviewer-fixture:browser-test-reviewer:g1:failed',
        superseded: false,
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
  let normalTurnFixture = null;
  let nativeHistoryFixture = null;
  let nativeGraphWrongVersion = false;
  await page.route(`**/api/sessions/${session.id}/turns*`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify([completedTurn, failedTurn, ...(normalTurnFixture ? [normalTurnFixture] : []), ...(nativeHistoryFixture ? [{history_version: 'execution_v2', turn: nativeHistoryFixture}] : [])]),
  }));
  let controlPlaneReads = 0;
  let exactCommandFixture = false;
  let extendedCommandFixture = false;
  let lastCommandReceipt = null;
  const controlRequests = [];
  await page.route(`**/api/sessions/${session.id}/turns/*/control-plane`, (route) => {
    controlPlaneReads++;
    if (nativeHistoryFixture && route.request().url().includes(nativeHistoryFixture.turn_id)) {
      const available = value => ({status:'available',value}); const missing = {status:'not_recorded'};
      const row = nativeHistoryFixture.activations[0]; const exact = row.activation.activation;
      return route.fulfill({status:200,contentType:'application/json',body:JSON.stringify({
        schema_version:1,history_version:nativeGraphWrongVersion?'legacy_v1':'execution_v2',session_id:session.id,
        turn_id:nativeHistoryFixture.turn_id,state:nativeHistoryFixture.state,request:available(nativeHistoryFixture.request.content.display_input),
        turn_revision:available(nativeHistoryFixture.revision),graph_revision:available(1),
        nodes:[{node_id:exact.node_id,label:'Retained native reviewer',definition_id:'native-definition',definition:missing,dependencies:[],activations:[{
          reference:{kind:'exact',activation:exact},generation:available(exact.generation),state:row.activation.state,
          reason:missing,started_at:missing,completed_at:missing,input:missing,output:missing,partial_outputs:[],usage:missing,
          evidence:[],capabilities:{inspect:true,stop:{enabled:false},retry:{enabled:false},revise:{enabled:false}}}]}],edges:[],warnings:[],
      })});
    }
    if (normalTurnFixture && route.request().url().includes(normalTurnFixture.id)) {
      const available = value => ({ status: 'available', value });
      const missing = { status: 'not_recorded' };
      return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({
        schema_version: 1, history_version: 'legacy_v1', session_id: session.id, turn_id: normalTurnFixture.id,
        state: 'completed', request: available(normalTurnFixture.user_input), edges: [], warnings: [],
        nodes: [{ node_id: normalTurnFixture.agent_id, label: normalTurnFixture.agent_id, definition: missing,
          dependencies: [], activations: [{ reference: { kind: 'legacy', session_id: session.id,
            turn_id: normalTurnFixture.id, node_id: normalTurnFixture.agent_id, generation: null },
            generation: missing, state: 'completed', reason: missing, started_at: missing, completed_at: missing,
            input: missing, output: available(normalTurnFixture.final_output), partial_outputs: [], usage: missing,
            evidence: [], capabilities: { inspect: true, stop: { enabled: false }, retry: { enabled: false } } }] }],
      }) });
    }
    const turn = route.request().url().includes(completedTurn.id) ? completedTurn : failedTurn;
    const available = (value) => ({ status: 'available', value });
    const missing = { status: 'not_recorded' };
    const planned = turn.execution_events.find(event => event.kind === 'coordination_planned').metadata.agents;
    const nodes = planned.map(agent => {
      const output = turn.agent_outputs.find(item => item.agent_id === agent.id);
      const last = turn.execution_events.filter(event => event.metadata.agent_id === agent.id).at(-1);
      return { node_id: agent.id, definition_id: agent.id, label: agent.name || agent.id,
        definition: missing, dependencies: agent.depends_on || [], activations: [{
          reference: exactCommandFixture
            ? { kind: 'exact', activation: { session_id: session.id, turn_id: turn.id, node_id: agent.id,
              execution_epoch_id: 'epoch-browser-fixture', activation_id: `activation-${agent.id}`, generation: 1 } }
            : { kind: 'legacy', session_id: session.id, turn_id: turn.id, node_id: agent.id, generation: 1 },
          generation: available(1), state: extendedCommandFixture ? 'accepted' : exactCommandFixture ? 'running' : output.disposition, reason: available(last.metadata.summary),
          started_at: missing, completed_at: missing, input: missing, output: available(output.output),
          partial_outputs: [], usage: available(last.metadata.usage),
          capabilities: { inspect: true, stop: { enabled: exactCommandFixture && !lastCommandReceipt, reason: 'Legacy history' }, retry: { enabled: false, reason: 'Legacy history' }, revise: {enabled: extendedCommandFixture, reason: ''}, revise_invalidates: [] },
          evidence: [{ kind: last.kind, reference: available(last.operation_id), summary: available(last.metadata.summary),
            recorded_at: missing, details: available(last.metadata) }],
        }] };
    });
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({
      schema_version: 1, history_version: exactCommandFixture ? 'execution_v2' : 'legacy_v1', session_id: session.id, turn_id: turn.id,
      turn_revision: available(4), graph_revision: available(2), commands: available(lastCommandReceipt ? [lastCommandReceipt] : []),
      turn_controls: extendedCommandFixture ? {execution_epoch_id: 'epoch-browser-fixture', continue_turn: {enabled: true}, finish: {enabled: true},
        continuation_choices: [{activation: nodes[0].activations[0].reference.activation, state: 'failed', capability: {enabled: true}}], check_choices: []} : undefined,
      state: turn.status, request: available(turn.user_input), nodes, edges: [], warnings: [],
    }) });
  });
  await page.route(`**/api/sessions/${session.id}/turns/*/control-commands`, async (route) => {
    const request = route.request().postDataJSON();
    controlRequests.push(request);
    if (controlRequests.length === 1) {
      // A response without a recognizable receipt is uncertain. The browser
      // must retain this exact request and reuse its ID to confirm the outcome.
      return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
    }
    lastCommandReceipt = { request: { ...request, parameters: { kind: {stop:'stop_activation', revise:'revise_activation', continue:'continue_turn', finish:'finish_turn'}[request.action], ...(request.activation ? {activation: request.activation} : {}) } },
      source: { kind: 'human', session_id: request.session_id, turn_id: request.turn_id }, revision: 2,
      state: ['revise', 'continue'].includes(request.action) ? 'settled' : 'accepted', last_transition: { state: 'accepted' } };
    return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(lastCommandReceipt) });
  });
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
        && graph?.querySelectorAll('ax-edge').length === 1;
    }, session.id);

    await page.locator('#panes-menu-btn').click();
    await page.getByRole('menuitem', { name: 'Agent graph' }).click();
    const graph = page.locator('#session-lattice-host ax-lattice');
    await graph.waitFor({ state: 'visible' });
    assert.equal(
      await graph.getAttribute('aria-label'),
      `Agent execution graph for ${session.name}: 2 agents. Select an agent to inspect recorded evidence.`,
    );
    assert.equal(await page.locator('#session-lattice-host ax-edge').count(), 1);
    assert.equal(
      await page.locator('#session-lattice-host ax-edge').getAttribute('aria-label'),
      'browser-test-reviewer depends on browser-test-coder',
    );
    assert.equal(
      await page.locator('#session-lattice-host #sl-browser-test-reviewer').getAttribute('aria-label'),
      'Browser Test Reviewer; failed; depends on browser-test-coder',
    );
    await page.getByRole('button', { name: '← Conversation' }).click();
    const transcript = page.locator('#session-msgs');
    const historicalCoordination = transcript.locator(
      'ax-coordination-turn[data-turn-id="turn-historical-research-fixture"]',
    );
    const coordination = transcript.locator(
      'ax-coordination-turn[data-turn-id="turn-blank-reviewer-fixture"]',
    );
    await historicalCoordination.waitFor({ state: 'visible' });
    await coordination.waitFor({ state: 'visible' });
    assert.equal(await transcript.locator('ax-coordination-turn').count(), 2);
    assert.equal(
      await historicalCoordination.locator('.summary').textContent(),
      'Coordination · 2 agents · complete',
    );
    assert.equal(
      await coordination.locator('.summary').textContent(),
      'Coordination · 2 agents · failed',
    );
    assert.equal(
      await coordination.locator('.agent[data-agent-id="browser-test-reviewer"]').getAttribute('data-state'),
      'failed',
      'durable coordination events rebuild the failed node rather than resetting the graph to idle',
    );
    assert.match(await transcript.textContent(), /Invalidate catalog entries immediately/);
    assert.match(
      await transcript.textContent(),
      /Failed: agent 'browser-test-reviewer' completed without a user-visible output/,
    );
    assert.equal(
      await transcript.locator('.smsg[data-agent-id="browser-test-coder"]').count(),
      1,
      'the prior non-empty Agent output remains visible as evidence',
    );
    assert.equal(
      await transcript.locator('.smsg[data-agent-id="browser-test-reviewer"]').count(),
      0,
      'a blank downstream output is not rendered as a successful answer',
    );
    await historicalCoordination.getByRole('button', { name: 'Open Agent graph' }).click();
    await page.waitForFunction(() => {
      const host = document.querySelector('#session-lattice-host');
      return host?.querySelector('#sl-historical-researcher')?.getAttribute('status') === 'success'
        && host?.querySelector('#sl-historical-writer')?.getAttribute('status') === 'success';
    });
    assert.equal(await page.locator('#session-lattice-host ax-node').count(), 2);
    assert.equal(await page.locator('#session-lattice-host #sl-browser-test-coder').count(), 0);
    assert.equal(
      await page.locator('#session-lattice-host ax-edge').getAttribute('aria-label'),
      'historical-writer depends on historical-researcher',
    );
    await page.getByRole('button', { name: '← Conversation' }).click();
    await coordination.getByRole('button', { name: 'Open Agent graph' }).click();
    await page.waitForFunction(() => {
      const host = document.querySelector('#session-lattice-host');
      return host?.querySelector('#sl-browser-test-coder')?.getAttribute('status') === 'success'
        && host?.querySelector('#sl-browser-test-reviewer')?.getAttribute('status') === 'error';
    });
    assert.equal(await page.locator('#session-lattice-host ax-node').count(), 2);
    assert.equal(await page.locator('#session-lattice-host #sl-historical-researcher').count(), 0);
    assert.equal(
      await page.locator('#session-lattice-host #sl-browser-test-coder').getAttribute('status'),
      'success',
    );
    assert.equal(
      await page.locator('#session-lattice-host #sl-browser-test-reviewer').getAttribute('status'),
      'error',
    );
    assert.equal(
      await historicalCoordination.locator('.agent[data-agent-id="historical-writer"]').getAttribute('data-state'),
      'completed',
      'opening another historical turn must not mutate the first card snapshot',
    );
    assert.equal(await graph.getAttribute('mode'), 'view');
    assert.equal(await graph.getAttribute('aria-roledescription'), 'execution graph');
    await graph.locator('#sl-browser-test-reviewer').click();
    const inspector = page.locator('#session-lattice-host ax-activation-inspector');
    await inspector.waitFor({ state: 'visible' });
    await page.waitForFunction(() => document.querySelector('ax-activation-inspector')?.model?.controlPlane?.history_version === 'legacy_v1');
    assert.match(await inspector.locator('.content').textContent(), /Agent completed without a user-visible output/);
    assert.match(await inspector.locator('.content').textContent(), /Definition used for this turnNot recorded/);
    assert.equal(await inspector.getByText('Recorded empty output', { exact: true }).count(), 1);
    await page.waitForFunction(() => {
      const lattice = document.querySelector('#session-lattice-host ax-lattice');
      const selected = lattice?.querySelector('#sl-browser-test-reviewer');
      if (!selected) return false;
      const canvas = lattice.getBoundingClientRect(), node = selected.getBoundingClientRect();
      return node.left >= canvas.left && node.right <= canvas.right && node.top >= canvas.top && node.bottom <= canvas.bottom;
    });
    assert.equal(await inspector.locator('.stop:visible,.retry:visible').count(), 0);
    await inspector.getByRole('button', { name: 'Add Browser Test Reviewer · event · generation 1 to chat', exact: true }).click();
    const referenceChip = page.locator('#chat-refs .chat-ref');
    await referenceChip.waitFor({ state: 'visible' });
    assert.match(await referenceChip.textContent(), /Browser Test Reviewer · event · generation 1/);
    const canonicalReference = await page.evaluate(() => canonicalInlineReferences(S.session.refs)[0]);
    assert.equal(canonicalReference.kind, 'coordination_reference');
    assert.equal(canonicalReference.metadata.source_session_id, session.id);
    assert.equal(canonicalReference.metadata.source_turn_id, failedTurn.id);
    assert.equal(canonicalReference.metadata.reference_id, failedTurn.execution_events[4].operation_id);
    assert.equal(canonicalReference.metadata.type, 'event');
    assert.doesNotMatch(JSON.stringify(canonicalReference), /Agent completed without a user-visible output/);
    await referenceChip.getByRole('button', { name: 'Inspect Browser Test Reviewer · event · generation 1', exact: true }).click();
    await inspector.waitFor({ state: 'visible' });
    assert.match(await inspector.locator('.content').textContent(), /Agent completed without a user-visible output/);
    await inspector.getByRole('button', { name: 'Close', exact: true }).click();
    await page.getByRole('button', { name: '← Conversation' }).click();
    await referenceChip.getByRole('button', { name: 'Remove Browser Test Reviewer · event · generation 1 from this turn', exact: true }).click();
    assert.equal(await page.locator('#chat-refs .chat-ref').count(), 0);
    await coordination.getByRole('button', { name: 'Open Agent graph' }).click();
    await graph.locator('#sl-browser-test-reviewer').click();
    const nodePositions = await graph.locator('ax-node').evaluateAll(nodes => nodes.map(node => [node.id, node.x, node.y]));
    await graph.focus();
    await page.keyboard.press('Delete');
    await page.keyboard.press('ArrowRight');
    assert.deepEqual(await graph.locator('ax-node').evaluateAll(nodes => nodes.map(node => [node.id, node.x, node.y])), nodePositions);
    await graph.locator('#sl-browser-test-reviewer').click();
    const previousReads = controlPlaneReads;
    await page.evaluate(async () => {
      const turnId = 'turn-blank-reviewer-fixture';
      await showCoordinationGraph(turnId);
    });
    await page.waitForFunction(() => document.querySelector('ax-activation-inspector')?.model?.controlPlane?.history_version === 'legacy_v1');
    assert.ok(controlPlaneReads > previousReads, 'reopening refreshes authoritative evidence');
    assert.deepEqual(await graph.evaluate(node => node.selectedIds()), ['sl-browser-test-reviewer']);
    assert.match(await inspector.locator('.content').textContent(), /Agent completed without a user-visible output/);
    if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) {
      await page.screenshot({ path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/workbench-graph.png`, fullPage: true });
    }
    for (const [name, theme, viewport, reducedMotion] of [
      ['wide-dark', 'dark', {width:1280,height:800}, false],
      ['narrow-light', 'light', {width:390,height:844}, false],
      ['narrow-dark-reduced', 'dark', {width:390,height:844}, true],
    ]) {
      await page.evaluate(theme => { document.documentElement.dataset.theme = theme; }, theme);
      await page.setViewportSize(viewport);
      await page.emulateMedia({reducedMotion: reducedMotion ? 'reduce' : 'no-preference'});
      await page.waitForFunction(narrow => {
        const dialog = document.querySelector('#session-lattice-host ax-activation-inspector')?.shadowRoot.querySelector('dialog');
        return dialog?.matches(':modal') === narrow;
      }, viewport.width < 720);
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      assert.equal(await page.locator('#session-lattice-host ax-activation-inspector').count(), 1);
      assert.equal(await inspector.locator('dialog[open]').count(), 1);
      assert.equal(await inspector.locator('header:visible').count(), 1);
      if (viewport.width < 720) {
        const bounds = await inspector.locator('dialog[open]').boundingBox();
        assert.ok(bounds.x >= 0 && bounds.y >= 0 && bounds.x + bounds.width <= viewport.width
          && bounds.y + bounds.height <= viewport.height, `${name} keeps its dialog in the viewport`);
      }
      if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) {
        await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/workbench-graph-${name}.png`, fullPage:true});
      }
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${name} has no page overflow`);
    }
    await page.setViewportSize({width:1280,height:800});
    await page.emulateMedia({reducedMotion:'no-preference'});
    await page.evaluate(() => { document.documentElement.dataset.theme = 'light'; });
    await inspector.getByRole('button', { name: 'Close', exact: true }).click();
    await page.getByRole('button', { name: '← Conversation' }).click();
    await page.reload({ waitUntil: 'domcontentloaded' });
    const restored = page.locator('ax-coordination-turn[data-turn-id="turn-blank-reviewer-fixture"]');
    await restored.waitFor({ state: 'visible' });
    await restored.getByRole('button', { name: 'Open Agent graph' }).click();
    await page.locator('#sl-browser-test-reviewer').click();
    await inspector.waitFor({ state: 'visible' });
    assert.match(await inspector.locator('.content').textContent(), /Agent completed without a user-visible output/);
    exactCommandFixture = true;
    await page.evaluate(() => showCoordinationGraph('turn-blank-reviewer-fixture'));
    await page.waitForFunction(() => document.querySelector('ax-activation-inspector')?.model?.controlPlane?.history_version === 'execution_v2');
    const stopActivation = inspector.getByRole('button', { name: 'Stop', exact: true });
    await stopActivation.click();
    await inspector.getByRole('button', { name: 'Check command status' }).waitFor({ state: 'visible' });
    assert.match(await inspector.locator('.command-receipt').textContent(), /Outcome unknown/);
    assert.equal(await stopActivation.isDisabled(), true);
    await inspector.getByRole('button', { name: 'Check command status' }).click();
    await page.waitForFunction(() => document.querySelector('ax-activation-inspector')?.commandHistory?.some(item => item.receipt?.state === 'accepted'));
    assert.equal(controlRequests.length, 2);
    assert.deepEqual(controlRequests[1], controlRequests[0], 'uncertain command retry keeps the complete original identity and revisions');
    assert.equal(controlRequests[0].activation.node_id, 'browser-test-reviewer');
    assert.equal(controlRequests[0].expected_turn_revision, 4);
    assert.equal(controlRequests[0].expected_graph_revision, 2);
    assert.match(await inspector.locator('.command-receipt').textContent(), /Accepted · waiting for application/);
    assert.doesNotMatch(await inspector.locator('.command-receipt').textContent(), /Settled/);
    lastCommandReceipt = { ...lastCommandReceipt, revision: 4, state: 'settled', last_transition: { state: 'settled' } };
    await inspector.getByRole('button', { name: 'Check command status' }).click();
    await page.waitForFunction(() => document.querySelector('ax-activation-inspector')?.commandHistory?.some(item => item.receipt?.state === 'settled'));
    assert.equal(controlRequests.length, 2, 'confirmed pending receipts refresh with GET without issuing another command');
    assert.match(await inspector.locator('.command-receipt').textContent(), /Settled/);
    extendedCommandFixture = true;
    await page.evaluate(() => showCoordinationGraph('turn-blank-reviewer-fixture'));
    await inspector.getByLabel('Revision instruction').fill('Explain the exact failure.');
    await inspector.locator('.revision-context').check();
    await inspector.getByRole('button', {name: 'Revise', exact: true}).click();
    await page.waitForFunction(() => document.querySelector('ax-activation-inspector')?.commandHistory?.some(item => item.request.action === 'revise' && item.receipt?.state === 'settled'));
    assert.equal(controlRequests[2].action, 'revise');
    assert.equal(controlRequests[2].instruction, 'Explain the exact failure.');
    assert.equal(controlRequests[2].include_previous_output, true);
    assert.equal(controlRequests[2].activation.node_id, 'browser-test-reviewer');
    await page.locator('.session-turn-controls').click();
    assert.equal(await inspector.locator('.continue-turn').isDisabled(), true);
    await inspector.locator('.continue-work').check();
    await inspector.locator('.continue-turn').click();
    await page.waitForFunction(() => document.querySelector('ax-activation-inspector')?.commandHistory?.some(item => item.request.action === 'continue' && item.receipt?.state === 'settled'));
    assert.equal(controlRequests[3].action, 'continue');
    assert.equal(controlRequests[3].activation, undefined);
    assert.equal(controlRequests[3].continuation.restart[0].node_id, 'browser-test-coder');
    assert.deepEqual(controlRequests[3].continuation.checks, []);
    await inspector.locator('.finish-turn').click();
    await page.waitForFunction(() => document.querySelector('ax-activation-inspector')?.commandHistory?.some(item => item.request.action === 'finish' && item.receipt?.state === 'accepted'));
    assert.equal(controlRequests[4].action, 'finish');
    assert.equal(controlRequests[4].activation, undefined);
    assert.equal(controlRequests[4].mode, undefined, 'browser cannot choose forced Finish');
    assert.equal(new Set(controlRequests.slice(1).map(item => item.command_id)).size, 4);
    await page.reload({waitUntil: 'domcontentloaded'});
    await page.locator('ax-coordination-turn[data-turn-id="turn-blank-reviewer-fixture"]').getByRole('button', {name: 'Open Agent graph'}).click();
    await page.locator('.session-turn-controls').click();
    assert.match(await inspector.locator('.content').textContent(), /Revise · Settled/);
    assert.match(await inspector.locator('.content').textContent(), /Continue · Settled/);
    assert.match(await inspector.locator('.content').textContent(), /Finish · Accepted · waiting for application/);
    await inspector.getByRole('button', {name: 'Check command status'}).click();
    assert.equal(controlRequests.length, 5, 'reload and receipt refresh do not replay extended commands');
    normalTurnFixture = { ...failedTurn, id: 'turn-direct-inspection-fixture', agent_id: 'browser-test-reviewer',
      user_input: 'Review the client change directly', execution_events: [], agent_outputs: [],
      status: 'completed', final_output: 'DIRECT_TARGET_EVIDENCE', partial_output: '', error: null,
      metadata: { target_agent: 'browser-test-reviewer' } };
    await inspector.getByRole('button', { name: 'Close', exact: true }).click();
    await page.getByRole('button', { name: '← Conversation' }).click();
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.waitForFunction(() => document.querySelector('#session-lattice-host ax-lattice')?.dataset.turnId === 'turn-direct-inspection-fixture');
    await page.locator('#panes-menu-btn').click();
    await page.getByRole('menuitem', { name: 'Agent graph' }).click();
    await graph.waitFor({ state: 'visible' });
    assert.equal(await graph.locator('ax-node').count(), 1, 'direct-target historical membership does not expand to the current team');
    assert.equal(await graph.locator('ax-edge').count(), 0);
    await graph.locator('#sl-browser-test-reviewer').click();
    await inspector.waitFor({ state: 'visible' });
    assert.match(await inspector.locator('.content').textContent(), /DIRECT_TARGET_EVIDENCE/);
    assert.match(await inspector.locator('.content').textContent(), /GenerationNot recorded/);
    assert.equal(await inspector.locator('.stop:visible,.retry:visible').count(), 0);
    const nativeExact = {session_id:session.id,turn_id:'native-history-inspection',execution_epoch_id:'native-epoch',
      node_id:'retained-native-reviewer',generation:6,activation_id:'native-activation-6'};
    nativeHistoryFixture = {owner:{session_id:session.id,workspace_id:session.workspace_id},turn_id:nativeExact.turn_id,
      revision:7,state:'needs_attention',epochs:[],request:{status:'available',reference:'native-request',content:{
        turn_id:nativeExact.turn_id,display_input:'Inspect native retained work',effective_input:'same',recorded_at_unix_ms:3000,context:[]}},
      activations:[{activation:{activation:nativeExact,state:'interrupted'},currently_accepted:false,output:{status:'not_recorded'},
        partial_outputs:[],reserved_outputs:[{reference:'native-partial',content:{output:{activation:nativeExact,kind:'partial',text:'NATIVE_PARTIAL_EVIDENCE'},
          original_byte_len:100,original_sha256:'a'.repeat(64)}}],stream:[]}]};
    await inspector.getByRole('button',{name:'Close',exact:true}).click();
    await page.getByRole('button',{name:'← Conversation'}).click();
    await page.reload({waitUntil:'domcontentloaded'});
    const nativeUser = page.locator('#session-msgs .smsg.user[data-turn-id="native-history-inspection"]');
    await nativeUser.waitFor({state:'visible'});
    assert.match(await page.locator('#session-msgs').textContent(),/generation 6 · interrupted · partial output · truncated/);
    assert.equal(await nativeUser.getByRole('button',{name:'Rewind',exact:false}).count(),0,'unfinished native work does not offer rewind');
    await nativeUser.getByRole('button',{name:'Open Agent graph'}).click();
    await page.waitForFunction(()=>document.querySelector('#session-lattice-host ax-lattice')?.dataset.turnId==='native-history-inspection');
    assert.equal(await graph.locator('ax-node').count(),1);
    assert.equal(await graph.locator('#sl-retained-native-reviewer').count(),1,'native graph uses the retained node rather than current Settings');
    await graph.locator('#sl-retained-native-reviewer').click();
    await inspector.waitFor({state:'visible'});
    await page.evaluate(async()=>{
      window.nativeSelectionRefreshGraph=document.querySelector('#session-lattice-host ax-lattice');
      await showCoordinationGraph('native-history-inspection');
    });
    // An in-flight read can schedule a newer canonical render after the joined
    // promise resolves. Observe that replacement, not a transient old graph.
    await page.waitForFunction(()=>{
      const current=document.querySelector('#session-lattice-host ax-lattice');
      const model=document.querySelector('#session-lattice-host ax-activation-inspector')?.model;
      return current!==window.nativeSelectionRefreshGraph
        && current?.dataset.turnId==='native-history-inspection'
        && model?.controlPlane?.turn_revision?.value===7
        && current.selectedIds().length===1
        && current.selectedIds()[0]==='sl-retained-native-reviewer';
    });
    assert.deepEqual(await graph.evaluate(el=>el.selectedIds()),['sl-retained-native-reviewer'],'canonical refresh preserves the exact selected node');
    assert.match(await inspector.locator('.content').textContent(),/Generation6/);
    await page.evaluate(({sessionId,turnId})=>handleWsFrame({kind:'token',workflow:sessionId,turn_id:turnId,agent:'retained-native-reviewer',delta:'LEGACY_TOKEN_MUST_NOT_APPEND'}),{sessionId:session.id,turnId:nativeExact.turn_id});
    assert.doesNotMatch(await page.locator('#session-msgs').textContent(),/LEGACY_TOKEN_MUST_NOT_APPEND/);
    const nativeRewind = await page.evaluate(() => {
      const mode = S.session.mode; const agents = S.agents;
      S.session.mode = {kind:'single_agent',agent_id:'native-fixture-autonomous'};
      S.agents = [...agents,{id:'native-fixture-autonomous',role:'autonomous'}];
      const supported = sessionSupportsRewind();
      const actions = sessionActionRow({role:'user',text:'Native task',keepThroughTurnId:null});
      const visibleRewind = [...actions.querySelectorAll('button')].some(button => button.textContent.includes('Rewind'));
      S.session.mode = mode; S.agents = agents;
      return {supported,visibleRewind};
    });
    assert.deepEqual(nativeRewind,{supported:true,visibleRewind:false},'native single-Agent history supports rewind, while unfinished work does not offer it');
    nativeGraphWrongVersion = true;
    await page.evaluate(()=>showCoordinationGraph('native-history-inspection'));
    assert.equal(await page.locator('#session-lattice-host ax-node').count(),0);
    assert.match(await page.locator('#session-lattice-host .session-graph-notice').textContent(),/requires its exact execution graph/);
    nativeGraphWrongVersion = false;
    assert.deepEqual(
      [...errors, ...failedResponses],
      [],
      [...errors, ...failedResponses].join('\n'),
    );
  } finally {
    await context.close();
  }
});
