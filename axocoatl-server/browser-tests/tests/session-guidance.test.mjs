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

function attentionEnvelope({accepted = true} = {}) {
  const ref = (node_id, generation = 1) => ({session_id:'session',turn_id:'turn',execution_epoch_id:'epoch',node_id,activation_id:`${node_id}-${generation}`,generation});
  const lead = ref('lead'), scout = ref('scout');
  const review = {selected_activations:[], stop_activations:[], missing_conditions:['check-evidence'], unrun_nodes:accepted ? ['reviewer'] : [], confirmed:false};
  return {schema_version:1,history_version:'execution_v2',session_id:'session',turn_id:'turn',state:'needs_attention',
    turn_revision:{status:'available',value:7},graph_revision:{status:'available',value:2},
    nodes:[
      {node_id:'lead',label:'Lead',activations:[{state:accepted ? 'accepted' : 'failed',reference:{kind:'exact',activation:lead},
        capabilities:{guide:{enabled:false,reason:'paused'},revise:accepted ? {enabled:true,reason:''} : {enabled:false,reason:'Revise requires a recorded accepted result'}}}]},
      {node_id:'scout',label:'Scout',activations:[{state:'failed',reference:{kind:'exact',activation:scout},
        capabilities:{guide:{enabled:false,reason:'paused'},revise:{enabled:false,reason:'Revise requires a recorded accepted result'}}}]},
      ...(accepted ? [{node_id:'reviewer',label:'Reviewer',activations:[]}] : []),
    ],
    turn_controls:{execution_epoch_id:'epoch',continue_turn:{enabled:true,reason:''},finish:{enabled:false,reason:'A required check failed.'},
      continuation_choices:[],check_choices:[],
      partial_finish:{capability:{enabled:true,reason:''},available_sinks:accepted ? [lead] : [],review}}};
}

