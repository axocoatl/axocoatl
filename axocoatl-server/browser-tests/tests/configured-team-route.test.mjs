import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { launchTestDaemon, resolveChromiumExecutable, newAuthorizedContext } from '../support/daemon.mjs';

let runtime, browser;
before(async () => {
  runtime = await launchTestDaemon();
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({headless:true, ...(executablePath ? {executablePath} : {})});
});
after(async () => { await browser?.close(); await runtime?.stop(); });

test('before-turn graph adopts the approved native Session team when its response arrives late', async () => {
  const session = {...runtime.fixtures.alpha.sessions[0], mode:{kind:'custom',agents:['browser-test-coder','template-reviewer']}};
  const context = await newAuthorizedContext(browser, {viewport:{width:1280,height:800}});
  const page = await context.newPage(), errors=[];
  page.on('pageerror', error=>errors.push(error.message));
  let releaseTeam;
  const pendingTeam = new Promise(resolve=>{releaseTeam=resolve;});
  const team = {history_version:'execution_v2',approved:true,configuration_revision:3,
    slots:[{slot_id:'slot-architect',name:'Systems Architect',model:'native-model',provider:'ollama'},
      {slot_id:'slot-reviewer',name:'Critical Reviewer',model:'native-model',provider:'ollama'}],
    dependencies:[{parent:'slot-architect',child:'slot-reviewer'}],layout:[],templates:[]};
  await page.route('**/api/agents', route=>route.fulfill({json:[
    {id:'browser-test-coder',name:'Global Coder',model:'old-model',provider:'ollama',depends_on:[]},
    {id:'template-reviewer',name:'Global Reviewer',model:'old-model',provider:'ollama',depends_on:[]},
  ]}));
  await page.route('**/api/sessions', route=>route.fulfill({json:[session]}));
  await page.route(`**/api/workspaces/${session.workspace_id}/sessions`, route=>route.fulfill({json:[session]}));
  await page.route(`**/api/sessions/${session.id}/turns*`, route=>route.fulfill({json:[]}));
  await page.route(`**/api/sessions/${session.id}/messages*`, route=>route.fulfill({json:[]}));
  await page.route(`**/api/sessions/${session.id}/active-turn`, route=>route.fulfill({json:{run:null}}));
  await page.route(`**/api/sessions/${session.id}/team`, async route=>{await pendingTeam; await route.fulfill({json:team});});
  try {
    await page.goto(`${runtime.baseUrl}/?session=${session.id}`, {waitUntil:'domcontentloaded'});
    await page.getByRole('button',{name:'More ▾',exact:true}).click();
    await page.getByRole('menuitem',{name:'Agent graph',exact:true}).click();
    const graph = page.locator('#session-lattice-host ax-lattice');
    await graph.waitFor({state:'visible'});
    await page.getByRole('button',{name:'Global Coder; configured; no configured dependencies',exact:true}).waitFor();
    releaseTeam();
    await page.getByRole('button',{name:'Critical Reviewer; configured; depends on slot-architect',exact:true}).waitFor();
    assert.equal(await graph.getAttribute('aria-label'),`Configured Agent graph for ${session.name}: 2 agents. No turn selected.`);
    assert.equal(await graph.locator('ax-node').count(),2);
    assert.equal(await graph.locator('ax-edge').count(),1);
    assert.equal(await graph.locator('ax-edge').getAttribute('aria-label'),'slot-reviewer depends on slot-architect');
    assert.equal(await page.getByRole('button',{name:'Global Coder; configured; no configured dependencies',exact:true}).count(),0);
    assert.equal(await page.getByRole('button',{name:'Systems Architect; configured; no configured dependencies',exact:true}).count(),1);
    await page.reload();
    await page.getByRole('button',{name:'More ▾',exact:true}).click();
    await page.getByRole('menuitem',{name:'Agent graph',exact:true}).click();
    await page.getByRole('button',{name:'Critical Reviewer; configured; depends on slot-architect',exact:true}).waitFor();
    assert.equal(await graph.locator('ax-edge').count(),1);
    assert.deepEqual(errors,[]);
  } finally {releaseTeam();await context.close();}
});
