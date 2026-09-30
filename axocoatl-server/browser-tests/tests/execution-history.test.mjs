import assert from 'node:assert/strict';
import {after, before, test} from 'node:test';
import {chromium} from 'playwright';
import {historyPresentation, executionHistorySummary, createExecutionHistoryInvalidator} from '../../static/ui/execution-history.js';
import {launchTestDaemon, resolveChromiumExecutable} from '../support/daemon.mjs';

function fixture() {
  const exact = generation => ({session_id:'history-session',turn_id:'native-turn',execution_epoch_id:`epoch-${generation}`,node_id:'reviewer',generation,activation_id:`activation-${generation}`});
  const output = (generation,text,kind='final') => ({activation:exact(generation),text,kind,recorded_at_unix_ms:1000,usage:{kind:'unknown',known_subtotal:{input_tokens:3}}});
  const row = (generation,state,text) => ({activation:{activation:exact(generation),state},currently_accepted:state==='accepted',
    output:text===null ? {status:'not_recorded'} : {status:'available',reference:`output-${generation}`,content:output(generation,text)},
    partial_outputs:[],reserved_outputs:[],stream:[]});
  const first=row(1,'superseded','PRIOR_OUTPUT'); const second=row(2,'accepted',''); const third=row(3,'failed',null);
  third.reserved_outputs=[{reference:'failed-partial',content:{output:output(3,'RETAINED_PREFIX','partial'),original_byte_len:500,original_sha256:'a'.repeat(64),slot:{kind:'settlement'}}}];
  third.stream=[{reference:'stream-0',content:{schema_version:1,activation:exact(3),sequence:0,payload:{kind:'text',delta:'OBSERVED_TEXT'}}},
    {reference:'stream-1',content:{schema_version:1,activation:exact(3),sequence:1,payload:{kind:'tool_proposed',name:'check',call_id:'call-1',arguments_bytes:2,arguments_sha256:'b'.repeat(64)}}}];
  return {owner:{session_id:'history-session',workspace_id:'workspace'},turn_id:'native-turn',revision:9,state:'needs_attention',epochs:[],
    request:{status:'available',reference:'request',content:{turn_id:'native-turn',display_input:'Review the retained change',effective_input:'same',recorded_at_unix_ms:1000,context:[{reference_id:'selection',kind:'code_selection',display_name:'Selection'}]}},
    activations:[first,second,third]};
}

test('versioned normalization preserves opaque legacy IDs and rejects malformed version and exact identities',()=>{
  const legacy={id:'  raw/legacy id  ',session_id:'history-session',user_input:'old',metadata:{checkpoint:'legacy'}};
  assert.equal(historyPresentation(legacy),legacy);
  assert.equal(historyPresentation({history_version:'legacy_v1',turn:legacy}),legacy);
  const value=fixture(); const native=historyPresentation({history_version:'execution_v2',turn:value});
  assert.equal(native.execution,value); assert.equal(native.metadata,undefined); assert.equal(native.agent_id,undefined);
  for(const version of [null,'',2,'future']) assert.throws(()=>historyPresentation({history_version:version,turn:legacy}),/unsupported/);
  const foreign=fixture();foreign.activations[0].output.content.activation.session_id='foreign';
  assert.throws(()=>historyPresentation({history_version:'execution_v2',turn:foreign}),/another activation/);
  const badSequence=fixture();badSequence.activations[2].stream[0].content.sequence=7;
  assert.throws(()=>executionHistorySummary(badSequence),/sequence/);
  const badAccepted=fixture();badAccepted.activations[0].currently_accepted=true;
  assert.throws(()=>executionHistorySummary(badAccepted),/acceptance/);
  const malformedTool=fixture();malformedTool.activations[2].stream[1].content.payload={kind:'tool_result',name:'check',call_id:'call-1',result_bytes:2,result_sha256:'a'.repeat(64)};
  assert.throws(()=>executionHistorySummary(malformedTool),/tool evidence/,'missing error disposition cannot appear as a successful result');
});

