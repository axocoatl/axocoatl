import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { launchTestDaemon } from '../support/daemon.mjs';

// This product-seam test provisions a real Session runtime. Select an existing,
// repository-tool-capable Podman image explicitly; ordinary browser tests do
// not require a running VM. No daemon route or environment state is mocked.
// AXOCOATL_REFERENCE_RUNTIME_IMAGE=localhost/axocoatl-one-app-demo:latest \
// AXOCOATL_E2E_BINARY=/absolute/path/to/axocoatl node --test this-file.mjs
const runtimeImage = process.env.AXOCOATL_REFERENCE_RUNTIME_IMAGE;
const agentId = 'browser-test-coder';
const sourceTurnId = 'reference-source-turn';
const outputReference = 'reference-source-output';
const sourceOutput = 'Recorded QA evidence α: checkout rejects expired credentials.\nExact retained line two.';
const providerOutput = 'The recorded QA evidence has been reviewed.';

async function fixtureProvider() {
  const requests = [];
  const server = createServer(async (request, response) => {
    if (request.method === 'HEAD') {
      response.writeHead(200).end();
      return;
    }
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
    if (request.method !== 'POST' || request.url !== '/v1/chat/completions') {
      response.writeHead(404).end();
      return;
    }
    let body = '';
    for await (const chunk of request) body += chunk;
    let payload;
    try { payload = JSON.parse(body); } catch {
      response.writeHead(400).end();
      return;
    }
    requests.push(payload);
    const id = `reference-fixture-${requests.length}`;
    const first = {
      id, model: 'browser-test-model',
      choices: [{ index: 0, delta: { content: providerOutput }, finish_reason: null }],
    };
    const done = {
      id, model: 'browser-test-model',
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
      usage: { prompt_tokens: 37, completion_tokens: 9 },
    };
    response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    response.end(`data: ${JSON.stringify(first)}\n\ndata: ${JSON.stringify(done)}\n\ndata: [DONE]\n\n`);
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  return {
    requests,
    baseUrl: `http://127.0.0.1:${server.address().port}`,
    async close() {
      server.closeAllConnections();
      await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
    },
  };
}

function retainedSource(sessionId, turnId, referenceId, output) {
  const recordedAt = Date.now() - 60_000;
  return [
    {
      schema_version: 1, operation_id: `begin:${turnId}`, recorded_at: recordedAt,
      kind: 'begin', turn: {
        id: turnId, session_id: sessionId, user_input: 'Record QA findings.',
        agent_id: agentId, status: 'running', partial_output: '',
        created_at: recordedAt, updated_at: recordedAt, execution_events: [], agent_outputs: [],
      },
    },
    {
      schema_version: 1, operation_id: `plan:${turnId}`, recorded_at: recordedAt + 1,
      kind: 'execution', turn_id: turnId, execution: {
        kind: 'coordination_planned', execution_id: turnId,
        metadata: { agents: [{ id: agentId, label: 'Recorded QA coder', depends_on: [] }] },
      },
    },
    {
      schema_version: 1, operation_id: `activation:${turnId}`, recorded_at: recordedAt + 2,
      kind: 'execution', turn_id: turnId, execution: {
        kind: 'coordination_agent_activated', execution_id: turnId,
        metadata: { agent_id: agentId, generation: 1 },
      },
    },
    {
      schema_version: 1, operation_id: referenceId, recorded_at: recordedAt + 3,
      kind: 'agent_output', turn_id: turnId, agent_id: agentId,
      model: 'browser-test-model', output, attempt_id: null,
      identity: { activation_generation: 1, disposition: 'completed' },
    },
    {
      schema_version: 1, operation_id: `terminal:${turnId}`, recorded_at: recordedAt + 4,
      kind: 'transition', turn_id: turnId,
      transition: { status: 'completed', final_output: output, error: null },
    },
  ];
}

function reference(sessionId, overrides = {}) {
  return {
    reference_id: outputReference, display_name: 'Untrusted browser label',
    kind: 'coordination_reference', scope: 'this_turn', origin: null,
    metadata: {
      history_version: 'legacy_v1', source_session_id: sessionId,
      source_turn_id: sourceTurnId, node_id: agentId, generation: 1,
      reference_id: outputReference, type: 'output', ...overrides,
    },
  };
}

async function jsonApi(runtime, pathname, options) {
  const response = await fetch(`${runtime.baseUrl}${pathname}`, {
    ...options, signal: AbortSignal.timeout(60_000),
  });
  const text = await response.text();
  assert.ok(response.ok, `${pathname}: HTTP ${response.status}: ${text}\n${runtime.logs()}`);
  return text ? JSON.parse(text) : null;
}

function sendTurn(runtime, sessionId, turnId, references, input = 'Use the attached QA findings to explain the failure.') {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`${runtime.baseUrl.replace(/^http/, 'ws')}/ws`, {
      headers: { authorization: `Bearer ${runtime.token}` },
    });
    const frames = [];
    let settled = false;
    const finish = (error, frame) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.close();
      if (error) reject(error);
      else resolve({ terminal: frame, frames });
    };
    const timer = setTimeout(() => finish(new Error(
      `Turn ${turnId} timed out. Frames: ${JSON.stringify(frames)}\n${runtime.logs()}`,
    )), 45_000);
    socket.addEventListener('error', () => finish(new Error(`WebSocket failed for ${turnId}`)));
    socket.addEventListener('close', () => {
      if (!settled) finish(new Error(`WebSocket closed before terminal for ${turnId}`));
    });
    socket.addEventListener('message', ({ data }) => {
      let frame;
      try { frame = JSON.parse(data); } catch (error) { finish(error); return; }
      // Send after the authoritative reconnect snapshot so the request's
      // Accepted/terminal publications cannot be swallowed by its cursor.
      if (frame.kind === 'snapshot') {
        socket.send(JSON.stringify({
          cmd: 'session', id: sessionId, turn_id: turnId, idempotency_key: turnId,
          input, display_input: input, reference_ids: [], context_references: references,
        }));
      }
      if (frame.kind === 'error') finish(new Error(JSON.stringify(frame)));
      if (frame.session !== sessionId || frame.turn_id !== turnId) return;
      frames.push(frame);
      if (['session-done', 'session-error', 'session-request-rejected'].includes(frame.kind)) {
        finish(null, frame);
      }
    });
  });
}

