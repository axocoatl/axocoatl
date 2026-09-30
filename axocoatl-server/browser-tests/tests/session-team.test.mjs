import assert from 'node:assert/strict';
import {mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';
let runtime,browser,screenshots;
before(async()=>{screenshots=await mkdtemp(join(tmpdir(),'axocoatl-session-team-'));runtime=process.env.AXOCOATL_COMPONENT_BASE_URL?{baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}}:await launchTestDaemon();const executablePath=await resolveChromiumExecutable();browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});});
after(async()=>{try{await browser?.close();await runtime?.stop();}finally{if(screenshots)await rm(screenshots,{recursive:true,force:true});}});
const template={slot_id:'slot-reviewer',template_id:'reviewer',source_slot_id:null,name:'QA reviewer <literal>',provider:'ollama',model:'local-model',instructions:'Inspect actual client changes.',max_output_tokens:128,required:true,reset_history:true,limits:null,expires_at_ms:null};
async function fixture({theme='light',approved=false,reject=false,loseReply=false,coordinator=false,role=coordinator?'coordinator':null,helper=coordinator,legacy=false,suggested=false,checks=[],proposal=false,reviewers=[],requiredReview=null,toolless=false}={}){
 const context=await browser.newContext({viewport:theme==='dark'?{width:390,height:840}:{width:1100,height:820},colorScheme:theme,reducedMotion:'reduce'}),page=await context.newPage(),errors=[],calls=[];page.on('pageerror',error=>errors.push(error.message));
 const view={history_version:legacy?'legacy_v1':'execution_v2',configuration_revision:approved?1:0,slots:[{...structuredClone(template),template_id:approved?null:template.template_id,reset_history:!approved,...(approved?{limits:{activations:2,invocations:12,tokens:32768,cost_microunits:0},expires_at_ms:Date.now()+86400000}:{})}],dependencies:[],layout:[],templates:[structuredClone(template)],approved,required_checks:structuredClone(checks),suggested_check:suggested?['sh','-c','npm test']:null,reviewers,...(requiredReview?{required_review:structuredClone(requiredReview)}:{})};
 if(role){view.slots[0].role=role;view.templates[0].role=role;}
 if(helper)view.templates.push({...structuredClone(template),template_id:'worker',slot_id:'worker',name:'Worker reviewer',role:'worker'});
 if(proposal){for(const[id,name]of[['scout','Scout'],['critic','Critic']])view.templates.push({...structuredClone(template),template_id:id,slot_id:`slot-${id}`,name,role:'worker',writes:[],max_output_tokens:256});view.proposed_delegation={slot_id:'slot-reviewer',helpers:['scout','critic'],operations:['add_agent'],max_nodes:6,max_edges:5};}
 await page.route('**/team-fixture',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="${theme}"><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-session-team session="fixture-session"></ax-session-team><script type="module" src="/ui/session-team.js"></script></body></html>`}));
 await page.route('**/api/sessions/fixture-session/team**',async route=>{const suffix=new URL(route.request().url()).pathname.split('/team')[1],body=route.request().method()==='POST'?route.request().postDataJSON():null;calls.push({suffix,body});if(!suffix)return route.fulfill({json:view});if(suffix==='/cancel')return route.fulfill({json:{cancelled:true}});if(suffix==='/preview')return route.fulfill({json:{edit:body,review_digest:'exact-review',configuration_revision:body.expected_configuration_revision+1,applies_to:'future_turns',changes:body.slots.map(slot=>({slot_id:slot.slot_id,kind:'changed',history:'new conversation'})),coordinators:body.slots.filter(slot=>slot.delegation).map(slot=>[slot.slot_id,{...slot.delegation,resource:{environment_generation:1,backend:'podman',network:'none',setup_command:null}}]),profiles:body.slots.map(slot=>({definition:slot.slot_id,provider:slot.provider,model:slot.model,isolation:'in-process',tools:toolless?[]:['read_file','write_file'],...(slot.writes==null?{}:{write_scope:slot.writes})})),toolless_slots:toolless?body.slots.map(slot=>slot.slot_id):[]}});if(suffix==='/apply'){if(reject)return route.fulfill({status:409,json:{error:'Session configuration changed; refresh'}});if(loseReply){loseReply=false;return route.abort('failed');}view.configuration_revision=body.edit.expected_configuration_revision+1;view.slots=body.edit.slots;view.approved=true;return route.fulfill({json:{configuration_revision:view.configuration_revision}});}});
 await page.goto(`${runtime.baseUrl}/team-fixture`);await page.getByRole('button',{name:'Team and budget',exact:true}).click();await page.getByText(legacy?'This Session uses legacy history.':approved?'Saved Session configuration 1.':'Approve explicit budgets before sending',{exact:false}).waitFor();return{context,page,calls,view,errors};
}
async function enterBudget(page){await page.getByRole('button',{name:'Edit',exact:true}).click();for(const[label,value]of[['Activation limit','2'],['Provider and tool invocation limit','12'],['Total token limit','32768'],['Cost limit (USD)','0'],['Budget expires (your local time)','2099-10-10T10:00']])await page.getByLabel(label,{exact:true}).fill(value);}
async function review(page){await page.getByRole('button',{name:'Preview changes',exact:true}).click();await page.getByText('Review these changes.',{exact:false}).waitFor();}
for(const theme of ['light','dark'])test(`Session team ${theme}: explicit budgets, typed review and future Apply`,async()=>{const{page,context,calls,errors}=await fixture({theme});try{assert.equal(await page.getByLabel('Activation limit',{exact:true}).inputValue(),'');await enterBudget(page);await review(page);const preview=calls.find(call=>call.suffix==='/preview').body;assert.equal(preview.slots[0].template_id,'reviewer');assert.deepEqual(preview.slots[0].limits,{activations:2,invocations:12,tokens:32768,cost_microunits:0});assert.equal(preview.expected_configuration_revision,0);assert.equal(await page.locator('ax-session-team').evaluate(element=>element.shadowRoot.querySelector('img')),null);await page.screenshot({path:join(screenshots,`team-${theme}.png`),fullPage:true});await page.getByRole('button',{name:'Apply to this Session',exact:true}).click();await page.getByText('Saved Session configuration 1.',{exact:false}).waitFor();const apply=calls.find(call=>call.suffix==='/apply').body;assert.deepEqual(apply.edit,preview);assert.equal(apply.review_digest,'exact-review');assert.deepEqual(errors,[]);}finally{await context.close();}});
test('Session team draft Lattice copy/undo and Cancel never apply',async()=>{const{page,context,calls,errors}=await fixture({approved:true});try{await page.getByRole('button',{name:'Edit',exact:true}).click();await page.locator('ax-session-team').evaluate(element=>element.shadowRoot.querySelector('ax-lattice').setSelection(['slot-reviewer']));await page.getByRole('button',{name:'Copy',exact:true}).click();await page.getByRole('button',{name:'Paste',exact:true}).click();await page.waitForFunction(()=>document.querySelector('ax-session-team').draft.slots.length===2);const copied=await page.locator('ax-session-team').evaluate(element=>element.draft.slots[1]);assert.equal(copied.template_id,null);assert.equal(copied.source_slot_id,'slot-reviewer');assert.equal(copied.reset_history,true);await page.getByRole('button',{name:'Undo',exact:true}).click();assert.equal(await page.locator('ax-session-team').evaluate(element=>element.draft.slots.length),1);await page.getByRole('button',{name:'Redo',exact:true}).click();assert.equal(await page.locator('ax-session-team').evaluate(element=>element.draft.slots.length),2);await page.getByRole('button',{name:'Cancel changes',exact:true}).click();await page.locator('dialog').waitFor({state:'hidden'});assert.equal(calls.filter(call=>call.suffix==='/apply').length,0);assert.equal(calls.filter(call=>call.suffix==='/cancel').length,1);assert.deepEqual(errors,[]);}finally{await context.close();}});
test('Session team stale Apply permits refresh',async()=>{const{page,context,calls}=await fixture({reject:true});try{await enterBudget(page);await review(page);await page.getByRole('button',{name:'Apply to this Session',exact:true}).click();await page.getByText('Refresh the Session team and review your changes again.',{exact:false}).waitFor();assert.equal(await page.evaluate(()=>localStorage.getItem('axocoatl-session-team-apply:fixture-session')),null);await page.getByRole('button',{name:'Refresh',exact:true}).click();await page.getByText('Approve explicit budgets before sending',{exact:false}).waitFor();assert.equal(calls.filter(call=>call.suffix==='/apply').length,1);}finally{await context.close();}});
test('Session team pointer connections survive field edits and reach whole-graph review',async()=>{
 const{page,context,calls,errors}=await fixture({approved:true});try{
  const team=page.locator('ax-session-team');await page.getByRole('button',{name:'Edit',exact:true}).click();await page.getByRole('button',{name:'Add Agent',exact:true}).click();
  await page.getByLabel('Name',{exact:true}).fill('Security reviewer');await page.getByLabel('Model',{exact:true}).fill('review-model');
  for(const[label,value]of[['Activation limit','2'],['Provider and tool invocation limit','12'],['Total token limit','32768'],['Cost limit (USD)','0'],['Budget expires (your local time)','2099-10-10T10:00']])await page.getByLabel(label,{exact:true}).fill(value);
  const nodes=team.locator('ax-node');assert.match(await nodes.nth(1).innerText(),/Security reviewer[\s\S]*review-model/);
  const connect=async(from,to)=>{await page.screenshot();const a=await nodes.nth(from).locator('ax-handle[type="source"]').boundingBox(),b=await nodes.nth(to).locator('ax-handle[type="target"]').boundingBox();assert.ok(a&&b);await page.mouse.move(a.x+a.width/2,a.y+a.height/2);await page.mouse.down();await page.mouse.move(b.x+b.width/2,b.y+b.height/2,{steps:12});await page.mouse.up();};
  await connect(0,1);await page.waitForFunction(()=>document.querySelector('ax-session-team').draft.dependencies.length===1);await connect(1,0);await page.waitForFunction(()=>document.querySelector('ax-session-team').draft.dependencies.length===2);
  await review(page);const edit=calls.find(call=>call.suffix==='/preview').body;assert.equal(edit.dependencies.length,2);assert.equal(edit.slots[1].name,'Security reviewer');assert.equal(edit.slots[1].model,'review-model');
  await page.getByRole('button',{name:'Undo',exact:true}).click();assert.equal(await team.evaluate(element=>element.draft.dependencies.length),1);await page.getByRole('button',{name:'View',exact:true}).click();assert.equal(await nodes.nth(0).getAttribute('draggable'),'false');assert.deepEqual(errors,[]);
 }finally{await context.close();}
});
test('Session team lost Apply reply survives reload with exact retry',async()=>{const{page,context,calls}=await fixture({loseReply:true});try{await enterBudget(page);await review(page);await page.getByRole('button',{name:'Apply to this Session',exact:true}).click();await page.getByText('The exact Apply is retained.',{exact:false}).waitFor();const first=calls.find(call=>call.suffix==='/apply').body;await page.reload();await page.getByRole('button',{name:'Team and budget',exact:true}).click();await page.getByText('An Apply reply was not received.',{exact:false}).waitFor();await page.getByRole('button',{name:'Edit',exact:true}).click();await page.getByText('Resolve the pending Apply before editing this team.',{exact:false}).waitFor();await page.getByRole('button',{name:'Apply to this Session',exact:true}).click();await page.getByText('Saved Session configuration 1.',{exact:false}).waitFor();assert.deepEqual(calls.filter(call=>call.suffix==='/apply').map(call=>call.body),[first,first]);}finally{await context.close();}});

test('Coordinator helper approval requires explicit helper limits and graph bounds',async()=>{const{page,context,calls,errors}=await fixture({coordinator:true});try{
 await enterBudget(page);await page.getByRole('heading',{name:'Helpers this Agent may delegate to',exact:true}).waitFor();
 await page.getByRole('button',{name:'Preview changes',exact:true}).click();await page.getByText('Select at least one helper and explicit graph bounds for QA reviewer',{exact:false}).waitFor();
 await page.getByLabel('Let this Agent delegate to helpers',{exact:true}).check();
 await page.getByLabel('Maximum Agents in the turn, helpers included',{exact:true}).fill('4');await page.getByLabel('Maximum connections in the turn graph',{exact:true}).fill('3');
 await page.getByLabel('Use helper: Worker reviewer',{exact:true}).check();assert.equal(await page.getByLabel('Worker reviewer: Activation limit',{exact:true}).inputValue(),'');
 await page.getByRole('button',{name:'Preview changes',exact:true}).click();await page.getByText('Enter explicit limits within QA reviewer',{exact:false}).waitFor();assert.equal(calls.filter(call=>call.suffix==='/preview').length,0);
 for(const[label,value]of [['Activation limit','1'],['Invocation limit','4'],['Token limit','8000'],['Cost limit (USD)','0'],['Maximum output tokens per request','128']])await page.getByLabel(`Worker reviewer: ${label}`,{exact:true}).fill(value);
 assert.equal(await page.getByText('ad hoc',{exact:false}).count(),0);assert.equal(await page.getByLabel('Add work from approved templates',{exact:true}).count(),0);
 await review(page);
 const policy=calls.find(call=>call.suffix==='/preview').body.slots[0].delegation;assert.deepEqual(policy,{max_nodes:4,max_edges:3,operations:['add_agent'],workers:[{template_id:'worker',limits:{activations:1,invocations:4,tokens:8000,cost_microunits:0},max_output_tokens:128,adhoc_allowed:false}]});
 await page.getByText('may delegate to helpers: at most 4 Agents and 3 connections in the turn',{exact:false}).waitFor();await page.getByText('Helper worker: 1 activations, 4 invocations, 8000 tokens',{exact:false}).waitFor();assert.deepEqual(errors,[]);
 }finally{await context.close();}});

test('An Autonomous Agent may approve helpers and a Worker slot is not offered delegation',async()=>{
 {const{page,context,calls,errors}=await fixture({helper:true,toolless:true});try{
  await enterBudget(page);await page.getByRole('heading',{name:'Helpers this Agent may delegate to',exact:true}).waitFor();
  await page.getByLabel('Let this Agent delegate to helpers',{exact:true}).check();
  await page.getByLabel('Maximum Agents in the turn, helpers included',{exact:true}).fill('3');await page.getByLabel('Maximum connections in the turn graph',{exact:true}).fill('0');
  await page.getByLabel('Use helper: Worker reviewer',{exact:true}).check();
  for(const[label,value]of [['Activation limit','1'],['Invocation limit','2'],['Token limit','4000'],['Cost limit (USD)','0'],['Maximum output tokens per request','64']])await page.getByLabel(`Worker reviewer: ${label}`,{exact:true}).fill(value);
  await review(page);const slot=calls.find(call=>call.suffix==='/preview').body.slots[0];assert.equal(slot.role,undefined);
  assert.doesNotMatch(await page.locator('ax-session-team').locator('.review').textContent(),/has no tools/,'an Agent that delegates is not only answering');
  assert.deepEqual(slot.delegation,{max_nodes:3,max_edges:0,operations:['add_agent'],workers:[{template_id:'worker',limits:{activations:1,invocations:2,tokens:4000,cost_microunits:0},max_output_tokens:64,adhoc_allowed:false}]});assert.deepEqual(errors,[]);
 }finally{await context.close();}}
 {const{page,context,errors}=await fixture({role:'worker'});try{
  await enterBudget(page);assert.equal(await page.getByRole('heading',{name:'Helpers this Agent may delegate to',exact:true}).count(),0);assert.equal(await page.getByLabel('Let this Agent delegate to helpers',{exact:true}).count(),0);assert.deepEqual(errors,[]);
 }finally{await context.close();}}
});

 test('Legacy Team read leaves its existing workflow available without offering native approval',async()=>{const{page,context,calls,errors}=await fixture({legacy:true});try{assert.equal(await page.getByRole('button',{name:'Edit',exact:true}).isDisabled(),true);assert.equal(await page.getByRole('button',{name:'Preview changes',exact:true}).isDisabled(),true);assert.equal(await page.getByRole('button',{name:'Apply to this Session',exact:true}).isDisabled(),true);assert.equal(await page.locator('ax-node').count(),0);assert.equal(await page.locator('ax-session-team').evaluate(element=>element.draft),null);assert.ok(calls.every(call=>call.body===null));assert.deepEqual(errors,[]);}finally{await context.close();}});


test('Changing the selected Agent template keeps its editable configuration visible',async()=>{
 const{page,context,view,calls,errors}=await fixture({approved:true});try{
  await page.getByRole('button',{name:'Edit',exact:true}).click();
  await page.locator('ax-node').first().click();
  await page.locator('ax-session-team .panel select').first().selectOption('reviewer');
  await page.getByLabel('Model',{exact:true}).fill('reviewed-template-model');
  await page.getByLabel('Maximum output tokens per request',{exact:true}).fill('256');
  await review(page);const edit=calls.find(call=>call.suffix==='/preview').body;
  assert.equal(edit.slots[0].template_id,'reviewer');assert.equal(edit.slots[0].model,'reviewed-template-model');assert.equal(edit.slots[0].max_output_tokens,256);assert.deepEqual(edit.slots[0].limits,view.slots[0].limits);assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('Read-only toggle sends empty writes, and the review says what each Agent may change',async()=>{
 const{page,context,calls,errors}=await fixture();try{
  const team=page.locator('ax-session-team'),mode=page.getByLabel('May change',{exact:true}),paths=page.getByLabel('Paths it may change, one per line (for example lib/ or docs/*.md)',{exact:true});
  await enterBudget(page);assert.equal(await mode.inputValue(),'any');assert.equal(await paths.isVisible(),false);
  await mode.selectOption('none');assert.deepEqual(await team.evaluate(element=>element.draft.slots[0].writes),[]);
  await review(page);let preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;assert.deepEqual(preview.slots[0].writes,[]);
  await page.getByText('may change: nothing (read-only; write tools withheld).',{exact:false}).waitFor();
  // Only these paths with nothing entered is not silently read-only.
  await mode.selectOption('paths');assert.equal(await paths.isVisible(),true);
  await page.getByRole('button',{name:'Preview changes',exact:true}).click();await page.getByText('Enter at least one path QA reviewer <literal> may change',{exact:false}).waitFor();
  await paths.fill('lib/\n  docs/*.md \n\n');await review(page);preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;assert.deepEqual(preview.slots[0].writes,['lib/','docs/*.md']);
  await page.getByText('may change: lib/, docs/*.md.',{exact:false}).waitFor();
  await mode.selectOption('any');await review(page);preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;assert.equal(preview.slots[0].writes,null);
  await page.getByText('may change: any file.',{exact:false}).waitFor();assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('The review says an Agent with no tools can only answer from the conversation',async()=>{
 {const{page,context,errors}=await fixture({toolless:true});try{
  await enterBudget(page);await review(page);
  assert.equal(await page.locator('ax-session-team').locator('.review li').first().textContent(),'QA reviewer <literal> has no tools: it can only answer from the conversation.');
  await page.getByText('tools: none;',{exact:false}).waitFor();assert.deepEqual(errors,[]);
 }finally{await context.close();}}
 {const{page,context,errors}=await fixture();try{
  await enterBudget(page);await review(page);assert.doesNotMatch(await page.locator('ax-session-team').locator('.review').textContent(),/has no tools/);assert.deepEqual(errors,[]);
 }finally{await context.close();}}
});

test('The detected check is only offered: nothing runs it until the person adds it, and the review says what a failure means',async()=>{
 const{page,context,calls,errors}=await fixture({suggested:true});try{
  const team=page.locator('ax-session-team'),lines=page.getByLabel('Required checks, one command per line',{exact:true}),detected=page.getByRole('button',{name:'Add detected: npm test',exact:true});
  await enterBudget(page);await team.locator('.checks summary').click();
  assert.equal(await team.locator('.checks summary').textContent(),'Required checks: none');
  assert.equal(await lines.inputValue(),'','the detected command is not prefilled as a check');assert.equal(await detected.isVisible(),true);
  await review(page);let preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;assert.deepEqual(preview.required_checks,[]);
  assert.doesNotMatch(await team.locator('.review').textContent(),/Required check/);
  await detected.click();assert.equal(await lines.inputValue(),'npm test');assert.equal(await detected.isVisible(),false);
  assert.equal(await team.locator('.checks summary').textContent(),'Required checks (1)');
  await lines.fill('npm test\n  cargo test --quiet \n\n');
  await review(page);preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;
  assert.deepEqual(preview.required_checks,[['sh','-c','npm test'],['sh','-c','cargo test --quiet']]);
  const listed=await team.locator('.review').textContent();
  assert.match(listed,/Required check after each turn: npm test/);assert.match(listed,/Required check after each turn: cargo test --quiet/);
  assert.match(listed,/A failure leaves the turn needing attention/);
  await page.getByRole('button',{name:'Apply to this Session',exact:true}).click();await page.getByText('Saved Session configuration 1.',{exact:false}).waitFor();
  assert.deepEqual(calls.find(call=>call.suffix==='/apply').body.edit.required_checks,preview.required_checks);assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('Checks the person did not edit keep their exact arguments; a changed line runs through sh',async()=>{
 const{page,context,calls,errors}=await fixture({approved:true,checks:[['pytest','-k','not slow'],['sh','-c','npm test'],['echo',"it's"]]});try{
  const team=page.locator('ax-session-team'),lines=page.getByLabel('Required checks, one command per line',{exact:true});
  await page.getByRole('button',{name:'Edit',exact:true}).click();await team.locator('.checks summary').click();
  // An argv shows as a shell would read it, never as words it would split.
  assert.equal(await lines.inputValue(),"pytest -k 'not slow'\nnpm test\necho 'it'\\''s'");
  await lines.fill("pytest -k 'not slow'\nnpm test -- --ci\necho 'it'\\''s'\ncargo test");
  await review(page);const preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;
  assert.deepEqual(preview.required_checks,[['pytest','-k','not slow'],['sh','-c','npm test -- --ci'],['echo',"it's"],['sh','-c','cargo test']]);
  const listed=await team.locator('.review').textContent();
  if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/session-team-checks.png`,fullPage:true});
  assert.match(listed,/Required check after each turn: pytest -k 'not slow'/);
  assert.match(listed,/Required checks do not run on attempts made with Explore several ways\. Run them with Run checks before you keep one\./);
  assert.match(await team.locator('.checks p').first().textContent(),/They do not run on attempts made with Explore several ways/);
  assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('A new Session drafts its proposed read-only helpers; the person enters their limits or removes them',async()=>{
 {const{page,context,calls,errors}=await fixture({proposal:true});try{
  await page.getByText('QA reviewer <literal> may delegate to the read-only helpers Scout and Critic: enter their limits too, or clear Let this Agent delegate to helpers.',{exact:false}).waitFor();
  await enterBudget(page);
  assert.equal(await page.getByLabel('Let this Agent delegate to helpers',{exact:true}).isChecked(),true);
  assert.equal(await page.getByLabel('Maximum Agents in the turn, helpers included',{exact:true}).inputValue(),'6');
  assert.equal(await page.getByLabel('Maximum connections in the turn graph',{exact:true}).inputValue(),'5');
  for(const name of ['Scout','Critic']){
   assert.equal(await page.getByLabel(`Use helper: ${name}`,{exact:true}).isChecked(),true);
   assert.equal(await page.getByLabel(`${name}: Activation limit`,{exact:true}).inputValue(),'','helper limits are never prefilled');
   assert.equal(await page.getByLabel(`${name}: Maximum output tokens per request`,{exact:true}).inputValue(),'256');
  }
  await page.getByRole('button',{name:'Preview changes',exact:true}).click();await page.getByText('Enter explicit limits within QA reviewer',{exact:false}).waitFor();
  assert.equal(calls.filter(call=>call.suffix==='/preview').length,0);
  for(const name of ['Scout','Critic'])for(const[label,value]of [['Activation limit','1'],['Invocation limit','4'],['Token limit','8000'],['Cost limit (USD)','0']])await page.getByLabel(`${name}: ${label}`,{exact:true}).fill(value);
  await review(page);
  const worker=template_id=>({template_id,limits:{activations:1,invocations:4,tokens:8000,cost_microunits:0},max_output_tokens:256,adhoc_allowed:false});
  assert.deepEqual(calls.find(call=>call.suffix==='/preview').body.slots[0].delegation,{workers:[worker('scout'),worker('critic')],operations:['add_agent'],max_nodes:6,max_edges:5});
  await page.getByText('may delegate to helpers: at most 6 Agents and 5 connections in the turn',{exact:false}).waitFor();assert.deepEqual(errors,[]);
 }finally{await context.close();}}
 {const{page,context,calls,errors}=await fixture({proposal:true});try{
  await enterBudget(page);await page.getByLabel('Let this Agent delegate to helpers',{exact:true}).uncheck();
  await review(page);assert.equal(calls.find(call=>call.suffix==='/preview').body.slots[0].delegation,null);assert.deepEqual(errors,[]);
 }finally{await context.close();}}
});

