import assert from 'node:assert/strict';
import {mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {after,before,test} from 'node:test';
import {chromium} from 'playwright';
import {launchTestDaemon,resolveChromiumExecutable} from '../support/daemon.mjs';
let runtime,browser,appSource,screenshots;
before(async()=>{screenshots=await mkdtemp(join(tmpdir(),'axocoatl-ways-graph-'));runtime=process.env.AXOCOATL_COMPONENT_BASE_URL?{baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}}:await launchTestDaemon();const response=await fetch(`${runtime.baseUrl}/${process.env.AXOCOATL_COMPONENT_BASE_URL?'index.html':''}`);assert.equal(response.ok,true);appSource=await response.text();const executablePath=await resolveChromiumExecutable();browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});});
after(async()=>{try{await browser?.close();await runtime?.stop();}finally{if(screenshots)await rm(screenshots,{recursive:true,force:true});}});
const text=value=>({state:'complete',text:value,sha256:'a'.repeat(64)});
function retained(){return{decision_id:'decision',session_id:'session',set_id:'set-original',task:text('Verify client change'),human_decision:{choice:{kind:'no_keep'},decided_at_unix_ms:1},application:{state:'no_keep_recorded'},cleanup:{completed_at_unix_ms:2},candidates:[{id:{set_id:'set-original',index:0},agent:'captured-agent',model:{state:'available',value:{provider_id:'ollama',model_id:'captured-model'}},isolation:{state:'available',value:'podman'},terminal:'failed',outcome:text('Recorded failure'),route:text('Exact recorded Route'),reviewable_diff:{state:'truncated',text:'diff body',original_bytes:99,offset_bytes:0},changed_paths:{items:['src/a.rs'],original_count:1},checks:[{command:{state:'available',value:'cargo test'},outcome:'failed',exit_code:{state:'available',value:1},duration_ms:{state:'available',value:2},output:text('actual failure')}],usage:{tokens:{kind:'unknown',known_subtotal:{input_tokens:3,output_tokens:1}},cost_usd_known_subtotal:0,cost_complete:false}}]};}
async function fixture(theme='light'){
 const context=await browser.newContext({viewport:{width:theme==='dark'?390:1100,height:900},reducedMotion:'reduce'}),page=await context.newPage(),errors=[],calls=[];
 const state={results:{attempt_set:{id:'set-original',session_id:'session',task:'Verify client change',state:'running',lanes:[{index:0,agent:'captured-agent',provider:'ollama',model:'captured-model',worktree:'/actual/clone'}]},lane_states:[{index:0,state:'running'}],usage:[],outputs:[],verdicts:[]},archive:{limits:{version:1,field_bytes:1024,record_bytes:1048576,aggregate_bytes:8388608,records:100,candidates:100,items_per_field:1000},decisions:[]}};
 page.on('pageerror',error=>errors.push(error.message));
 await page.route('**/fixture-app-source',route=>route.fulfill({contentType:'text/plain',body:appSource}));
 await page.route('**/graph-fixture',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="${theme}"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"><body style="margin:0;background:var(--bg);color:var(--text)"><ax-ways-graph></ax-ways-graph></body></html>`}));
 await page.route('**/api/**',route=>{const url=new URL(route.request().url());calls.push({path:url.pathname,params:Object.fromEntries(url.searchParams),method:route.request().method()});let body=[];if(url.pathname.endsWith('/variants/results'))body=state.results;else if(url.pathname.endsWith('/ways-history'))body=state.archive;else if(url.pathname.endsWith('/variants/trajectories'))body={lanes:[0],rows:[{cells:[{kind:'tool',name:'read_file',arguments:'src/a.rs'}]}]};else if(url.pathname.endsWith('/variants/status'))body=[{index:0,status:{files:[{path:'src/a.rs'}]}}];else if(url.pathname.endsWith('/variants/diff'))body='EXACT_CURRENT_DIFF';else if(url.pathname==='/api/agents')body=[{id:'captured-agent',role:'autonomous',provider:'ollama',model:'captured-model'}];else if(url.pathname==='/api/sessions')body=[{id:'session',mode:{kind:'single_agent',agent_id:'captured-agent'}}];return route.fulfill({json:body});});
 await page.goto(`${runtime.baseUrl}/graph-fixture`);await page.evaluate(async()=>{await import('/ui/ways-graph.js');document.addEventListener('review-ways-decision',event=>window.review=event.detail);});
 return{context,page,state,calls,errors};
}
for(const theme of ['light','dark'])test(`closed Ways graph is expandable retained evidence without another Keep (${theme})`,async()=>{
 const{context,page,calls,errors}=await fixture(theme);try{
  await page.evaluate(record=>document.querySelector('ax-ways-graph').show({sessionId:'session',setId:'set-original',record}),retained());
  assert.equal(await page.locator('ax-node[data-kind=decision]').count(),1);
  assert.equal(await page.locator('ax-node[data-kind=agent]').count(),0);
  await page.getByRole('button',{name:'Expand Attempt 1',exact:true}).click();
  assert.equal(await page.locator('ax-node[data-kind=agent]').count(),1);
  assert.match(await page.locator('.evidence').textContent(),/Exact recorded Route/);
  assert.match(await page.locator('.evidence').textContent(),/Truncated: 9 of 99/);
  assert.match(await page.locator('.evidence').textContent(),/known subtotal; total unknown/);
  assert.equal(await page.getByRole('button',{name:'Open comparison',exact:true}).count(),0);
  assert.equal(await page.getByRole('button',{name:/Keep this/}).count(),0);
  assert.equal(calls.length,0,'retained evidence never reacquires a runtime');
  await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
  const framing=await page.locator('ax-lattice').evaluate(graph=>({viewport:graph.getViewport(),bounds:graph.getBoundingClientRect().toJSON(),nodes:[...graph.querySelectorAll('ax-node')].map(item=>({x:item.getAttribute('data-x'),y:item.getAttribute('data-y'),box:item.getBoundingClientRect().toJSON()}))}));
  assert.ok(framing.nodes.every(item=>item.box.x>=framing.bounds.x-2&&item.box.x+item.box.width<=framing.bounds.x+framing.bounds.width+2),JSON.stringify(framing));
  await page.screenshot({path:join(screenshots,`graph-${theme}.png`),fullPage:true});
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
  assert.deepEqual(errors,[]);
 }finally{await context.close();}
});
test('live Ways graph keys exact candidate requests and becomes read-only on closure',async()=>{
 const{context,page,state,calls,errors}=await fixture();try{
  await page.evaluate(()=>document.querySelector('ax-ways-graph').show({sessionId:'session',setId:'set-original'}));
  await page.getByRole('button',{name:'Expand Attempt 1',exact:true}).click();
  await page.getByRole('button',{name:'Read diff: src/a.rs',exact:true}).click();
  await page.getByText('EXACT_CURRENT_DIFF',{exact:true}).waitFor();
  assert.ok(calls.filter(call=>/trajectories|status|diff/.test(call.path)).every(call=>call.params.attempt_set_id==='set-original'));
  await page.getByRole('button',{name:'Open comparison',exact:true}).click();
  assert.deepEqual(await page.evaluate(()=>window.review),{sessionId:'session',setId:'set-original'});
  state.results={attempt_set:null};state.archive.decisions=[retained()];
  await page.getByRole('button',{name:'Refresh evidence',exact:true}).click();
  await page.getByText('Finished without keeping',{exact:true}).first().waitFor();
  assert.equal(await page.getByRole('button',{name:'Open comparison',exact:true}).count(),0);
  assert.equal(await page.locator('ax-node[data-kind=agent]').count(),1,'expanded original candidate survives closure');
  assert.ok(calls.every(call=>call.method==='GET'));assert.deepEqual(errors,[]);
 }finally{await context.close();}
});
test('a different live set cannot retarget an old graph or open comparison',async()=>{
 const{context,page,state,calls,errors}=await fixture();try{
  state.results.attempt_set.id='another-set';
  await page.evaluate(()=>document.querySelector('ax-ways-graph').show({sessionId:'session',setId:'set-original'}));
  await page.getByText('This attempt set is no longer active and its retained decision is unavailable.',{exact:true}).waitFor();
  assert.equal(await page.getByRole('button',{name:'Open comparison',exact:true}).count(),0);
  assert.equal(await page.locator('ax-node').count(),0);assert.ok(calls.every(call=>call.method==='GET'));assert.deepEqual(errors,[]);
 }finally{await context.close();}
});
test('existing Attempts and Ways History expose exact graph navigation',async()=>{
 const{context,page,state,errors}=await fixture();try{
  await page.evaluate(async()=>{await import('/ui/attempts.js');await import('/ui/ways-history.js');document.querySelector('ax-ways-graph').remove();document.addEventListener('open-ways-graph',event=>window.graphEntry=event.detail);const attempts=document.createElement('ax-attempts');attempts.setAttribute('session','session');document.body.append(attempts);});
  await page.getByRole('button',{name:'Open Ways graph',exact:true}).click();
  assert.deepEqual(await page.evaluate(()=>window.graphEntry),{sessionId:'session',setId:'set-original'});
  state.archive.decisions=[retained()];
  await page.evaluate(async()=>{const history=document.createElement('ax-ways-history');document.body.append(history);await history.show({sessionId:'session'});});
  await page.getByText('No keep · no keep recorded',{exact:false}).click();
  await page.getByRole('button',{name:'Open decision graph',exact:true}).click();
  const entry=await page.evaluate(()=>window.graphEntry);assert.equal(entry.record.decision_id,'decision');assert.equal(entry.setId,'set-original');assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('Ways availability follows the applied native team without changing pre-Team single Agent behavior',async()=>{
 const {context,page,errors}=await fixture();try{
  const actual=await page.evaluate(async()=>{const source=await fetch('/fixture-app-source').then(response=>response.text());const start=source.indexOf('function sessionSupportsAttempts('),end=source.indexOf('\nfunction sessionHasExecutionHistory',start);const supports=new Function('S',`return (${source.slice(start,end)});`)({agents:[{id:'original',role:'autonomous'}]});const session={mode:{kind:'single_agent',agent_id:'original'}};return{before:supports(session),multiple:supports({...session,currentTeam:{approved:true,slots:[{role:'autonomous'},{role:'autonomous'}]}}),coordinator:supports({...session,currentTeam:{approved:true,slots:[{role:'coordinator'}]}}),single:supports({...session,currentTeam:{approved:true,slots:[{role:'autonomous'}]}}),omittedAutonomous:supports({...session,currentTeam:{approved:true,slots:[{}]}}),worker:supports({...session,currentTeam:{approved:true,slots:[{role:'worker'}]}}),invalid:supports({...session,currentTeam:{approved:true,slots:[{role:null}]}})};});
  assert.deepEqual(actual,{before:true,multiple:false,coordinator:false,single:true,omittedAutonomous:true,worker:false,invalid:false});assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('Ways storage preparation renders lifecycle and disables the actual composer Send control',async()=>{
 const {context,page,errors}=await fixture();try{
  const result=await page.evaluate(async()=>{
   const source=await fetch('/fixture-app-source').then(response=>response.text());
   const functionText=(name,next)=>source.slice(source.indexOf(`function ${name}(`),source.indexOf(`\nfunction ${next}(`,source.indexOf(`function ${name}(`)));
   document.body.insertAdjacentHTML('beforeend','<div id="cockpit-live"><span class="cockpit-live-label"></span></div><span id="cockpit-status"></span><button id="session-run-action"></button><button id="session-send"></button><div class="session-input"></div>');
   window.S={session:{id:'session',historyState:'ready',waysStoragePending:{},status:'active'},liveConnection:'connected'};
   window.$=selector=>document.querySelector(selector);window.sessionEnvironmentReady=()=>true;window.sessionRuntimeSurfaceReady=()=>true;window.isExecutionHistoryTurn=()=>false;
   window.syncSessionLifecycleChrome=new Function(`return (${functionText('syncSessionLifecycleChrome','initCockpitChrome')});`)();
   const sync=new Function(`return (${functionText('syncSessionComposerState','stopSessionTurn')});`)();sync();
   return{disabled:$('#session-send').disabled,label:$('#session-send').textContent,status:$('#cockpit-status').textContent};
  });assert.deepEqual(result,{disabled:true,label:'Checking Ways storage…',status:'checking Ways storage…'});assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('Ways storage preflight cannot double-send or consume a changed composer draft',async()=>{
 const {context,page,errors}=await fixture();try{
  const result=await page.evaluate(async()=>{
   const source=await fetch('/fixture-app-source').then(response=>response.text());const start=source.indexOf('async function sendSessionMessage()'),end=source.indexOf('\nfunction setSessionTurnPending',start);if(start<0||end<0)throw Error('Actual asynchronous Send entry is absent');
   const input=document.createElement('textarea');input.id='session-text';input.value='Original reviewed task';document.body.append(input);const fanout=document.createElement('div');fanout.id='fanout';fanout.validateApprovals=()=>true;document.body.append(fanout);
   window.S={session:{id:'session',historyState:'ready',fanoutOn:true,fanoutLanes:[{agent:'a'},{agent:'b'}]}};window.$=selector=>document.querySelector(selector);window.sessionEnvironmentReady=()=>true;window.sessionRuntimeSurfaceReady=()=>true;window.presentSessionRuntimeGate=()=>false;window.sessionSupportsAttempts=()=>true;window.syncSessionComposerState=()=>{};window.toast=()=>{};
   const actualFetch=window.fetch;let complete,calls=0;window.fetch=()=>{calls++;return new Promise(resolve=>{complete=resolve;});};
   try{const send=new Function(`return (${source.slice(start,end)});`)();const first=send();await send();input.value='Newer unsent draft';complete({ok:true,json:async()=>({limits:{records:8}})});await first;return{calls,draft:input.value,pending:S.session.waysStoragePending};}finally{window.fetch=actualFetch;}
  });
  assert.deepEqual(result,{calls:1,draft:'Newer unsent draft',pending:null});assert.deepEqual(errors,[]);
 }finally{await context.close();}
});
