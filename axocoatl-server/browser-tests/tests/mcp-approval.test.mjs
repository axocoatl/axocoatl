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

function runningTurn({ id, sessionId, city, recordedAt }) {
  return {
    id,
    session_id: sessionId,
    user_input: `What is the current weather in ${city}?`,
    agent_id: 'browser-test-coder',
    context: [],
    status: 'running',
    partial_output: '',
    created_at: recordedAt,
    updated_at: recordedAt,
    idempotency_key: id,
    metadata: { mode: 'single_agent' },
    execution_events: [{
      operation_id: `run-started:${id}`,
      recorded_at: recordedAt,
      kind: 'run_started',
      execution_id: id,
    }],
    agent_outputs: [],
    superseded: false,
  };
}

function completedTurn(turn, { output, result, isError, recordedAt }) {
  return {
    ...turn,
    status: 'completed',
    partial_output: output,
    final_output: output,
    updated_at: recordedAt,
    completed_at: recordedAt,
    execution_events: [
      ...turn.execution_events,
      {
        operation_id: `tool-started:${turn.id}`,
        recorded_at: recordedAt - 2,
        kind: 'tool_started',
        execution_id: `weather:${turn.id}`,
        metadata: {
          agent_id: 'browser-test-coder',
          arguments: { city: turn.user_input.match(/in ([^?]+)/)?.[1] || '' },
          call_id: `weather:${turn.id}`,
          call_id_sha256: 'a'.repeat(64),
          occurrence: 0,
          tool_name: 'mcp__weather__get_weather',
        },
      },
      {
        operation_id: `tool-result:${turn.id}`,
        recorded_at: recordedAt - 1,
        kind: 'tool_result',
        execution_id: `weather:${turn.id}`,
        metadata: {
          agent_id: 'browser-test-coder',
          call_id: `weather:${turn.id}`,
          call_id_sha256: 'a'.repeat(64),
          occurrence: 0,
          tool_name: 'mcp__weather__get_weather',
          is_error: isError,
          result,
        },
      },
    ],
    agent_outputs: [{
      agent_id: 'browser-test-coder',
      output,
      recorded_at: recordedAt,
    }],
  };
}

function approval({ id, sessionId, city, requestedAt }) {
  return {
    approval_id: id,
    run: sessionId,
    agent_id: `${sessionId}:browser-test-coder`,
    server: 'weather',
    tool: 'mcp__weather__get_weather',
    tool_display: 'get_weather',
    arguments_preview: JSON.stringify({ city }),
    requested_at: requestedAt,
  };
}

function runSnapshot(sessionId, turnId, pendingApproval) {
  return {
    kind: 'snapshot',
    runs: turnId ? [{
      workflow: sessionId,
      turn_id: turnId,
      kind: 'session',
      agents: [],
      goal: '',
      subtasks: [],
      ...(pendingApproval ? {
        awaiting: {
          approval_id: pendingApproval.approval_id,
          question: `approve ${pendingApproval.tool_display} on ${pendingApproval.server}?`,
          since: pendingApproval.requested_at,
        },
      } : {}),
    }] : [],
    approvals: pendingApproval ? [pendingApproval] : [],
  };
}