test('Add Agent adds the template chosen beside it',async()=>{
 const{page,context,errors}=await fixture({approved:true,helper:true});try{
  const picker=page.getByLabel('Agent template to add',{exact:true});assert.equal(await picker.isDisabled(),true);
  await page.getByRole('button',{name:'Edit',exact:true}).click();assert.equal(await picker.isDisabled(),false);
  assert.deepEqual(await picker.locator('option').allTextContents(),['QA reviewer <literal> · ollama','Worker reviewer · ollama']);
  await picker.selectOption('worker');await page.getByRole('button',{name:'Add Agent',exact:true}).click();
  const added=await page.locator('ax-session-team').evaluate(element=>element.draft.slots[1]);
  assert.equal(added.template_id,'worker');assert.equal(added.name,'Worker reviewer');assert.notEqual(added.slot_id,'worker');assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('On a real daemon, a new Session on the lead drafts its read-only Worker helpers by name',async()=>{
 const daemon=await launchTestDaemon({nativeDataRoot:true,agentTools:['read_file','bash'],
  helpers:[{id:'scout',name:'Scout',tools:['read_file','grep','bash'],writes:[]},{id:'reviewer',name:'Reviewer',tools:['read_file','bash'],writes:[]}]});
 const context=await browser.newContext({viewport:{width:1100,height:820}}),page=await context.newPage(),errors=[];page.on('pageerror',error=>errors.push(error.message));
 try{
  const session=daemon.fixtures.alpha.sessions[0].id;
  await page.route('**/team-fixture',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html><head><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-session-team session="${session}"></ax-session-team><script type="module" src="/ui/session-team.js"></script></body></html>`}));
  await page.goto(`${daemon.baseUrl}/team-fixture`);await page.getByRole('button',{name:'Team and budget',exact:true}).click();
  await page.getByText('Browser Test Coder may delegate to the read-only helpers Scout and Reviewer',{exact:false}).waitFor();
  await page.getByRole('button',{name:'Edit',exact:true}).click();
  for(const name of ['Scout','Reviewer']){
   assert.equal(await page.getByLabel(`Use helper: ${name}`,{exact:true}).isChecked(),true);
   await page.getByText(`${name} · browser-test-model`,{exact:true}).waitFor();
  }
  assert.equal(await page.getByText('Previously approved definition',{exact:false}).count(),0);
  assert.equal(await page.getByText('Add a Worker in Settings',{exact:false}).count(),0);
  assert.equal(await page.getByLabel('Maximum Agents in the turn, helpers included',{exact:true}).inputValue(),'6');
  assert.deepEqual(errors,[]);
 }finally{await context.close();await daemon.stop();}
});

test('A required review names a read-only Worker, its rounds and its budget, and the review says what it enforces',async()=>{
 const{page,context,calls,errors}=await fixture({helper:true,reviewers:['worker']});try{
  const team=page.locator('ax-session-team'),reviewer=page.getByLabel('Required reviewer',{exact:true}),rounds=page.getByLabel('Review rounds',{exact:true});
  await enterBudget(page);await team.locator('.review-setting summary').click();
  assert.equal(await team.locator('.review-setting summary').textContent(),'Required review: none');
  assert.deepEqual(await reviewer.locator('option').allTextContents(),['No required review','Worker reviewer']);
  assert.equal(await rounds.isDisabled(),true,'rounds need a reviewer first');
  await review(page);let preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;assert.equal('required_review' in preview,false,'no review keeps the edit unchanged');
  await reviewer.selectOption('worker');
  assert.equal(await team.locator('.review-setting summary').textContent(),'Required review by Worker reviewer · up to 2 rounds');
  assert.equal(await rounds.inputValue(),'2');assert.equal(await page.getByLabel('Reviewer invocation limit',{exact:true}).inputValue(),'6');
  await page.getByLabel('Reviewer token limit',{exact:true}).fill('40000');
  await review(page);preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;
  assert.deepEqual(preview.required_review,{template_id:'worker',max_rounds:2,limits:{activations:2,invocations:6,tokens:40000,cost_microunits:0},max_output_tokens:4096});
  const listed=await team.locator('.review').textContent();
  assert.match(listed,/Required review by Worker reviewer after each turn: up to 2 rounds; reviewer budget per turn 2 activations, 6 invocations, 40000 tokens, \$0\./);
  assert.match(listed,/The turn completes only when the reviewer approves the exact result\./);
  if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:`${process.env.AXOCOATL_P1_SCREENSHOT_DIR}/session-team-review.png`,fullPage:true});
  await rounds.fill('4');await page.getByRole('button',{name:'Preview changes',exact:true}).click();
  await page.getByText('Choose 1 to 3 review rounds.',{exact:true}).waitFor();
  await rounds.fill('1');assert.equal(await team.locator('.review-setting summary').textContent(),'Required review by Worker reviewer · up to 1 round');
  await reviewer.selectOption('');await review(page);preview=calls.filter(call=>call.suffix==='/preview').at(-1).body;assert.equal('required_review' in preview,false);
  assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('A saved required review is shown and kept exactly in the next edit',async()=>{
 const saved={template_id:'worker',max_rounds:3,limits:{activations:3,invocations:9,tokens:50000,cost_microunits:0}};
 const{page,context,calls,errors}=await fixture({approved:true,helper:true,reviewers:['worker'],requiredReview:saved});try{
  const team=page.locator('ax-session-team');
  assert.equal(await team.locator('.review-setting summary').textContent(),'Required review by Worker reviewer · up to 3 rounds');
  assert.equal(await page.getByLabel('Required reviewer',{exact:true}).isDisabled(),true,'View mode changes nothing');
  await page.getByRole('button',{name:'Edit',exact:true}).click();await review(page);
  assert.deepEqual(calls.filter(call=>call.suffix==='/preview').at(-1).body.required_review,saved);assert.deepEqual(errors,[]);
 }finally{await context.close();}
});
