import assert from 'node:assert/strict';
import {after,before,test} from 'node:test';
import {chromium} from 'playwright';
import {launchTestDaemon,resolveChromiumExecutable} from '../support/daemon.mjs';

let runtime,browser;
before(async()=>{runtime=process.env.AXOCOATL_COMPONENT_BASE_URL?{baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}}:await launchTestDaemon();const executablePath=await resolveChromiumExecutable();browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});});
after(async()=>{await browser?.close();await runtime?.stop();});

const NOW=1_790_000_000_000;
const slots=[['slot-paths','Paths Owner'],['slot-manifest','Manifest Owner'],['slot-review','Contract Reviewer']].map(([slot_id,name])=>({slot_id,name,limits:{activations:8,invocations:200,tokens:6000000,cost_microunits:0},expires_at_ms:4096902600000}));
const binding={binding_id:'signals',binding_revision:1,team_revision:2,session_id:'session',event_kind:'signal'};
const source={kind:'signal_field',routes:[{slot_id:'slot-paths',watches:['lib/paths.js'],threshold_milli:1000},{slot_id:'slot-manifest',watches:['lib/manifest.js'],threshold_milli:1000},{slot_id:'slot-review',watches:['lib/'],threshold_milli:500}],half_life_ms:1800000,max_dispatches:8};
function field(){
  const deposits=[
    {id:'flag:one',kind:'human',paths:['lib/manifest.js'],observed:{'lib/manifest.js':'a'.repeat(64)},strength:1,deposited_at_ms:NOW-600000,producer:null,summary:'Person: manifest contract',cause:{kind:'human',author:null},withdrawn:null,sensed_by:[],routed:true,producer_label:null},
    {id:'change:two',kind:'change',paths:['lib/manifest.js'],observed:{},strength:0.5,deposited_at_ms:NOW-300000,producer:'node-manifest',summary:'Manifest Owner changed lib/manifest.js',cause:{kind:'turn',session_id:'session',turn_id:'turn-manifest'},withdrawn:null,sensed_by:['slot-review'],routed:true,producer_label:'Manifest Owner'},
    {id:'finding:three',kind:'finding',paths:['lib/paths.js'],observed:{'lib/paths.js':'b'.repeat(64)},strength:1,deposited_at_ms:NOW-120000,producer:'node-review',summary:'Contract Reviewer: clean accepts ".."',cause:{kind:'knowledge_proposal',proposal_id:'proposal-x',note_id:'dotdot',journal_id:'journal',turn_id:'turn-review'},withdrawn:null,sensed_by:['slot-paths'],routed:true,producer_label:'Contract Reviewer'},
    {id:'finding:four',kind:'finding',paths:['docs/readme.md'],observed:{},strength:1,deposited_at_ms:NOW-60000,producer:'node-review',summary:'Contract Reviewer: README is stale',cause:{kind:'knowledge_proposal',proposal_id:'proposal-y',note_id:'readme',journal_id:'journal',turn_id:'turn-review'},withdrawn:null,sensed_by:[],routed:false,producer_label:'Contract Reviewer'},
  ];
  return [{binding_id:'signals',binding_revision:1,armed:true,event_kind:'signal',half_life_ms:1800000,max_dispatches:8,automatic_dispatches:2,observed_at_ms:NOW-5000,observation_truncated:false,now_ms:NOW,error:null,
    sensors:[
      {slot_id:'slot-paths',node_id:'node-paths',label:'Paths Owner',watches:['lib/paths.js'],threshold:1,intensity:0.96,crossed:false,state:'sensing',deposits:[{deposit:'finding:three',weight:0.96}]},
      {slot_id:'slot-manifest',node_id:'node-manifest',label:'Manifest Owner',watches:['lib/manifest.js'],threshold:1,intensity:0,crossed:false,state:'quiet',deposits:[{deposit:'flag:one',weight:0.79,excluded:'consumed'},{deposit:'change:two',weight:0.45,excluded:'own_deposit'}]},
      {slot_id:'slot-review',node_id:'node-review',label:'Contract Reviewer',watches:['lib/'],threshold:0.5,intensity:0.45,crossed:false,state:'waiting',deposits:[{deposit:'flag:one',weight:0.79,excluded:'source_changed'},{deposit:'change:two',weight:0.45},{deposit:'finding:three',weight:0.96,excluded:'own_deposit'}]},
    ],
    deposits,
    dispatches:[
      {id:'dispatch-a',sensor:'node-manifest',at_ms:NOW-590000,intensity:1,threshold:1,contributions:[{deposit:'flag:one',weight:1}],manual:false,label:'Manifest Owner',receipt_id:'receipt-a',turn_id:'turn-manifest',disposition:'reserved'},
      {id:'dispatch-b',sensor:'node-review',at_ms:NOW-290000,intensity:0.5,threshold:0.5,contributions:[{deposit:'change:two',weight:0.5}],manual:false,label:'Contract Reviewer',receipt_id:'receipt-b',turn_id:'turn-review',disposition:'reserved'},
    ]}];
}
async function fixture(){
  const context=await browser.newContext({viewport:{width:390,height:844},colorScheme:'dark',reducedMotion:'reduce'}),page=await context.newPage(),calls=[],errors=[];
  const state={bindings:[],views:[],receipts:[]};
  page.on('pageerror',error=>errors.push(error.message));
  await page.route('**/signals-fixture',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><html data-theme="dark"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"><ax-session-work session="session"></ax-session-work><script type="module" src="/ui/session-work.js"></script></html>'}));
  await page.route('**/api/sessions/session/team',route=>route.fulfill({json:{configuration_revision:2,approved:true,slots}}));
  await page.route('**/api/sessions/session/work**',route=>{
    const suffix=new URL(route.request().url()).pathname.split('/work')[1],body=route.request().method()==='POST'?route.request().postDataJSON():null;calls.push({suffix,body});
    if(!suffix)return route.fulfill({json:{bindings:state.bindings,receipts:state.receipts}});
    if(suffix==='/bindings'){const saved={binding:{...binding,binding_id:body.binding_id},source:body.source,armed:body.armed,instruction:body.instruction,required_checks:body.required_checks,grants:[],authorized_at_ms:1};state.bindings=[saved];state.views=field();return route.fulfill({json:saved});}
    if(suffix==='/signals')return route.fulfill({json:state.views});
    if(suffix.endsWith('/flags')){state.views[0].deposits.push({id:'flag:new',kind:'human',paths:body.paths,observed:{},strength:1,deposited_at_ms:NOW,producer:null,summary:`Person: ${body.summary}`,cause:{kind:'human',author:null},withdrawn:null,sensed_by:[],routed:true,producer_label:null});return route.fulfill({json:state.views});}
    if(suffix.endsWith('/sense'))return route.fulfill({json:state.views});
    if(suffix.includes('/withdraw')){const deposit=state.views[0].deposits.find(item=>suffix.includes(encodeURIComponent(item.id)));deposit.withdrawn=`Person: ${body.reason}`;return route.fulfill({json:state.views});}
    if(suffix.endsWith('/dispatch')){state.views[0].sensors[0].state='waiting';state.views[0].dispatches.push({id:'manual-c',sensor:'node-paths',at_ms:NOW,intensity:0.96,threshold:1,contributions:[{deposit:'finding:three',weight:0.96}],manual:true,label:'Paths Owner',receipt_id:'receipt-c',turn_id:'turn-paths',disposition:'queued'});return route.fulfill({json:state.views});}
    return route.fulfill({status:404,json:{error:'unexpected'}});
  });
  await page.goto(`${runtime.baseUrl}/signals-fixture`);
  await page.getByRole('button',{name:'Work sources',exact:true}).click();
  await page.getByText('No source is configured',{exact:false}).waitFor();
  return {page,context,calls,errors,state};
}

test('signal field routes are reviewed per Agent and saved as exact thresholds',async()=>{
  const {page,context,calls,errors}=await fixture();
  try{
    await page.getByRole('button',{name:'Add work source',exact:true}).click();
    await page.getByLabel('Event kind',{exact:true}).fill('signal');
    await page.getByLabel('Source',{exact:true}).selectOption('signal_field');
    assert.equal(await page.getByLabel('Configured webhook name',{exact:true}).isVisible(),false,'fields for other sources stay hidden');
    await page.getByLabel('Paths Paths Owner watches',{exact:true}).fill('lib/paths.js');
    await page.getByLabel('Paths Manifest Owner watches',{exact:true}).fill('lib/manifest.js');
    await page.getByLabel('Paths Contract Reviewer watches',{exact:true}).fill('lib/\n*.test.js');
    await page.getByLabel('Contract Reviewer threshold',{exact:true}).fill('0.5');
    await page.getByLabel('Contract Reviewer is read-only',{exact:true}).check();
    assert.equal(await page.getByLabel('Paths Contract Reviewer may change',{exact:true}).isDisabled(),true,'a read-only Agent names no owned paths');
    await page.getByLabel('Half-life in minutes (blank keeps full strength)',{exact:true}).fill('30');
    await page.getByLabel('Standing instruction',{exact:true}).fill('Coordinate through signals only.');
    await page.getByLabel('Arm this source for new events',{exact:true}).check();
    await page.getByRole('button',{name:'Review source changes',exact:true}).click();
    await page.getByText('Contract Reviewer watches lib/, *.test.js, read-only at 0.50',{exact:false}).waitFor();
    assert.equal(calls.filter(call=>call.suffix==='/bindings').length,0,'review does not save');
    await page.getByRole('button',{name:'Apply source',exact:true}).click();
    await page.getByText('signal · armed',{exact:true}).waitFor();
    const saved=calls.find(call=>call.suffix==='/bindings').body;
    assert.deepEqual(saved.source,{kind:'signal_field',routes:[{slot_id:'slot-paths',watches:['lib/paths.js'],threshold_milli:1000},{slot_id:'slot-manifest',watches:['lib/manifest.js'],threshold_milli:1000},{slot_id:'slot-review',watches:['lib/','*.test.js'],threshold_milli:500,owns:[]}],half_life_ms:1800000,max_dispatches:8});
    assert.equal(saved.expected_team_revision,2);
    assert.deepEqual(errors,[]);
  }finally{await context.close();}
});

test('the live field explains what each Agent senses and why each deposit counts or not',async()=>{
  const {page,context,calls,errors,state}=await fixture();
  try{
    state.bindings=[{binding,source,armed:true,instruction:'Signals only.',required_checks:[],grants:[],authorized_at_ms:1}];state.views=field();
    await page.getByRole('button',{name:'Refresh work',exact:true}).click();
    await page.getByText('Signal field · armed',{exact:true}).waitFor();
    assert.equal(await page.getByRole('meter',{name:'Paths Owner signal 0.96 of threshold 1.00',exact:true}).count(),1);
    // Exclusions are explained per Agent rather than silently dropped.
    await page.getByText('Manifest Owner: already acted on',{exact:false}).waitFor();
    await page.getByText('Contract Reviewer: source changed since',{exact:false}).waitFor();
    await page.getByText('No Agent watches these paths; nobody is woken by it.',{exact:true}).waitFor();
    await page.getByText('Contract Reviewer · 0.50 ≥ threshold 0.50',{exact:false}).waitFor();
    // The cause graph links deposits, dispatches and the deposits their turns left.
    await page.getByRole('application',{name:'Signal cause graph'}).waitFor();
    assert.ok(await page.locator('ax-session-work').evaluate(host=>host.shadowRoot.querySelector('ax-session-signals').shadowRoot.querySelectorAll('ax-edge').length)>=4);
    // The cause chain reads top to bottom: a root signal, then each dispatch
    // with what caused it and the signals its turn left.
    await page.getByText('Cause chain (2 dispatches)',{exact:true}).waitFor();
    await page.getByText('Signal · 10 min ago · flagged by a person',{exact:true}).waitFor();
    await page.getByText('1. Manifest Owner · 1.00 ≥ threshold 1.00',{exact:false}).waitFor();
    await page.getByText('↳ left Change on lib/manifest.js → sensed by Contract Reviewer',{exact:true}).waitFor();
    await page.getByText('↳ left Finding on docs/readme.md → nobody watches these paths',{exact:true}).waitFor();
    const expand=page.getByRole('button',{name:'Expand graph',exact:true});
    await expand.click();
    await page.getByRole('button',{name:'Shrink graph',exact:true}).waitFor();
    assert.ok(await page.locator('ax-session-work').evaluate(host=>host.shadowRoot.querySelector('ax-session-signals').shadowRoot.querySelector('.graph-wrap').classList.contains('expanded')));

    // A half-typed flag survives background refresh.
    await page.getByLabel('Paths to flag',{exact:true}).fill('lib/paths.js');
    await page.getByLabel('What should be looked at',{exact:true}).fill('Reject traversal before normalizing');
    await page.getByRole('button',{name:'Refresh work',exact:true}).click();
    await page.getByText('Signal field · armed',{exact:true}).waitFor();
    assert.equal(await page.getByLabel('Paths to flag',{exact:true}).inputValue(),'lib/paths.js');
    await page.getByRole('button',{name:'Flag paths',exact:true}).click();
    await page.getByRole('paragraph').filter({hasText:'Person: Reject traversal before normalizing'}).waitFor();
    assert.deepEqual(calls.find(call=>call.suffix.endsWith('/flags')).body,{paths:['lib/paths.js'],summary:'Reject traversal before normalizing'});
    assert.equal(await page.getByLabel('Paths to flag',{exact:true}).inputValue(),'','a recorded flag clears its draft');

    // Withdrawal needs a reason and is retained.
    await page.getByRole('button',{name:'Withdraw',exact:true}).first().click();
    await page.getByText('Enter a reason for withdrawing this signal.',{exact:true}).waitFor();
    assert.equal(calls.filter(call=>call.suffix.includes('/withdraw')).length,0);

    // A person can send an Agent its current signals below threshold.
    await page.getByRole('button',{name:'Send to Paths Owner now',exact:true}).click();
    await page.getByText('Paths Owner · 0.96 sent by a person below threshold 1.00',{exact:false}).waitFor();
    assert.ok(calls.some(call=>call.suffix==='/signals/signals/routes/slot-paths/dispatch'));
    assert.deepEqual(errors,[]);
  }finally{await context.close();}
});

test('a dispatched turn opens from the field and closes the dialog',async()=>{
  const {page,context,errors,state}=await fixture();
  try{
    state.bindings=[{binding,source,armed:true,instruction:'Signals only.',required_checks:[],grants:[],authorized_at_ms:1}];state.views=field();
    state.receipts=[{receipt:{receipt_id:'receipt-a',turn_id:'turn-manifest',request:{binding,event:{event_id:'signal:dispatch-a',correlation_id:'signals:signals',subject:{kind:'signal_field',reference_id:'node-manifest',version:'dispatch-a'},evidence_refs:['flag:one']}},disposition:{state:'reserved'}},state:'completed',reason:null,can_dismiss:false,readiness:{state:'unmet',checks:[]}}];
    await page.getByRole('button',{name:'Refresh work',exact:true}).click();
    await page.getByText('Signal work for Manifest Owner · completed',{exact:true}).waitFor();
    const opened=page.evaluate(()=>new Promise(resolve=>document.addEventListener('open-session-work-turn',event=>resolve(event.detail),{once:true})));
    await page.getByRole('button',{name:'Inspect dispatched turn',exact:true}).first().click();
    assert.deepEqual(await opened,{session_id:'session',turn_id:'turn-manifest'});
    assert.equal(await page.locator('ax-session-work').evaluate(host=>host.shadowRoot.querySelector('dialog').open),false);
    assert.deepEqual(errors,[]);
  }finally{await context.close();}
});

test('an unpublished finding can be accepted and the panel says whom it reaches',async()=>{
  const {page,context,errors,state}=await fixture();
  try{
    const views=field();
    views[0].stranded=[{proposal_id:'proposal-z',expected_revision:0,title:'clean() keeps dot segments',paths:['lib/paths.js'],turn_id:'turn-5c0ffee1',turn_state:'cancelled'}];
    state.bindings=[{binding,source,armed:true,instruction:'Signals only.',required_checks:[],grants:[],authorized_at_ms:1}];state.views=views;
    let accepted=null;
    await page.route('**/api/sessions/session/knowledge/proposals/**',route=>{
      accepted=new URL(route.request().url()).pathname;
      state.views[0].stranded=[];
      state.views[0].deposits.push({id:'finding:proposal-z',kind:'finding',paths:['lib/paths.js'],observed:{},strength:1,deposited_at_ms:NOW,producer:'node-review',summary:'Contract Reviewer: clean() keeps dot segments (accepted by a person from a turn that did not finish)',cause:{kind:'knowledge_proposal',proposal_id:'proposal-z',note_id:'paths-clean',journal_id:'journal',turn_id:'turn-5c0ffee1'},withdrawn:null,sensed_by:['slot-paths'],routed:true,producer_label:'Contract Reviewer'});
      return route.fulfill({json:{}});
    });
    await page.getByRole('button',{name:'Refresh work',exact:true}).click();
    await page.getByText('Findings that were not published',{exact:true}).waitFor();
    await page.getByText('lib/paths.js · turn 5c0ffee1 · cancelled',{exact:true}).waitFor();
    await page.getByRole('button',{name:'Accept as signal',exact:true}).click();
    await page.getByText('Finding accepted; it now signals Paths Owner.',{exact:true}).waitFor();
    assert.equal(accepted,'/api/sessions/session/knowledge/proposals/proposal-z/accept');
    assert.deepEqual(errors,[]);
  }finally{await context.close();}
});
