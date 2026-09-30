import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';

let runtime, browser;
before(async () => {
  runtime = process.env.AXOCOATL_COMPONENT_BASE_URL
    ? { baseUrl: process.env.AXOCOATL_COMPONENT_BASE_URL, stop: async () => {} }
    : await launchTestDaemon();
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
});
after(async () => { await browser?.close(); await runtime?.stop(); });

test('stream invalidations coalesce with one trailing read and no response feedback loop', async () => {
 const {context,page,errors}=await fixture();const held=[];let reads=0;
 try {
  await page.route('**/api/sessions/session/turns/turn/control-plane',route=>{reads++;held.push(route);});
  await page.evaluate(()=>{const guide=document.querySelector('ax-session-guidance');for(let i=0;i<100;i++)void guide.refresh();});
  await page.waitForFunction(()=>document.querySelector('ax-session-guidance').read?.dirty===true);
  await new Promise(resolve=>setTimeout(resolve,150));assert.equal(reads,1);
  await held.shift().fulfill({json:envelope()});
  for(let i=0;i<30&&reads<2;i++)await new Promise(resolve=>setTimeout(resolve,20));
  assert.equal(reads,2,'one trailing read observes changes during the first read');
  await held.shift().fulfill({json:envelope(2)});
  await page.waitForFunction(()=>document.querySelector('ax-session-guidance').envelope?.turn_revision.value===4);
  await new Promise(resolve=>setTimeout(resolve,250));assert.equal(reads,2);
  assert.equal(await page.locator('ax-session-guidance').evaluate(guide=>guide.canSend),false,'replacement still requires an explicit choice');
  assert.deepEqual(errors,[]);
 } finally {for(const route of held)await route.abort().catch(()=>{});await context.close();}
});

function envelope(generation = 1, state = 'running') {
  const activation = {session_id:'session',turn_id:'turn',execution_epoch_id:'epoch',node_id:'review',activation_id:`review-${generation}`,generation};
  return {schema_version:1,history_version:'execution_v2',session_id:'session',turn_id:'turn',state,
    turn_revision:{status:'available',value:generation+2},graph_revision:{status:'available',value:1},
    nodes:[{node_id:'review',label:'QA reviewer <literal>',activations:[{reference:{kind:'exact',activation},capabilities:{guide:{enabled:state==='running'}}}]}]};
}
async function fixture(initial = envelope()) {
  const context = await browser.newContext({viewport:{width:390,height:844},colorScheme:'dark',reducedMotion:'reduce'});
  const page = await context.newPage(), errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/guidance-fixture', route => route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="dark"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"><ax-session-guidance hidden></ax-session-guidance><script type="module">import '/ui/session-guidance.js';const guide=document.querySelector('ax-session-guidance');guide.setIdentity('session','turn');window.calls=[];guide.commandHandler=async input=>{window.calls.push(input);return {request:input.request||{command_id:'guidance-original',session_id:'session',turn_id:'turn',action:'guide',instruction:input.instruction},receipt:{state:'accepted'}};};</script></html>`}));
  await page.route('**/api/sessions/session/turns/turn/control-plane', route => route.fulfill({json:initial}));
  await page.goto(`${runtime.baseUrl}/guidance-fixture`);
  // Before the module upgrades the element, `envelope` is undefined. Wait for
  // the actual retained read, not a predicate that also accepts an unready element.
  await page.waitForFunction(() => document.querySelector('ax-session-guidance').envelope?.turn_id === 'turn');
  return {context,page,errors};
}