test('a message while the turn needs attention offers continuing it or finishing it and sending a new request',async()=>{
  const {context,page,errors}=await fixture(attentionEnvelope());
  try {
    const guide=page.locator('ax-session-guidance');
    await guide.evaluate(element=>element.addEventListener('attention-choice',event=>{window.chosen=event.detail;}));
    assert.equal(await guide.locator('.attention').isVisible(),false,'the choice appears only when the person sends');
    assert.equal(await guide.evaluate(element=>element.offerChoice()),true);
    await guide.getByRole('heading',{name:'This turn needs attention. Where should your message go?'}).waitFor();
    assert.equal(await guide.getByLabel('Agent receiving your message').inputValue(),'lead-1','the one Agent with an accepted answer is the target');
    assert.match(await guide.locator('.continue-detail').textContent(),/^Lead revises its accepted answer, with your message as the instruction/);
    const finish=await guide.locator('.finish-detail').textContent();
    for(const expected of [/The accepted answer of Lead carries into its conversation, as after a completed turn\./,
      /What Scout did in this turn does not carry forward: only accepted answers do\./,
      /Work that never started is skipped: Reviewer\./,/Required checks that have not passed stay unmet\./])assert.match(finish,expected);
    if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/attention-choice.png`,fullPage:true});
    await guide.getByRole('button',{name:'Continue this turn with your message'}).click();
    assert.deepEqual(await page.evaluate(()=>window.chosen),{choice:'continue',sessionId:'session',turnId:'turn'});
    await guide.getByRole('button',{name:'Finish this turn as it is and send as a new request'}).click();
    assert.deepEqual(await page.evaluate(()=>window.chosen),{choice:'finish',sessionId:'session',turnId:'turn'});
    assert.equal(await page.evaluate(()=>window.calls.length),0,'choosing is only an event; the shell supplies the message');

    const context={references:[],attachment_ids:['upload-1']};
    assert.equal(await guide.evaluate((element,context)=>element.continueWithMessage('Use the failing test output.',context),context),true);
    const revise=await page.evaluate(()=>window.calls[0]);
    assert.equal(revise.kind,'revise');assert.equal(revise.reference.activation.activation_id,'lead-1');
    assert.equal(revise.instruction,'Use the failing test output.');assert.equal(revise.includePreviousOutput,true);assert.deepEqual(revise.context,context);
    assert.equal(revise.model.controlPlane.turn_revision.value,7);
    assert.equal(await guide.locator('.attention').isVisible(),false);

    await guide.evaluate(element=>{element.commandHandler=async input=>{window.calls.push(input);return {request:{command_id:'finish',session_id:'session',turn_id:'turn',action:'finish'},receipt:{state:'applied'}};};});
    await guide.evaluate(element=>element.offerChoice());
    assert.equal(await guide.evaluate(element=>element.finishAsItIs()),true);
    const finished=await page.evaluate(()=>window.calls[1]);
    assert.equal(finished.kind,'finish');
    assert.deepEqual(finished.partialFinish,{selected_activations:[{session_id:'session',turn_id:'turn',execution_epoch_id:'epoch',node_id:'lead',activation_id:'lead-1',generation:1}],
      stop_activations:[],missing_conditions:['check-evidence'],unrun_nodes:['reviewer'],confirmed:true},'the exact offered review, with every accepted final answer carried');
    await guide.getByText('This turn is finished. Sending your message as a new request…').waitFor();

    await guide.evaluate(element=>{element.commandHandler=async()=>({request:{command_id:'stale'},error:'Turn revision changed',errorStatus:409});});
    await guide.evaluate(element=>element.offerChoice());
    assert.equal(await guide.evaluate(element=>element.finishAsItIs()),false);
    await guide.getByText('Turn revision changed',{exact:true}).waitFor();
    assert.equal(await guide.locator('.attention').isVisible(),true,'a refusal keeps the choice open');
    assert.deepEqual(errors,[]);
  } finally { await context.close(); }
});

test('with no accepted answer, continuing with the message is unavailable and the finish text says what is lost',async()=>{
  const {context,page,errors}=await fixture(attentionEnvelope({accepted:false}));
  try {
    const guide=page.locator('ax-session-guidance');
    await guide.evaluate(element=>element.offerChoice());
    assert.equal(await guide.getByRole('button',{name:'Continue this turn with your message'}).isDisabled(),true);
    assert.match(await guide.locator('.continue-detail').textContent(),/No Agent in this turn has an accepted answer to revise with your message/);
    assert.equal(await guide.evaluate(element=>element.continueWithMessage('Try again.')),false);
    const finish=await guide.locator('.finish-detail').textContent();
    assert.match(finish,/No Agent has an accepted answer in this turn, so the new request starts from the conversation before it\./);
    assert.match(finish,/What Lead and Scout did in this turn does not carry forward/);
    assert.equal(await guide.getByRole('button',{name:'Finish this turn as it is and send as a new request'}).isDisabled(),false);
    await guide.getByRole('button',{name:'Keep editing'}).click();
    assert.equal(await guide.locator('.attention').isVisible(),false);
    assert.equal(await page.evaluate(()=>window.calls.length),0);
    assert.deepEqual(errors,[]);
  } finally { await context.close(); }
});

test('the shell sends the finished turn\'s message as a new request only once History shows the turn closed',async()=>{
  const {context,page,errors}=await fixture(attentionEnvelope());
  try {
    const appSource=await fetch(`${runtime.baseUrl}/`).then(response=>response.text());
    const result=await page.evaluate(async source=>{
      const start=source.indexOf('async function answerAttentionChoice(');
      const end=source.indexOf('\n// ── Variants in the thread',start);
      if(start<0||end<start)throw new Error('actual attention consumer unavailable');
      const guidance=document.querySelector('ax-session-guidance');guidance.id='session-guidance';
      const text=document.createElement('textarea');text.id='session-text';document.body.append(text);
      const sent=[],toasts=[];let historyStatus='finished';
      const S={session:{id:'session',refs:[{kind:'code'}],historyState:'ready',controlPlaneTurns:new Map()}};
      const $=selector=>document.querySelector(selector);
      const reloadSessionTranscript=async()=>{S.session.historyState='ready';S.session.controlPlaneTurns=new Map([['turn',{id:'turn',status:historyStatus}]]);return [];};
      const answer=new Function('S','$','canonicalInlineReferences','ensureRefs','renderChatRefs','reloadSessionTranscript','toast','sendSessionMessage',
        `${source.slice(start,end)}; return answerAttentionChoice;`)(S,$,refs=>refs.map(ref=>({...ref,inline:true})),()=>S.session.refs,()=>{},reloadSessionTranscript,
        (...args)=>toasts.push(args),()=>sent.push(text.value));
      guidance.commandHandler=async input=>({request:{command_id:input.kind},receipt:{state:input.kind==='finish'?'applied':'accepted'}});
      text.value='Continue with the failing assertion.';
      const continued=await answer({choice:'continue',sessionId:'session',turnId:'turn'});
      const afterContinue={value:text.value,refs:S.session.refs.length};
      text.value='Start over with a smaller change.';
      const finished=await answer({choice:'finish',sessionId:'session',turnId:'turn'});
      historyStatus='needs_attention';text.value='Not yet.';
      const early=await answer({choice:'finish',sessionId:'session',turnId:'turn'});
      const foreign=await answer({choice:'finish',sessionId:'other',turnId:'turn'});
      return {continued,afterContinue,finished,early,foreign,sent,toasts:toasts.map(item=>item[0]),left:text.value};
    },appSource);
    assert.equal(result.continued,true);assert.deepEqual(result.afterContinue,{value:'',refs:0},'a delivered continuation clears the composer');
    assert.equal(result.finished,true);assert.deepEqual(result.sent,['Start over with a smaller change.'],'the new request is the composer text, sent once');
    assert.equal(result.early,false);assert.deepEqual(result.toasts,['Your message was not sent']);assert.equal(result.left,'Not yet.','an unclosed turn never loses the message');
    assert.equal(result.foreign,false);
    assert.deepEqual(errors,[]);
  } finally { await context.close(); }
});