test('history summary separates exact accepted, partial, truncated, observed and missing evidence',()=>{
  const view=fixture(); const summary=executionHistorySummary(view);
  assert.match(summary.output,/generation 1 · superseded · output evidence\nPRIOR_OUTPUT/);
  assert.match(summary.output,/generation 2 · accepted\nRecorded empty output/);
  assert.match(summary.output,/generation 3 · failed · partial output · truncated\nRETAINED_PREFIX/);
  assert.match(summary.output,/original 500 bytes/); assert.match(summary.output,/observed output\nOBSERVED_TEXT/);
  view.activations[2].output={status:'missing',reference:'missing-final'};
  assert.match(executionHistorySummary(view).output,/Output evidence is unavailable/);
  assert.match(executionHistorySummary(view).output,/OBSERVED_TEXT/);
  view.request={status:'missing',reference:'missing-request'};
  assert.equal(executionHistorySummary(view).userInput,'Request text is unavailable.');
});

test('a provider retry validates and drops only the abandoned round of observed text',()=>{
  const view=fixture(); const exact=view.activations[2].activation.activation;
  const event=(sequence,payload)=>({reference:`retry-${sequence}`,content:{schema_version:1,activation:exact,sequence,payload}});
  view.activations[2].stream=[event(0,{kind:'text',delta:'KEPT_ROUND '}),
    event(1,{kind:'tool_proposed',name:'check',call_id:'call-1',arguments_bytes:2,arguments_sha256:'b'.repeat(64)}),
    event(2,{kind:'tool_result',name:'check',call_id:'call-1',result_bytes:2,result_sha256:'c'.repeat(64),is_error:false}),
    event(3,{kind:'text',delta:'ABANDONED'}),event(4,{kind:'provider_retry',reason:'stream ended before its final chunk'}),
    event(5,{kind:'text',delta:'RETRIED'})];
  const summary=executionHistorySummary(view);
  assert.match(summary.output,/observed output\nKEPT_ROUND RETRIED/);
  assert.doesNotMatch(summary.output,/ABANDONED/);
  view.activations[2].stream[4].content.payload={kind:'provider_retry'};
  assert.throws(()=>executionHistorySummary(view),/provider retry is invalid/);
});

test('retained native context reopens its exact prior generation and Ways decision after reload',async()=>{
 const context=await browser.newContext(),page=await context.newPage();try{
  const reference={kind:'coordination_reference',reference_id:'context:native-turn:0',display_name:'Reviewer output · generation 2',metadata:{history_version:'execution_v2',source_session_id:'history-session',source_turn_id:'prior-turn',execution_epoch_id:'prior-epoch',node_id:'prior-reviewer',activation_id:'prior-activation',generation:2,reference_id:'prior-output',type:'output'}};
  const decision={kind:'ways_decision',reference_id:'context:native-turn:1',display_name:'Retained Ways decision',metadata:{source_session_id:'history-session',decision_id:'decision'}};
  const view=fixture();view.request.content.context=[reference,decision,...[
   {source_session_id:'foreign'},{generation:0},{execution_epoch_id:''},{reference_id:''}
  ].map((patch,index)=>({...reference,display_name:`Unavailable reference ${index}`,metadata:{...reference.metadata,...patch}})),{...reference,reference_id:'context:another-turn:0',display_name:'Unavailable reference foreign receiving turn'},{...reference,reference_id:'context:native-turn:not-an-index',display_name:'Unavailable reference malformed capture'}];
  await page.route('**/retained-reference-fixture',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><main></main>'}));
  for(let pass=0;pass<2;pass++){
   if(pass)await page.reload();else await page.goto(`${runtime.baseUrl}/retained-reference-fixture`);
   await page.evaluate(async view=>{const {renderExecutionHistoryTurn}=await import('/ui/execution-history.js');window.opened=[];document.querySelector('main').append(renderExecutionHistoryTurn(view,{renderMarkdown:text=>text,onGraph:()=>{},onReference:reference=>window.opened.push(reference)}));},view);
   await page.getByRole('button',{name:'Inspect Reviewer output · generation 2',exact:true}).click();await page.getByRole('button',{name:'Inspect Retained Ways decision',exact:true}).click();
   assert.deepEqual(await page.evaluate(()=>window.opened),[{...reference,reference_id:'prior-output'},{...decision,reference_id:'decision'}]);assert.equal(await page.getByRole('button',{name:/Unavailable reference/}).count(),0);assert.equal(await page.locator('span.chat-ref').count(),6);
  }
 }finally{await context.close();}
});

test('stream invalidations coalesce slow reads and never lose a newly selected Session frame',async()=>{
  let session={id:'a'}; const scheduled=[]; const reads=[]; const resolvers=[];
  const observe=createExecutionHistoryInvalidator({getSession:()=>session,schedule:fn=>scheduled.push(fn),
    reload:async(id,options)=>{reads.push({id,options});await new Promise(resolve=>resolvers.push(resolve));}});
  const frame=(id,seq=0)=>({event:{reference:`event-${seq}`,content:{schema_version:1,sequence:seq,
    activation:{session_id:id,turn_id:'turn',execution_epoch_id:'epoch',node_id:'node',activation_id:'activation',generation:1}}}});
  observe(frame('foreign'));assert.equal(scheduled.length,0);
  observe(frame('a'));observe(frame('a',1));assert.equal(scheduled.length,1);
  session={id:'b'}; observe(frame('b'));assert.equal(scheduled.length,2);
  await scheduled.shift()();assert.equal(reads.length,0,'old Session timer cannot read the new Session');
  const first=scheduled.shift()(); assert.equal(reads.length,1);
  for(let index=1;index<20;index++)observe(frame('b',index));
  assert.equal(scheduled.length,0,'frames cannot abort or pile up concurrent reads');
  resolvers.shift()();await first;assert.equal(scheduled.length,1,'one catch-up read covers in-flight frames');
  const second=scheduled.shift()();assert.equal(reads.length,2);resolvers.shift()();await second;
  assert.equal(scheduled.length,0);assert.deepEqual(reads.map(item=>item.id),['b','b']);
});

let runtime,browser;
before(async()=>{
  runtime=process.env.AXOCOATL_COMPONENT_BASE_URL ? {baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}} : await launchTestDaemon();
  const executablePath=await resolveChromiumExecutable();browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});
});
after(async()=>{await browser?.close();await runtime?.stop();});