test('composer guidance targets one exact generation and never silently follows its replacement', async () => {
  const {context,page,errors} = await fixture();
  try {
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.canSend), true);
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.send('Check the failing assertion.')), true);
    const calls = await page.evaluate(() => window.calls);
    assert.equal(calls[0].reference.activation.activation_id, 'review-1');
    assert.equal(calls[0].instruction, 'Check the failing assertion.');
    await page.locator('ax-session-guidance').evaluate((element, next) => element.acceptEnvelope(next), envelope(2));
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.canSend), false);
    await page.locator('ax-session-guidance').evaluate((element, next) => element.acceptEnvelope(next), envelope(2));
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.canSend), false);
    await page.getByLabel('Agent receiving guidance').selectOption('review-2');
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.send('Use the new result.')), true);
    assert.equal(await page.evaluate(() => window.calls[1].reference.activation.activation_id), 'review-2');
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('NeedsAttention exposes exact turn controls and does not send another message as work', async () => {
  const {context,page,errors} = await fixture(envelope(1, 'needs_attention'));
  try {
    await page.locator('ax-session-guidance').evaluate(element => element.addEventListener('open-current-turn-controls', event => { window.openedTurn = event.detail; }));
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.send('Continue')), false);
    await page.getByRole('button', {name:'Turn controls', exact:true}).click();
    assert.deepEqual(await page.evaluate(() => window.openedTurn), {sessionId:'session',turnId:'turn'});
    assert.equal(await page.evaluate(() => window.calls.length), 0);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('canonical completion hides stale live guidance without a final stream frame',async()=>{
 const {context,page,errors}=await fixture();let reads=0;try{
  await page.route('**/api/sessions/session/turns/turn/control-plane',route=>{reads++;return route.fulfill({json:envelope(1,'completed')});});
  const turn={owner:{session_id:'session'},turn_id:'turn',revision:8,state:'completed'};
  await page.locator('ax-session-guidance').evaluate((guide,turn)=>{guide.observeRetainedTurn({...turn,owner:{session_id:'foreign'}});guide.observeRetainedTurn({...turn,turn_id:'foreign'});},turn);assert.equal(reads,0);
  await page.locator('ax-session-guidance').evaluate((guide,turn)=>{for(let i=0;i<20;i++)guide.observeRetainedTurn(turn);},turn);
  await page.locator('ax-session-guidance').waitFor({state:'hidden'});assert.equal(reads,1);assert.equal(await page.locator('ax-session-guidance').evaluate(guide=>guide.canSend),false);assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('lost guidance reply survives reload and checks the exact original command', async () => {
  const {context,page,errors} = await fixture();
  try {
    await page.locator('ax-session-guidance').evaluate(element => { element.commandHandler = async input => ({request:{command_id:'exact-old-command',session_id:'session',turn_id:'turn',action:'guide',instruction:input.instruction},error:'Reply lost'}); });
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.send('Retain this exact guidance.')), false);
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.canSend), false);
    await page.reload();
    await page.getByRole('button', {name:'Check guidance receipt'}).click();
    assert.deepEqual(await page.evaluate(() => window.calls[0].request), {command_id:'exact-old-command',session_id:'session',turn_id:'turn',action:'guide',instruction:'Retain this exact guidance.'});
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.pendingRequest), null);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('a stale command refusal preserves editable guidance without creating an uncertain retry', async () => {
  const {context,page} = await fixture();
  try {
    await page.locator('ax-session-guidance').evaluate(element => { element.commandHandler = async () => ({request:{command_id:'stale'},error:'Turn revision changed',errorStatus:409}); });
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.send('Keep this draft.')), false);
    assert.equal(await page.locator('ax-session-guidance').evaluate(element => element.pendingRequest), null);
    await page.getByText('Turn revision changed', {exact:true}).waitFor();
  } finally { await context.close(); }
});

test('same composer guidance preserves typed selected references and attachment IDs',async()=>{const{context,page}=await fixture();try{
 const selected={references:[{kind:'coordination_reference',reference_id:'output-1',display_name:'QA evidence',scope:'this_turn',metadata:{source_session_id:'session',source_turn_id:'prior-turn',history_version:'execution_v2',node_id:'review',generation:1,execution_epoch_id:'old-epoch',activation_id:'old-activation',type:'output',reference_id:'output-1'}}],attachment_ids:['selected-image']};
 await page.locator('ax-session-guidance').evaluate((element,selected)=>element.send('Inspect this evidence.',selected),selected);assert.deepEqual(await page.evaluate(()=>window.calls[0].context),selected);
 }finally{await context.close();}});

