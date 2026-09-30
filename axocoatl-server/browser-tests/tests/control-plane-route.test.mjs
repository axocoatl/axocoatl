import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { readFile } from 'node:fs/promises';
import path from 'node:path';

import { launchTestDaemon } from '../support/daemon.mjs';

let runtime;
const turnId = 'control-plane-retained-turn';

before(async () => {
  runtime = await launchTestDaemon({
    extraAgents: [{ id: 'browser-test-reviewer', name: 'Browser Test Reviewer', dependsOn: ['browser-test-coder'] }],
  });
  const session = runtime.fixtures.alpha.sessions[0];
  await runtime.restartWithSessionTurnEvents([
    {
      schema_version: 1, operation_id: `begin:${turnId}`, recorded_at: 1000,
      kind: 'begin', turn: {
        id: turnId, session_id: session.id, user_input: 'Inspect this retained execution',
        agent_id: 'browser-test-coder', status: 'running', partial_output: '',
        created_at: 1000, updated_at: 1000, execution_events: [], agent_outputs: [],
      },
    },
    // Coordination records written by 1.1.0 development builds. Nothing writes
    // them now, but history that contains them must still load.
    {
      schema_version: 1, operation_id: `plan:${turnId}`, recorded_at: 1001,
      kind: 'execution', turn_id: turnId, execution: {
        kind: 'coordination_planned', execution_id: turnId,
        metadata: { agents: [{ id: 'browser-test-coder', label: 'Recorded coder', depends_on: [] }] },
      },
    },
    {
      schema_version: 1, operation_id: `start:${turnId}`, recorded_at: 1002,
      kind: 'execution', turn_id: turnId, execution: {
        kind: 'coordination_agent_activated', execution_id: turnId,
        metadata: { agent_id: 'browser-test-coder', generation: 1 },
      },
    },
    {
      schema_version: 1, operation_id: `terminal:${turnId}`, recorded_at: 1100,
      kind: 'transition', turn_id: turnId,
      transition: { status: 'completed', final_output: 'Retained response.', error: null },
    },
  ]);
});

after(async () => { await runtime?.stop(); });

test('real control-plane route is exact, versioned, read-only and survives daemon restart', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const peer = runtime.fixtures.beta.sessions[0];
  const endpoint = `/api/sessions/${session.id}/turns/${turnId}/control-plane`;
  const ledger = path.join(runtime.runRoot, 'data', 'session-history', 'turns.v1.jsonl');
  const before = await readFile(ledger);
  const teamResponse = await fetch(`${runtime.baseUrl}/api/sessions/${session.id}/team`);
  assert.equal(teamResponse.status, 200, 'legacy capability reads do not emit background conflicts');
  const team = await teamResponse.json();
  assert.equal(team.history_version, 'legacy_v1');
  assert.deepEqual(team.slots, []);
  assert.equal(team.approved, false);
  const legacy = await fetch(`${runtime.baseUrl}/api/sessions/${session.id}/turns/${turnId}`)
    .then((response) => response.json());
  const response = await fetch(`${runtime.baseUrl}${endpoint}`);
  assert.equal(response.status, 200);
  const view = await response.json();
  assert.equal(view.schema_version, 1);
  assert.equal(view.history_version, 'legacy_v1');
  assert.equal(view.session_id, session.id);
  assert.equal(view.turn_id, turnId);
  assert.equal(view.request.value, legacy.user_input);
  assert.deepEqual(view.nodes.map((node) => node.node_id), ['browser-test-coder']);
  assert.deepEqual(view.edges, []);
  assert.equal(view.nodes[0].activations.length, 1);
  assert.equal(view.nodes[0].activations[0].output.value, 'Retained response.');
  assert.equal(view.nodes[0].activations[0].reference.kind, 'legacy');
  assert.equal(view.nodes[0].activations[0].capabilities.stop.enabled, false);
  assert.equal(view.nodes[0].activations[0].capabilities.retry.enabled, false);
  assert.equal(view.epochs.status, 'not_recorded');
  assert.equal(view.invocations.status, 'unknown');
  assert.deepEqual(await readFile(ledger), before, 'inspection must not write execution history');

  const wrongSession = await fetch(`${runtime.baseUrl}/api/sessions/${peer.id}/turns/${turnId}/control-plane`);
  assert.ok(wrongSession.status >= 400, 'a foreign Session must not expose the turn');
  const missing = await fetch(`${runtime.baseUrl}/api/sessions/${session.id}/turns/missing/control-plane`);
  assert.equal(missing.status, 404);
  assert.deepEqual(await readFile(ledger), before);

  await runtime.restart();
  assert.deepEqual(await fetch(`${runtime.baseUrl}${endpoint}`).then((result) => result.json()), view);
  assert.deepEqual(await readFile(ledger), before);
});

