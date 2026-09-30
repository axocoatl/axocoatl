import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { after, before, test } from 'node:test';
import { launchTestDaemon } from '../support/daemon.mjs';

let runtime, modelServer, unavailable=false;
const observed=[];
const model='browser-test-model:latest',digest='a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72';
before(async()=>{
  modelServer=createServer(async(req,res)=>{
    let raw='';for await(const chunk of req)raw+=chunk;observed.push({path:req.url,body:raw?JSON.parse(raw):null});
    if(unavailable){res.writeHead(503);res.end('{}');return;}
    const responses={
      '/api/version':{version:'0.20.6'},'/api/status':{cloud:{disabled:true}},
      '/api/show':{details:{format:'gguf'},capabilities:['completion']},
      '/api/tags':{models:[{name:model,model,digest}]},
      '/api/ps':{models:[{name:model,model,digest,details:{format:'gguf'},context_length:2048}]},
      '/api/generate':{model,created_at:'2026-09-15T00:00:00Z',response:'',done:true,done_reason:'load'},
    };
    res.writeHead(responses[req.url]?200:404,{'content-type':'application/json'});res.end(JSON.stringify(responses[req.url]||{}));
  });
  await new Promise(resolve=>modelServer.listen(0,'127.0.0.1',resolve));
  runtime=await launchTestDaemon({nativeDataRoot:true,ollamaBaseUrl:`http://127.0.0.1:${modelServer.address().port}`});
});
after(async()=>{await runtime?.stop();await new Promise(resolve=>modelServer?.close(resolve));});
async function call(session,suffix='',body,headers={}){
  const response=await fetch(`${runtime.baseUrl}/api/sessions/${session}/team${suffix}`,{method:body?'POST':'GET',headers:{...(body?{'content-type':'application/json'}:{}),...headers},body:body?JSON.stringify(body):undefined});const text=await response.text();return{status:response.status,value:text?JSON.parse(text):null};
}
test('actual Session team routes authenticate whole-graph Apply, retain exact retries and leave Cancel unchanged',async()=>{
  const id=runtime.fixtures.alpha.sessions[0].id,peer=runtime.fixtures.beta.sessions[0].id;
  const current=await call(id);assert.equal(current.status,200,JSON.stringify(current.value));assert.equal(current.value.history_version,'execution_v2');assert.equal(current.value.configuration_revision,0);assert.equal(current.value.approved,false);assert.equal(current.value.slots[0].template_id,'browser-test-coder');assert.equal(current.value.slots[0].limits,null);
  assert.equal(current.value.proposed_delegation,undefined,'a configuration without read-only Worker templates proposes no helpers');
  const edit={command_id:'team-route-approved',expected_configuration_revision:0,slots:current.value.slots.map(slot=>({...slot,max_output_tokens:128,limits:{activations:2,invocations:8,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000})),dependencies:current.value.dependencies,layout:current.value.layout};
  const forbidden=await call(id,'/preview',edit,{origin:'https://foreign.invalid'});assert.ok(forbidden.status>=400);
  const preview=await call(id,'/preview',edit);assert.equal(preview.status,200,JSON.stringify(preview.value));assert.equal(preview.value.applies_to,'future_turns');assert.equal((await call(id)).value.configuration_revision,0,'Preview cannot authorize a future turn');
  assert.deepEqual(preview.value.toolless_slots,[edit.slots[0].slot_id],'an Agent configured without tools has none in a native Session, and the review names it');
  const apply={edit,review_digest:preview.value.review_digest};const foreign=await call(peer,'/apply',apply);assert.ok(foreign.status>=400);assert.equal((await call(peer)).value.configuration_revision,0);
  const applied=await call(id,'/apply',apply);assert.equal(applied.status,200,JSON.stringify(applied.value));assert.equal(applied.value.configuration_revision,1);
  const saved=await call(id);assert.equal(saved.value.approved,true);assert.equal(saved.value.slots[0].template_id,null,'current slot preserves its captured definition rather than re-reading a template');
  unavailable=true;const before=observed.length;
  const repeated=await call(id,'/apply',apply);assert.equal(repeated.status,200,JSON.stringify(repeated.value));assert.equal(repeated.value.review_digest,preview.value.review_digest);assert.equal(observed.length,before,'saved Apply is resolved before model availability checks');
  const conflict=await call(id,'/apply',{...apply,edit:{...edit,slots:edit.slots.map(slot=>({...slot,instructions:'Different instructions under the same command'}))}});assert.equal(conflict.status,409);assert.equal(observed.length,before);
  const stale=await call(id,'/preview',{...edit,command_id:'stale-team-edit'});assert.equal(stale.status,409);assert.equal(observed.length,before);
  assert.equal((await call(id,'/cancel',{command_id:'cancel-team-draft'})).status,200);assert.deepEqual((await call(id)).value,saved.value,'Cancel cannot change saved team or grant');
  assert.equal(observed.filter(request=>request.path==='/api/chat').length,0,'Preview and Apply never perform inference');
  for(const request of observed.filter(request=>request.path==='/api/generate'))assert.equal(request.body.prompt??'','','native preparation may only load/observe context');
});


test('native Team preview rejects unsupported repository tools before provider observation', async () => {
  const invalid = await launchTestDaemon({nativeDataRoot:true, agentTools:['read_file','list_files'],
    ollamaBaseUrl:`http://127.0.0.1:${modelServer.address().port}`});
  try {
    const id = invalid.fixtures.alpha.sessions[0].id;
    const teamUrl = `${invalid.baseUrl}/api/sessions/${id}/team`;
    const current = await (await fetch(teamUrl)).json();
    const edit = {command_id:'invalid-tool-preview', expected_configuration_revision:0,
      slots:current.slots.map(slot=>({...slot,max_output_tokens:128,
        limits:{activations:1,invocations:2,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000})),
      dependencies:[],layout:[]};
    const before = observed.length;
    const response = await fetch(`${teamUrl}/preview`, {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(edit)});
    const value = await response.json();
    assert.ok(response.status >= 400, JSON.stringify(value));
    assert.match(value.error, /native Session repository tool 'list_files'/);
    assert.match(value.error, /list_dir/);
    assert.equal(observed.length,before,'Invalid tool names must fail before model metadata or load calls');
    assert.equal((await (await fetch(teamUrl)).json()).configuration_revision,0);
    assert.deepEqual(await (await fetch(`${invalid.baseUrl}/api/sessions/${id}/turns?history_version=2`)).json(),[]);
  } finally { await invalid.stop(); }
});

test('actual Session team carries what each Agent may change into its reviewed profile',async()=>{
  unavailable=false;
  const id=runtime.fixtures.alpha.sessions[1].id,current=await call(id);assert.equal(current.status,200,JSON.stringify(current.value));
  assert.equal(current.value.slots[0].writes,null,'an Agent configured without writes: may change any file, and the view says so explicitly');
  const edit=(command_id,writes)=>({command_id,expected_configuration_revision:current.value.configuration_revision,slots:current.value.slots.map(slot=>({...slot,...(writes===undefined?{}:{writes}),max_output_tokens:128,limits:{activations:2,invocations:8,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000})),dependencies:current.value.dependencies,layout:current.value.layout});
  for(const [command,writes] of [['writes-open',undefined],['writes-read-only',[]],['writes-scoped',['lib/','docs/*.md']]]){
    const preview=await call(id,'/preview',edit(command,writes));assert.equal(preview.status,200,JSON.stringify(preview.value));
    assert.deepEqual(preview.value.profiles.map(profile=>profile.write_scope),[writes],command);
  }
  for(const [index,writes] of [['../x'],['/etc'],['lib/','lib/']].entries()){
    const refused=await call(id,'/preview',edit(`writes-refused-${index}`,writes));assert.equal(refused.status,409,JSON.stringify(refused.value));
    assert.match(refused.value.error,/invalid list of paths it may change/);assert.match(refused.value.error,/Nothing for a read-only helper/);
  }
  const scoped=edit('writes-applied',['lib/']),preview=await call(id,'/preview',scoped);assert.equal(preview.status,200,JSON.stringify(preview.value));
  const applied=await call(id,'/apply',{edit:scoped,review_digest:preview.value.review_digest});assert.equal(applied.status,200,JSON.stringify(applied.value));
  const saved=await call(id);assert.deepEqual(saved.value.slots[0].writes,['lib/'],'the saved definition keeps its scope for the next edit');
  // Leaving writes out keeps the saved scope; only an explicit null widens it.
  const omitted={...edit('writes-omitted',undefined),expected_configuration_revision:saved.value.configuration_revision};
  omitted.slots=saved.value.slots.map(({writes,...slot})=>({...slot,max_output_tokens:128,limits:{activations:2,invocations:8,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000}));
  const kept=await call(id,'/preview',omitted);assert.equal(kept.status,200,JSON.stringify(kept.value));
  assert.deepEqual(kept.value.profiles.map(profile=>profile.write_scope),[['lib/']],'omitting writes never widens what the Agent may change');
  const opened=await call(id,'/preview',{...omitted,command_id:'writes-explicit-any',slots:omitted.slots.map(slot=>({...slot,writes:null}))});assert.equal(opened.status,200,JSON.stringify(opened.value));
  assert.deepEqual(opened.value.profiles.map(profile=>profile.write_scope),[undefined]);
});

test('a Team edit that leaves writes out keeps the template\'s read-only scope',async()=>{
  const readOnly=await launchTestDaemon({nativeDataRoot:true,agentWrites:[],ollamaBaseUrl:`http://127.0.0.1:${modelServer.address().port}`});
  try{
    const id=readOnly.fixtures.alpha.sessions[0].id,teamUrl=`${readOnly.baseUrl}/api/sessions/${id}/team`;
    const current=await (await fetch(teamUrl)).json();assert.deepEqual(current.slots[0].writes,[]);
    const edit={command_id:'template-scope-kept',expected_configuration_revision:0,
      slots:current.slots.map(({writes,...slot})=>({...slot,max_output_tokens:128,limits:{activations:2,invocations:8,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000})),
      dependencies:current.dependencies,layout:current.layout};
    const response=await fetch(`${teamUrl}/preview`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(edit)});const preview=await response.json();
    assert.equal(response.status,200,JSON.stringify(preview));
    assert.deepEqual(preview.profiles.map(profile=>profile.write_scope),[[]],'the template\'s writes: [] still applies');
  }finally{await readOnly.stop();}
});

test('actual Session team offers the detected check without enabling it and refuses checks nobody can pay for',async()=>{
  unavailable=false;
  const id=runtime.fixtures.beta.sessions[0].id;
  const set=await fetch(`${runtime.baseUrl}/api/sessions/${id}/check`,{method:'PUT',headers:{'content-type':'application/json'},body:JSON.stringify({check_command:'npm test'})});assert.equal(set.status,200);
  const current=await call(id);assert.equal(current.status,200,JSON.stringify(current.value));
  assert.deepEqual(current.value.suggested_check,['sh','-c','npm test']);assert.deepEqual(current.value.required_checks,[],'a detected check is only a suggestion');
  const edit=(command_id,required_checks)=>({command_id,expected_configuration_revision:current.value.configuration_revision,slots:current.value.slots.map(slot=>({...slot,max_output_tokens:128,limits:{activations:2,invocations:8,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000})),dependencies:current.value.dependencies,layout:current.value.layout,required_checks});
  const plain=await call(id,'/preview',edit('checks-none',[]));assert.equal(plain.status,200,JSON.stringify(plain.value));
  const refused=await call(id,'/preview',edit('checks-without-bash',[current.value.suggested_check]));assert.equal(refused.status,409,JSON.stringify(refused.value));
  assert.match(refused.value.error,/no required Agent in this team has bash/);
  const invalid=await call(id,'/preview',edit('checks-invalid',[[]]));assert.equal(invalid.status,409,JSON.stringify(invalid.value));
  assert.match(invalid.value.error,/Each required check must be a command/);
  const after=await call(id);assert.equal(after.value.configuration_revision,current.value.configuration_revision);assert.deepEqual(after.value.required_checks,[]);
});

test('actual Session team with a bash Agent applies required checks and reports them for the next edit',async()=>{
  const daemon=await launchTestDaemon({nativeDataRoot:true,agentTools:['bash','read_file'],ollamaBaseUrl:`http://127.0.0.1:${modelServer.address().port}`});
  try{
    const id=daemon.fixtures.alpha.sessions[0].id,url=`${daemon.baseUrl}/api/sessions/${id}/team`;
    const current=await (await fetch(url)).json();
    const checks=[['sh','-c','npm test'],['cargo','test','--quiet']];
    const edit={command_id:'checks-applied',expected_configuration_revision:0,slots:current.slots.map(slot=>({...slot,max_output_tokens:128,limits:{activations:2,invocations:12,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000})),dependencies:current.dependencies,layout:current.layout,required_checks:checks};
    const post=async(suffix,body)=>{const response=await fetch(`${url}${suffix}`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});return{status:response.status,value:await response.json()};};
    // Two checks need 8 invocations for two passes, 2 captures and 1 answer.
    const tight=structuredClone(edit);tight.command_id='checks-tight';for(const slot of tight.slots)slot.limits.invocations=10;
    const refused=await post('/preview',tight);assert.equal(refused.status,409,JSON.stringify(refused.value));
    assert.match(refused.value.error,/runs the required checks on its budget, so its invocation limit must be at least 11/);
    const preview=await post('/preview',edit);assert.equal(preview.status,200,JSON.stringify(preview.value));assert.deepEqual(preview.value.edit.required_checks,checks);assert.deepEqual(preview.value.toolless_slots,[]);
    const applied=await post('/apply',{edit,review_digest:preview.value.review_digest});assert.equal(applied.status,200,JSON.stringify(applied.value));
    const saved=await (await fetch(url)).json();assert.deepEqual(saved.required_checks,checks);
  }finally{await daemon.stop();}
});

test('a new Session proposes the read-only Worker templates as helpers, and the person approves their limits',async()=>{
  const daemon=await launchTestDaemon({nativeDataRoot:true,agentTools:['read_file','bash'],ollamaBaseUrl:`http://127.0.0.1:${modelServer.address().port}`,
    helpers:[{id:'scout',name:'Scout',tools:['read_file','grep','bash'],writes:[]},{id:'shell',name:'Shell',tools:['read_file','bash']},{id:'reviewer',name:'Reviewer',tools:['read_file','bash'],writes:[]}]});
  try{
    unavailable=false;
    // Helpers are approved only after the Session's environment is reviewed.
    const created=await fetch(`${daemon.baseUrl}/api/workspaces/${encodeURIComponent(daemon.fixtures.alpha.workspace.id)}/sessions`,{method:'POST',headers:{'content-type':'application/json'},
      body:JSON.stringify({name:'Default team',mode:{kind:'single_agent',agent_id:'browser-test-coder'},enabled_skills:[],exposed_ports:[],setup_command:null,setup_approved:false,setup_reviewed:true})});
    const session=await created.json();assert.equal(created.status,200,JSON.stringify(session));
    const url=`${daemon.baseUrl}/api/sessions/${session.id}/team`,current=await (await fetch(url)).json();
    assert.deepEqual(current.proposed_delegation,{slot_id:'slot-browser-test-coder',helpers:['scout','reviewer'],operations:['add_agent'],max_nodes:6,max_edges:5});
    assert.equal(current.slots.length,1,'the helpers are not team members');assert.equal(current.slots[0].delegation,undefined,'the proposal is not an approval');
    const {helpers,slot_id,...bounds}=current.proposed_delegation;
    const edit={command_id:'default-team',expected_configuration_revision:0,dependencies:[],layout:[],
      slots:current.slots.map(slot=>({...slot,max_output_tokens:128,limits:{activations:2,invocations:12,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000,
        ...(slot.slot_id===slot_id?{delegation:{...bounds,workers:helpers.map(template_id=>({template_id,limits:{activations:1,invocations:4,tokens:8000,cost_microunits:0},max_output_tokens:64,adhoc_allowed:false}))}}:{})}))};
    const post=async(suffix,body)=>{const response=await fetch(`${url}${suffix}`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});return{status:response.status,value:await response.json()};};
    const preview=await post('/preview',edit);assert.equal(preview.status,200,JSON.stringify(preview.value));
    const [[lead,policy]]=preview.value.coordinators;assert.equal(lead,'slot-browser-test-coder');
    assert.deepEqual(policy.workers.map(worker=>worker.template_id),['scout','reviewer']);assert.equal(policy.max_nodes,6);assert.equal(policy.max_edges,5);
    assert.deepEqual(preview.value.profiles.filter(profile=>profile.write_scope).map(profile=>profile.write_scope),[[],[]],'both helpers are reviewed as read-only');
    const applied=await post('/apply',{edit,review_digest:preview.value.review_digest});assert.equal(applied.status,200,JSON.stringify(applied.value));
    const saved=await (await fetch(url)).json();assert.equal(saved.proposed_delegation,undefined,'an applied team is not proposed helpers again');
    assert.deepEqual(saved.slots[0].delegation.workers.map(worker=>worker.template_id),['scout','reviewer']);
  }finally{await daemon.stop();}
});

test('actual Session team applies a required review only with a read-only Worker whose budget covers every round',async()=>{
  const daemon=await launchTestDaemon({nativeDataRoot:true,agentTools:['read_file'],ollamaBaseUrl:`http://127.0.0.1:${modelServer.address().port}`,
    // Worker templates inside a Coordinator's workflow may review too.
    extraAgents:[{id:'browser-test-coordinator',name:'Browser Test Coordinator',role:'coordinator'},
      {id:'browser-test-reviewer',name:'Browser Test Reviewer',role:'worker',tools:['read_file','bash'],writes:[]},
      {id:'browser-test-writer',name:'Browser Test Writer',role:'worker',tools:['read_file','write_file']}],
    workflows:[{id:'browser-test-review-team',entryPoint:'browser-test-coordinator',agents:['browser-test-coordinator','browser-test-reviewer','browser-test-writer']}]});
  try{
    const id=daemon.fixtures.alpha.sessions[0].id,url=`${daemon.baseUrl}/api/sessions/${id}/team`;
    const current=await (await fetch(url)).json();
    assert.deepEqual(current.reviewers,['browser-test-reviewer'],'only a read-only Worker may review');
    assert.equal(current.required_review,undefined);
    const post=async(suffix,body)=>{const response=await fetch(`${url}${suffix}`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});return{status:response.status,value:await response.json()};};
    const edit=(command_id,required_review,activations=2)=>({command_id,expected_configuration_revision:0,slots:current.slots.map(slot=>({...slot,max_output_tokens:128,limits:{activations,invocations:8,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000})),dependencies:current.dependencies,layout:current.layout,...(required_review?{required_review}:{})});
    const review={template_id:'browser-test-reviewer',max_rounds:2,limits:{activations:2,invocations:6,tokens:32768,cost_microunits:0},max_output_tokens:128};
    const refusals=[
      [{...review,max_rounds:4},/runs 1 to 3 rounds/],
      [{...review,template_id:'browser-test-writer'},/can change files or run commands \(write_file\)/],
      [{...review,template_id:'browser-test-coder'},/is not a Worker template/],
      [{...review,limits:{...review.limits,invocations:5}},/needs at least 6 invocations for 2 rounds/],
    ];
    for(const [index,[setting,message]] of refusals.entries()){
      const refused=await post('/preview',edit(`review-refused-${index}`,setting));assert.equal(refused.status,409,JSON.stringify(refused.value));assert.match(refused.value.error,message);
    }
    const lead=await post('/preview',edit('review-lead-too-small',review,1));assert.equal(lead.status,409,JSON.stringify(lead.value));
    assert.match(lead.value.error,/its activation limit must be at least 2/);
    // Rounds left out default to two.
    const {max_rounds,...defaulted}=review;
    const reviewed=edit('review-applied',defaulted);
    const preview=await post('/preview',reviewed);assert.equal(preview.status,200,JSON.stringify(preview.value));
    assert.deepEqual(preview.value.edit.required_review,review);
    const applied=await post('/apply',{edit:reviewed,review_digest:preview.value.review_digest});assert.equal(applied.status,200,JSON.stringify(applied.value));
    const saved=await (await fetch(url)).json();assert.deepEqual(saved.required_review,review);
    // Removing it applies a team with no review again.
    const plain={...edit('review-removed'),expected_configuration_revision:saved.configuration_revision,slots:saved.slots.map(slot=>({...slot,limits:{activations:2,invocations:8,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000}))};
    const removedPreview=await post('/preview',plain);assert.equal(removedPreview.status,200,JSON.stringify(removedPreview.value));assert.equal(removedPreview.value.edit.required_review,undefined);
    const removed=await post('/apply',{edit:plain,review_digest:removedPreview.value.review_digest});assert.equal(removed.status,200,JSON.stringify(removed.value));
    assert.equal((await (await fetch(url)).json()).required_review,undefined);
  }finally{await daemon.stop();}
});
