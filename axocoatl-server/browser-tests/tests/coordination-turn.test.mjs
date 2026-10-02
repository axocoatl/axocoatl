import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';

import { launchTestDaemon, resolveChromiumExecutable, newAuthorizedContext } from '../support/daemon.mjs';

let runtime;
let browser;
let appSource;

before(async () => {
  const componentBaseUrl = process.env.AXOCOATL_COMPONENT_BASE_URL;
  runtime = componentBaseUrl
    ? { baseUrl: componentBaseUrl, stop: async () => {} }
    : await launchTestDaemon();
  const response = await fetch(`${runtime.baseUrl}/${componentBaseUrl ? 'index.html' : ''}`);
  assert.equal(response.ok, true, 'actual app source is available');
  appSource = await response.text();
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

async function componentPage(options = {}) {
  const context = await newAuthorizedContext(browser, { viewport: options.viewport || { width: 1000, height: 760 } });
  const page = await context.newPage();
  await page.route('**/fixture-app-source', route => route.fulfill({contentType:'text/plain',body:appSource}));
  const errors = [];
  page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`));
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`console: ${message.text()}`);
  });
  page.on('response', (response) => {
    if (response.status() >= 400) errors.push(`${response.status()} ${response.url()}`);
  });
  await page.route('**/activation-inspector-fixture', (route) => route.fulfill({
    status: 200,
    contentType: 'text/html',
    body: `<!doctype html><html data-theme="${options.theme || 'light'}"><head>
      <meta name="viewport" content="width=device-width, initial-scale=1">
      <link rel="stylesheet" href="/ui/tokens.css">
      <style>body { margin: 16px; background: var(--bg); color: var(--text); }</style>
    </head><body></body></html>`,
  }));
  if (options.reducedMotion) await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.goto(`${runtime.baseUrl}/activation-inspector-fixture`, { waitUntil: 'domcontentloaded' });
  await page.addScriptTag({
    type: 'module',
    url: `${runtime.baseUrl}/ui/activation-inspector.js`,
  });
  await page.waitForFunction(() => Boolean(customElements.get('ax-activation-inspector')));
  return { context, page, errors };
}

const available = (value) => ({ status: 'available', value });
const missing = { status: 'not_recorded' };
function controlPlaneFixture() {
  const activation = (generation, state, text) => ({
    reference: { kind: 'exact', activation: { session_id: 'session-inspect', turn_id: 'turn-inspect',
      execution_epoch_id: 'epoch-one', node_id: 'builder', generation, activation_id: `activation-${generation}` } },
    generation: available(generation), state, reason: missing,
    started_at: available(1789387200000), completed_at: state === 'completed' ? available(1789387201000) : missing,
    input: available({ task: 'Check the exact client repository' }), output: available(text), partial_outputs: [],
    usage: { status: 'unknown', reason: 'Provider usage has not settled.' },
    capabilities: { inspect: true, stop: { enabled: false, reason: 'Read only' }, retry: { enabled: false, reason: 'Read only' } },
    evidence: [{ kind: 'tool_result', reference: available(`tool-${generation}`),
      summary: available('The exact repository check passed.'), recorded_at: available(1789387200500),
      details: { status: 'truncated', value: { stdout: 'PASS' }, original_byte_len: 90000 } }],
  });
  return {
    schema_version: 1, history_version: 'execution_v2', session_id: 'session-inspect', turn_id: 'turn-inspect',
    state: 'completed', request: available('Check the client change'),
    nodes: [{ node_id: 'builder', definition_id: 'definition-one', label: 'Builder', dependencies: [],
      definition: available({ name: available('Builder used for this turn'), role: available('autonomous'),
        provider: available('ollama'), model: available('local-model'), configuration_revision: available(8) }),
      activations: [activation(1, 'superseded', 'FIRST_GENERATION_ONLY'), activation(2, 'completed', 'CURRENT_GENERATION_ONLY')] }],
    edges: [], warnings: [],
  };
}

function legacyControlPlaneFixture({ nodeId = 'legacy-agent', output = missing, evidence = [] } = {}) {
  return {
    schema_version: 1, history_version: 'legacy_v1', session_id: 'legacy-session', turn_id: 'legacy-turn',
    state: 'completed', request: available('Review the client change'),
    nodes: [{ node_id: nodeId, definition_id: nodeId, label: nodeId, dependencies: [], definition: missing,
      activations: [{
        reference: { kind: 'legacy', session_id: 'legacy-session', turn_id: 'legacy-turn', node_id: nodeId, generation: null },
        generation: missing, state: 'completed', reason: missing, started_at: missing, completed_at: missing,
        input: missing, output, partial_outputs: [], usage: missing,
        capabilities: { inspect: true, stop: { enabled: false, reason: 'Read only' }, retry: { enabled: false, reason: 'Read only' } },
        evidence,
      }] }],
    edges: [], warnings: [],
  };
}

test('control-plane fold reads both versioned history envelopes and never infers a graph from a raw turn', async () => {
  const {context, page, errors} = await componentPage();
  try {
    const result = await page.evaluate(async envelope => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js');
      const legacy = structuredClone(envelope); legacy.history_version = 'legacy_v1';
      legacy.turn_id = 'legacy café';
      for (const node of legacy.nodes) for (const activation of node.activations) {
        activation.reference = {kind: 'legacy', session_id: legacy.session_id,
          turn_id: legacy.turn_id, node_id: node.node_id, generation: null};
      }
      const raw = {id: 'legacy café', session_id: legacy.session_id, agent_id: 'legacy-agent',
        status: 'completed', final_output: 'Raw legacy answer', execution_events: []};
      const unsupported = foldControlPlane(raw);
      return {raw: {unsupported: unsupported.unsupported === true, nodes: unsupported.nodes.length},
        envelopes: [legacy, envelope].map(source => {
          const model = foldControlPlane(source);
          return {unsupported: model.unsupported === true, history: model.historyVersion,
            turn: model.turnId, nodes: model.nodes.length, output: model.nodes[0].activations.at(-1).output};
        })};
    }, controlPlaneFixture());
    assert.deepEqual(result.raw, {unsupported: true, nodes: 0});
    assert.deepEqual(result.envelopes.map(item => item.unsupported), [false, false]);
    assert.deepEqual(result.envelopes.map(item => item.history), ['legacy_v1', 'execution_v2']);
    assert.deepEqual(result.envelopes.map(item => item.turn), ['legacy café', 'turn-inspect']);
    assert.deepEqual(result.envelopes.map(item => item.nodes), [1, 1]);
    assert.deepEqual(result.envelopes.map(item => item.output.value), ['CURRENT_GENERATION_ONLY', 'CURRENT_GENERATION_ONLY']);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('unsupported or malformed control-plane data removes stale graphs and executable controls', async () => {
  const {context, page, errors} = await componentPage();
  try {
    const cases = await page.evaluate(async envelope => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js');
      const inspector = document.createElement('ax-activation-inspector'); inspector.commandHandler = () => { throw new Error('must never dispatch'); };
      inspector.model = foldControlPlane(envelope); inspector.nodeId = 'builder'; document.body.append(inspector);
      const mutations = [value => value.schema_version = 99, value => value.schema_version = '1',
        value => value.history_version = 'execution_v99', value => delete value.schema_version,
        value => value.nodes = {}, value => value.nodes[0].activations = {},
        value => value.nodes[0].activations[0].reference.activation.session_id = 'foreign-session',
        value => value.nodes[0].activations[0].evidence = [{}], value => value.edges = [null]];
      return mutations.map(mutate => {
        const invalid = structuredClone(envelope); mutate(invalid);
        invalid.agent_id = 'must-not-become-legacy';
        const model = foldControlPlane(invalid);
        inspector.model = model;
        return {unsupported: model.unsupported, nodes: model.nodes, agents: model.agents,
          controlPlane: model.controlPlane, historyVersion: model.historyVersion,
          warning: model.warnings[0], inspectorHidden: inspector.hidden};
      });
    }, controlPlaneFixture());
    for (const result of cases) {
      assert.equal(result.unsupported, true); assert.deepEqual(result.nodes, []); assert.deepEqual(result.agents, []);
      assert.equal(result.controlPlane, null); assert.equal(result.historyVersion, null);
      assert.match(result.warning, /unsupported|malformed/); assert.equal(result.inspectorHidden, true);
    }
    if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) {
      await page.screenshot({path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/unsupported-light.png`, fullPage: true});
      await page.setViewportSize({width: 390, height: 840});
      await page.emulateMedia({reducedMotion: 'reduce'});
      await page.evaluate(() => document.documentElement.dataset.theme = 'dark');
      await page.screenshot({path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/unsupported-narrow-dark.png`, fullPage: true});
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    }
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('malformed scalar envelopes fold as unsupported with no nodes', async () => {
  const {context, page, errors} = await componentPage();
  try {
    const results = await page.evaluate(async () => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js');
      return [null, false, 0, '', [], 'unexpected'].map(value => {
        const model = foldControlPlane(value);
        return {unsupported: model.unsupported, warning: model.warnings[0], nodes: model.nodes.length};
      });
    });
    for (const result of results) {
      assert.equal(result.unsupported, true); assert.match(result.warning, /malformed/); assert.equal(result.nodes, 0);
    }
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('the real graph consumer clears unsupported history before consulting current Settings', async () => {
  const {context, page, errors} = await componentPage();
  try {
    const result = await page.evaluate(async envelope => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js'); window.foldControlPlane = foldControlPlane;
      const source = await fetch('/fixture-app-source').then(response => response.text());
      const start = source.indexOf('async function sessionLatticeBuild(');
      const end = source.indexOf('\nfunction sessionLatticeStatus(', start);
      if (start < 0 || end < start) throw new Error('actual graph consumer function is unavailable');
      const historyStart = source.indexOf('function isExecutionHistoryTurn(');
      const historyEnd = source.indexOf('\nasync function reloadSessionTranscript(', historyStart);
      if (historyStart < 0 || historyEnd < historyStart) throw new Error('actual history discriminator is unavailable');
      const host = document.createElement('div'); host.id = 'session-lattice-host';
      host.innerHTML = '<div class="stale-graph">Old graph</div><ax-activation-inspector></ax-activation-inspector>';
      document.body.append(host);
      const state = {session: {coordinationGraphTurnId: envelope.turn_id,
        controlPlaneTurns: new Map(), graphAgents: ['old-agent'], lattice: null}};
      let consultedSettings = false;
      const build = new Function('S', '$', 'sessionActiveAgentIds', 'sessionAgentDependencyEdges', 'mkDiv',
        `let _sessionLatticeBuildId = 0; ${source.slice(historyStart, historyEnd)}; return (${source.slice(start, end)});`)(state,
        selector => document.querySelector(selector), () => ['current-setting-agent'],
        () => { consultedSettings = true; throw new Error('unsupported history must not infer current dependencies'); },
        (className, text) => { const node = document.createElement('div'); node.className = className; node.textContent = text; return node; });
      envelope.schema_version = 99;
      await build({id: envelope.session_id}, envelope.turn_id, envelope);
      return {consultedSettings, graphAgents: state.session.graphAgents, lattice: state.session.lattice,
        notice: host.textContent, children: host.children.length,
        controls: host.querySelectorAll('ax-lattice,ax-activation-inspector,button').length};
    }, controlPlaneFixture());
    assert.equal(result.consultedSettings, false); assert.deepEqual(result.graphAgents, []);
    assert.equal(result.lattice, null); assert.equal(result.children, 1); assert.equal(result.controls, 0);
    assert.match(result.notice, /unsupported/); assert.doesNotMatch(result.notice, /Old graph|current-setting-agent/);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('accepted native work opens exact Turn controls before any output or transcript repaint', async () => {
  const {context, page, errors} = await componentPage();
  let graphReads = 0;
  const envelope = controlPlaneFixture();
  envelope.state = 'running'; envelope.turn_revision = available(3); envelope.graph_revision = available(1);
  envelope.nodes[0].activations = envelope.nodes[0].activations.slice(0, 1);
  envelope.nodes[0].activations[0].state = 'running';
  envelope.nodes[0].activations[0].output = missing;
  envelope.nodes[0].activations[0].evidence = [];
  envelope.turn_controls = {execution_epoch_id:'epoch-one', continue_turn:{enabled:false}, finish:{enabled:false},
    continuation_choices:[], check_choices:[]};
  try {
    await page.route('**/api/sessions/session-inspect/turns/turn-inspect/control-plane', route => {
      graphReads++; return route.fulfill({json:envelope});
    });
    await page.route('**/api/sessions/session-inspect/turns/turn-inspect/grants', route => route.fulfill({json:{
      grants:[], proposals:[{state:{state:'pending'},request:{reason:'Approve this bounded Worker before execution.'}}],
    }}));
    await page.evaluate(async ({source, envelope}) => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js'); window.foldControlPlane = foldControlPlane;
      await import('/ui/session-guidance.js');
      document.body.innerHTML = '<ax-session-guidance id="session-guidance" hidden></ax-session-guidance><div id="session-lattice-host" style="height:600px"></div>';
      const S = {session:{id:envelope.session_id, currentTeam:{history_version:'execution_v2'}, activeTurnId:null,
        controlPlaneTurns:new Map(), graphAgents:[], lattice:null}};
      const $ = selector => document.querySelector(selector);
      const extract = (start, end) => {
        const a = source.indexOf(start), b = source.indexOf(end, a);
        if (a < 0 || b < a) throw Error(`Shell source unavailable: ${start}`);
        return source.slice(a,b);
      };
      const code = [extract('function isExecutionHistoryTurn(', '\nasync function reloadSessionTranscript('),
        extract('async function showCoordinationGraph(', '\nfunction renderSessionTurns('),
        extract('function scheduleCoordinationGraphRefresh(', '\nfunction sessionLatticeStatus('),
        extract('function setSessionTurnRunning(', '\nfunction syncSessionComposerState('),
        extract("customElements.whenDefined('ax-session-guidance').then(", "\nif ($('#session-text'))")].join('\n');
      const accepted = extract("case 'session-accepted':", "case 'session-start':");
      const mkDiv = (className, text) => { const el=document.createElement('div');el.className=className;el.textContent=text || '';return el; };
      const mkHandle = (type, id, position) => { const el=document.createElement('ax-handle');el.type=type;el.id=id;el.position=position;return el; };
      window.transcriptRefreshes = [];
      let api;
      const sync = () => $('#session-guidance').setIdentity(S.session.id, api?.native(S.session.activeTurnId) ? S.session.activeTurnId : '');
      api = new Function('S','$','sessionActiveAgentIds','sessionAgentDependencyEdges','mkDiv','mkHandle',
        'visibleSurfaces','hydrateCoordinationCommands','submitCoordinationCommand','coordinationCommandHistory',
        'keepSessionGraphSelectionVisible','syncSessionComposerState','openModule','reloadSessionTranscript','setSessionTurnPending',
        `let _sessionLatticeBuildId=0,_coordinationReadId=0,_coordinationInFlight=null,_coordinationRefreshTimer=null;const _pendingSessionTurns=new Map();
        ${code};return {native:isExecutionHistoryTurn,show:showCoordinationGraph,accept:d=>{switch('session-accepted'){${accepted}}}};`)(
          S, $, () => ['CURRENT_SETTINGS_MUST_NOT_BE_USED'], () => {throw Error('Used configured dependencies for active native work');},
          mkDiv, mkHandle, () => ['trace'], () => {}, async () => {}, () => [], () => {}, sync, () => {},
          async (...args) => window.transcriptRefreshes.push(args), () => {});
      window.activeGraphState = S; window.activeGraphApi = api;
      api.accept({session:envelope.session_id,turn_id:envelope.turn_id});
    }, {source:appSource,envelope});
    await page.locator('ax-session-guidance').getByRole('button',{name:'Turn controls',exact:true}).click();
    const inspector = page.locator('ax-activation-inspector');
    await inspector.getByRole('button',{name:'Review current authority',exact:true}).waitFor();
    assert.match(await inspector.locator('.content').textContent(), /turn-inspect/);
    assert.doesNotMatch(await inspector.locator('.content').textContent(), /Configured team|no turn selected/);
    assert.equal(await page.locator('ax-node').getAttribute('aria-label'), 'Builder; working; no configured dependencies');
    await inspector.getByRole('button',{name:'Review current authority',exact:true}).click();
    await page.getByText('Approve this bounded Worker before execution.',{exact:true}).waitFor();
    const state = await page.evaluate(async () => {
      const state=window.activeGraphState.session, api=window.activeGraphApi;
      const before=state.coordinationGraphTurnId;
      await api.show('unrelated-turn');
      state.activeTurnId=null;
      const retained=api.native('turn-inspect');
      state.executionHistoryTurnIds.clear();
      const guide=document.querySelector('ax-session-guidance');
      guide.envelope={...guide.envelope,session_id:'foreign-session',turn_id:'unrelated-turn'};
      return {before,after:state.coordinationGraphTurnId,retained,foreign:api.native('unrelated-turn'),empty:api.native(''),
        cachedTurns:state.controlPlaneTurns.size,transcriptRefreshes:window.transcriptRefreshes};
    });
    assert.deepEqual(state,{before:'turn-inspect',after:'turn-inspect',retained:true,foreign:false,empty:false,cachedTurns:0,
      transcriptRefreshes:[['session-inspect',{preserveLive:true}]]});
    assert.equal(graphReads,2,'one guidance read and one explicit graph read; no output polling');
    assert.deepEqual(errors,[]);
  } finally { await context.close(); }
});

for (const options of [{ theme: 'light', viewport: { width: 1100, height: 900 } },
  { theme: 'dark', viewport: { width: 390, height: 840 }, reducedMotion: true }]) {
  test(`Escape closes only the activation inspector and restores its graph opener (${options.theme})`, async () => {
    const {context, page, errors} = await componentPage(options);
    try {
      await page.evaluate(async ({source, envelope}) => {
        const {foldControlPlane} = await import('/ui/activation-inspector.js');
        window.foldControlPlane = foldControlPlane;
        document.body.innerHTML = '<style>.session-graph-layout{display:flex;height:600px}ax-lattice{flex:1;min-width:200px}ax-activation-inspector{flex:0 1 360px}</style><div id="session-cockpit"><div id="session-lattice-host" style="height:600px"></div></div>';
        const S = {session:{id:envelope.session_id,name:'Retained QA',
          controlPlaneTurns:new Map(),lattice:null},cockpitLayout:{center:'trace'}};
        const $ = selector => document.querySelector(selector);
        const extract = (start, end) => {
          const a=source.indexOf(start),b=source.indexOf(end,a);
          if(a<0||b<a) throw Error(`Missing shell function ${start}`);
          return source.slice(a,b);
        };
        const mkDiv=(name,text)=>{const node=document.createElement('div');node.className=name;node.textContent=text||'';return node;};
        const mkHandle=(type,id,position)=>{const node=document.createElement('ax-handle');node.type=type;node.id=id;node.position=position;return node;};
        const code=extract('async function sessionLatticeBuild(', '\nfunction sessionLatticeStatus(')
          +extract('function onCockpitKey(', '\n// File browsing');
        const api=new Function('S','$','sessionActiveAgentIds','isExecutionHistoryTurn','visibleSurfaces',
          'coordinationCommandHistory','submitCoordinationCommand','keepSessionGraphSelectionVisible',
          'mkDiv','mkHandle','centerSurface','openModule',
          `let _sessionLatticeBuildId=0;${code};return {build:sessionLatticeBuild,key:onCockpitKey};`)(
          S,$,()=>[],()=>true,()=>['trace'],()=>[],async()=>{},()=>{},mkDiv,mkHandle,
          ()=>S.cockpitLayout.center,center=>{S.cockpitLayout.center=center;$('#session-lattice-host').hidden=center!=='trace';});
        document.addEventListener('keydown',api.key);
        await api.build(S.session,envelope.turn_id,envelope);
        window.escapeClosures=[];
        document.addEventListener('close-inspector',event=>window.escapeClosures.push({detail:event.detail,active:document.activeElement?.tagName,cls:document.activeElement?.className}));
        window.inspectorEscapeState=S;
      },{source:appSource,envelope:controlPlaneFixture()});
      const graph=page.locator('ax-lattice');
      const inspector=page.locator('ax-activation-inspector');
      await graph.focus();
      await graph.press('Enter');
      await inspector.getByRole('button',{name:'Close',exact:true}).waitFor();
      await inspector.getByRole('button',{name:'Close',exact:true}).press('Escape');
      assert.equal(await inspector.isVisible(),false);
      assert.equal(await graph.isVisible(),true);
      assert.equal(await graph.evaluate(node=>document.activeElement===node),true);
      const turnControls=page.getByRole('button',{name:'Turn controls',exact:true});
      await turnControls.click();
      await inspector.getByRole('button',{name:'Close',exact:true}).press('Escape');
      assert.equal(await inspector.isVisible(),false);
      assert.equal(await turnControls.evaluate(node=>document.activeElement===node),true,
        await page.evaluate(()=>JSON.stringify({active:document.activeElement?.tagName,closes:window.escapeClosures})));
      assert.equal(await page.evaluate(()=>window.inspectorEscapeState.cockpitLayout.center),'trace');
      await turnControls.press('Escape');
      assert.equal(await page.evaluate(()=>window.inspectorEscapeState.cockpitLayout.center),'stream');
      assert.deepEqual(errors,[]);
    } finally {await context.close();}
  });

  test(`selected activation inspector shares durable projection and preserves selection (${options.theme})`, async () => {
    const { context, page, errors } = await componentPage(options);
    try {
      await page.evaluate(async (envelope) => {
        const { foldControlPlane } = await import('/ui/activation-inspector.js');
        window.testEnvelope = envelope;
        window.testFold = foldControlPlane;
        const inspector = document.createElement('ax-activation-inspector');
        inspector.model = foldControlPlane(envelope); inspector.nodeId = 'builder';
        document.body.append(inspector);
      }, controlPlaneFixture());
      const inspector = page.locator('ax-activation-inspector');
      assert.match(await inspector.textContent(), /^$/); // Shadow content is owned by the component.
      assert.match(await inspector.locator('.content').textContent(), /Builder used for this turn/);
      assert.match(await inspector.locator('.content').textContent(), /CURRENT_GENERATION_ONLY/);
      assert.doesNotMatch(await inspector.locator('.content').textContent(), /FIRST_GENERATION_ONLY/);
      assert.match(await inspector.locator('.content').textContent(), /Unknown · Provider usage has not settled/);
      assert.equal(await inspector.locator('.stop:visible,.retry:visible').count(), 0);
      await inspector.getByLabel('Activation generation').selectOption('0');
      await page.evaluate(() => {
        window.testEnvelope.nodes[0].activations[1].output = { status: 'available', value: 'UPDATED_SECOND_GENERATION' };
        document.querySelector('ax-activation-inspector').model = window.testFold(window.testEnvelope);
      });
      assert.match(await inspector.locator('.content').textContent(), /FIRST_GENERATION_ONLY/);
      assert.doesNotMatch(await inspector.locator('.content').textContent(), /UPDATED_SECOND_GENERATION/);
      assert.equal(await inspector.getByLabel('Activation generation').inputValue(), '0');
      const modal = await inspector.locator('dialog[open]').evaluate((dialog) => dialog.matches(':modal'));
      assert.equal(modal, options.viewport.width < 720);
      if (modal) {
        const bounds = await inspector.locator('dialog[open]').boundingBox();
        assert.ok(bounds.width <= 390 && bounds.x >= 0);
        await inspector.getByRole('button', { name: 'Close', exact: true }).focus();
        await page.keyboard.press('Shift+Tab');
        assert.equal(await inspector.evaluate((node) => {
          const dialog = node.shadowRoot.querySelector('dialog');
          return document.activeElement === node && dialog.contains(node.shadowRoot.activeElement);
        }), true);
      }
      if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) {
        await page.screenshot({ path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/inspector-${options.theme}.png`, fullPage: true });
      }
      await inspector.getByRole('button', { name: 'Close', exact: true }).click();
      assert.equal(await inspector.isVisible(), false);
      assert.deepEqual(errors, [], errors.join('\n'));
    } finally { await context.close(); }
  });
}