test('real human command route refuses extended authority and never executes against legacy history', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const endpoint = `${runtime.baseUrl}/api/sessions/${session.id}/turns/${turnId}/control-commands`;
  const ledger = path.join(runtime.runRoot, 'data', 'session-history', 'turns.v1.jsonl');
  const before = await readFile(ledger);
  const request = { schema_version: 1, command_id: 'legacy-finish-refused', session_id: session.id,
    turn_id: turnId, execution_epoch_id: 'cannot-invent-epoch', expected_turn_revision: 1,
    expected_graph_revision: 1, action: 'finish' };
  const send = body => fetch(endpoint, {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body)});
  for (const extra of [{mode: 'forced'}, {source: 'agent'}, {instruction: 'skip checks'}]) {
    const result = await send({...request, ...extra});
    assert.equal(result.status, 400, 'caller cannot add forced Finish or attribution');
  }
  const normal = await send(request);
  assert.ok(normal.status >= 400, 'legacy history never gains a canonical executable turn');
  assert.deepEqual(await readFile(ledger), before);
  const view = await fetch(`${runtime.baseUrl}/api/sessions/${session.id}/turns/${turnId}/control-plane`).then(result => result.json());
  assert.equal(view.turn_controls, undefined);
});

test('versioned legacy list, lookup, search and exports preserve opaque identity and original context', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const rawId = `  legacy / retained ${'x'.repeat(260)}  `;
  const context = [{reference_id:'legacy-reference',kind:'code_selection',scope:'this_turn',display_name:'LEGACY_CONTEXT_MARKER',metadata:{content:'retained source bytes'}}];
  await runtime.restartWithSessionTurnEvents([
    {schema_version:1,operation_id:'versioned-legacy-begin',recorded_at:2000,kind:'begin',turn:{id:rawId,session_id:session.id,
      user_input:'LEGACY_REQUEST_MARKER',agent_id:'browser-test-coder',status:'running',partial_output:'',context,created_at:2000,updated_at:2000,execution_events:[],agent_outputs:[]}},
    {schema_version:1,operation_id:'versioned-legacy-terminal',recorded_at:2100,kind:'transition',turn_id:rawId,
      transition:{status:'completed',final_output:'LEGACY_OUTPUT_MARKER',error:null}},
  ]);
  const base=`${runtime.baseUrl}/api/sessions/${session.id}`;
  const get=async url=>{const response=await fetch(url);assert.equal(response.status,200,url);return response.json();};
  const before=await readFile(path.join(runtime.runRoot,'data','session-history','turns.v1.jsonl'));
  const oldList=await get(`${base}/turns`);
  const versioned=await get(`${base}/turns?history_version=2`);
  assert.deepEqual(versioned,oldList.map(turn=>({history_version:'legacy_v1',turn})));
  assert.equal(versioned[0].turn.id,rawId);assert.deepEqual(versioned[0].turn.context,context);
  assert.equal(versioned[0].turn.final_output,'LEGACY_OUTPUT_MARKER');
  const exact=`${base}/turns/${encodeURIComponent(rawId)}`;
  assert.deepEqual(await get(`${exact}?history_version=2`),versioned[0]);
  assert.deepEqual(await get(exact),oldList[0]);
  const search=`${runtime.baseUrl}/api/session-turns/search?session_id=${encodeURIComponent(session.id)}&q=LEGACY_CONTEXT_MARKER`;
  const oldHits=await get(search);const hits=await get(`${search}&history_version=2`);
  assert.equal(hits[0].entry.history_version,'legacy_v1');assert.deepEqual(hits[0].entry.turn,oldHits[0].turn);
  assert.deepEqual(hits[0].matched_fields,oldHits[0].matched_fields);
  assert.deepEqual(await get(`${base}/export?format=json&history_version=2`),versioned);
  const markdown=await fetch(`${base}/export?format=markdown&history_version=2`);assert.equal(markdown.status,200);
  assert.match(await markdown.text(),/LEGACY_REQUEST_MARKER[\s\S]*LEGACY_OUTPUT_MARKER/);
  for(const endpoint of [`${base}/turns?`,`${exact}?`,`${search}&`,`${base}/export?format=json&`]) {
    assert.equal((await fetch(`${endpoint}history_version=99`)).status,400);
  }
  assert.deepEqual(await readFile(path.join(runtime.runRoot,'data','session-history','turns.v1.jsonl')),before);
});