test('actual Begin resolves retained evidence into provider input and pins it across restart', {
  skip: runtimeImage ? false : 'Set AXOCOATL_REFERENCE_RUNTIME_IMAGE to run with a real Podman Session runtime.',
  timeout: 240_000,
}, async (t) => {
  let runtime;
  let session;
  let projectRoot;
  const provider = await fixtureProvider();
  try {
    runtime = await launchTestDaemon({ ollamaBaseUrl: provider.baseUrl });
    // Sandbox admission correctly forbids mounting a repository containing
    // daemon configuration, history, or its socket. The read-only browser
    // fixtures share that root, so this executing Session needs a sibling.
    projectRoot = await mkdtemp(path.join(tmpdir(), 'axocoatl-reference-workspace-'));
    const packageDefinition = { name: 'coordination-reference-fixture', version: '0.0.0', private: true };
    await writeFile(path.join(projectRoot, 'package.json'), JSON.stringify(packageDefinition));
    await writeFile(path.join(projectRoot, 'package-lock.json'), JSON.stringify({
      ...packageDefinition, lockfileVersion: 3, requires: true,
      packages: { '': { name: packageDefinition.name, version: packageDefinition.version } },
    }));
    const workspace = await jsonApi(runtime, '/api/workspaces', {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ path: await realpath(projectRoot), name: 'Coordination reference workspace' }),
    });
    session = await jsonApi(runtime, `/api/workspaces/${workspace.id}/sessions`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        name: 'Coordination reference execution',
        mode: { kind: 'single_agent', agent_id: agentId }, enabled_skills: [], exposed_ports: [],
        setup_approved: false, setup_reviewed: false,
      }),
    });
    assert.equal(session.environment.state, 'awaiting_approval');
    const foreignSession = runtime.fixtures.beta.sessions[0];
    const configPath = path.join(runtime.runRoot, 'axocoatl.e2e.yaml');
    const originalConfig = await readFile(configPath, 'utf8');
    assert.ok(originalConfig.includes('allow_untrusted_images: false'));
    await writeFile(configPath, originalConfig
      .replace('allow_untrusted_images: false', 'allow_untrusted_images: true')
      .replace('Browser regression fixture. No turns are executed.', 'Answer the user without using tools.'));
    await runtime.restartWithSessionTurnEvents([
      ...retainedSource(session.id, sourceTurnId, outputReference, sourceOutput),
      ...retainedSource(foreignSession.id, 'foreign-source-turn', 'foreign-source-output', 'Foreign private evidence.'),
    ]);

    const prepared = await jsonApi(runtime, `/api/sessions/${session.id}/environment`, {
      method: 'PUT', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ image: runtimeImage, setup_command: null, setup_approved: false, setup_reviewed: true }),
    });
    assert.equal(prepared.environment.state, 'ready',
      `Real runtime preparation must succeed: ${JSON.stringify(prepared.environment)}\n${runtime.logs()}`);
    const source = await jsonApi(runtime, `/api/sessions/${session.id}/turns/${sourceTurnId}/control-plane`);
    const activation = source.nodes.find((node) => node.node_id === agentId)?.activations
      .find((item) => item.reference.generation === 1);
    assert.equal(activation?.output?.value, sourceOutput);
    assert.ok(activation.evidence.some((item) => item.reference?.value === outputReference));

    const ledgerPath = path.join(runtime.runRoot, 'data', 'session-history', 'turns.v1.jsonl');
    const rejectedCases = [
      ['body', reference(session.id, { content: 'FORGED_BODY_MUST_NEVER_REACH_PROVIDER' }), /invalid coordination reference/],
      ['hash', { ...reference(session.id), content_sha256: 'f'.repeat(64) }, /server-resolved content/],
      ['owner', reference(session.id, { source_session_id: foreignSession.id }), /foreign source identity/],
      ['foreign-turn', {
        ...reference(session.id, { source_turn_id: 'foreign-source-turn', reference_id: 'foreign-source-output' }),
        reference_id: 'foreign-source-output',
      }, /not found|belongs to|unavailable/],
      ['generation', reference(session.id, { generation: 2 }), /source activation is unavailable/],
    ];
    for (const [name, invalid, errorPattern] of rejectedCases) {
      const before = await readFile(ledgerPath);
      const turnId = `reference-rejected-${name}`;
      const result = await sendTurn(runtime, session.id, turnId, [invalid]);
      assert.ok(['session-error', 'session-request-rejected'].includes(result.terminal.kind), JSON.stringify(result));
      assert.match(result.terminal.error, errorPattern, 'the rejection must concern the exact invalid reference');
      assert.ok(!result.frames.some((frame) => frame.kind === 'session-accepted'));
      const missing = await fetch(`${runtime.baseUrl}/api/sessions/${session.id}/turns/${turnId}`);
      assert.equal(missing.status, 404, 'invalid evidence must be rejected before durable Begin');
      assert.deepEqual(await readFile(ledgerPath), before, 'invalid evidence must not mutate the turn ledger');
      assert.equal(provider.requests.length, 0, 'invalid evidence must not start a provider invocation');
    }
    t.diagnostic('Forged body/hash, foreign owner/turn, and wrong generation rejected before Begin or provider dispatch.');

    const turnId = 'reference-consumer-turn';
    const input = 'Use the attached QA findings to explain the failure.';
    const selectedReference = reference(session.id);
    const result = await sendTurn(runtime, session.id, turnId, [selectedReference], input);
    assert.equal(result.terminal.kind, 'session-done', `${JSON.stringify(result)}\n${runtime.logs()}`);
    assert.ok(result.frames.some((frame) => frame.kind === 'session-accepted'));
    assert.equal(provider.requests.length, 1);
    assert.equal(provider.requests[0].model, 'browser-test-model');
    const lastUser = provider.requests[0].messages.filter((message) => message.role === 'user').at(-1);
    assert.equal(typeof lastUser?.content, 'string');
    assert.ok(lastUser.content.includes(sourceOutput), 'the new provider request must include the exact retained bytes');
    assert.ok(lastUser.content.endsWith(input), 'the accepted user request remains intact');
    assert.equal(lastUser.content.split(sourceOutput).length - 1, 1, 'attached bytes must not be duplicated');
    assert.ok(!lastUser.content.includes('Untrusted browser label'), 'display identity is resolved by the daemon');

    const endpoint = `/api/sessions/${session.id}/turns/${turnId}`;
    const accepted = await jsonApi(runtime, endpoint);
    assert.equal(accepted.user_input, input, 'the visible transcript must retain plain composer text');
    assert.equal(accepted.final_output, providerOutput);
    assert.equal(accepted.context.length, 1);
    const captured = accepted.context[0];
    assert.equal(captured.kind, 'coordination_reference');
    assert.equal(captured.reference_id, `context:${turnId}:0`);
    assert.equal(captured.metadata.reference_id, outputReference);
    assert.equal(captured.metadata.content, sourceOutput);
    assert.equal(captured.content_sha256, createHash('sha256').update(sourceOutput).digest('hex'));
    const ledgerEvents = (await readFile(ledgerPath, 'utf8')).trim().split('\n').map((line) => JSON.parse(line));
    const begin = ledgerEvents.find((event) => event.kind === 'begin' && event.turn.id === turnId);
    assert.ok(begin, 'the resolver must capture bytes in the actual durable Begin, not a later view');
    assert.deepEqual(begin.turn.context, accepted.context);
    t.diagnostic('The actual Ollama request and fsynced Begin contain the same exact retained evidence and SHA-256.');

    await runtime.restart();
    assert.deepEqual((await jsonApi(runtime, endpoint)).context, accepted.context);
    const beforeReplay = await readFile(ledgerPath);
    const replay = await sendTurn(runtime, session.id, turnId, [selectedReference], input);
    assert.equal(replay.terminal.kind, 'session-done', JSON.stringify(replay));
    assert.equal(provider.requests.length, 1, 'exact terminal retry must return the retained outcome without invoking the provider');
    assert.deepEqual(await readFile(ledgerPath), beforeReplay, 'exact terminal retry must not append another Begin');
    assert.deepEqual((await jsonApi(runtime, endpoint)).context, accepted.context);
    const conflict = await sendTurn(runtime, session.id, turnId, [selectedReference], `${input} Changed request.`);
    assert.ok(['session-error', 'session-request-rejected'].includes(conflict.terminal.kind), JSON.stringify(conflict));
    assert.match(conflict.terminal.error, /conflict|different|mismatch|already/i);
    assert.equal(provider.requests.length, 1, 'an incompatible retry must not repeat the provider request');
    assert.deepEqual(await readFile(ledgerPath), beforeReplay, 'an incompatible retry must not change the accepted Begin');
    assert.deepEqual((await jsonApi(runtime, endpoint)).context, accepted.context);
    t.diagnostic('Daemon restart and exact retry preserve captured bytes and return SessionDone without another provider call; conflicting retry cannot replace Begin.');
  } finally {
    // Only this isolated fixture Session is eligible for container/volume
    // deletion. Ask the daemon to perform its own ownership-checked cleanup.
    try {
      if (runtime && session) await jsonApi(runtime, `/api/sessions/${session.id}?force=true`, { method: 'DELETE' });
    } finally {
      await runtime?.stop();
      await provider.close();
      if (projectRoot && path.basename(projectRoot).startsWith('axocoatl-reference-workspace-')) {
        await rm(projectRoot, { recursive: true, force: true });
      }
    }
  }
});