test('an absent pinned activation never falls back to a newer recorded generation',async()=>{
 const {context,page,errors}=await componentPage();try{
  const envelope=controlPlaneFixture();await page.evaluate(async envelope=>{const {foldControlPlane}=await import('/ui/activation-inspector.js');const inspector=document.createElement('ax-activation-inspector');inspector.model=foldControlPlane(envelope);inspector.nodeId='builder';document.body.append(inspector);},envelope);
  const inspector=page.locator('ax-activation-inspector');
  for(const patch of [{generation:99},{execution_epoch_id:'foreign-epoch'},{activation_id:'missing-activation'}]){
   await inspector.evaluate((element,{envelope,patch})=>{element.activationReference={kind:'exact',activation:{...envelope.nodes[0].activations[0].reference.activation,...patch}};element.model=element.model;},{envelope,patch});
   const text=await inspector.locator('.content').textContent();assert.match(text,/referenced activation is unavailable/);assert.doesNotMatch(text,/FIRST_GENERATION_ONLY|CURRENT_GENERATION_ONLY/);assert.equal(await inspector.getByLabel('Activation generation').inputValue(),'');assert.equal(await inspector.locator('.stop:visible,.retry:visible,.revise:visible,.guide:visible,.add-reference:visible').count(),0);
  }
  await inspector.getByLabel('Activation generation').selectOption('0');assert.match(await inspector.locator('.content').textContent(),/FIRST_GENERATION_ONLY/);assert.doesNotMatch(await inspector.locator('.content').textContent(),/CURRENT_GENERATION_ONLY/);assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('settled Stop refreshes its still-running exact activation until Retry becomes visible',async()=>{
 const {context,page,errors}=await componentPage();let reads=0;try{
  const before=controlPlaneFixture();before.state='running';before.nodes[0].activations=before.nodes[0].activations.slice(0,1);const target=before.nodes[0].activations[0];target.state='running';target.capabilities.stop={enabled:true};
  const receipt={state:'settled',request:{session_id:before.session_id,turn_id:before.turn_id,parameters:{kind:'stop_activation',activation:target.reference.activation}}};before.commands=available([receipt]);
  const after=structuredClone(before);after.nodes[0].activations[0].state='failed';after.nodes[0].activations[0].capabilities={inspect:true,stop:{enabled:false},retry:{enabled:true}};
  await page.route('**/api/sessions/session-inspect/turns/turn-inspect/control-plane',route=>{reads++;return route.fulfill({json:reads===1?before:after});});
  await page.evaluate(async({source,before})=>{
   const {foldControlPlane}=await import('/ui/activation-inspector.js');const host=document.createElement('div');host.id='session-lattice-host';document.body.append(host);const inspector=document.createElement('ax-activation-inspector');inspector.commandHandler=async()=>{};inspector.model=foldControlPlane(before);inspector.nodeId='builder';host.append(inspector);
   const S={session:{id:before.session_id,coordinationGraphTurnId:before.turn_id}};let api;window.visibleGraph=true;
   const build=async(session,turnId,envelope)=>{if(envelope)inspector.model=foldControlPlane(envelope);else await api.refresh(session.id,turnId);};
   const begin=source.indexOf('function scheduleCoordinationGraphRefresh('),end=source.indexOf('\nasync function sessionLatticeBuild(',begin);if(begin<0||end<0)throw Error('current graph refresh source unavailable');
   api=new Function('S','$','visibleSurfaces','isExecutionHistoryTurn','hydrateCoordinationCommands','sessionLatticeBuild','mkDiv',`let _coordinationReadId=0,_coordinationInFlight=null,_coordinationRefreshTimer=null;${source.slice(begin,end)};return {refresh:refreshCoordinationControlPlane,settling:coordinationControlStillSettling};`)(S,selector=>document.querySelector(selector),()=>window.visibleGraph?['trace']:[],()=>true,()=>{},build,className=>Object.assign(document.createElement('div'),{className}));
   window.graphReader=api;await api.refresh(before.session_id,before.turn_id);
  },{source:appSource,before});
  await page.locator('ax-activation-inspector').getByRole('button',{name:'Retry',exact:true}).waitFor({timeout:5000});assert.equal(reads,2);await new Promise(resolve=>setTimeout(resolve,1150));assert.equal(reads,2,'target settlement stops polling despite a running logical turn');
  const wrong=structuredClone(before);wrong.commands.value[0].request.parameters.activation={...wrong.commands.value[0].request.parameters.activation,generation:99};assert.equal(await page.evaluate(value=>window.graphReader.settling(value),wrong),false);assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('legacy inspector never invents a generation, output, definition, usage or executable control', async () => {
  const { context, page, errors } = await componentPage();
  try {
    await page.evaluate(async (envelope) => {
      const { foldControlPlane } = await import('/ui/activation-inspector.js');
      const inspector = document.createElement('ax-activation-inspector');
      inspector.model = foldControlPlane(envelope);
      inspector.nodeId = 'legacy-agent'; document.body.append(inspector);
    }, legacyControlPlaneFixture());
    const inspector = page.locator('ax-activation-inspector');
    const copy = await inspector.locator('.content').textContent();
    assert.match(copy, /GenerationNot recorded/);
    assert.match(copy, /Definition used for this turnNot recorded/);
    assert.match(copy, /OutputNot recorded/);
    assert.equal(await inspector.locator('.stop:visible,.retry:visible').count(), 0);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('inspector keeps one bounded dialog across responsive transitions and labels recorded empty output', async () => {
  const { context, page, errors } = await componentPage({ viewport: { width: 1100, height: 900 } });
  try {
    const envelope = controlPlaneFixture();
    envelope.nodes[0].activations.at(-1).output = available('');
    await page.evaluate(async (value) => {
      const { foldControlPlane } = await import('/ui/activation-inspector.js');
      const inspector = document.createElement('ax-activation-inspector');
      inspector.model = foldControlPlane(value); inspector.nodeId = 'builder'; document.body.append(inspector);
    }, envelope);
    const inspector = page.locator('ax-activation-inspector');
    assert.equal(await inspector.getByText('Recorded empty output', { exact: true }).count(), 1);
    assert.equal(await inspector.locator('pre').evaluateAll(nodes => nodes.filter(node => node.textContent === '').length), 0);
    for (const width of [390, 1100, 390]) {
      await page.setViewportSize({ width, height: 844 });
      await page.waitForFunction((narrow) => {
        const dialog = document.querySelector('ax-activation-inspector')?.shadowRoot.querySelector('dialog');
        return dialog?.matches(':modal') === narrow && getComputedStyle(dialog).position === (narrow ? 'fixed' : 'static');
      }, width < 720);
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      assert.equal(await page.locator('ax-activation-inspector').count(), 1);
      assert.equal(await inspector.locator('dialog[open]').count(), 1);
      assert.equal(await inspector.locator('header:visible').count(), 1);
      if (width < 720) {
        const bounds = await inspector.locator('dialog[open]').boundingBox();
        assert.ok(bounds.x >= 0 && bounds.y >= 0 && bounds.x + bounds.width <= width && bounds.y + bounds.height <= 844);
      }
    }
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('inspector selection uses exact activation identity when epochs reuse a generation', async () => {
  const { context, page, errors } = await componentPage();
  try {
    const envelope = controlPlaneFixture();
    const earlier = envelope.nodes[0].activations[0];
    const later = envelope.nodes[0].activations[1];
    later.generation = available(1);
    later.reference.activation.generation = 1;
    later.reference.activation.execution_epoch_id = 'epoch-two';
    await page.evaluate(async (value) => {
      const { foldControlPlane } = await import('/ui/activation-inspector.js');
      window.epochEnvelope = value; window.epochFold = foldControlPlane;
      const inspector = document.createElement('ax-activation-inspector');
      inspector.model = foldControlPlane(value); inspector.nodeId = 'builder'; document.body.append(inspector);
    }, envelope);
    const inspector = page.locator('ax-activation-inspector');
    await inspector.getByLabel('Activation generation').selectOption('0');
    assert.match(await inspector.locator('.content').textContent(), /FIRST_GENERATION_ONLY/);
    await inspector.getByLabel('Activation generation').selectOption('1');
    assert.match(await inspector.locator('.content').textContent(), /CURRENT_GENERATION_ONLY/);
    await page.evaluate(() => document.querySelector('ax-activation-inspector').model = window.epochFold(window.epochEnvelope));
    assert.match(await inspector.locator('.content').textContent(), /CURRENT_GENERATION_ONLY/);
    assert.equal(await inspector.getByLabel('Activation generation').inputValue(), '1');
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('retained legacy direct-target turn exposes only its recorded Agent and attributed output', async () => {
  const { context, page, errors } = await componentPage();
  try {
    await page.evaluate(async (envelope) => {
      const { foldControlPlane } = await import('/ui/activation-inspector.js');
      const inspector = document.createElement('ax-activation-inspector');
      inspector.model = foldControlPlane(envelope);
      inspector.nodeId = 'reviewer'; document.body.append(inspector);
    }, legacyControlPlaneFixture({ nodeId: 'reviewer', output: available('RECORDED_DIRECT_OUTPUT') }));
    const inspector = page.locator('ax-activation-inspector');
    assert.match(await inspector.locator('.content').textContent(), /RECORDED_DIRECT_OUTPUT/);
    assert.match(await inspector.locator('.content').textContent(), /GenerationNot recorded/);
    assert.deepEqual(await inspector.evaluate(node => node.model.nodes.map(item => item.node_id)), ['reviewer']);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('Add to chat emits only a typed exact retained reference, including the execution epoch', async () => {
  const { context, page, errors } = await componentPage();
  try {
    await page.evaluate(async (value) => {
      const { foldControlPlane } = await import('/ui/activation-inspector.js');
      window.attachedCoordination = [];
      document.addEventListener('attach-coordination-reference', event => window.attachedCoordination.push(event.detail));
      const inspector = document.createElement('ax-activation-inspector');
      inspector.model = foldControlPlane(value); inspector.nodeId = 'builder'; document.body.append(inspector);
    }, controlPlaneFixture());
    await page.locator('ax-activation-inspector').getByRole('button', { name: 'Add Builder · event · generation 2 to chat', exact: true }).click();
    const attached = await page.evaluate(() => window.attachedCoordination);
    assert.equal(attached.length, 1);
    assert.deepEqual(attached[0].reference, {
      reference_id: 'tool-2', display_name: 'Builder · event · generation 2', kind: 'coordination_reference',
      scope: 'this_turn', origin: null, metadata: { history_version: 'execution_v2', source_session_id: 'session-inspect',
        source_turn_id: 'turn-inspect', node_id: 'builder', generation: 2, reference_id: 'tool-2', type: 'event',
        execution_epoch_id: 'epoch-one', activation_id: 'activation-2' },
    });
    assert.doesNotMatch(JSON.stringify(attached[0].reference), /stdout|PASS|CURRENT_GENERATION_ONLY/);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('legacy Add to chat requires a recorded evidence reference', async () => {
  const { context, page, errors } = await componentPage();
  try {
    await page.evaluate(async (envelope) => {
      const { foldControlPlane } = await import('/ui/activation-inspector.js');
      window.legacyRefFold = foldControlPlane;
      window.legacyRefEnvelope = envelope;
      const inspector = document.createElement('ax-activation-inspector');
      inspector.model = foldControlPlane(envelope); inspector.nodeId = 'reviewer'; document.body.append(inspector);
    }, legacyControlPlaneFixture({ nodeId: 'reviewer', evidence: [{ kind: 'tool_result', reference: missing,
      summary: available('Recorded check summary'), recorded_at: missing, details: missing }] }));
    const inspector = page.locator('ax-activation-inspector');
    assert.equal(await inspector.locator('.add-reference').count(), 0);
    await page.evaluate(() => {
      window.legacyRefEnvelope.nodes[0].activations[0].evidence[0].reference = { status: 'available', value: 'retained-check-event' };
      document.querySelector('ax-activation-inspector').model = window.legacyRefFold(window.legacyRefEnvelope);
    });
    assert.equal(await inspector.locator('.add-reference').count(), 1);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

for (const options of [{theme: 'light', viewport: {width: 1100, height: 900}},
  {theme: 'dark', viewport: {width: 390, height: 840}, reducedMotion: true}]) {
  test(`exact human controls preserve draft choices and show durable receipts (${options.theme})`, async () => {
    const {context, page, errors} = await componentPage(options);
    try {
      const envelope = controlPlaneFixture();
      const selected = envelope.nodes[0].activations.at(-1);
      selected.state = 'accepted'; selected.capabilities.revise = {enabled: true, reason: ''};
      selected.capabilities.revise_invalidates = [{...selected.reference.activation, node_id: 'reviewer', activation_id: 'reviewer-3', generation: 3}];
      const restart = {...selected.reference.activation, node_id: 'failed-work', activation_id: 'failed-4', generation: 4};
      envelope.turn_controls = {execution_epoch_id: 'epoch-one', continue_turn: {enabled: true, reason: ''}, finish: {enabled: true, reason: ''},
        continuation_choices: [{activation: restart, state: 'failed', capability: {enabled: true, reason: ''}}],
        check_choices: [{condition_id: 'repository-check', required_conditions: ['candidate-before', 'candidate-after', 'readiness'], capability: {enabled: false, reason: 'Recorded outcome is still unknown.'}}]};
      await page.evaluate(async value => {
        const {foldControlPlane} = await import('/ui/activation-inspector.js');
        window.controlEnvelope = value; window.controlFold = foldControlPlane; window.humanCommands = [];
        const inspector = document.createElement('ax-activation-inspector');
        inspector.commandHandler = async command => window.humanCommands.push({kind: command.kind, reference: command.reference,
          instruction: command.instruction, includePreviousOutput: command.includePreviousOutput, continuation: command.continuation});
        inspector.receiptRefreshHandler = async () => window.receiptRefreshes = (window.receiptRefreshes || 0) + 1;
        inspector.model = foldControlPlane(value); inspector.nodeId = 'builder'; document.body.append(inspector);
      }, envelope);
      const inspector = page.locator('ax-activation-inspector');
      assert.equal(await inspector.locator('.revise').isDisabled(), true);
      assert.equal(await inspector.locator('.revision-context').isChecked(), false, 'prior answer context requires an explicit choice');
      assert.match(await inspector.locator('.revision-impact').textContent(), /reviewer · generation 3/);
      await inspector.getByLabel('Revision instruction').fill('Explain the check failure without changing the task.');
      await inspector.locator('.revision-context').check();
      await page.evaluate(() => document.querySelector('ax-activation-inspector').model = window.controlFold(window.controlEnvelope));
      assert.equal(await inspector.getByLabel('Revision instruction').inputValue(), 'Explain the check failure without changing the task.');
      assert.equal(await inspector.locator('.revision-context').isChecked(), true);
      if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) await page.screenshot({path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/human-revision-${options.theme}.png`, fullPage: true});
      await inspector.locator('.revise').click();
      assert.deepEqual(await page.evaluate(() => window.humanCommands[0]), {kind: 'revise', reference: selected.reference,
        instruction: 'Explain the check failure without changing the task.', includePreviousOutput: true,
        continuation: undefined});
      await page.evaluate(() => document.querySelector('ax-activation-inspector').showTurnControls());
      assert.equal(await inspector.locator('.continue-turn').isDisabled(), true, 'no restart policy is preselected');
      assert.equal(await inspector.locator('.continue-check').isDisabled(), true);
      assert.match(await inspector.locator('.content').textContent(), /Recorded outcome is still unknown/);
      assert.match(await inspector.locator('.continuation-dependencies').textContent(), /Also runs again: candidate-before, candidate-after, readiness\./);
      await inspector.locator('.continue-work').check();
      await page.evaluate(() => document.querySelector('ax-activation-inspector').model = window.controlFold(window.controlEnvelope));
      assert.equal(await inspector.locator('.continue-work').isChecked(), true);
      if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) await page.screenshot({path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/human-continuation-${options.theme}.png`, fullPage: true});
      await inspector.locator('.continue-turn').click();
      assert.deepEqual(await page.evaluate(() => window.humanCommands[1].continuation), {restart: [restart], checks: []});
      await inspector.locator('.finish-turn').click();
      assert.equal(await page.evaluate(() => window.humanCommands[2].kind), 'finish');
      assert.equal(await inspector.locator('.guide,.resume,.force-finish').count(), 0);
      // Recreate the component as reload does; canonical receipt state remains
      // Accepted and permits only a read refresh without replaying guessed input.
      await page.evaluate(() => {
        const old = document.querySelector('ax-activation-inspector');
        const inspector = document.createElement('ax-activation-inspector');
        inspector.model = window.controlFold(window.controlEnvelope);
        inspector.commandHistory = [{request: {action: 'continue', command_id: 'retained-continue'}, canResubmit: false,
          receipt: {state: 'accepted', revision: 2}}];
        inspector.receiptRefreshHandler = async () => window.receiptRefreshes = (window.receiptRefreshes || 0) + 1;
        old.replaceWith(inspector); inspector.showTurnControls();
      });
      assert.match(await inspector.locator('.command-receipt').textContent(), /Continue · Accepted · waiting for application/);
      await inspector.getByRole('button', {name: 'Check command status'}).click();
      assert.equal(await page.evaluate(() => window.receiptRefreshes), 1);
      assert.equal(await page.evaluate(() => window.humanCommands.length), 3);
      if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) {
        await page.screenshot({path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/human-controls-${options.theme}.png`, fullPage: true});
      }
      assert.deepEqual(errors, []);
    } finally { await context.close(); }
  });
}

test('whole-turn Stop shows exact never-started node evidence without inventing an activation',async()=>{
  const {context,page,errors}=await componentPage({theme:'dark',viewport:{width:390,height:840},reducedMotion:true});
  try {
    const envelope=controlPlaneFixture();envelope.state='cancelled';envelope.turn_revision=available(10);
    envelope.nodes.push({node_id:'unstarted-checker',definition_id:'checker',label:'Checker',definition:{status:'not_recorded'},dependencies:[],activations:[]});
    envelope.stop_requested={command_id:'human-stop',requested_revision:1,evidence:'retained-request',unrun_nodes:['unstarted-checker']};
    const result=await page.evaluate(async envelope=>{
      const {foldControlPlane}=await import('/ui/activation-inspector.js');
      const model=foldControlPlane(envelope);window.stopModel=model;
      const inspector=document.createElement('ax-activation-inspector');inspector.model=model;inspector.nodeId='unstarted-checker';document.body.append(inspector);
      return {agent:model.agents.find(agent=>agent.id==='unstarted-checker'),node:model.nodes.find(node=>node.node_id==='unstarted-checker'),prior:model.agents.find(agent=>agent.id==='builder')};
    },envelope);
    assert.equal(result.agent.state,'stopped');assert.equal(result.agent.summary,'Stopped before starting');
    assert.deepEqual(result.node.activations,[]);assert.equal(result.prior.state,'completed');
    const inspector=page.locator('ax-activation-inspector');
    assert.match(await inspector.locator('.content').textContent(),/Stopped before starting/);
    assert.match(await inspector.locator('.content').textContent(),/Stop command human-stop/);
    assert.match(await inspector.locator('.content').textContent(),/No activation recorded/);
    assert.equal(await inspector.locator('select:visible,.stop:visible,.retry:visible,.revise:visible,.add-reference:visible').count(),0);
    const malformed=await page.evaluate(async envelope=>{
      const {foldControlPlane}=await import('/ui/activation-inspector.js');
      return [null,{...envelope.stop_requested,unrun_nodes:['foreign']},{...envelope.stop_requested,unrun_nodes:['builder']},
        {...envelope.stop_requested,unrun_nodes:['unstarted-checker','unstarted-checker']},{...envelope.stop_requested,requested_revision:1e8}]
        .map(stop_requested=>{const model=foldControlPlane({...envelope,stop_requested});return {unsupported:model.unsupported,agents:model.agents.length,nodes:model.nodes.length};});
    },envelope);
    assert.ok(malformed.every(value=>value.unsupported===true&&value.agents===0&&value.nodes===0));
    if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/stopped-before-starting-dark-narrow.png`,fullPage:true});
    assert.deepEqual(errors,[]);
  } finally {await context.close();}
});

test('Guide uses the actual command consumer and retains an unacknowledged exact request across reload',async()=>{
  const {context,page,errors}=await componentPage({theme:'light',viewport:{width:1100,height:900}});
  const envelope=controlPlaneFixture();envelope.state='running';envelope.turn_revision=available(9);envelope.graph_revision=available(3);
  const activation=envelope.nodes[0].activations.at(-1);activation.state='running';activation.capabilities.guide={enabled:true,reason:''};
  const requests=[];let retainedReceipt=null;
  const receipt=(request,state)=>({request:{schema_version:1,command_id:request.command_id,session_id:request.session_id,turn_id:request.turn_id,
    execution_epoch_id:request.execution_epoch_id,expected_turn_revision:request.expected_turn_revision,expected_graph_revision:request.expected_graph_revision,
    issued_at_ms:1,parameters:{kind:'steer_activation',activation:request.activation,instruction:'exact-retained-guidance',mode:'next_safe_boundary'}},
    source:{kind:'human',session_id:request.session_id,turn_id:request.turn_id,request_evidence:'human-request'},revision:state==='settled'?4:2,state,
    last_transition:state==='settled'?{state:'settled',result:'exact-append-ack'}:{state:'accepted',validation:'validated',pending:'pending'}});
  await page.route('**/api/sessions/session-inspect/turns/turn-inspect/control-commands',async route=>{
    const request=route.request().postDataJSON();requests.push(request);
    if(requests.length===1) return route.fulfill({contentType:'application/json',body:'{"error":"fixture lost the acknowledgement"}'});
    retainedReceipt=receipt(request,'accepted');
    await route.fulfill({contentType:'application/json',body:JSON.stringify(retainedReceipt)});
  });
  await page.route('**/api/sessions/session-inspect/turns/turn-inspect/control-plane',route=>route.fulfill({contentType:'application/json',body:JSON.stringify({...envelope,commands:retainedReceipt?available([retainedReceipt]):missing})}));
  const install=async()=>page.evaluate(async envelope=>{
    const {foldControlPlane}=await import('/ui/activation-inspector.js');
    const source=await fetch('/fixture-app-source').then(response=>response.text());
    const start=source.indexOf('const _coordinationCommands = new Map();');
    const end=source.indexOf('\nfunction scheduleCoordinationGraphRefresh(',start);
    if(start<0||end<start)throw new Error('actual command consumer is unavailable');
    const host=document.createElement('div');host.id='session-lattice-host';document.body.append(host);
    const inspector=document.createElement('ax-activation-inspector');host.append(inspector);
    const state={session:{id:envelope.session_id,coordinationGraphTurnId:envelope.turn_id}};
    let api;
    const refresh=async(sessionId,turnId)=>{
      const current=await fetch(`/api/sessions/${sessionId}/turns/${turnId}/control-plane`).then(response=>response.json());
      inspector.model=foldControlPlane(current);api.hydrateCoordinationCommands(current);
    };
    api=new Function('S','$','refreshCoordinationControlPlane',`${source.slice(start,end)}; return {submitCoordinationCommand,hydrateCoordinationCommands,coordinationCommandHistory};`)(state,
      selector=>document.querySelector(selector),refresh);
    inspector.commandHandler=api.submitCoordinationCommand;inspector.receiptRefreshHandler=()=>refresh(envelope.session_id,envelope.turn_id);
    inspector.model=foldControlPlane(envelope);inspector.nodeId='builder';
    inspector.commandHistory=api.coordinationCommandHistory(envelope.session_id,envelope.turn_id);
    window.guideConsumer={api,inspector,foldControlPlane,envelope};
    await refresh(envelope.session_id,envelope.turn_id);
  },envelope);
  try {
    await install();
    let inspector=page.locator('ax-activation-inspector');
    assert.equal(await inspector.locator('.guide').isDisabled(),true);
    await inspector.getByLabel('Guidance instruction').fill(' Keep the exact QA target. ');
    await inspector.locator('.guide').click();
    await inspector.locator('.command-receipt').filter({hasText:'Outcome unknown'}).waitFor();
    assert.equal(requests.length,1);
    const original=structuredClone(requests[0]);
    assert.equal(original.action,'guide');assert.equal(original.instruction,' Keep the exact QA target. ');
    assert.deepEqual(original.activation,activation.reference.activation);
    assert.equal(original.execution_epoch_id,'epoch-one');assert.equal(original.expected_turn_revision,9);assert.equal(original.expected_graph_revision,3);
    assert.equal(original.include_previous_output,undefined);assert.equal(original.continuation,undefined);
    await page.reload();await page.addScriptTag({type:'module',url:`${runtime.baseUrl}/ui/activation-inspector.js`});await install();
    inspector=page.locator('ax-activation-inspector');
    assert.match(await inspector.locator('.command-receipt').textContent(),/Guide · Outcome unknown/);
    assert.equal(await inspector.locator('.guide').isDisabled(),true);
    assert.deepEqual(await page.evaluate(()=>window.guideConsumer.api.coordinationCommandHistory('session-inspect','turn-inspect')[0].request),original);
    await inspector.getByRole('button',{name:'Check command status'}).click();
    await inspector.locator('.command-receipt').filter({hasText:'Accepted'}).waitFor();
    assert.equal(requests.length,2);assert.deepEqual(requests[1],original,'transport retry keeps exact ID, revisions, target and bytes');
    // Canonical receipts also work with no browser cache or saved instruction.
    retainedReceipt=receipt(original,'settled');
    await page.evaluate(()=>sessionStorage.clear());await page.reload();await page.addScriptTag({type:'module',url:`${runtime.baseUrl}/ui/activation-inspector.js`});await install();
    inspector=page.locator('ax-activation-inspector');
    assert.match(await inspector.locator('.command-receipt').textContent(),/Guide · Input received by the Agent/);
    assert.equal(requests.length,2,'receipt hydration never replays input');
    if(process.env.AXOCOATL_P1_SCREENSHOT_DIR) {
      await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/guide-receipt-light.png`,fullPage:true});
      await page.setViewportSize({width:390,height:840});await page.emulateMedia({reducedMotion:'reduce'});
      await page.evaluate(()=>document.documentElement.dataset.theme='dark');
      await page.waitForFunction(()=>document.querySelector('ax-activation-inspector').shadowRoot.querySelector('dialog').matches(':modal'));
      await inspector.locator('.guidance-form').scrollIntoViewIfNeeded();
      await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/guide-receipt-dark-narrow.png`,fullPage:true});
    }
    await page.evaluate(()=>{
      const {inspector,foldControlPlane,envelope}=window.guideConsumer;
      inspector.model=foldControlPlane({...envelope,stop_requested:{command_id:'stop-after-guide',requested_revision:9,evidence:'retained-request',unrun_nodes:[]}});
    });
    assert.equal(await inspector.locator('.guidance-form').count(),0,'whole-turn Stop removes guidance despite stale capability');
    await page.evaluate(()=>{const {inspector,foldControlPlane,envelope}=window.guideConsumer;inspector.model=foldControlPlane(envelope);inspector.generation=1;});
    assert.equal(await inspector.locator('.guidance-form').count(),0,'historical generation has no executable capability');
    assert.deepEqual(errors,[]);
  } finally {await context.close();}
});


for (const options of [{theme:'light',viewport:{width:1100,height:900}}, {theme:'dark',viewport:{width:390,height:840},reducedMotion:true}]) {
  test(`human response uses the exact live wait and preserves a lost response across reload (${options.theme})`,async()=>{
    const {context,page,errors}=await componentPage(options);
    const envelope=controlPlaneFixture();envelope.state='running';envelope.turn_revision=available(11);envelope.graph_revision=available(3);
    const activation=envelope.nodes[0].activations.at(-1);activation.state='running';
    activation.capabilities.human_responses=[{blocker_id:'human-wait-exact',request:'protected-tool-request',state:{state:'pending'},
      display:{approval_id:'approval-exact',agent_id:'conversation',server:'client-ci',tool:'mcp__client_ci__deploy',tool_display:'deploy',
        arguments_preview:'{"candidate":"abc123","note":"<img src=x onerror=alert(1)>"}',requested_at:1},
      approve:{enabled:true,reason:''},decline:{enabled:true,reason:''}}];
    const requests=[];let retainedReceipt=null;
    const receipt=(request,state)=>({request:{schema_version:1,command_id:request.command_id,session_id:request.session_id,turn_id:request.turn_id,
      execution_epoch_id:request.execution_epoch_id,expected_turn_revision:request.expected_turn_revision,expected_graph_revision:request.expected_graph_revision,
      issued_at_ms:1,parameters:{kind:'resume_blocked',activation:request.activation,blocker_id:request.blocker_id,response:{kind:'approval',approval:'retained-response'}}},
      source:{kind:'human',session_id:request.session_id,turn_id:request.turn_id,request_evidence:'human-request'},revision:state==='settled'?4:2,state});
    await page.route('**/api/sessions/session-inspect/turns/turn-inspect/control-commands',async route=>{
      const request=route.request().postDataJSON();requests.push(request);
      if(requests.length===1)return route.fulfill({contentType:'application/json',body:'{"error":"lost acknowledgement"}'});
      retainedReceipt=receipt(request,'accepted');return route.fulfill({contentType:'application/json',body:JSON.stringify(retainedReceipt)});
    });
    await page.route('**/api/sessions/session-inspect/turns/turn-inspect/control-plane',route=>route.fulfill({contentType:'application/json',body:JSON.stringify({...envelope,commands:retainedReceipt?available([retainedReceipt]):missing})}));
    const install=async()=>page.evaluate(async envelope=>{
      const {foldControlPlane}=await import('/ui/activation-inspector.js');
      const source=await fetch('/fixture-app-source').then(response=>response.text());
      const start=source.indexOf('const _coordinationCommands = new Map();');const end=source.indexOf('\nfunction scheduleCoordinationGraphRefresh(',start);
      if(start<0||end<start)throw new Error('actual command consumer unavailable');
      const host=document.createElement('div');host.id='session-lattice-host';document.body.append(host);
      const inspector=document.createElement('ax-activation-inspector');host.append(inspector);
      let api;const refresh=async(sessionId,turnId)=>{const current=await fetch(`/api/sessions/${sessionId}/turns/${turnId}/control-plane`).then(response=>response.json());
        inspector.model=foldControlPlane(current);api.hydrateCoordinationCommands(current);};
      api=new Function('S','$','refreshCoordinationControlPlane',`${source.slice(start,end)}; return {submitCoordinationCommand,hydrateCoordinationCommands,coordinationCommandHistory};`)(
        {session:{id:envelope.session_id,coordinationGraphTurnId:envelope.turn_id}},selector=>document.querySelector(selector),refresh);
      inspector.commandHandler=api.submitCoordinationCommand;inspector.receiptRefreshHandler=()=>refresh(envelope.session_id,envelope.turn_id);
      inspector.model=foldControlPlane(envelope);inspector.nodeId='builder';inspector.commandHistory=api.coordinationCommandHistory(envelope.session_id,envelope.turn_id);
      window.resumeConsumer={api,inspector,foldControlPlane,envelope};await refresh(envelope.session_id,envelope.turn_id);
    },envelope);
    try {
      await install();let inspector=page.locator('ax-activation-inspector');
      assert.equal(await inspector.locator('.resume-approve').isDisabled(),false);
      assert.equal(await inspector.locator('.resume-decline').isDisabled(),true);
      assert.equal(await inspector.locator('.human-response-arguments').textContent(),activation.capabilities.human_responses[0].display.arguments_preview);
      assert.equal(await inspector.locator('.human-response-arguments img').count(),0);
      await inspector.locator('.human-response-form').scrollIntoViewIfNeeded();
      if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/resume-${options.theme}.png`,fullPage:true});
      if(options.theme==='dark') {
        await inspector.getByLabel('Reason for declining').fill(' Use the staging candidate first. ');
        await inspector.locator('.resume-decline').click();
      } else await inspector.locator('.resume-approve').click();
      await inspector.locator('.command-receipt').filter({hasText:'Outcome unknown'}).waitFor();
      const original=structuredClone(requests[0]);assert.equal(original.action,'resume');assert.equal(original.blocker_id,'human-wait-exact');
      assert.deepEqual(original.activation,activation.reference.activation);
      assert.deepEqual(original.human_response,options.theme==='dark'?{kind:'decline',reason:' Use the staging candidate first. '}:{kind:'approval'});
      assert.equal(original.instruction,undefined);assert.equal(original.expected_turn_revision,11);
      await page.reload();await page.addScriptTag({type:'module',url:`${runtime.baseUrl}/ui/activation-inspector.js`});await install();inspector=page.locator('ax-activation-inspector');
      assert.equal(await inspector.locator('.resume-approve').isDisabled(),true);
      assert.deepEqual(await page.evaluate(()=>window.resumeConsumer.api.coordinationCommandHistory('session-inspect','turn-inspect')[0].request),original);
      await inspector.getByRole('button',{name:'Check command status'}).click();
      await inspector.locator('.command-receipt').filter({hasText:'Accepted'}).waitFor();assert.deepEqual(requests[1],original);
      retainedReceipt=receipt(original,'settled');await page.evaluate(()=>sessionStorage.clear());
      await page.reload();await page.addScriptTag({type:'module',url:`${runtime.baseUrl}/ui/activation-inspector.js`});await install();inspector=page.locator('ax-activation-inspector');
      assert.match(await inspector.locator('.command-receipt').textContent(),/Response received by the Agent/);assert.equal(requests.length,2);
      await page.evaluate(()=>{
        const {inspector,foldControlPlane,envelope}=window.resumeConsumer;const response=envelope.nodes[0].activations.at(-1).capabilities.human_responses[0];
        response.state={state:'interrupted',epoch_id:'epoch-one'};response.approve={enabled:false,reason:'No live wait'};response.decline=response.approve;
        inspector.model=foldControlPlane(envelope);
      });
      assert.equal(await inspector.locator('.resume-approve,.resume-decline').count(),0);assert.match(await inspector.locator('.human-response-form').textContent(),/cannot be resumed/);
      const invalid=await page.evaluate(()=>{
        const {foldControlPlane,envelope}=window.resumeConsumer;const values=[];
        for(const mutate of [value=>value.nodes[0].activations.at(-1).capabilities.human_responses=null,
          value=>value.nodes[0].activations.at(-1).capabilities.human_responses[0].approve.enabled=true,
          value=>value.nodes[0].activations.at(-1).capabilities.human_responses[0].state={state:'unknown-future'}]) {
          const value=structuredClone(envelope);mutate(value);values.push(foldControlPlane(value).unsupported);
        } return values;
      });assert.deepEqual(invalid,[true,true,true]);assert.deepEqual(errors,[]);
    } finally {await context.close();}
  });
}

test('graph refresh bounds pending reads and rejects a response for a replaced turn', async () => {
 const {context,page,errors}=await componentPage();try{
  const result=await page.evaluate(async()=>{
   const source=await fetch('/fixture-app-source').then(response=>response.text());
   const start=source.indexOf('function refreshCoordinationControlPlane('),end=source.indexOf('\nasync function sessionLatticeBuild(',start);
   const state={session:{id:'session',coordinationGraphTurnId:'first'}};
   const pending=[],rendered=[],scheduled=[];let active=0,maximum=0;
   const fetcher=(url,options)=>new Promise(resolve=>{active++;maximum=Math.max(maximum,active);pending.push({url,signal:options.signal,resolve:body=>{active--;resolve({ok:true,json:async()=>body});}});});
   const api=new Function('S','$','fetch','isExecutionHistoryTurn','hydrateCoordinationCommands','sessionLatticeBuild','scheduleCoordinationGraphRefresh',`let _coordinationReadId=0,_coordinationInFlight=null;${source.slice(start,end)};return refreshCoordinationControlPlane;`)(state,()=>null,fetcher,()=>true,()=>{},async(_session,turn)=>rendered.push(turn),turn=>scheduled.push(turn));
   const response=turn=>({schema_version:1,history_version:'execution_v2',session_id:'session',turn_id:turn,nodes:[]});
   const initial=api('session','first');for(let i=0;i<100;i++)void api('session','first');
   const concurrent=pending.length;pending[0].resolve(response('first'));await initial;
   const trailing=api('session','first');state.session.coordinationGraphTurnId='second';const second=api('session','second');
   const oldAborted=pending[1].signal.aborted;pending[2].resolve(response('second'));await second;pending[1].resolve(response('first'));await trailing;
   return{concurrent,scheduled,rendered,oldAborted,maximum};
  });assert.deepEqual(result,{concurrent:1,scheduled:['first'],rendered:['first','second'],oldAborted:true,maximum:2});assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('reloaded atomic Revise continuation keeps its exact target and Revise label', async () => {
 const {context,page,errors}=await componentPage();try{
  const result=await page.evaluate(async()=>{
   const source=await fetch('/fixture-app-source').then(response=>response.text());const start=source.indexOf('const _coordinationCommands = new Map();'),end=source.indexOf('\nfunction scheduleCoordinationGraphRefresh(',start);
   const state={session:{id:'session',coordinationGraphTurnId:'turn'}};
   const api=new Function('S','$','refreshCoordinationControlPlane',`${source.slice(start,end)};return {hydrateCoordinationCommands,coordinationCommandHistory};`)(state,()=>null,()=>{});
   const previous={session_id:'session',turn_id:'turn',execution_epoch_id:'prior-epoch',node_id:'engineer',activation_id:'accepted-one',generation:1};
   api.hydrateCoordinationCommands({session_id:'session',turn_id:'turn',commands:{status:'available',value:[{revision:4,state:'settled',source:{kind:'human'},request:{schema_version:1,command_id:'reviewed-revise',session_id:'session',turn_id:'turn',execution_epoch_id:'attention-epoch',expected_turn_revision:12,expected_graph_revision:2,parameters:{kind:'continue_turn',plan:{selections:[{kind:'revise',previous},{kind:'await_dependencies',node_id:'reviewer'}]}}}}]}});
   const record=api.coordinationCommandHistory('session','turn')[0];return{action:record.request.action,activation:record.request.activation,canResubmit:record.canResubmit,previous};
  });assert.equal(result.action,'revise');assert.deepEqual(result.activation,result.previous);assert.equal(result.canResubmit,false);assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

for (const options of [{theme:'light',viewport:{width:1100,height:900}}, {theme:'dark',viewport:{width:390,height:840},reducedMotion:true}]) {
  test(`recovered controls submit exact requests through the existing validator (${options.theme})`, async () => {
    const {context,page,errors}=await componentPage(options);
    const envelope=controlPlaneFixture();envelope.state='needs_attention';envelope.turn_revision=available(22);envelope.graph_revision=available(4);
    const reason='Axocoatl will reconnect this Session runtime and revalidate the exact revision, remaining budget, grant and repository before applying this request.';
    const pending={enabled:false,requires_revalidation:true,reason};
    const selected=envelope.nodes[0].activations.at(-1);selected.state='accepted';selected.capabilities.revise={...pending};
    selected.capabilities.revise_invalidates=[{...selected.reference.activation,node_id:'reviewer',activation_id:'reviewer-three',generation:3}];
    const restart={...selected.reference.activation,node_id:'interrupted-checker',activation_id:'checker-four',generation:4};
    envelope.turn_controls={execution_epoch_id:'recovered-current-epoch',continue_turn:{...pending},finish:{...pending},
      continuation_choices:[{activation:restart,state:'interrupted',capability:{...pending}}],
      check_choices:[{condition_id:'check-two',required_conditions:['candidate-before','candidate-after','ready'],capability:{...pending}}]};
    const requests=[];
    await page.route('**/api/sessions/session-inspect/turns/turn-inspect/control-commands',route=>{
      const request=route.request().postDataJSON();requests.push(request);
      return route.fulfill({contentType:'application/json',body:JSON.stringify({request:{...request,parameters:{kind:'finish_turn',mode:'normal'}},
        revision:2,state:'rejected',last_transition:{state:'rejected',failure:{code:'invalid_control',message:'dispatch/control is outside the current grant'}}})});
    });
    try {
      await page.evaluate(async envelope=>{
        const {foldControlPlane}=await import('/ui/activation-inspector.js');
        const source=await fetch('/fixture-app-source').then(response=>response.text());
        const start=source.indexOf('const _coordinationCommands = new Map();');const end=source.indexOf('\nfunction scheduleCoordinationGraphRefresh(',start);
        if(start<0||end<start)throw new Error('actual command consumer unavailable');
        const host=document.createElement('div');host.id='session-lattice-host';document.body.append(host);
        const inspector=document.createElement('ax-activation-inspector');host.append(inspector);
        const api=new Function('S','$','refreshCoordinationControlPlane',`${source.slice(start,end)}; return {submitCoordinationCommand,coordinationCommandHistory};`)(
          {session:{id:envelope.session_id,coordinationGraphTurnId:envelope.turn_id}},selector=>document.querySelector(selector),async()=>{});
        inspector.commandHandler=api.submitCoordinationCommand;inspector.model=foldControlPlane(envelope);inspector.nodeId='builder';
        window.recovered={inspector,foldControlPlane,envelope};
      },envelope);
      const inspector=page.locator('ax-activation-inspector');
      assert.match(await inspector.locator('.recovery-notice').textContent(),/revalidate.*remaining budget/);
      assert.match(await inspector.locator('.revision-impact').textContent(),/reviewer · generation 3/);
      assert.equal(await inspector.locator('.actions > button.retry').count(),0,'paused work uses explicit Continue in a new epoch');
      await inspector.getByLabel('Revision instruction').fill('Use the failed check output to revise this exact result.');
      await inspector.locator('.revise').click();
      await inspector.locator('.command-receipt').filter({hasText:'Rejected'}).waitFor();
      assert.match(await inspector.locator('.command-receipt').textContent(),/outside the current grant/);
      assert.equal(requests[0].action,'revise');assert.deepEqual(requests[0].activation,selected.reference.activation);
      assert.equal(requests[0].execution_epoch_id,'recovered-current-epoch');assert.equal(requests[0].expected_turn_revision,22);assert.equal(requests[0].expected_graph_revision,4);
      assert.equal(requests[0].requires_revalidation,undefined,'presentation marker never becomes execution authority');
      await page.evaluate(()=>window.recovered.inspector.showTurnControls());
      assert.equal(await inspector.locator('.continue-turn').isDisabled(),true);
      await inspector.locator('.continue-work').check();await inspector.locator('.continue-check').check();
      assert.match(await inspector.locator('.continuation-dependencies').textContent(),/candidate-before, candidate-after, ready/);
      await inspector.locator('.continue-turn').click();
      await inspector.locator('.command-receipt').filter({hasText:'Continue · Rejected'}).waitFor();
      assert.equal(requests[1].action,'continue');assert.deepEqual(requests[1].continuation,{restart:[restart],checks:['check-two']});
      await inspector.locator('.finish-turn').click();
      await inspector.locator('.command-receipt').filter({hasText:'Finish · Rejected'}).waitFor();
      assert.equal(requests[2].action,'finish');assert.equal(requests[2].expected_turn_revision,22);assert.equal(requests[2].execution_epoch_id,'recovered-current-epoch');
      if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/recovered-controls-${options.theme}.png`,fullPage:true});
      const unsupported=await page.evaluate(()=>{
        const {foldControlPlane,envelope}=window.recovered;
        return ['completed','running'].map(state=>foldControlPlane({...envelope,state}).unsupported);
      });
      assert.deepEqual(unsupported,[true,true]);
      assert.deepEqual(errors,[]);
    } finally {await context.close();}
  });
}

for (const options of [{theme:'light',viewport:{width:1100,height:900}}, {theme:'dark',viewport:{width:390,height:840},reducedMotion:true}]) {
  test(`partial Finish requires exact review and explicit sink selection (${options.theme})`, async()=>{
    const {context,page,errors}=await componentPage(options);
    try {
      const envelope=controlPlaneFixture(); envelope.state='needs_attention'; envelope.turn_revision=available(9); envelope.graph_revision=available(1);
      const selected=envelope.nodes[0].activations.at(-1); selected.state='accepted';
      envelope.nodes.push({node_id:'unstarted-checker',label:'Checker',definition:missing,dependencies:[],activations:[]});
      const review={selected_activations:[],stop_activations:[],unrun_nodes:['unstarted-checker'],missing_conditions:['repository-check'],confirmed:true};
      envelope.turn_controls={execution_epoch_id:'epoch-one',continue_turn:{enabled:false,reason:'No continuation selected'},finish:{enabled:false,reason:'Required check is unmet'},continuation_choices:[],check_choices:[],
        partial_finish:{capability:{enabled:true,reason:''},available_sinks:[selected.reference.activation],review}};
      await page.evaluate(async envelope=>{
        const {foldControlPlane}=await import('/ui/activation-inspector.js');window.partialEnvelope=envelope;window.partialFold=foldControlPlane;window.partialCommands=[];
        const inspector=document.createElement('ax-activation-inspector');inspector.model=foldControlPlane(envelope);inspector.commandHandler=async command=>window.partialCommands.push({kind:command.kind,partialFinish:command.partialFinish});document.body.append(inspector);inspector.showTurnControls();
      },envelope);
      const inspector=page.locator('ax-activation-inspector');
      assert.equal(await inspector.locator('.finish-turn').isDisabled(),true);
      assert.equal(await inspector.locator('.finish-partial').isDisabled(),true);
      assert.equal(await inspector.locator('.confirm-partial-finish').isChecked(),false);
      assert.equal(await inspector.locator('.partial-result').isChecked(),false);
      assert.match(await inspector.locator('.partial-finish-review').textContent(),/unstarted-checker/);
      assert.match(await inspector.locator('.partial-finish-review').textContent(),/repository-check/);
      assert.match(await inspector.locator('.partial-finish-review').textContent(),/Leaving every result unselected finishes with no result/);
      await page.evaluate(()=>document.querySelector('ax-activation-inspector').commandHistory=[{request:{action:'finish',command_id:'normal-finish-waiting'},receipt:{state:'accepted',revision:2}}]);
      await inspector.locator('.partial-result').check();await inspector.locator('.confirm-partial-finish').check();
      assert.equal(await inspector.locator('.finish-partial').isDisabled(),false,'pending normal Finish cannot prevent explicit partial finalization');
      await page.evaluate(()=>document.querySelector('ax-activation-inspector').model=window.partialFold(window.partialEnvelope));
      assert.equal(await inspector.locator('.partial-result').isChecked(),true,'same-revision refresh preserves review');
      await page.evaluate(()=>{window.partialEnvelope.turn_revision.value++;document.querySelector('ax-activation-inspector').model=window.partialFold(window.partialEnvelope)});
      assert.equal(await inspector.locator('.confirm-partial-finish').isChecked(),false,'changed revision requires a fresh confirmation');
      await inspector.locator('.partial-result').check();await inspector.locator('.confirm-partial-finish').check();
      if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/partial-finish-${options.theme}.png`,fullPage:true});
      await inspector.locator('.finish-partial').click();
      assert.deepEqual(await page.evaluate(()=>window.partialCommands),[{kind:'finish',partialFinish:{...review,selected_activations:[selected.reference.activation]}}]);
      const folded=await page.evaluate(async envelope=>{
        envelope.state='finished';envelope.stop_requested={command_id:'human-partial',requested_revision:9,evidence:'retained-review',unrun_nodes:['unstarted-checker'],closure:'finished',partial_finish:{selected_activations:[],stop_activations:[],missing_conditions:['check-evidence'],missing_condition_ids:['repository-check']}};
        const first=window.partialFold(envelope);envelope.stop_requested.partial_finish.selected_activations=[envelope.nodes[0].activations.at(-1).reference.activation];const selected=window.partialFold(envelope);
        return {empty:first.answer,selected:selected.answer,summary:selected.agents.find(item=>item.id==='unstarted-checker').summary,unsupported:selected.unsupported};
      },envelope);
      assert.equal(folded.empty,'');assert.equal(folded.selected,'CURRENT_GENERATION_ONLY');assert.equal(folded.summary,'Skipped by partial Finish');assert.ok(!folded.unsupported);
      assert.deepEqual(errors,[]);
    }finally{await context.close();}
  });
}

test('never-started native descendants are blocked by the current failed generation and unblock after Retry',async()=>{const{context,page,errors}=await componentPage();try{const envelope=controlPlaneFixture();envelope.state='running';envelope.nodes[0].activations=envelope.nodes[0].activations.slice(0,1);envelope.nodes[0].activations[0].state='failed';envelope.nodes.push({node_id:'reviewer',definition_id:'reviewer-definition',label:'Reviewer',definition:missing,dependencies:['builder'],activations:[]},{node_id:'report',definition_id:'report-definition',label:'Report',definition:missing,dependencies:['reviewer'],activations:[]},{node_id:'peer',definition_id:'peer-definition',label:'Independent peer',definition:missing,dependencies:[],activations:[]});const result=await page.evaluate(async envelope=>{const{foldControlPlane}=await import('/ui/activation-inspector.js');const inspect=()=>{const model=foldControlPlane(envelope);return{states:Object.fromEntries(model.agents.map(agent=>[agent.id,agent.state])),histories:model.nodes.map(node=>node.activations.length),text:model.agents.map(agent=>agent.summary).join(' ')}};const failed=inspect();const retry=structuredClone(envelope.nodes[0].activations[0]);retry.state='running';retry.reference.activation.generation=2;retry.reference.activation.activation_id='retry-generation';retry.generation={status:'available',value:2};envelope.nodes[0].activations.push(retry);const running=inspect();retry.state='accepted';const accepted=inspect();return{failed,running,accepted,originalState:envelope.nodes[0].activations[0].state};},envelope);assert.deepEqual(result.failed.states,{builder:'failed',reviewer:'blocked',report:'blocked',peer:'waiting'});assert.deepEqual(result.failed.histories,[1,0,0,0]);assert.match(result.failed.text,/Blocked by Builder/);assert.deepEqual(result.running.states,{builder:'working',reviewer:'waiting',report:'waiting',peer:'waiting'});assert.deepEqual(result.accepted.states,{builder:'completed',reviewer:'waiting',report:'waiting',peer:'waiting'});assert.equal(result.originalState,'failed');assert.deepEqual(result.accepted.histories,[2,0,0,0]);assert.deepEqual(errors,[]);}finally{await context.close();}});

for (const theme of ['light', 'dark']) test(`turn controls show each required check with its failed output and name the check to rerun (${theme})`, async () => {
  const {context, page, errors} = await componentPage(theme === 'dark' ? {theme, viewport: {width: 390, height: 844}} : {theme});
  try {
    const envelope = controlPlaneFixture();
    envelope.state = 'needs_attention';
    envelope.nodes[0].activations.at(-1).state = 'accepted';
    envelope.required_checks = [{argv: ['sh', '-c', 'npm test'], state: 'failed', run_id: 'required-check-run',
      process_status: {kind: 'exited', code: 1}, effect_disposition: 'outcome_recorded', primary_exit: null, quiescent: true,
      reason: null, evidence: 'check-result', candidate_sha256: null, exit_code: 1, stdout: '1 test failed\n',
      stderr: 'AssertionError: expected 2 <b>\n', stdout_truncated: false, stderr_truncated: true},
      {argv: ['cargo', 'test', '--', 'not slow'], state: 'passed', run_id: 'required-check-two', process_status: {kind: 'exited', code: 0},
        effect_disposition: 'outcome_recorded', primary_exit: null, quiescent: true, reason: null, evidence: 'check-two',
        candidate_sha256: null, exit_code: 0, stdout: '', stderr: '', stdout_truncated: false, stderr_truncated: false}];
    // The second check passed on its own, but not together with the first on
    // the current tree: the summary says so above the per-check results.
    envelope.required_check_readiness = {state: 'failed', candidate_sha256: 'tree',
      reason: 'Some checks ran on an older tree than the current one. Continue runs them all again.'};
    const group = ['required-check:0', 'required-check:1', 'required-check:2', 'required-check:3', 'required-check:ready'];
    envelope.turn_controls = {execution_epoch_id: 'epoch-one', continue_turn: {enabled: true, reason: ''},
      finish: {enabled: false, reason: 'A required check failed.'}, continuation_choices: [],
      check_choices: group.map(condition_id => ({condition_id,
        required_conditions: group.filter(id => id !== condition_id),
        capability: {enabled: true, reason: ''}}))};
    await page.evaluate(async value => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js');
      const inspector = document.createElement('ax-activation-inspector');
      inspector.commandHandler = async command => { window.continued = command.continuation; };
      inspector.model = foldControlPlane(value); document.body.append(inspector); inspector.showTurnControls();
    }, envelope);
    const inspector = page.locator('ax-activation-inspector');
    const readiness = inspector.locator('.check-readiness');
    assert.equal(await readiness.locator('.check-readiness-state').textContent(), 'Not ready');
    assert.equal(await readiness.locator('.check-readiness-reason').textContent(),
      'Some checks ran on an older tree than the current one. Continue runs them all again.');
    const checks = inspector.locator('.required-check');
    assert.equal(await checks.count(), 2);
    if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) await page.screenshot({path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/required-check-readiness-${theme}.png`, fullPage: true});
    const failed = checks.nth(0);
    assert.equal(await failed.locator('.check-command').textContent(), 'npm test');
    assert.equal(await failed.locator('.check-state').textContent(), 'Failed · exit code 1');
    assert.equal(await failed.locator('.check-stdout').textContent(), '1 test failed\n');
    assert.equal(await failed.locator('.check-stderr').textContent(), 'AssertionError: expected 2 <b>\n', 'output is text, never markup');
    assert.match(await failed.textContent(), /Errors \(truncated\)/);
    assert.equal(await checks.nth(1).locator('.check-state').textContent(), 'Passed · exit code 0');
    assert.equal(await checks.nth(1).locator('pre').count(), 1, 'an empty output adds no preview');
    const content = await inspector.locator('.content').textContent();
    for (const label of ['Repository capture before the required checks', 'Required check · npm test',
      'Repository capture after the required checks', 'Readiness of the required checks']) assert.match(content, new RegExp(label));
    // An argv shows as a shell reads it, never as words a shell would split.
    assert.equal(await checks.nth(1).locator('.check-command').textContent(), "cargo test -- 'not slow'");
    assert.match(content, /Required check · cargo test -- 'not slow'/);
    assert.equal(await inspector.locator('.continuation-dependencies').count(), 1, 'said once for the whole group');
    assert.equal(await inspector.locator('.continuation-dependencies').textContent(),
      'Any required check you select runs every required check again between fresh repository captures, then records readiness.');
    await inspector.getByLabel('Required check · npm test').check();
    await inspector.locator('.continue-turn').click();
    assert.deepEqual(await page.evaluate(() => window.continued), {restart: [], checks: ['required-check:1']});
    // Once every check passed together on the current tree, the summary says
    // the turn is ready and gives no reason to act.
    await page.evaluate(async value => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js');
      value.required_check_readiness = {state: 'passed', candidate_sha256: 'tree', reason: 'Every check passed on the current tree and left it unchanged.'};
      const inspector = document.querySelector('ax-activation-inspector');
      inspector.model = foldControlPlane(value); inspector.showTurnControls();
    }, envelope);
    assert.equal(await readiness.locator('.check-readiness-state').textContent(), 'Ready · every check passed on the current tree');
    assert.equal(await readiness.locator('.check-readiness-reason').count(), 0);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

for (const theme of ['light', 'dark']) test(`turn controls show the required review's verdict, findings and round, and rerun it by name (${theme})`, async () => {
  const {context, page, errors} = await componentPage(theme === 'dark' ? {theme, viewport: {width: 390, height: 844}} : {theme});
  try {
    const envelope = controlPlaneFixture();
    envelope.state = 'needs_attention';
    envelope.nodes[0].activations.at(-1).state = 'accepted';
    envelope.required_review = {reviewer: 'reviewer', state: 'changes', verdict: 'changes', round: 2, max_rounds: 2, current: true,
      candidate_sha256: 'tree', findings: 'src/lib.rs:3: the new function has no test <b>',
      reason: 'The reviewer asked for changes in round 2 of 2, the last round the host runs. Read the findings, then Revise the lead with them, change the files yourself and Continue the review, or Finish.'};
    envelope.turn_controls = {execution_epoch_id: 'epoch-one', continue_turn: {enabled: true, reason: ''},
      finish: {enabled: false, reason: 'The review did not approve.'}, continuation_choices: [],
      check_choices: [{condition_id: 'required-review:verdict', required_conditions: [], capability: {enabled: true, reason: ''}}]};
    await page.evaluate(async value => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js');
      const inspector = document.createElement('ax-activation-inspector');
      inspector.commandHandler = async command => { window.continued = command.continuation; };
      inspector.model = foldControlPlane(value); document.body.append(inspector); inspector.showTurnControls();
    }, envelope);
    const inspector = page.locator('ax-activation-inspector');
    const review = inspector.locator('.required-review');
    assert.equal(await review.locator('.review-state').textContent(), 'Changes requested · round 2 of 2');
    assert.match(await review.locator('.review-reason').textContent(), /the last round the host runs/);
    assert.equal(await review.locator('.review-findings').textContent(), 'src/lib.rs:3: the new function has no test <b>', 'findings are text, never markup');
    if (process.env.AXOCOATL_P1_SCREENSHOT_DIR) await page.screenshot({path: `${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/required-review-${theme}.png`, fullPage: true});
    await inspector.getByLabel('Required review · run the reviewer again').check();
    await inspector.locator('.continue-turn').click();
    assert.deepEqual(await page.evaluate(() => window.continued), {restart: [], checks: ['required-review:verdict']});
    await page.evaluate(async value => {
      const {foldControlPlane} = await import('/ui/activation-inspector.js');
      value.state = 'completed';
      value.required_review = {...value.required_review, state: 'approved', verdict: 'approve', findings: '', reason: 'The reviewer approved this result in round 2 of 2.'};
      const inspector = document.querySelector('ax-activation-inspector');
      inspector.model = foldControlPlane(value); inspector.showTurnControls();
    }, envelope);
    assert.equal(await review.locator('.review-state').textContent(), 'Approved · round 2 of 2');
    assert.equal(await review.locator('.review-findings').count(), 0);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});