function sendTurn(sessionId, turnId, input) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`${runtime.baseUrl.replace(/^http/, 'ws')}/ws`);
    let settled = false;
    const finish = (error, frame) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.close();
      if (error) reject(error);
      else resolve(frame);
    };
    const timer = setTimeout(() => finish(new Error(`Turn ${turnId} timed out.\n${runtime.logs()}`)), 30_000);
    socket.addEventListener('error', () => finish(new Error(`WebSocket failed for ${turnId}`)));
    socket.addEventListener('message', ({ data }) => {
      const frame = JSON.parse(data);
      if (frame.kind === 'snapshot') {
        socket.send(JSON.stringify({
          cmd: 'session', id: sessionId, turn_id: turnId, idempotency_key: turnId,
          input, display_input: input, reference_ids: [], context_references: [],
        }));
      }
      if (frame.kind === 'error') finish(new Error(JSON.stringify(frame)));
      if (frame.session !== sessionId || frame.turn_id !== turnId) return;
      if (['session-accepted', 'session-done', 'session-error', 'session-request-rejected'].includes(frame.kind)) {
        finish(null, frame);
      }
    });
  });
}

test('a legacy multi-Agent turn is refused before it starts and names the upgrade command', async () => {
  const workspace = runtime.fixtures.alpha.workspace;
  const response = await fetch(`${runtime.baseUrl}/api/workspaces/${encodeURIComponent(workspace.id)}/sessions`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      name: 'Legacy two-Agent team',
      mode: { kind: 'custom', agents: ['browser-test-coder', 'browser-test-reviewer'] },
      enabled_skills: [], exposed_ports: [], setup_approved: false, setup_reviewed: false,
    }),
  });
  assert.ok(response.ok, await response.clone().text());
  const session = await response.json();
  const ledger = path.join(runtime.runRoot, 'data', 'session-history', 'turns.v1.jsonl');
  const before = await readFile(ledger);
  const turn = 'legacy-multi-agent-refused';
  const terminal = await sendTurn(session.id, turn, 'Review the retained change.');
  assert.ok(['session-error', 'session-request-rejected'].includes(terminal.kind), JSON.stringify(terminal));
  assert.match(terminal.error, /2 Agents/);
  assert.match(terminal.error, /`axocoatl session upgrade --confirm`/);
  const missing = await fetch(`${runtime.baseUrl}/api/sessions/${session.id}/turns/${turn}`);
  assert.equal(missing.status, 404, 'the refused turn is never begun');
  assert.deepEqual(await readFile(ledger), before, 'the refusal writes no history');
});