test('bounded planner presents exact proposal and applies through the existing human command handler',async()=>{const{context,page,errors}=await fixture();const plans=[];try{
 await page.route('**/api/sessions/session/team',route=>route.fulfill({json:{templates:[{template_id:'local-planner',name:'Local planner',provider:'ollama',model:'local',max_output_tokens:null}]}}));
 const proposal={schema_version:1,command_id:'planned-exact',session_id:'session',turn_id:'turn',execution_epoch_id:'epoch',expected_turn_revision:3,expected_graph_revision:1,activation:envelope().nodes[0].activations[0].reference.activation,action:'guide',instruction:'Inspect the failing check.'};
 await page.route('**/control-plan',route=>{plans.push(route.request().postDataJSON());return route.fulfill({json:{request_id:'plan-one',provider:'ollama',model:'local',calls:1,usage:{usage:{input_tokens:43,output_tokens:17},complete:true},cost_known:false,cost_microunits:null,latency_ms:24,failure:null,explanation:'Guide the selected QA reviewer',impact:['Exact generation 1 of review','Next safe boundary'],proposal}});});
 await page.locator('ax-session-guidance').evaluate(element=>element.openPlanner('Ask the reviewer to inspect the failure.',{references:[],attachment_ids:[]}));
 await page.getByLabel('Planning Agent',{exact:true}).selectOption('local-planner');assert.equal(await page.getByLabel('Maximum planning output tokens').inputValue(),'');await page.getByLabel('Maximum planning output tokens').fill('256');
 await page.getByRole('button',{name:'Create proposal',exact:true}).click();await page.getByText('Guide the selected QA reviewer',{exact:true}).waitFor();assert.equal(plans.length,1);assert.equal(plans[0].max_output_tokens,256);assert.equal(plans[0].selected_activation.activation_id,'review-1');assert.equal(await page.evaluate(()=>window.calls.length),0,'Planning cannot execute');
 await page.getByRole('button',{name:'Apply proposed change',exact:true}).click();await page.getByText('Proposed change accepted.',{exact:false}).waitFor();assert.deepEqual(await page.evaluate(()=>window.calls[0].request),proposal);assert.deepEqual(errors,[]);
 }finally{await context.close();}});

test('planner stale target disables Apply and unavailable planning keeps manual controls',async()=>{const{context,page}=await fixture();try{
 await page.route('**/api/sessions/session/team',route=>route.fulfill({json:{templates:[{template_id:'local-planner',name:'Local planner',provider:'ollama',model:'local',max_output_tokens:256}]}}));
 await page.route('**/control-plan',route=>route.fulfill({json:{provider:'ollama',model:'local',calls:1,usage:{usage:{input_tokens:0,output_tokens:0},complete:false},cost_known:false,latency_ms:5,explanation:'Exact old target',impact:[],proposal:{session_id:'session',turn_id:'turn',command_id:'old',expected_turn_revision:3,expected_graph_revision:1,action:'guide',instruction:'Old target'}}}));
 await page.locator('ax-session-guidance').evaluate(element=>element.openPlanner('Inspect the failure.',{references:[],attachment_ids:[]}));await page.getByLabel('Planning Agent',{exact:true}).selectOption('local-planner');await page.getByRole('button',{name:'Create proposal',exact:true}).click();await page.getByText('Exact old target',{exact:true}).waitFor();
 await page.locator('ax-session-guidance').evaluate((element,next)=>element.acceptEnvelope(next),envelope(2));assert.equal(await page.getByRole('button',{name:'Apply proposed change',exact:true}).isDisabled(),true);await page.getByText('Its target and revisions stay unchanged.',{exact:false}).waitFor();assert.equal(await page.evaluate(()=>window.calls.length),0);
 await page.route('**/control-plan',route=>route.fulfill({json:{provider:'ollama',model:'local',calls:1,usage:{usage:{input_tokens:0,output_tokens:0},complete:false},cost_known:false,latency_ms:5,failure:'Provider unavailable',impact:[],proposal:null}}));await page.getByRole('button',{name:'Create proposal',exact:true}).click();await page.getByText('Provider unavailable',{exact:false}).waitFor();assert.equal(await page.getByRole('button',{name:'Use Turn controls',exact:true}).isEnabled(),true);
 }finally{await context.close();}});
