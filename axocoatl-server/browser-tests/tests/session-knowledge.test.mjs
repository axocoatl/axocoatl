import assert from 'node:assert/strict';
import {after,before,test} from 'node:test';
import {chromium} from 'playwright';
import {mkdir} from 'node:fs/promises';
import path from 'node:path';
import {launchTestDaemon,resolveChromiumExecutable} from '../support/daemon.mjs';

let runtime,browser;
before(async()=>{runtime=process.env.AXOCOATL_COMPONENT_BASE_URL?{baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}}:await launchTestDaemon();const executablePath=await resolveChromiumExecutable();browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});});
after(async()=>{await browser?.close();await runtime?.stop();});
const hash='a'.repeat(64);
const activation={session_id:'session',turn_id:'turn-recorded',execution_epoch_id:'epoch',node_id:'reviewer',generation:2,activation_id:'activation-exact'};
function initialView(){return{workspace_id:'workspace',notes:[{id:'decision',revision:3,title:'Parser contract <literal>',body:'# Keep bytes exact\n\n<script>window.injection=true</script>',kind:'decision',freshness:'stale',links:[{kind:'supports',target:'architecture'}],sources:[{path:'src/parser.py',sha256:hash,symbol:'parse'}],provenance:{kind:'observed',journal_id:'journal',activation,evidence:['exact-check']},backlinks:[{id:'architecture',title:'Parser architecture',kind:'related'}]},{id:'architecture',revision:1,title:'Parser architecture',body:'Module boundaries.',kind:'architecture',freshness:'current',links:[],sources:[],provenance:{kind:'human'},backlinks:[]}],proposals:[{id:'proposal',status:'pending',expected_revision:3,note:{id:'decision',title:'Proposed parser contract',body:'Retain the empty-frame boundary.',kind:'finding',links:[{kind:'supports',target:'architecture'}],sources:[{path:'src/parser.py',sha256:hash}],provenance:{kind:'model',journal_id:'journal',activation}}}],code_index:{status:'current',files:1,symbols:1,entries:[{path:'src/parser.py',sha256:hash,symbols:[{name:'parse',kind:'function',line:7}]}]}};}
async function fixture({theme='light',narrow=false,conflict=false,readError=false,delayGraph=false}={}){
 const context=await browser.newContext({viewport:narrow?{width:390,height:844}:{width:1280,height:900},colorScheme:theme,reducedMotion:'reduce'}),page=await context.newPage(),calls=[],errors=[],view=initialView();page.on('pageerror',error=>errors.push(error.message));
 let releaseGraph;const graphGate=new Promise(resolve=>{releaseGraph=resolve;});let graphRequests=0;
 if(delayGraph)await page.route('**/lattice/index.js',async route=>{graphRequests++;await graphGate;await route.continue();});
 await page.route('**/knowledge-fixture',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="${theme}"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"><ax-session-knowledge session="session"></ax-session-knowledge><script type="module" src="/ui/session-knowledge.js"></script></html>`}));
 await page.route('**/api/sessions/*/knowledge**',async route=>{
  const request=route.request(),url=new URL(request.url()),suffix=url.pathname.split('/knowledge')[1],body=request.method()==='GET'?null:request.postDataJSON();calls.push({suffix,body,method:request.method(),session:url.pathname.split('/')[3],query:url.searchParams.get('q')});
  if(request.method()==='GET'){if(readError)return route.fulfill({status:503,json:{error:'Knowledge storage unavailable'}});return route.fulfill({json:view});}
  if(suffix.endsWith('/attach'))return route.fulfill({json:{id:'attachment-exact',scope:'this_turn'}});
  if(suffix==='/import-preview')return route.fulfill({json:{id:'decision',expected_revision:3,title:'Imported decision',body:'Reviewed Markdown edit.',kind:'decision',links:[],sources:[]}});
  if(suffix==='/index')return route.fulfill({json:view});
  if(suffix.startsWith('/proposals/')){if(conflict)return route.fulfill({status:409,json:{error:'Knowledge revision changed'}});view.proposals[0].status=suffix.endsWith('/accept')?'published':'rejected';return route.fulfill({json:view});}
  if(conflict)return route.fulfill({status:409,json:{error:'Knowledge revision changed'}});
  const saved={...body,revision:body.expected_revision+1,freshness:'unverified',provenance:{kind:'human'},backlinks:[]};delete saved.expected_revision;view.notes=[saved,...view.notes.filter(note=>note.id!==saved.id)];return route.fulfill({json:saved});
 });
 await page.goto(`${runtime.baseUrl}/knowledge-fixture`);await page.evaluate(()=>{window.knowledgeEvents=[];for(const name of ['knowledge-attached','knowledge-open-source','knowledge-open-evidence'])document.addEventListener(name,event=>window.knowledgeEvents.push({name,detail:event.detail}));});await page.getByRole('button',{name:'Knowledge',exact:true}).click();
 await page.getByText(readError?'Knowledge storage unavailable':'Parser contract <literal>',{exact:true}).first().waitFor();
 if(process.env.AXOCOATL_KNOWLEDGE_SCREENSHOTS&&!readError){await mkdir(process.env.AXOCOATL_KNOWLEDGE_SCREENSHOTS,{recursive:true});await page.screenshot({path:path.join(process.env.AXOCOATL_KNOWLEDGE_SCREENSHOTS,`knowledge-${theme}-${narrow?'narrow':'desktop'}.png`),fullPage:true});}
 return{context,page,calls,errors,view,releaseGraph,graphRequests:()=>graphRequests};
}

for(const [theme,narrow] of [['light',false],['dark',false],['dark',true]])test(`Knowledge ${theme}${narrow?' narrow':''}: edit exact revisions and preserve typed sources`,async()=>{
 const{context,page,calls,errors}=await fixture({theme,narrow});try{
  assert.equal(await page.evaluate(()=>window.injection),undefined);await page.getByText('A recorded source has changed.',{exact:false}).waitFor();
  await page.getByRole('button',{name:'Edit note',exact:true}).click();await page.getByLabel('Markdown',{exact:true}).fill('# Revised decision\nPreserve exact boundaries.');await page.getByRole('button',{name:'Add link',exact:true}).click();await page.getByLabel('Link 2 kind',{exact:true}).selectOption('depends_on');await page.getByLabel('Link 2 target',{exact:true}).fill('architecture');await page.getByRole('button',{name:'Save note',exact:true}).click();await page.locator('.badge').filter({hasText:'decision · revision 4 · unverified'}).waitFor();
  const save=calls.find(call=>call.method==='PUT');assert.equal(save.body.expected_revision,3);assert.equal(save.body.id,'decision');assert.equal(save.body.body,'# Revised decision\nPreserve exact boundaries.');assert.deepEqual(save.body.sources,[{path:'src/parser.py',sha256:hash,symbol:'parse'}]);assert.deepEqual(save.body.links[1],{kind:'depends_on',target:'architecture'});
  await page.getByRole('button',{name:'New note',exact:true}).click();await page.getByLabel('Title',{exact:true}).fill('Transport observation');await page.getByLabel('Kind',{exact:true}).selectOption('finding');await page.getByLabel('Markdown',{exact:true}).fill('Retain receipt IDs.');await page.getByRole('button',{name:'Add source',exact:true}).click();await page.getByLabel('Source 1 path',{exact:true}).fill('src/parser.py');await page.getByLabel('Source 1 path',{exact:true}).blur();assert.equal(await page.getByLabel('Source 1 SHA-256',{exact:true}).inputValue(),hash);await page.getByRole('button',{name:'Save note',exact:true}).click();await page.getByRole('heading',{name:'Transport observation',exact:true}).waitFor();
  const created=calls.find(call=>call.method==='POST'&&call.suffix==='').body;assert.equal(created.expected_revision,0);assert.match(created.id,/^note-/);assert.equal(created.kind,'finding');
  const geometry=await page.locator('ax-session-knowledge').evaluate(element=>{const dialog=element.shadowRoot.querySelector('dialog');return{left:dialog.getBoundingClientRect().left,right:dialog.getBoundingClientRect().right,width:innerWidth,overflow:dialog.scrollWidth>dialog.clientWidth,background:getComputedStyle(dialog).backgroundColor,motion:matchMedia('(prefers-reduced-motion: reduce)').matches};});assert.ok(geometry.left>=0&&geometry.right<=geometry.width);assert.equal(geometry.overflow,false);assert.notEqual(geometry.background,'rgba(0, 0, 0, 0)');assert.equal(geometry.motion,true);assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('Knowledge graph, backlinks, code search and exact source navigation remain in the Session',async()=>{
 const{context,page,calls,errors}=await fixture();try{
  await page.getByRole('button',{name:'Graph',exact:true}).click();await page.locator('ax-lattice ax-node').first().waitFor();assert.equal(await page.locator('ax-lattice ax-node').count(),4);assert.equal(await page.locator('ax-lattice ax-edge').count(),3);assert.equal(await page.locator('ax-lattice').getAttribute('mode'),'view');await page.locator('ax-lattice').focus();await page.keyboard.press('Home');await page.keyboard.press('Enter');await page.getByRole('heading',{name:'Parser contract <literal>',exact:true}).waitFor();await page.getByRole('button',{name:'Graph',exact:true}).click();await page.locator('ax-node').filter({hasText:'Parser architecture'}).click();await page.getByRole('heading',{name:'Parser architecture',exact:true}).waitFor();
  await page.getByRole('button',{name:/Parser contract <literal>.*decision/}).click();await page.getByRole('button',{name:'Parser architecture',exact:true}).first().click();await page.getByRole('heading',{name:'Parser architecture',exact:true}).waitFor();
  await page.getByRole('button',{name:'Code index',exact:true}).click();await page.getByRole('button',{name:'Refresh code index',exact:true}).click();await page.getByLabel('Search knowledge and code',{exact:true}).fill('parse');await page.waitForTimeout(300);await page.getByRole('button',{name:'parse · function · line 7',exact:true}).click();const event=await page.evaluate(()=>window.knowledgeEvents.at(-1));assert.deepEqual(event,{name:'knowledge-open-source',detail:{session_id:'session',path:'src/parser.py',line:7}});assert.ok(calls.some(call=>call.suffix==='/index'));assert.ok(calls.some(call=>call.query==='parse'));assert.deepEqual(errors,[]);
 }finally{await context.close();}
});

test('Knowledge attachments capture selected revision and investigation prepares an editable prompt',async()=>{
 const{context,page,calls}=await fixture();try{
  await page.getByRole('button',{name:'Attach to chat',exact:true}).click();await page.waitForFunction(()=>window.knowledgeEvents.length===1);assert.deepEqual(calls.find(call=>call.suffix==='/decision/attach').body,{expected_revision:3});let event=await page.evaluate(()=>window.knowledgeEvents.at(-1));assert.equal(event.detail.instruction,null);assert.equal(event.detail.note.revision,3);
  await page.getByRole('button',{name:'Knowledge',exact:true}).click();await page.getByRole('button',{name:'Investigate in chat',exact:true}).click();await page.waitForFunction(()=>window.knowledgeEvents.length===2);event=await page.evaluate(()=>window.knowledgeEvents.at(-1));assert.match(event.detail.instruction,/revision 3/);assert.equal(calls.filter(call=>call.suffix.endsWith('/attach')).length,2);assert.ok(calls.every(call=>!call.suffix.includes('execute')));
  await page.getByRole('button',{name:'Knowledge',exact:true}).click();await page.getByRole('button',{name:'Inspect source turn · reviewer · generation 2',exact:true}).click();event=await page.evaluate(()=>window.knowledgeEvents.at(-1));assert.deepEqual(event.detail.activation,activation);
 }finally{await context.close();}
});

test('Knowledge proposal decisions preserve expected revision and never imply acceptance on conflict',async()=>{
 const{context,page,calls,view}=await fixture({conflict:true});try{
  await page.getByRole('button',{name:'Proposals (1)',exact:true}).click();await page.getByText('Retain the empty-frame boundary.',{exact:true}).waitFor();await page.getByRole('button',{name:'Accept proposal',exact:true}).click();await page.getByText('The recorded revision changed.',{exact:false}).waitFor();assert.deepEqual(calls.find(call=>call.suffix.endsWith('/accept')).body,{expected_revision:3});assert.equal(view.proposals[0].status,'pending');assert.equal(await page.getByRole('button',{name:'Accept proposal',exact:true}).isDisabled(),false);
 }finally{await context.close();}
});

test('Knowledge supports explicit rejection and save conflicts retain draft bytes',async()=>{
 const{context,page,view}=await fixture();try{await page.getByRole('button',{name:'Proposals (1)',exact:true}).click();await page.getByRole('button',{name:'Reject proposal',exact:true}).click();await page.getByText('rejected · finding · expected revision 3',{exact:true}).waitFor();assert.equal(view.proposals[0].status,'rejected');}finally{await context.close();}
 const failed=await fixture({conflict:true});try{await failed.page.getByRole('button',{name:'Edit note',exact:true}).click();await failed.page.getByLabel('Markdown',{exact:true}).fill('Unsaved exact draft');await failed.page.getByRole('button',{name:'Save note',exact:true}).click();await failed.page.getByText('The recorded revision changed.',{exact:false}).waitFor();assert.equal(await failed.page.getByLabel('Markdown',{exact:true}).inputValue(),'Unsaved exact draft');await failed.page.getByRole('button',{name:'Refresh',exact:true}).click();assert.equal(await failed.page.getByLabel('Markdown',{exact:true}).inputValue(),'Unsaved exact draft');}finally{await failed.context.close();}
});

test('Knowledge errors are distinct from an empty workspace and Session change clears old state',async()=>{
 const failed=await fixture({readError:true});try{assert.equal(await failed.page.getByText('No knowledge proposals are recorded.',{exact:true}).count(),0);assert.equal(await failed.page.getByRole('button',{name:'Refresh',exact:true}).isEnabled(),true);}finally{await failed.context.close();}
 const{context,page,calls}=await fixture();try{await page.getByRole('button',{name:'Edit note',exact:true}).click();await page.getByLabel('Markdown',{exact:true}).fill('Private draft');await page.locator('ax-session-knowledge').evaluate(element=>element.setAttribute('session','second-session'));assert.equal(await page.locator('dialog').isVisible(),false);await page.getByRole('button',{name:'Knowledge',exact:true}).click();await page.getByRole('heading',{name:'Parser contract <literal>',exact:true}).waitFor();assert.equal(await page.getByText('Private draft',{exact:true}).count(),0);assert.equal(calls.at(-1).session,'second-session');assert.ok(calls.every(call=>call.method==='GET'));}finally{await context.close();}
});

test('Markdown import validates a selected file and previews the exact current revision before Save',async()=>{
 const{context,page,calls}=await fixture();try{
  await page.getByLabel('Import knowledge Markdown',{exact:true}).setInputFiles({name:'decision.md',mimeType:'text/markdown',buffer:Buffer.from('---\nid: decision\n---\nReviewed Markdown edit.')});
  await page.getByText('Import preview ready.',{exact:false}).waitFor();assert.equal(await page.getByLabel('Title',{exact:true}).inputValue(),'Imported decision');assert.equal(await page.getByLabel('Markdown',{exact:true}).inputValue(),'Reviewed Markdown edit.');assert.equal(calls.filter(call=>call.method==='PUT').length,0);assert.equal(calls.filter(call=>call.suffix==='/import-preview').length,1);
  await page.getByRole('button',{name:'Save note',exact:true}).click();await page.getByRole('heading',{name:'Imported decision',exact:true}).waitFor();assert.equal(calls.find(call=>call.method==='PUT').body.expected_revision,3);
 }finally{await context.close();}
});


test('Knowledge loads without the graph bundle and ignores a late graph load after leaving its tab',async()=>{
 const{context,page,errors,releaseGraph,graphRequests}=await fixture({delayGraph:true});try{
  assert.equal(graphRequests(),0,'Notes must not acquire the optional graph bundle');
  await page.getByRole('button',{name:'Graph',exact:true}).click();await page.getByText('Loading knowledge graph…',{exact:true}).waitFor();
  await page.getByRole('button',{name:'Notes',exact:true}).click();await page.getByRole('heading',{name:'Parser contract <literal>',exact:true}).waitFor();releaseGraph();
  await page.waitForFunction(()=>customElements.get('ax-lattice'));
  assert.equal(await page.locator('ax-lattice').count(),0,'late import must not replace the selected Notes tab');
  await page.getByRole('button',{name:'Graph',exact:true}).click();await page.locator('ax-node').first().waitFor();assert.equal(await page.locator('ax-node').count(),4);assert.deepEqual(errors,[]);
 }finally{releaseGraph();await context.close();}
});
