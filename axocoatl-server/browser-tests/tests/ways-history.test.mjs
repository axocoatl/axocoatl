import assert from 'node:assert/strict';
import {after,before,test} from 'node:test';
import {chromium} from 'playwright';
import {launchTestDaemon,resolveChromiumExecutable,newAuthorizedContext} from '../support/daemon.mjs';
let runtime,browser;
before(async()=>{runtime=process.env.AXOCOATL_COMPONENT_BASE_URL?{baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}}:await launchTestDaemon();const executablePath=await resolveChromiumExecutable();browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});});
after(async()=>{await browser?.close();await runtime?.stop();});
const text=value=>({state:'complete',text:value,sha256:'a'.repeat(64)});
const fixture=()=>({limits:{version:1,field_bytes:1024,record_bytes:1048576,aggregate_bytes:8388608,records:100,candidates:100,items_per_field:1000},deleted:[],decisions:[{decision_id:'decision-original',session_id:'session',task:text('Fix subtraction'),set_id:'set',human_decision:{decided_at_unix_ms:1000,choice:{kind:'no_keep'}},application:{state:'no_keep_recorded'},cleanup:{completed_at_unix_ms:2000},candidates:[{id:{index:0},model:{state:'available',value:{provider_id:'ollama',model_id:'local'}},terminal:'failed',outcome:text('Tests found a failure'),route:text('Read then check'),reviewable_diff:{state:'truncated',text:'RETAINED_DIFF',original_bytes:100,offset_bytes:0},changed_paths:{items:['src/lib.rs'],original_count:1},checks:[],usage:{tokens:{kind:'unknown',known_subtotal:{input_tokens:4,output_tokens:2}},cost_usd_known_subtotal:0,cost_complete:false}}]}]});
for(const theme of ['light','dark'])test(`closed decision remains inspectable and attachable with unknown usage (${theme})`,async()=>{
  const context=await newAuthorizedContext(browser, {viewport:{width:theme==='dark'?390:1100,height:800},reducedMotion:'reduce'});const page=await context.newPage();const errors=[];page.on('pageerror',error=>errors.push(error.message));
  try{
    await page.route('**/ways-fixture',route=>route.fulfill({contentType:'text/html',body:`<html data-theme="${theme}"><head><link rel="stylesheet" href="/ui/tokens.css"></head><body></body></html>`}));
    await page.route('**/api/sessions/session/ways-history',route=>route.fulfill({contentType:'application/json',body:JSON.stringify(fixture())}));
    await page.goto(`${runtime.baseUrl}/ways-fixture`);await page.evaluate(async()=>{await import('/ui/ways-history.js');const view=document.createElement('ax-ways-history');document.body.append(view);view.addEventListener('attach-ways-decision',event=>window.attached=event.detail);await view.show({sessionId:'session'});});
    await page.getByText('No keep · no keep recorded', {exact:false}).click();await page.getByText('Attempt 1 · ollama/local · failed',{exact:true}).click();
    assert.match(await page.locator('ax-ways-history dialog').innerText(),/known subtotal; total unknown/);assert.match(await page.locator('ax-ways-history dialog').innerText(),/Truncated: 13 of 100/);
    assert.equal(await page.getByRole('button',{name:'Keep this one',exact:true}).count(),0);
    await page.getByRole('button',{name:'Attach to request',exact:true}).click();const attached=await page.evaluate(()=>window.attached);assert.equal(attached.reference.kind,'ways_decision');assert.equal(attached.reference.reference_id,'decision-original');assert.equal(attached.reference.metadata.source_session_id,'session');assert.deepEqual(errors,[]);
  }finally{await context.close();}
});
test('storage requires explicit limits, preserves an Apply error, and retries the same values',async()=>{
  const context=await newAuthorizedContext(browser);const page=await context.newPage();const bodies=[];
  try{
    await page.route('**/ways-fixture',route=>route.fulfill({contentType:'text/html',body:'<body></body>'}));
    await page.route('**/api/sessions/session/ways-history',route=>route.fulfill({contentType:'application/json',body:JSON.stringify({limits:null,decisions:[],deleted:[]})}));
    await page.route('**/api/sessions/session/ways-history/configuration',async route=>{bodies.push(route.request().postDataJSON());await route.fulfill({status:bodies.length===1?409:200,contentType:'application/json',body:JSON.stringify(bodies.length===1?{error:'Storage unavailable'}:{limits:bodies[0].limits,decisions:[],deleted:[]})});});
    await page.goto(`${runtime.baseUrl}/ways-fixture`);await page.evaluate(async()=>{await import('/ui/ways-history.js');const view=document.createElement('ax-ways-history');document.body.append(view);await view.show({sessionId:'session',configure:true});});
    assert.equal(await page.getByLabel('Total retained storage (MiB)',{exact:true}).inputValue(),'');
    for(const [name,value] of [['field_bytes','1'],['record_bytes','1'],['aggregate_bytes','8'],['records','100'],['candidates','100'],['items_per_field','1000']])await page.locator(`input[name=${name}]`).fill(value);
    await page.getByRole('button',{name:'Approve storage limits'}).click();await page.getByText('Storage unavailable',{exact:true}).waitFor();assert.equal(await page.locator('input[name=aggregate_bytes]').inputValue(),'8');
    await page.getByRole('button',{name:'Approve storage limits'}).click();await page.getByText('No Ways decision has been closed yet.').waitFor();assert.deepEqual(bodies[0],bodies[1]);assert.equal(bodies[0].limits.aggregate_bytes,8*1024*1024);
  }finally{await context.close();}
});
