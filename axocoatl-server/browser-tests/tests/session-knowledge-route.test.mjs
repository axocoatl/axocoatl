import assert from 'node:assert/strict';
import {after,before,test} from 'node:test';
import {chromium} from 'playwright';
import {launchTestDaemon,resolveChromiumExecutable} from '../support/daemon.mjs';

let runtime,browser;
before(async()=>{runtime=await launchTestDaemon({nativeDataRoot:true});const executablePath=await resolveChromiumExecutable();browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});});
after(async()=>{await browser?.close();await runtime?.stop();});
async function call(session,suffix='',body,method=body?'POST':'GET'){
 const response=await fetch(`${runtime.baseUrl}/api/sessions/${encodeURIComponent(session)}/knowledge${suffix}`,{method,headers:body?{'content-type':'application/json'}:{},body:body?JSON.stringify(body):undefined});const text=await response.text();let value;try{value=JSON.parse(text);}catch{value=text;}return{status:response.status,value,headers:response.headers};
}

test('Knowledge HTTP preserves workspace ownership, exact revisions and immutable chat attachment bytes',async()=>{
 const id=runtime.fixtures.alpha.sessions[0].id,sibling=runtime.fixtures.alpha.sessions[1].id,foreign=runtime.fixtures.beta.sessions[0].id;
 const team=await(await fetch(`${runtime.baseUrl}/api/sessions/${id}/team`)).json();assert.equal(team.history_version,'execution_v2');
 const first=await call(id);assert.equal(first.status,200,JSON.stringify(first.value));assert.equal(first.value.workspace_id,runtime.fixtures.alpha.workspace.id);
 const edit={id:'route-knowledge',expected_revision:0,title:'Retained parser decision',body:'# Exact knowledge\n\nKeep the original boundary.',kind:'decision',links:[],sources:[]};
 const created=await call(id,'',edit);assert.equal(created.status,200,JSON.stringify(created.value));assert.equal(created.value.revision,1);assert.equal(created.value.provenance.kind,'human');
 assert.ok((await call(sibling)).value.notes.some(note=>note.id===edit.id));assert.ok(!(await call(foreign)).value.notes.some(note=>note.id===edit.id));
 const attachment=await call(id,'/route-knowledge/attach',{expected_revision:1});assert.equal(attachment.status,200,JSON.stringify(attachment.value));assert.equal(attachment.value.session_id,id);assert.equal(attachment.value.scope,'this_turn');assert.equal(attachment.value.active,true);
 const contentUrl=`${runtime.baseUrl}/api/sessions/${id}/attachments/${attachment.value.reference_id}/content`;
 const originalBytes=await(await fetch(contentUrl)).text();assert.match(originalBytes,/Keep the original boundary/);
 const updated=await call(id,'/route-knowledge',{...edit,expected_revision:1,body:'The reviewed second revision.'},'PUT');assert.equal(updated.status,200,JSON.stringify(updated.value));assert.equal(updated.value.revision,2);
 assert.ok((await call(id,'/route-knowledge',{...edit,expected_revision:1,body:'A stale overwrite'},'PUT')).status>=400);const historical=await call(id,'/route-knowledge/attach',{expected_revision:1});assert.equal(historical.status,200);assert.equal(historical.value.metadata.knowledge_revision,1);assert.equal(await(await fetch(`${runtime.baseUrl}/api/sessions/${id}/attachments/${historical.value.reference_id}/content`)).text(),originalBytes);assert.ok((await call(id,'/route-knowledge/attach',{expected_revision:999})).status>=400);assert.ok((await call(foreign,'/route-knowledge/attach',{expected_revision:2})).status>=400);
 assert.equal(await(await fetch(contentUrl)).text(),originalBytes,'later note revisions cannot replace the captured attachment');
 const exportResult=await call(id,'/export');assert.equal(exportResult.status,200);assert.match(exportResult.headers.get('content-type'),/text\/markdown/);assert.match(exportResult.headers.get('content-disposition'),/attachment/);assert.match(exportResult.value,/The reviewed second revision/);
 const singleExport=await call(id,'/export?note_id=route-knowledge');assert.equal(singleExport.status,200);const preview=await call(id,'/import-preview',{markdown:singleExport.value});assert.equal(preview.status,200);assert.equal(preview.value.id,'route-knowledge');assert.equal(preview.value.expected_revision,2);assert.equal(preview.value.body,'The reviewed second revision.');assert.equal((await call(id)).value.notes.find(note=>note.id==='route-knowledge').revision,2,'preview cannot save');
 const invalidRoute=await call(id,'/different-note',{...edit,expected_revision:2},'PUT');assert.equal(invalidRoute.status,400);
});

test('visible Session Knowledge creates a note and attaches its exact revision without starting work',async()=>{
 const context=await browser.newContext({viewport:{width:1280,height:900},reducedMotion:'reduce'}),page=await context.newPage(),errors=[],mutations=[];
 page.on('pageerror',error=>errors.push(error.message));page.on('request',request=>{if(request.method()==='POST')mutations.push(new URL(request.url()).pathname);});
 try{
  await page.goto(`${runtime.baseUrl}/?session=${encodeURIComponent(runtime.fixtures.alpha.sessions[0].id)}`);
  const panel=page.locator('ax-session-knowledge');await page.waitForFunction(id=>document.querySelector('ax-session-knowledge')?.getAttribute('session')===id,runtime.fixtures.alpha.sessions[0].id);
  await panel.getByRole('button',{name:'Knowledge',exact:true}).click();await panel.getByRole('button',{name:'New note',exact:true}).click();await panel.getByLabel('Title',{exact:true}).fill('Visible browser decision');await panel.getByLabel('Markdown',{exact:true}).fill('Retain this exact browser-created note.');await panel.getByRole('button',{name:'Save note',exact:true}).click();await panel.getByRole('heading',{name:'Visible browser decision',exact:true}).waitFor();
  await panel.getByRole('button',{name:'Investigate in chat',exact:true}).click();await page.waitForFunction(()=>document.querySelector('#session-text').value.includes('Visible browser decision'));
  assert.match(await page.locator('#session-text').inputValue(),/revision 1/);await page.locator('ax-session-context .chip').last().waitFor();const refs=await(await fetch(`${runtime.baseUrl}/api/sessions/${runtime.fixtures.alpha.sessions[0].id}/attachments`)).json();assert.ok(refs.some(ref=>ref.active&&ref.metadata?.knowledge_revision===1));
  assert.equal(new URL(page.url()).pathname,'/');assert.ok(mutations.every(path=>!path.endsWith('/execute')&&!path.endsWith('/tasks')));assert.deepEqual(errors,[]);
 }finally{await context.close();}
});