test('a Session MCP decision survives reconnect and renders honest denied and approved evidence', { timeout: 90_000 }, async () => {
  const session = structuredClone(runtime.fixtures.alpha.sessions[0]);
  session.status = 'active';
  session.environment = {
    ...session.environment,
    state: 'ready',
    setup_reviewed: true,
    setup_results: [],
    error: null,
  };

  const paris = runningTurn({
    id: 'turn-mcp-deny',
    sessionId: session.id,
    city: 'Paris',
    recordedAt: 1_000,
  });
  const berlin = runningTurn({
    id: 'turn-mcp-allow',
    sessionId: session.id,
    city: 'Berlin',
    recordedAt: 2_000,
  });
  const parisApproval = approval({
    id: 'approval-paris',
    sessionId: session.id,
    city: 'Paris',
    requestedAt: 1_010,
  });
  const berlinApproval = approval({
    id: 'approval-berlin',
    sessionId: session.id,
    city: 'Berlin',
    requestedAt: 2_010,
  });

  let turns = [paris];
  let pendingApproval = parisApproval;
  let activeTurnId = paris.id;
  let socket = null;
  let socketConnections = 0;
  let toolDispatches = 0;
  const decisions = [];
  const errors = [];
  const failedResponses = [];

  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`console: ${message.text()}`);
  });
  page.on('response', (response) => {
    if (response.status() >= 400) failedResponses.push(`${response.status()} ${response.url()}`);
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
  await page.route(`**/api/sessions/${session.id}/turns`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify(turns),
  }));
  await page.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: '[]',
  }));
  await page.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: '[]',
  }));
  await page.route(`**/api/sessions/${session.id}/git/**`, (route) => {
    const pathname = new URL(route.request().url()).pathname;
    const body = pathname.endsWith('/git/status')
      ? { branch: 'main', files: [], clean: true }
      : pathname.endsWith('/git/branches')
        ? { current: 'main', branches: ['main'] }
        : {};
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(body),
    });
  });

  const send = (frame) => socket?.send(JSON.stringify(frame));
  await page.routeWebSocket('**/ws', (webSocket) => {
    socket = webSocket;
    socketConnections += 1;
    webSocket.onMessage((message) => {
      const command = JSON.parse(String(message));
      if (command.cmd === 'ping') {
        webSocket.send(JSON.stringify({ kind: 'pong' }));
        return;
      }
      if (command.cmd !== 'mcp-approve') return;
      decisions.push(command);

      if (command.approval_id === parisApproval.approval_id) {
        pendingApproval = null;
        turns = [completedTurn(paris, {
          output: 'The weather request was denied and did not run.',
          result: { error: 'User denied mcp__weather__get_weather on weather' },
          isError: true,
          recordedAt: 1_100,
        })];
        webSocket.send(JSON.stringify({
          kind: 'mcp-approval-resolved',
          approval_id: parisApproval.approval_id,
          decision: 'deny',
        }));
        webSocket.send(JSON.stringify({
          kind: 'session-done',
          session: session.id,
          turn_id: paris.id,
          input_tokens: 12,
          output_tokens: 9,
        }));
        return;
      }

      if (command.approval_id === berlinApproval.approval_id) {
        toolDispatches += 1;
        pendingApproval = null;
        turns = [
          turns[0],
          completedTurn(berlin, {
            output: 'The current weather in Berlin is 18°C, partly cloudy.',
            result: { text: 'Weather in Berlin: 18°C, partly cloudy.' },
            isError: false,
            recordedAt: 2_100,
          }),
        ];
        webSocket.send(JSON.stringify({
          kind: 'mcp-approval-resolved',
          approval_id: berlinApproval.approval_id,
          decision: 'allow',
        }));
        webSocket.send(JSON.stringify({
          kind: 'tool-call',
          workflow: session.id,
          agent: 'browser-test-coder',
          turn_id: berlin.id,
          call_id: `weather:${berlin.id}`,
          occurrence: 0,
          name: 'mcp__weather__get_weather',
          phase: 'start',
          arguments: { city: 'Berlin' },
          is_error: false,
        }));
        webSocket.send(JSON.stringify({
          kind: 'tool-call',
          workflow: session.id,
          agent: 'browser-test-coder',
          turn_id: berlin.id,
          call_id: `weather:${berlin.id}`,
          occurrence: 0,
          name: 'mcp__weather__get_weather',
          phase: 'result',
          result: { text: 'Weather in Berlin: 18°C, partly cloudy.' },
          is_error: false,
        }));
        webSocket.send(JSON.stringify({
          kind: 'session-done',
          session: session.id,
          turn_id: berlin.id,
          input_tokens: 13,
          output_tokens: 11,
        }));
      }
    });
    setTimeout(() => {
      if (socket === webSocket) {
        webSocket.send(JSON.stringify(runSnapshot(session.id, activeTurnId, pendingApproval)));
      }
    }, 20);
  });

  try {
    await page.goto(`${runtime.baseUrl}/?session=${encodeURIComponent(session.id)}`, {
      waitUntil: 'domcontentloaded',
    });
    const parisDialog = page.getByRole('dialog', { name: 'Allow MCP tool call?' });
    await parisDialog.waitFor({ state: 'visible' });
    assert.match(await parisDialog.innerText(), new RegExp(`${session.id}:browser-test-coder`));
    assert.match(await parisDialog.innerText(), /get_weather on weather/);
    assert.match(await parisDialog.innerText(), /"city":"Paris"/);
    assert.equal(toolDispatches, 0, 'the tool remains undispatched while approval is pending');

    await page.reload({ waitUntil: 'domcontentloaded' });
    await parisDialog.waitFor({ state: 'visible' });
    assert.ok(socketConnections >= 2, 'reload establishes a fresh live connection');
    assert.match(await parisDialog.innerText(), /"city":"Paris"/);
    assert.equal(toolDispatches, 0, 'reconnect cannot silently dispatch a pending call');

    await parisDialog.getByRole('button', { name: 'Deny', exact: true }).click();
    await parisDialog.waitFor({ state: 'hidden' });
    assert.deepEqual(decisions[0], {
      cmd: 'mcp-approve',
      approval_id: parisApproval.approval_id,
      decision: 'deny',
      persist: 'once',
    });
    assert.equal(toolDispatches, 0, 'denial never dispatches the MCP tool');
    await page.getByText('The weather request was denied and did not run.').waitFor();
    await page.getByText('User denied mcp__weather__get_weather on weather', { exact: true }).waitFor();

    activeTurnId = berlin.id;
    pendingApproval = berlinApproval;
    turns = [turns[0], berlin];
    send({ kind: 'session-accepted', session: session.id, turn_id: berlin.id });
    send({ kind: 'session-start', session: session.id, turn_id: berlin.id });
    send({ kind: 'mcp-approval-required', ...berlinApproval });

    const berlinDialog = page.getByRole('dialog', { name: 'Allow MCP tool call?' });
    await berlinDialog.waitFor({ state: 'visible' });
    assert.match(await berlinDialog.innerText(), /"city":"Berlin"/);
    assert.equal(toolDispatches, 0, 'an approved-path call also waits for the exact click');
    await berlinDialog.getByRole('button', { name: 'Allow once', exact: true }).click();
    await berlinDialog.waitFor({ state: 'hidden' });
    assert.deepEqual(decisions[1], {
      cmd: 'mcp-approve',
      approval_id: berlinApproval.approval_id,
      decision: 'allow',
      persist: 'once',
    });
    assert.equal(toolDispatches, 1, 'the allowed call dispatches exactly once');
    await page.getByRole('button', { name: /mcp__weather__get_weather.*Done/ }).last().click();
    await page.getByText('Weather in Berlin: 18°C, partly cloudy.').waitFor();
    await page.getByText('The current weather in Berlin is 18°C, partly cloudy.').waitFor();

    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.getByText('The weather request was denied and did not run.').waitFor();
    await page.getByText('User denied mcp__weather__get_weather on weather', { exact: true }).waitFor();
    await page.getByRole('button', { name: /mcp__weather__get_weather.*Done/ }).last().click();
    await page.getByText('Weather in Berlin: 18°C, partly cloudy.').waitFor();
    await page.getByText('The current weather in Berlin is 18°C, partly cloudy.').waitFor();
    assert.equal(await page.getByRole('dialog', { name: 'Allow MCP tool call?' }).count(), 0);
    assert.deepEqual(
      [...errors, ...failedResponses],
      [],
      [...errors, ...failedResponses].join('\n'),
    );
  } finally {
    await context.close();
  }
});