test('typed composer guidance presents human text while preserving exact retained metadata',()=>{
  const view=guidanceFixture();const item=view.activations[0].guidance[0];
  const retained=JSON.stringify({kind:'authenticated_control_context_v1',instruction:'Review this image',original:{references:[],attachment_ids:['image']},references:[],attachments:['retained-image']});
  item.instruction.content=retained;const summary=executionHistorySummary(view).guidance[0];
  assert.equal(summary.text,'Review this image');assert.equal(summary.retained,retained);assert.match(summary.contextNote,/1 retained attachments/);assert.equal(item.instruction.content,retained);
  for(const body of ['{"instruction":"ordinary JSON"}',retained.replace('authenticated_control_context_v1','future')]) {item.instruction.content=body;assert.equal(executionHistorySummary(view).guidance[0].text,body);}
});

for(const options of [{theme:'light',width:1100},{theme:'dark',width:390}]) {
  test(`unfinished native history preserves evidence and disables rewind (${options.theme})`,async()=>{
    const context=await browser.newContext({viewport:{width:options.width,height:840},reducedMotion:'reduce'});const page=await context.newPage();const errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    try {
      await page.route('**/execution-history-fixture',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="${options.theme}"><head><link rel="stylesheet" href="/ui/tokens.css"></head><body><main id="transcript"></main></body></html>`}));
      const native=fixture(); const rawId='  raw/legacy id  ';
      await page.route('**/api/sessions/history-session/turns?history_version=2',route=>route.fulfill({contentType:'application/json',body:JSON.stringify([
        {history_version:'legacy_v1',turn:{id:rawId,session_id:'history-session',user_input:'Opaque legacy ID',status:'completed',context:[],created_at:500}},
        {history_version:'execution_v2',turn:native}])}));
      await page.goto(`${runtime.baseUrl}/execution-history-fixture`);
      await page.evaluate(async view=>{
        const {renderExecutionHistoryTurn}=await import('/ui/execution-history.js');await import('/ui/session-history.js');
        window.graphOpens=[];
        document.querySelector('#transcript').append(renderExecutionHistoryTurn(view,{renderMarkdown:text=>text,onGraph:exact=>window.graphOpens.push(exact)}));
        const history=document.createElement('ax-session-history');document.body.append(history);window.openHistory=()=>history.show({sessionId:'history-session',sessionName:'History Session'});
      },native);
      assert.equal(await page.locator('[data-activation-id="activation-2"] .smsg-body').textContent(),'Recorded empty output');
      assert.match(await page.locator('#transcript').textContent(),/partial output · truncated/);
      assert.match(await page.locator('#transcript').textContent(),/original 500 bytes/);
      assert.equal(await page.locator('#transcript [data-epoch-id="epoch-3"]').count(),4);
      await page.getByRole('button',{name:'Open Agent graph'}).click();
      assert.deepEqual(await page.evaluate(()=>window.graphOpens),[{sessionId:'history-session',turnId:'native-turn'}]);
      assert.equal(await page.locator('#transcript [data-action="rewind"],#transcript [data-action="retry"]').count(),0);
      await page.evaluate(()=>window.openHistory());
      const history=page.locator('ax-session-history');await history.locator('.turn').first().waitFor();
      assert.equal(await history.locator('.turn').count(),2);
      assert.ok((await history.locator('.turn').evaluateAll(nodes=>nodes.map(node=>node.dataset.turnId))).includes(rawId));
      assert.equal(await history.locator('.rewind:not([disabled])').count(),0);
      assert.match(await history.locator('.results').textContent(),/RETAINED_PREFIX/);
      if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/versioned-history-${options.theme}.png`,fullPage:true});
      assert.deepEqual(errors,[]);
    } finally {await context.close();}
  });
}

