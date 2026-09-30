import assert from 'node:assert/strict';
import {mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {after, before, test} from 'node:test';
import {chromium} from 'playwright';
import {launchTestDaemon, resolveChromiumExecutable} from '../support/daemon.mjs';

let runtime, browser, source, screenshots;
before(async()=>{screenshots=await mkdtemp(join(tmpdir(),'axocoatl-session-model-'));
  runtime=process.env.AXOCOATL_COMPONENT_BASE_URL ? {baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}} : await launchTestDaemon();
  source=await(await fetch(`${runtime.baseUrl}/${process.env.AXOCOATL_COMPONENT_BASE_URL?'index.html':''}`)).text();
  const executablePath=await resolveChromiumExecutable();
  browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});
});
after(async()=>{try{await browser?.close();await runtime?.stop();}finally{if(screenshots)await rm(screenshots,{recursive:true,force:true});}});
function slice(start,end){const at=source.indexOf(start);assert.ok(at>=0,start);const until=source.indexOf(end,at);assert.ok(until>at,end);return source.slice(at,until);}
async function fixture(theme='light'){
  const context=await browser.newContext({viewport:{width:theme==='dark'?390:1100,height:820},colorScheme:theme,reducedMotion:'reduce'}),page=await context.newPage(),errors=[],requests=[];
  page.on('pageerror',error=>errors.push(error.message));
  const team={history_version:'execution_v2',approved:false,slots:[{slot_id:'session-author',model:'reviewed-model'}]};
  await page.route('**/model-fixture',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="${theme}"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"><style>body{background:var(--bg);color:var(--text);padding:16px}.hide{display:none}</style><p>Session composer</p><ax-select id="session-model"></ax-select><ax-select id="session-target"></ax-select></html>`}));
  await page.route('**/api/**',route=>{requests.push(new URL(route.request().url()).pathname);return route.fulfill({json:route.request().url().includes('/team')?team:['other-legacy-model']});});
  await page.goto(`${runtime.baseUrl}/model-fixture`);
  await page.addScriptTag({content:`${slice('class AxSelect extends HTMLElement {','\nfunction renderPanesMenu(')}\n`});
  await page.addScriptTag({content:`
    window.$=selector=>document.querySelector(selector);
    window.S={session:{id:'session',mode:{kind:'single_agent',agent_id:'template'},currentTeam:null},agents:[{id:'template',model:'old-template-model',provider:'ollama'}]};
    window.sessionActiveAgentIds=()=>['template'];window.visibleSurfaces=()=>['trace'];window.graphRefreshes=0;window.sessionLatticeBuild=()=>window.graphRefreshes++;
    window.renderSessionActive=window.renderStatusPearls=window.syncSessionRoleCapabilities=window.syncSessionComposerState=()=>{};
    ${slice('let _sessionModelPickerReadId = 0;','let _sessionLatticeBuildId = 0;')}
    ${slice('async function refreshSessionTeamCapabilities(', 'function sessionSupportsAttempts(')}
  `});
  return{context,page,team,errors,requests};
}
for(const theme of ['light','dark'])test(`native composer uses approved Session models after Apply (${theme})`,async()=>{
  const {context,page,team,errors,requests}=await fixture(theme);try{
    await page.evaluate(()=>refreshSessionTeamCapabilities('session'));
    assert.match(await page.locator('#session-model .lbl').textContent(),/reviewed-model · review in Team and budget/);
    assert.equal(await page.locator('#session-model [role=combobox]').getAttribute('aria-disabled'),'true');
    team.approved=true;team.slots[0].model='approved-exact-model';
    await page.evaluate(()=>document.dispatchEvent(new CustomEvent('session-team-applied',{detail:{session_id:'session'}})));
    await page.waitForFunction(()=>document.querySelector('#session-model').shadowRoot.querySelector('.lbl').textContent==='model · approved-exact-model · approved');
    assert.equal(await page.locator('#session-model').evaluate(picker=>picker.querySelectorAll('option').length),1);
    await page.locator('#session-model [role=combobox]').click({force:true});
    assert.equal(await page.locator('#session-model').evaluate(picker=>picker._open),false);
    assert.equal(await page.evaluate(()=>{const picker=$('#session-model');picker.value='stale-injected-override';return sessionComposerModelOverride(S.session,picker);}),undefined);
    assert.ok(!requests.includes('/api/llm/models'),'native profile must not discover global template choices');
    assert.equal(await page.evaluate(()=>graphRefreshes),2);
    await page.evaluate(()=>populateSessionModelPicker(S.session));
    await page.screenshot({path:join(screenshots,`composer-${theme}.png`),fullPage:true});
    assert.deepEqual(errors,[]);
  }finally{await context.close();}
});
test('native multi-Agent target follows Session slots and does not replace a selected execution graph',async()=>{
  const{context,page,team,errors}=await fixture();try{
    team.approved=true;team.slots=[{slot_id:'author',model:'author-model'},{slot_id:'reviewer',model:'reviewer-model'}];
    await page.evaluate(()=>{S.session.coordinationGraphTurnId='retained-turn';return refreshSessionTeamCapabilities('session');});
    assert.match(await page.locator('#session-model .lbl').textContent(),/author-model, reviewer-model/);
    await page.evaluate(()=>{$('#session-target').value='reviewer';$('#session-target').dispatchEvent(new Event('change'));});
    assert.match(await page.locator('#session-model .lbl').textContent(),/reviewer-model · approved/);
    await page.evaluate(()=>document.dispatchEvent(new CustomEvent('session-team-loaded',{detail:{session_id:'session',team:S.session.currentTeam}})));
    assert.equal(await page.locator('#session-target').evaluate(picker=>picker.value),'reviewer');
    assert.equal(await page.evaluate(()=>graphRefreshes),0);assert.deepEqual(errors,[]);
  }finally{await context.close();}
});
test('late legacy discovery cannot overwrite native approval; legacy overrides stay available',async()=>{
  const{context,page,team,errors}=await fixture();try{
    team.history_version='legacy_v1';await page.evaluate(()=>refreshSessionTeamCapabilities('session'));
    await page.waitForFunction(()=>document.querySelector('#session-model').querySelector('option[value="other-legacy-model"]'));
    assert.equal(await page.locator('#session-model').evaluate(picker=>picker.disabled),false);
    assert.equal(await page.evaluate(()=>{const picker=$('#session-model');picker.value='other-legacy-model';return sessionComposerModelOverride(S.session,picker);}),'other-legacy-model');
    const raced=await page.evaluate(async()=>{const original=window.fetch;let resolve;window.fetch=()=>new Promise(r=>resolve=r);const pending=populateSessionModelPicker(S.session);S.session.currentTeam={history_version:'execution_v2',approved:true,slots:[{slot_id:'native',model:'final-model'}]};refreshSessionTeamComposer();resolve({ok:true,json:async()=>['stale-catalog']});await pending;window.fetch=original;return{options:[...$('#session-model').querySelectorAll('option')].map(x=>x.textContent),disabled:$('#session-model').disabled};});
    assert.deepEqual(raced,{options:['model · final-model · approved'],disabled:true});assert.deepEqual(errors,[]);
    assert.match(source,/const model_override = sessionComposerModelOverride\(S\.session, modelSel\);/,'actual Send uses the native override guard');
  }finally{await context.close();}
});
