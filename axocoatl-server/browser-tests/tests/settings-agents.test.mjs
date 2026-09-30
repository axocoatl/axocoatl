import assert from 'node:assert/strict';
import {after, before, test} from 'node:test';
import {chromium} from 'playwright';
import {launchTestDaemon, resolveChromiumExecutable} from '../support/daemon.mjs';

let runtime, browser;
before(async()=>{
  runtime=process.env.AXOCOATL_COMPONENT_BASE_URL
    ? {baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}} : await launchTestDaemon();
  browser=await chromium.launch({headless:true,executablePath:await resolveChromiumExecutable()});
});
after(async()=>{await browser?.close();await runtime?.stop();});

test('Settings reads global actor status without polling reusable Worker templates',async()=>{
  const context=await browser.newContext(),page=await context.newPage();
  const calls=[],errors=[];
  page.on('pageerror',error=>errors.push(error.message));
  page.on('console',message=>{if(message.type()==='error')errors.push(message.text());});
  const agents=[
    {id:'autonomous',name:'Autonomous',provider:'ollama',model:'finite'},
    {id:'coordinator',name:'Coordinator',role:'coordinator',provider:'ollama',model:'finite'},
    {id:'worker',name:'Reusable Worker',role:'worker',provider:'ollama',model:'finite'},
  ];
  await page.route('**/settings-agents-fixture',route=>route.fulfill({contentType:'text/html',body:
    '<!doctype html><html><head><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-settings-agents></ax-settings-agents><script type="module" src="/ui/settings-agents.js"></script></body></html>'}));
  await page.route('**/api/agents',route=>route.fulfill({json:agents}));
  await page.route('**/api/tokens/report',route=>route.fulfill({json:{agents:[]}}));
  await page.route('**/api/agents/*/status',route=>{
    const id=new URL(route.request().url()).pathname.split('/')[3];calls.push(`status:${id}`);
    const status=id==='coordinator'?(calls.includes('restart:coordinator')?'Running after restart':'Running'):'Idle';
    return route.fulfill(id==='worker'?{status:404,json:{error:'Worker has no global actor'}}:{json:{status}});
  });
  await page.route('**/api/agents/*/restart',route=>{
    calls.push(`restart:${new URL(route.request().url()).pathname.split('/')[3]}`);
    return route.fulfill({json:{ok:true}});
  });
  try{
    await page.goto(`${runtime.baseUrl}/settings-agents-fixture`);
    const settings=page.locator('ax-settings-agents');
    const worker=settings.locator('tbody tr').filter({hasText:'worker'});
    await worker.getByText('Worker template',{exact:true}).waitFor();
    assert.equal(await worker.getByRole('button',{name:'Restart',exact:true}).isDisabled(),true);
    assert.deepEqual(calls.sort(),['status:autonomous','status:coordinator']);
    calls.length=0;
    await page.evaluate(()=>document.querySelector('ax-settings-agents').refresh());
    assert.deepEqual(calls.sort(),['status:autonomous','status:coordinator']);
    calls.length=0;
    await settings.locator('tbody tr').filter({hasText:'coordinator'}).getByRole('button',{name:'Restart',exact:true}).click();
    await settings.getByText('Running after restart',{exact:true}).waitFor();
    assert.deepEqual(calls.sort(),['restart:coordinator','status:autonomous','status:coordinator']);
    await worker.getByText('Worker template',{exact:true}).waitFor();
    assert.equal(await settings.locator('.errors').textContent(),'');
    assert.deepEqual(errors,[]);
  }finally{await context.close();}
});