test('Stop evidence preserves never-started nodes and rejects malformed or contradictory history',()=>{
  const view=fixture(); view.state='cancelled';
  view.stop_requested={command_id:'human-stop',requested_revision:8,evidence:'retained-request',unrun_nodes:['unstarted-checker']};
  const summary=executionHistorySummary(view);
  assert.match(summary.output,/unstarted-checker · Stopped before starting/);
  assert.match(summary.output,/generation 2 · accepted/);
  assert.equal(view.activations.length,3,'no synthetic activation');
  for(const mutation of [value=>value.stop_requested=null,value=>value.stop_requested.unrun_nodes=['reviewer'],
    value=>value.stop_requested.unrun_nodes=['duplicate','duplicate'],value=>value.stop_requested.requested_revision=99,
    value=>value.stop_requested.evidence='',value=>value.state='completed']) {
    const invalid=structuredClone(view);mutation(invalid);
    assert.throws(()=>executionHistorySummary(invalid),/Stop request/);
  }
});

test('Stopped-before-start transcript never gains a generation or action row',async()=>{
  const context=await browser.newContext({viewport:{width:390,height:840},reducedMotion:'reduce'});const page=await context.newPage();
  try {
    await page.route('**/stop-history-fixture',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><main></main>'}));
    await page.goto(`${runtime.baseUrl}/stop-history-fixture`);
    const view=fixture();view.state='cancelled';view.stop_requested={command_id:'human-stop',requested_revision:8,evidence:'retained-request',unrun_nodes:['unstarted-checker']};
    await page.evaluate(async view=>{const {renderExecutionHistoryTurn}=await import('/ui/execution-history.js');document.querySelector('main').append(renderExecutionHistoryTurn(view,{renderMarkdown:text=>text,onGraph:()=>{}}));},view);
    const evidence=page.locator('.turn-stop-evidence').filter({hasText:'unstarted-checker'});
    assert.equal(await evidence.textContent(),'unstarted-checker · Stopped before starting');
    assert.equal(await evidence.getAttribute('data-generation'),null);
    assert.equal(await evidence.locator('button').count(),0);
    assert.equal(await page.locator('[data-activation-id="activation-2"] .smsg-body').textContent(),'Recorded empty output');
  } finally {await context.close();}
});

function guidanceFixture() {
  const view=fixture();
  const deliveries=[{status:'handoff_recorded'},{status:'delivered',receipt_revision:4},{status:'unknown',reason:'The retained receipt does not confirm input delivery.'}];
  view.activations.forEach((row,index)=>{
    const reference=`guidance-${index}`;
    row.guidance=[{amendment:{activation:structuredClone(row.activation.activation),control_command_id:`guide-command-${index}`,instruction:reference,request:`guide-request-${index}`},
      instruction:{status:'available',reference,content:`  Exact guidance ${index}\nKeep <img src=x onerror=alert(1)> as literal text.  `},delivery:deliveries[index]}];
  });
  return view;
}

test('guidance history binds exact amendments and separates recorded handoff, acknowledged input and unknown delivery',()=>{
  const view=guidanceFixture(); const before=structuredClone(view);
  const summary=executionHistorySummary(view);
  assert.equal(summary.guidance[0].label,'reviewer · generation 1 · superseded');
  assert.equal(summary.guidance[0].disposition,'Handoff recorded');
  assert.equal(summary.guidance[1].label,'reviewer · generation 2 · accepted');
  assert.equal(summary.guidance[1].disposition,'Input received by the Agent');
  assert.equal(summary.guidance[2].label,'reviewer · generation 3 · failed');
  assert.equal(summary.guidance[2].disposition,'Delivery unknown');
  assert.equal(summary.guidance[0].text,view.activations[0].guidance[0].instruction.content);
  assert.doesNotMatch(summary.output,/Exact guidance/,'guidance must not become generated output');
  assert.match(summary.guidance[0].note,/The Agent’s input acknowledgement is not recorded/);
  assert.match(summary.guidance[1].note,/Follow its output for the result/);
  assert.deepEqual(view,before,'presentation cannot change recorded instruction, delivery or acceptance');
  for(const mutate of [value=>value.activations[0].guidance=null,
    value=>value.activations[0].guidance[0].amendment.activation.execution_epoch_id='foreign-epoch',
    value=>value.activations[0].guidance[0].amendment.activation.generation=2,
    value=>value.activations[0].guidance[0].instruction.reference='different-instruction',
    value=>value.activations[0].guidance[0].instruction.content={},
    value=>value.activations[0].guidance[0].delivery={status:'delivered',receipt_revision:0},
    value=>value.activations[0].guidance[0].delivery={status:'future'},
    value=>value.activations[0].guidance[0].delivery={status:'unknown'},
    value=>value.activations[1].guidance[0].amendment.control_command_id='guide-command-0']) {
    const invalid=structuredClone(view);mutate(invalid);
    assert.throws(()=>historyPresentation({history_version:'execution_v2',turn:invalid}),/Guidance/);
  }
  view.activations[0].guidance[0].instruction={status:'missing',reference:'guidance-0'};
  view.activations[1].guidance[0].instruction={status:'not_recorded'};
  view.activations[2].guidance[0].instruction.content='';
  const unavailable=executionHistorySummary(view).guidance.map(item=>item.text).join('\n');
  assert.match(unavailable,/Instruction text is unavailable. Reference: guidance-0/);
  assert.match(unavailable,/Instruction text was not recorded/);
  assert.match(unavailable,/Recorded empty instruction/);
  assert.doesNotMatch(unavailable,/Exact guidance/);
  assert.deepEqual(executionHistorySummary(fixture()).guidance,[]);
});

for(const options of [{theme:'light',width:1100},{theme:'dark',width:390}]) {
  test(`guidance stays visible in exact transcript, history and Context search (${options.theme})`,async()=>{
    const context=await browser.newContext({viewport:{width:options.width,height:840},reducedMotion:'reduce'});const page=await context.newPage();const errors=[];const searches=[];
    page.on('pageerror',error=>errors.push(error.message));
    try {
      const view=guidanceFixture();const entry={history_version:'execution_v2',turn:view};
      await page.route('**/guidance-history-fixture',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="${options.theme}"><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"></head><body><main></main></body></html>`}));
      await page.route('**/api/sessions/history-session/turns?history_version=2',route=>route.fulfill({contentType:'application/json',body:JSON.stringify([entry])}));
      await page.route('**/api/session-turns/search?*',route=>{searches.push(new URL(route.request().url()));return route.fulfill({contentType:'application/json',body:JSON.stringify([{entry,matched_fields:['context']}])});});
      await page.goto(`${runtime.baseUrl}/guidance-history-fixture`);
      await page.evaluate(async view=>{
        const {renderExecutionHistoryTurn}=await import('/ui/execution-history.js');await import('/ui/session-history.js');
        document.querySelector('main').append(renderExecutionHistoryTurn(view,{renderMarkdown:text=>text,onGraph:()=>{}}));
        const history=document.createElement('ax-session-history');document.body.append(history);window.openGuidanceHistory=()=>history.show({sessionId:'history-session',sessionName:'Guidance history'});
      },view);
      for(let index=0;index<3;index++) {
        const row=page.locator(`.execution-guidance[data-command-id="guide-command-${index}"]`);
        assert.equal(await row.getAttribute('data-epoch-id'),`epoch-${index+1}`);
        assert.equal(await row.getAttribute('data-generation'),String(index+1));
        assert.equal(await row.getAttribute('data-evidence-ref'),`guidance-${index}`);
        assert.equal(await row.locator('.smsg-body').textContent(),view.activations[index].guidance[0].instruction.content);
        assert.equal(await row.locator('img,button').count(),0,'instruction stays literal, without generated controls');
      }
      assert.equal(await page.locator('.execution-guidance[data-guidance-delivery="delivered"]').count(),1);
      await page.evaluate(()=>window.openGuidanceHistory());
      const history=page.locator('ax-session-history');await history.locator('.turn').waitFor();
      assert.match(await history.locator('.results').textContent(),/guidance · Handoff recorded/);
      assert.match(await history.locator('.results').textContent(),/guidance · Input received by the Agent/);
      assert.match(await history.locator('.results').textContent(),/guidance · Delivery unknown/);
      await history.getByRole('searchbox',{name:'Search session turns'}).fill('Exact guidance 1');
      await history.getByRole('button',{name:'Search history',exact:true}).click();
      await history.locator('.matches').waitFor();
      assert.equal(searches.at(-1).searchParams.get('q'),'Exact guidance 1');
      assert.equal(searches.at(-1).searchParams.get('history_version'),'2');
      assert.equal(searches.at(-1).searchParams.get('session_id'),'history-session');
      assert.match(await history.locator('.matches').textContent(),/context/i);
      assert.match(await history.locator('.results').textContent(),/Exact guidance 1/);
      assert.equal(await history.locator('.rewind:not([disabled])').count(),0);
      const guidance=history.locator('.guidance');assert.equal(await guidance.getAttribute('open'),'');
      const retained=guidance.locator('[data-command-id="guide-command-1"]');
      assert.equal(await retained.locator('.guidance-instruction').textContent(),view.activations[1].guidance[0].instruction.content);
      assert.equal(await retained.locator('img,button').count(),0);
      await retained.scrollIntoViewIfNeeded();
      if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/guidance-history-${options.theme}.png`,fullPage:true});
      assert.deepEqual(errors,[]);
    } finally {await context.close();}
  });
}


test('exact human-wait invalidation reloads retained history without inventing a stream event',async()=>{
  const {createExecutionHistoryInvalidator}=await import('../../static/ui/execution-history.js');
  const session={id:'history-session'};const tasks=[];const reads=[];
  const invalidate=createExecutionHistoryInvalidator({getSession:()=>session,reload:async(...args)=>reads.push(args),schedule:fn=>tasks.push(fn)});
  const frame={kind:'activation-control-changed',activation:fixture().activations[0].activation.activation,
    blocker_id:'approval-exact',canonical_command_id:'open-exact',turn_revision:3};
  assert.equal(invalidate(frame),'native-turn');
  assert.equal(invalidate({...frame,turn_revision:0}),undefined);
  assert.equal(invalidate({...frame,activation:{...frame.activation,session_id:'foreign'}}),undefined);
  assert.equal(invalidate({...frame,canonical_command_id:''}),undefined);
  assert.equal(tasks.length,1);await tasks.shift()();assert.deepEqual(reads,[['history-session',{preserveLive:true}]]);
  assert.equal(frame.event,undefined);
});


test('closed native history exposes exact rewind boundary while preserving evidence',async()=>{
  const context=await browser.newContext({viewport:{width:1000,height:840}});const page=await context.newPage();
  const native=fixture();native.state='completed';
  let rows=[{history_version:'legacy_v1',turn:{id:'prior-turn',session_id:'history-session',user_input:'Prior request',status:'completed',context:[],created_at:500}},{history_version:'execution_v2',turn:native}];
  const requests=[];
  try {
    await page.route('**/native-rewind-fixture',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><html><head><link rel="stylesheet" href="/ui/tokens.css"></head><body></body></html>'}));
    await page.route('**/api/sessions/history-session/turns?history_version=2',route=>route.fulfill({contentType:'application/json',body:JSON.stringify(rows)}));
    await page.route('**/api/sessions/history-session/rewind',route=>{requests.push(route.request().postDataJSON());rows=rows.slice(0,1);return route.fulfill({contentType:'application/json',body:JSON.stringify({ok:true,turns:rows})});});
    await page.goto(`${runtime.baseUrl}/native-rewind-fixture`);
    await page.evaluate(async()=>{await import('/ui/session-history.js');const history=document.createElement('ax-session-history');document.body.append(history);history.show({sessionId:'history-session',sessionName:'Native History',rewindEnabled:true});});
    const history=page.locator('ax-session-history');await history.locator('.turn').first().waitFor();
    const boundary=history.locator('.turn[data-turn-id="prior-turn"] [data-action="rewind"]');
    assert.equal(await boundary.isEnabled(),true);await boundary.click();
    await history.locator('[data-action="confirm-rewind"]').click();
    await history.locator('.turn[data-turn-id="native-turn"]').waitFor({state:'detached'});
    assert.deepEqual(requests,[{keep_through_turn_id:'prior-turn'}]);
    assert.equal(await history.locator('.turn').count(),1);
  } finally {await context.close();}
});

test('Ways settlement shares bounded history invalidation with streams and ignores duplicate or foreign results',async()=>{
  let session={id:'history-session'};const tasks=[];const reads=[];let resolve;
  const observe=createExecutionHistoryInvalidator({getSession:()=>session,schedule:fn=>tasks.push(fn),
    reload:async id=>{reads.push(id);await new Promise(done=>resolve=done);}});
  const results={attempt_set:{id:'ways-one',session_id:session.id,lanes:[{index:0},{index:1}]},lane_states:[{index:0,state:'completed'},{index:1,state:'running'}]};
  observe.attemptResults({...results,attempt_set:{...results.attempt_set,session_id:'foreign'}});assert.equal(tasks.length,0);
  observe.attemptResults(results);observe.attemptResults(results);assert.equal(tasks.length,1);
  const first=tasks.shift()();assert.equal(reads.length,1);
  const finished={...results,lane_states:[{index:0,state:'completed'},{index:1,state:'failed'}]};
  observe.attemptResults(finished);observe.attemptResults(finished);assert.equal(tasks.length,0);
  resolve();await first;assert.equal(tasks.length,1,'one final read catches settlement after an in-flight read');
  const last=tasks.shift()();resolve();await last;
  observe.attemptResults(finished);assert.equal(tasks.length,0,'polling an unchanged terminal result cannot create a refresh loop');
  session={id:'other'};observe.attemptResults(finished);assert.equal(tasks.length,0);
  assert.deepEqual(reads,['history-session','history-session']);
});

test('actual Ways result consumer refreshes stale native conversation acceptance without a final token',async()=>{
  const context=await browser.newContext({viewport:{width:1100,height:900}});const page=await context.newPage();
  const source=await fetch(`${runtime.baseUrl}/${process.env.AXOCOATL_COMPONENT_BASE_URL ? 'index.html' : ''}`).then(response=>response.text());
  let reads=0;
  const before=fixture();before.state='running';before.activations=before.activations.slice(2);before.activations[0].activation.state='running';before.activations[0].reserved_outputs=[];
  const after=structuredClone(before);after.state='completed';after.activations[0].activation.state='accepted';after.activations[0].currently_accepted=true;
  after.activations[0].output={status:'available',reference:'accepted-final',content:{activation:after.activations[0].activation.activation,text:'EXACT_ACCEPTED_WAY_RESULT',kind:'final',recorded_at_unix_ms:2000,usage:{kind:'unknown',known_subtotal:{input_tokens:3}}}};
  try{
    await page.route('**/ways-settlement-fixture',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><main></main>'}));
    await page.route('**/api/sessions/history-session/turns?history_version=2',route=>{reads++;return route.fulfill({json:[{history_version:'execution_v2',turn:after}]});});
    await page.goto(`${runtime.baseUrl}/ways-settlement-fixture`);
    await page.evaluate(async({source,before})=>{
      const {createExecutionHistoryInvalidator,renderExecutionHistoryTurn}=await import('/ui/execution-history.js');
      const host=document.querySelector('main');const render=value=>host.replaceChildren(renderExecutionHistoryTurn(value,{renderMarkdown:text=>text,onGraph:()=>{}}));render(before);
      const state={session:{id:'history-session',currentTeam:{history_version:'execution_v2'}},threadVariants:{sessionId:'history-session',attemptSetId:'set-one',variants:[{index:0,dot:{},laneState:'running'}],active:0}};
      const observer=createExecutionHistoryInvalidator({getSession:()=>state.session,reload:async id=>{const entries=await fetch(`/api/sessions/${id}/turns?history_version=2`).then(response=>response.json());render(entries[0].turn);}});
      const observeStart=source.indexOf('function observeAttemptHistoryResults(');const observeEnd=source.indexOf('\nfunction isExecutionHistoryTurn(',observeStart);
      const applyStart=source.indexOf('function applyThreadVariantResults(');const applyEnd=source.indexOf('\nasync function refreshThreadVariantStatus(',applyStart);
      if(observeStart<0||applyStart<0)throw new Error('actual Ways history join missing');
      const noop=()=>{};
      const apply=new Function('S','_executionHistoryInvalidator','sessionHasExecutionHistory','applyThreadAttemptSet','attemptVisualState','syncVariantAccessibility','syncAttemptRailFromThread','setActiveVariant','isAttemptTerminal','threadVariantNeedsPoll','stopThreadVariantPoll','refreshCompare','setAttemptsDock',
        `${source.slice(observeStart,observeEnd)}\n${source.slice(applyStart,applyEnd)}\nreturn applyThreadVariantResults;`)(state,observer,()=>true,()=>true,value=>value,noop,noop,noop,value=>['completed','failed'].includes(value),()=>false,noop,noop,noop);
      window.waysSettlement={apply,results:{attempt_set:{id:'set-one',session_id:'history-session',state:'ready',lanes:[{index:0}]},lane_states:[{index:0,state:'completed'}]}};
    },{source,before});
    assert.match(await page.locator('main').textContent(),/running/);assert.equal(await page.getByText('EXACT_ACCEPTED_WAY_RESULT').count(),0);
    await page.evaluate(()=>window.waysSettlement.apply(window.waysSettlement.results));
    await page.getByText('EXACT_ACCEPTED_WAY_RESULT',{exact:true}).waitFor();
    assert.match(await page.locator('main').textContent(),/accepted/);assert.match(await page.locator('main').textContent(),/Turn: completed/);assert.equal(reads,1);
    await page.evaluate(()=>{for(let i=0;i<20;i++)window.waysSettlement.apply(window.waysSettlement.results);});
    await page.waitForTimeout(250);assert.equal(reads,1);
  }finally{await context.close();}
});

test('partial Finish history retains selected results, skipped work and unmet checks without changing raw acceptance',()=>{
  const view=fixture();view.state='finished';view.stop_requested={command_id:'partial-finish',requested_revision:8,evidence:'human-review',unrun_nodes:['unstarted-checker'],closure:'finished',partial_finish:{selected_activations:[view.activations[1].activation.activation],stop_activations:[],missing_conditions:['review-evidence'],missing_condition_ids:['review-check']}};
  const summary=executionHistorySummary(view);assert.match(summary.output,/Partial Finish confirmed/);assert.match(summary.output,/review-check · Missing or unmet check/);assert.match(summary.output,/unstarted-checker · Skipped by partial Finish/);assert.match(summary.output,/reviewer · generation 2 · Selected accepted result/);assert.equal(view.activations[1].currently_accepted,true);
  for(const change of [value=>value.stop_requested.closure='cancelled',value=>value.stop_requested.partial_finish.selected_activations=[{}],value=>value.stop_requested.partial_finish.missing_condition_ids=['review-check','review-check']]){const invalid=structuredClone(view);change(invalid);assert.throws(()=>executionHistorySummary(invalid),/Partial Finish/);}
});
