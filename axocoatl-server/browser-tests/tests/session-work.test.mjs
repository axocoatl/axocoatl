import assert from 'node:assert/strict';
import {after,before,test} from 'node:test';
import {chromium} from 'playwright';
import {launchTestDaemon,resolveChromiumExecutable} from '../support/daemon.mjs';
let runtime,browser;
before(async()=>{runtime=process.env.AXOCOATL_COMPONENT_BASE_URL?{baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}}:await launchTestDaemon();const executablePath=await resolveChromiumExecutable();browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});});
after(async()=>{await browser?.close();await runtime?.stop();});
async function fixture({loseApply=false}={}){const context=await browser.newContext({viewport:{width:390,height:844},colorScheme:'dark',reducedMotion:'reduce'}),page=await context.newPage(),calls=[],errors=[],view={bindings:[],receipts:[]};page.on('pageerror',error=>errors.push(error.message));
 await page.route('**/work-fixture',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><html data-theme="dark"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"><ax-session-work session="session"></ax-session-work><script type="module" src="/ui/session-work.js"></script></html>'}));
 await page.route('**/api/sessions/session/team',route=>route.fulfill({json:{configuration_revision:3,approved:true,slots:[{name:'QA reviewer',limits:{activations:8,invocations:32,tokens:80000,cost_microunits:0},expires_at_ms:4096902600000}]}}));
 await page.route('**/api/sessions/session/work**',route=>{const suffix=new URL(route.request().url()).pathname.split('/work')[1],body=route.request().method()==='POST'?route.request().postDataJSON():null;calls.push({suffix,body});if(!suffix)return route.fulfill({json:view});if(suffix==='/bindings'){let source=view.bindings.find(item=>item.binding.binding_id===body.binding_id);if(!source||source.binding.binding_revision===body.expected_binding_revision){source={binding:{binding_id:body.binding_id,binding_revision:body.expected_binding_revision+1,team_revision:body.expected_team_revision,session_id:'session',event_kind:body.event_kind},armed:body.armed,source:body.source,instruction:body.instruction,required_checks:body.required_checks,grants:[],authorized_at_ms:123};view.bindings=[source];}if(loseApply){loseApply=false;return route.abort('failed');}return route.fulfill({json:source});}if(suffix.endsWith('/manual')){const receipt={receipt_id:'receipt',turn_id:'recorded-turn',request:{binding:view.bindings[0].binding,event:{event_id:body.event_id,correlation_id:body.correlation_id,subject:body.subject}},disposition:{state:'queued'}};view.receipts=[{receipt,state:'queued',reason:null}];return route.fulfill({json:receipt});}if(suffix.endsWith('/settle-at-ceiling')){view.receipts[0].state='settled';view.receipts[0].reason=null;delete view.receipts[0].ceiling;return route.fulfill({json:view});}if(suffix.endsWith('/dismiss')){view.receipts[0].state='dismissed';view.receipts[0].reason=body.reason;return route.fulfill({json:view.receipts[0].receipt});}return route.fulfill({json:view});});
 await page.goto(`${runtime.baseUrl}/work-fixture`);await page.getByRole('button',{name:'Work sources',exact:true}).click();await page.getByText('No source is configured',{exact:false}).waitFor();return{page,context,calls,errors,view};}
async function configure(page){await page.getByRole('button',{name:'Add work source',exact:true}).click();await page.getByLabel('Event kind',{exact:true}).fill('client-build');await page.getByLabel('Standing instruction',{exact:true}).fill('Verify the exact client build and report evidence.');await page.getByLabel('Required checks (one JSON argument array per line)',{exact:true}).fill('["npm","test"]');await page.getByLabel('Arm this source for new events',{exact:true}).check();await page.getByRole('button',{name:'Review source changes',exact:true}).click();}
test('standing work source review admits a declared candidate and preserves its exact binding',async()=>{const{page,context,calls,errors}=await fixture();try{
 await configure(page);assert.equal(calls.filter(call=>call.suffix==='/bindings').length,0);await page.getByRole('button',{name:'Apply source',exact:true}).click();await page.getByText('client-build · armed',{exact:true}).waitFor();const binding=calls.find(call=>call.suffix==='/bindings').body;assert.equal(binding.expected_team_revision,3);assert.equal(binding.expected_binding_revision,0);assert.deepEqual(binding.source,{kind:'manual'});assert.deepEqual(binding.required_checks,[['npm','test']]);
 await page.getByRole('button',{name:'Add candidate to client-build',exact:true}).click();await page.getByLabel('Correlation ID',{exact:true}).fill('release-4');await page.getByLabel('Candidate kind',{exact:true}).selectOption('build');await page.getByLabel('Candidate identity',{exact:true}).fill('client-build-104');await page.getByLabel('Immutable candidate version',{exact:true}).fill('sha256:recorded-build');await page.getByRole('button',{name:'Submit declared candidate',exact:true}).click();await page.getByText('client-build-104 · queued',{exact:true}).waitFor();const event=calls.find(call=>call.suffix.endsWith('/manual')).body;assert.equal(event.expected_binding_revision,1);assert.deepEqual(event.subject,{kind:'build',reference_id:'client-build-104',version:'sha256:recorded-build'});assert.equal(event.caused_by_turn_id,null);assert.equal(await page.getByText('The approved team can receive work',{exact:false}).count(),1);
 await page.getByLabel(`Dismissal reason for ${event.event_id}`,{exact:true}).fill('Superseded by a newer exact build');await page.getByRole('button',{name:'Dismiss event',exact:true}).click();await page.getByText('client-build-104 · dismissed',{exact:true}).waitFor();assert.deepEqual(calls.find(call=>call.suffix.endsWith('/dismiss')).body,{reason:'Superseded by a newer exact build'});assert.deepEqual(errors,[]);
 }finally{await context.close();}});
test('standing source lost Apply survives reload and retries the exact original request',async()=>{const{page,context,calls,errors}=await fixture({loseApply:true});try{
 await configure(page);await page.getByRole('button',{name:'Apply source',exact:true}).click();await page.getByText('Resolve the exact pending request',{exact:false}).waitFor();const original=calls.find(call=>call.suffix==='/bindings').body;await page.reload();await page.getByRole('button',{name:'Work sources',exact:true}).click();await page.getByRole('button',{name:'Resolve pending request',exact:true}).waitFor();assert.equal(await page.getByRole('button',{name:'Add work source',exact:true}).isDisabled(),true);await page.getByRole('button',{name:'Resolve pending request',exact:true}).click();await page.getByText('Events stay attached to their original team',{exact:false}).waitFor();assert.deepEqual(calls.filter(call=>call.suffix==='/bindings').map(call=>call.body),[original,original]);assert.deepEqual(errors,[]);
 }finally{await context.close();}});

test('Session completion source preserves exact source identity and explicit checks',async()=>{
 const {page,context,calls,errors}=await fixture();
 try{
  await page.getByRole('button',{name:'Add work source',exact:true}).click();
  await page.getByLabel('Event kind',{exact:true}).fill('verified-client-build');
  await page.getByLabel('Source',{exact:true}).selectOption('session_completion');
  await page.getByLabel('Source Session ID',{exact:true}).fill('source-session-exact');
  await page.getByLabel('Standing instruction',{exact:true}).fill('Review the exact completed repository candidate.');
  await page.getByLabel('Required checks (one JSON argument array per line)',{exact:true}).fill('["npm","test"]\n["git","diff","--check"]');
  await page.getByLabel('Arm this source for new events',{exact:true}).check();
  await page.getByRole('button',{name:'Review source changes',exact:true}).click();
  assert.equal(calls.filter(call=>call.suffix==='/bindings').length,0);
  await page.getByRole('button',{name:'Apply source',exact:true}).click();
  await page.getByText('verified-client-build · armed',{exact:true}).waitFor();
  const saved=calls.find(call=>call.suffix==='/bindings').body;
  assert.deepEqual(saved.source,{kind:'session_completion',session_id:'source-session-exact'});
  assert.deepEqual(saved.required_checks,[['npm','test'],['git','diff','--check']]);
  assert.equal(await page.getByRole('button',{name:'Add candidate to verified-client-build',exact:true}).count(),0);
  await page.getByRole('button',{name:'Review verified-client-build',exact:true}).click();
  assert.equal(await page.getByLabel('Source Session ID',{exact:true}).inputValue(),'source-session-exact');
  assert.equal(await page.getByLabel('Required checks (one JSON argument array per line)',{exact:true}).inputValue(),'["npm","test"]\n["git","diff","--check"]');
  assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('blocked reserved work retains retry and inspection without offering unsafe dismissal',async()=>{
 const {page,context,view,errors}=await fixture();
 try{
  view.receipts=[{receipt:{receipt_id:'reserved-receipt',turn_id:'exact-recorded-turn',request:{binding:{team_revision:3,binding_revision:7},event:{event_id:'reserved-event',correlation_id:'release',subject:{kind:'build',reference_id:'declared-build',version:'producer-label'}}},disposition:{state:'reserved'}},state:'blocked',can_dismiss:false,reason:'Repository owner needs recovery',readiness:{state:'unmet',candidate_sha256:null,evidence:null,checks:[],reason:'No checked candidate is available'}}];
  await page.getByRole('button',{name:'Refresh work',exact:true}).click();
  await page.getByText('declared-build · blocked',{exact:true}).waitFor();
  assert.equal(await page.getByRole('button',{name:'Try queued work',exact:true}).count(),1);
  assert.equal(await page.getByRole('button',{name:'Inspect recorded turn',exact:true}).count(),1);
  assert.equal(await page.getByRole('button',{name:'Dismiss event',exact:true}).count(),0);
  assert.equal(await page.getByText('Readiness: ready',{exact:true}).count(),0);
  view.receipts[0].can_dismiss=true;
  await page.getByRole('button',{name:'Refresh work',exact:true}).click();
  await page.getByRole('button',{name:'Dismiss event',exact:true}).waitFor();
  view.receipts[0].can_dismiss=false;
  view.receipts[0].state='completed';view.receipts[0].readiness={state:'ready',candidate_sha256:'exact-tree-sha256',evidence:'readiness-evidence',checks:[{argv:['npm','test'],state:'passed',evidence:'actual-check-evidence',exit_code:0,stdout:'Actual test success',stderr:'',stdout_truncated:true,stderr_truncated:false,candidate_sha256:'exact-tree-sha256'}],reason:null};
  await page.getByRole('button',{name:'Refresh work',exact:true}).click();
  await page.getByText('Readiness: ready',{exact:true}).waitFor();
  assert.equal(await page.getByText('Recorded tree SHA-256: exact-tree-sha256',{exact:true}).count(),1);
  assert.equal(await page.getByText('["npm","test"] · passed · evidence actual-check-evidence',{exact:true}).count(),1);
  assert.match(await page.locator('.receipts').textContent(),/Declared build: producer-label/);
  await page.getByText('["npm","test"] · passed · evidence actual-check-evidence',{exact:true}).click();
  await page.getByText('Actual test success',{exact:true}).waitFor();
  assert.equal(await page.getByText('Standard output · truncated',{exact:true}).count(),1);
  await page.evaluate(()=>{document.querySelector('ax-session-work').addEventListener('open-session-work-turn',event=>{window.recordedWorkTurn=event.detail;});});
  await page.getByRole('button',{name:'Inspect recorded turn',exact:true}).click();
  assert.deepEqual(await page.evaluate(()=>window.recordedWorkTurn),{session_id:'session',turn_id:'exact-recorded-turn'});
  assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('dismissed internal event opens its exact causing turn rather than an unstarted receipt turn',async()=>{
 const {page,context,view,errors}=await fixture();try{
  view.receipts=[{receipt:{receipt_id:'duplicate',turn_id:'never-started',request:{binding:{team_revision:3,binding_revision:1},event:{event_id:'causal',correlation_id:'release',source_id:'session:producer-session',caused_by_turn_id:'producer-turn',subject:{kind:'commit',reference_id:'candidate',version:'full-commit'}}},disposition:{state:'dismissed'}},state:'dismissed',can_dismiss:false,reason:'Candidate was already verified'}];
  await page.getByRole('button',{name:'Refresh work',exact:true}).click();
  await page.getByText('candidate · dismissed',{exact:true}).waitFor();
  assert.equal(await page.getByRole('button',{name:'Inspect recorded turn',exact:true}).count(),0);
  await page.evaluate(()=>document.addEventListener('open-session-work-source-turn',event=>{window.sourceTurn=event.detail;}));
  await page.getByRole('button',{name:'Inspect source turn',exact:true}).click();
  assert.deepEqual(await page.evaluate(()=>window.sourceTurn),{sessionId:'producer-session',turnId:'producer-turn'});assert.deepEqual(errors,[]);
 }finally{await context.close();}
});


test('Work sources distinguishes retained process outcomes from readiness, missing outcomes and explicit skips',async()=>{
 const {page,context,view,errors}=await fixture();
 try {
  const cases=[
   {state:'stale',process_status:{kind:'exited',code:0},label:'stale',text:'Recorded process: exited with code 0',evidence:'old-pass'},
   {state:'failed',process_status:{kind:'exited',code:7},label:'failed',text:'Recorded process: exited with code 7',evidence:'failed'},
   {state:'timed_out',process_status:{kind:'timed_out'},primary_exit:{kind:'exited',code:0},quiescent:false,label:'timed out',text:'Recorded process: timed out',evidence:'timeout'},
   {state:'interrupted',process_status:{kind:'interrupted'},label:'interrupted',text:'Recorded process: interrupted',evidence:'interrupt'},
   {state:'launch_failed',process_status:{kind:'launch_failed',message:'Owned environment unavailable'},label:'could not start',text:'Recorded process: could not start: Owned environment unavailable',evidence:'launch'},
   {state:'not_dispatched',process_status:{kind:'not_dispatched'},label:'not dispatched',text:'Recorded process: not dispatched',evidence:'no-dispatch'},
   {state:'outcome_unknown',process_status:{kind:'uncertain',message:'Transport lost'},label:'outcome unknown',text:'Recorded process: outcome unknown: Transport lost',evidence:'uncertain'},
   {state:'outcome_unknown',process_status:null,label:'outcome unknown',text:null,evidence:null},
   {state:'skipped',process_status:null,label:'skipped',text:null,evidence:null,reason:'Not run; explicitly left unmet by the recorded partial Finish'},
  ];
  for(const [index,scenario] of cases.entries()) {
   const check={argv:['check',String(index)],run_id:scenario.state==='skipped'?null:`run-${index}`,effect_disposition:scenario.state==='not_dispatched'?'not_dispatched':'outcome_unknown',stdout:'',stderr:'',...scenario};
   view.receipts=[{receipt:{receipt_id:'receipt',turn_id:'exact-turn',request:{binding:{team_revision:3,binding_revision:1},event:{event_id:'check-status',correlation_id:'release',subject:{kind:'build',reference_id:'candidate',version:'exact'}}},disposition:{state:'reserved'}},state:'needs_attention',can_dismiss:false,readiness:{state:'unmet',candidate_sha256:null,evidence:null,checks:[check],reason:'This candidate has unmet checks'}}];
   await page.getByRole('button',{name:'Refresh work',exact:true}).click();
   const summary=page.getByText(`${JSON.stringify(check.argv)} · ${scenario.label}${check.evidence?` · evidence ${check.evidence}`:''}`,{exact:true});
   await summary.click();
   if(scenario.text)await page.getByText(scenario.text,{exact:true}).waitFor();
   else assert.equal(await page.getByText('Recorded process:',{exact:false}).count(),0);
   if(scenario.reason)await page.getByText(scenario.reason,{exact:true}).waitFor();
   if(scenario.primary_exit) {await page.getByText('Primary process: exited with code 0',{exact:true}).waitFor();await page.getByText('Process-tree settlement is not established',{exact:false}).waitFor();}
   if(!scenario.evidence)assert.equal(await page.getByText('Recorded empty output',{exact:true}).count(),0,'absence is not an observed empty output');
   assert.equal(await page.getByText('Readiness: ready',{exact:true}).count(),0);
  }
  assert.deepEqual(errors,[]);
 }finally{await context.close();}
});


test('background Work refresh preserves only the same exact expanded check and its focus',async()=>{
 const {page,context,view,errors}=await fixture();
 try {
  view.receipts=[{receipt:{receipt_id:'first',turn_id:'turn',request:{binding:{team_revision:3,binding_revision:1},event:{event_id:'event',correlation_id:'release',subject:{kind:'build',reference_id:'candidate',version:'exact'}}},disposition:{state:'reserved'}},state:'needs_attention',readiness:{state:'unmet',checks:[{argv:['check'],run_id:'run-one',state:'failed',process_status:{kind:'exited',code:7},evidence:'result-one',stdout:'exact first output',stderr:''}]}}];
  await page.getByRole('button',{name:'Refresh work',exact:true}).click();
  const detail=page.locator('.receipts details'),summary=detail.locator('summary');await summary.click();await summary.focus();
  // Invoke the same load used by the background timer without taking focus away.
  await page.locator('ax-session-work').evaluate(element=>element.load());
  assert.equal(await detail.getAttribute('open'),'');
  assert.equal(await summary.evaluate(node=>node.getRootNode().activeElement===node),true);
  await page.getByText('exact first output',{exact:true}).waitFor();
  view.receipts[0].readiness.checks[0]={...view.receipts[0].readiness.checks[0],run_id:'run-two',evidence:null,state:'outcome_unknown',process_status:null,stdout:''};
  await page.locator('ax-session-work').evaluate(element=>element.load());
  assert.equal(await detail.getAttribute('open'),null,'a new run does not inherit an old expanded result');
  await summary.click();
  view.receipts[0].receipt.receipt_id='different-receipt';
  await page.locator('ax-session-work').evaluate(element=>element.load());
  assert.equal(await detail.getAttribute('open'),null,'a different work receipt does not inherit expansion');
  assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('closed work held only by unknown provider usage offers one explicit ceiling settlement',async()=>{
 const {page,context,view,calls,errors}=await fixture();
 try{
  view.receipts=[{receipt:{receipt_id:'held-receipt',turn_id:'closed-turn',request:{binding:{team_revision:3,binding_revision:1},event:{event_id:'held-event',correlation_id:'signal',subject:{kind:'build',reference_id:'held-build',version:'exact'}}},disposition:{state:'reserved'}},state:'finished',can_dismiss:false,reason:'Provider usage is unknown for 1 call(s); the shared budget stays reserved until a person settles it at its reserved ceiling, which charges this work 110938 tokens in total',ceiling:{tokens:110938,cost_microunits:0,unknown_calls:1},readiness:{state:'unmet',candidate_sha256:null,evidence:null,checks:[],reason:'No required checks were configured'}}];
  await page.getByRole('button',{name:'Refresh work',exact:true}).click();
  const settle=page.getByRole('button',{name:'Settle at reserved ceiling (110938 tokens in total)',exact:true});
  await settle.waitFor();
  await page.getByText('This can only over-count.',{exact:false}).waitFor();
  assert.equal(calls.filter(call=>call.suffix?.endsWith('/settle-at-ceiling')).length,0,'nothing settles without the click');
  await settle.click();
  await page.getByText('held-build · settled',{exact:true}).waitFor();
  assert.equal(calls.filter(call=>call.suffix==='/receipts/held-receipt/settle-at-ceiling').length,1);
  assert.equal(await page.getByRole('button',{name:/Settle at reserved ceiling/}).count(),0);
  assert.deepEqual(errors,[]);
 }finally{await context.close();}
});
