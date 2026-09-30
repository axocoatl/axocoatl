import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';

let runtime, browser;
before(async () => {
  runtime = await launchTestDaemon();
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({headless:true, ...(executablePath ? {executablePath} : {})});
});
after(async () => { await browser?.close(); await runtime?.stop(); });

const available = value => ({status:'available', value});
const missing = {status:'not_recorded'};

test('Agent graph draws a delegated edge from a lead to its helper without making it a dependency', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const turnId = 'native-delegated-turn';
  const exact = (node, generation) => ({session_id:session.id, turn_id:turnId, execution_epoch_id:'native-epoch',
    node_id:node, generation, activation_id:`${node}-activation-${generation}`});
  const lead = exact('lead-node', 1), helper = exact('child-scout-node', 1);
  let helperState = 'running';
  const envelope = () => ({
    schema_version:1, history_version:'execution_v2', session_id:session.id, turn_id:turnId,
    state: helperState === 'running' ? 'running' : 'completed', request:available('Survey the parser'),
    turn_revision:available(helperState === 'running' ? 4 : 6), graph_revision:available(2),
    nodes:[[lead, 'Lead reviewer', helperState === 'running' ? 'running' : 'accepted', 'LEAD_ANSWER'],
      [helper, 'Scout helper', helperState, 'HELPER_ANSWER']].map(([activation, label, state, output]) => ({
      node_id:activation.node_id, label, definition_id:`${activation.node_id}-definition`, definition:missing, dependencies:[],
      activations:[{reference:{kind:'exact', activation}, generation:available(1), state, reason:missing,
        started_at:missing, completed_at:missing, input:missing, output:state === 'accepted' ? available(output) : missing,
        partial_outputs:[], usage:missing, evidence:[],
        capabilities:{inspect:true, stop:{enabled:false}, retry:{enabled:false}, revise:{enabled:false}}}],
    })),
    edges:[{id:`delegated:${lead.activation_id}:${helper.node_id}`, kind:'delegated_by', source:lead.node_id,
      target:helper.node_id, generation:available(1), recorded_at:missing, summary:available('scout'),
      evidence:available('child-grant-evidence')}],
    warnings:[],
  });
  const retained = {owner:{session_id:session.id, workspace_id:session.workspace_id}, turn_id:turnId, revision:4,
    state:'running', epochs:[], request:{status:'available', reference:'native-request', content:{turn_id:turnId,
      display_input:'Survey the parser', effective_input:'Survey the parser', recorded_at_unix_ms:3000, context:[]}},
    activations:[lead, helper].map(activation => ({activation:{activation, state:'running'}, currently_accepted:false,
      output:missing, partial_outputs:[], reserved_outputs:[], stream:[]}))};
  const team = {history_version:'execution_v2', approved:true, configuration_revision:2,
    slots:[{slot_id:'slot-lead', name:'Lead reviewer', model:'native-model', provider:'ollama'}],
    dependencies:[], layout:[], templates:[]};
  const context = await browser.newContext({viewport:{width:1280, height:800}});
  const page = await context.newPage(), errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/api/sessions', route => route.fulfill({json:[session]}));
  await page.route(`**/api/workspaces/${session.workspace_id}/sessions`, route => route.fulfill({json:[session]}));
  await page.route(`**/api/sessions/${session.id}/turns*`, route => route.fulfill({json:[{history_version:'execution_v2', turn:retained}]}));
  await page.route(`**/api/sessions/${session.id}/messages*`, route => route.fulfill({json:[]}));
  await page.route(`**/api/sessions/${session.id}/active-turn`, route => route.fulfill({json:{run:null}}));
  await page.route(`**/api/sessions/${session.id}/team`, route => route.fulfill({json:team}));
  await page.route(`**/api/sessions/${session.id}/turns/${turnId}/control-plane`, route => route.fulfill({json:envelope()}));
  try {
    await page.goto(`${runtime.baseUrl}/?session=${session.id}`, {waitUntil:'domcontentloaded'});
    const user = page.locator(`#session-msgs .smsg.user[data-turn-id="${turnId}"]`);
    await user.waitFor({state:'visible'});
    await user.getByRole('button', {name:'Open Agent graph'}).click();
    const graph = page.locator('#session-lattice-host ax-lattice');
    await page.waitForFunction(id => document.querySelector('#session-lattice-host ax-lattice')?.dataset.turnId === id
      && document.querySelector('#session-lattice-host ax-edge'), turnId);
    const edge = graph.locator('ax-edge');
    assert.equal(await edge.count(), 1, 'a delegation is the only edge; no dependency is invented');
    assert.equal(await edge.getAttribute('label'), 'delegated');
    assert.equal(await edge.getAttribute('aria-label'), 'Lead reviewer delegated a task to Scout helper');
    assert.equal(await edge.getAttribute('from'), `sl-${lead.node_id}:out`);
    assert.equal(await edge.getAttribute('to'), `sl-${helper.node_id}:in`);
    assert.equal(await edge.getAttribute('active'), '', 'the edge is live while the helper runs');
    await page.getByRole('button', {name:'Scout helper; working; no configured dependencies; delegated by Lead reviewer', exact:true}).waitFor();
    await page.getByRole('button', {name:'Lead reviewer; working; no configured dependencies', exact:true}).waitFor();
    const x = async id => Number(await graph.locator(`#sl-${id}`).getAttribute('data-x'));
    assert.ok(await x(helper.node_id) > await x(lead.node_id), 'the helper is laid out after its lead');

    helperState = 'accepted';
    await page.evaluate(id => showCoordinationGraph(id), turnId);
    await page.waitForFunction(id => document.querySelector(`#session-lattice-host #sl-${id}`)?.getAttribute('status') === 'success', helper.node_id);
    assert.equal(await graph.locator('ax-edge').count(), 1);
    assert.equal(await graph.locator('ax-edge').getAttribute('active'), null, 'a finished helper leaves the edge at rest');
    const folded = await page.evaluate(source => {
      const model = window.foldControlPlane(source, []);
      return {agents:model.agents.map(({id, dependsOn, delegatedBy, state}) => ({id, dependsOn, delegatedBy, state})),
        answers:model.answers.map(answer => answer.agentId), handoffs:model.handoffs};
    }, envelope());
    assert.deepEqual(folded.agents, [
      {id:lead.node_id, dependsOn:[], delegatedBy:[], state:'completed'},
      {id:helper.node_id, dependsOn:[], delegatedBy:[lead.node_id], state:'completed'},
    ]);
    assert.deepEqual(folded.answers, [lead.node_id], "a helper's output is not the turn's answer");
    assert.deepEqual(folded.handoffs, [{from:lead.node_id, to:helper.node_id, kind:'delegated_by',
      summary:'Delegated a task to helper scout'}]);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});
