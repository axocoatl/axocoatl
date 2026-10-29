import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { browserEngines, launchTestDaemon, newAuthorizedContext } from '../support/daemon.mjs';

const engines = await browserEngines();
let runtime;
before(async () => { runtime = process.env.AXOCOATL_COMPONENT_BASE_URL ? {baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL, stop:async()=>{}} : await launchTestDaemon(); });
after(async () => { await runtime?.stop(); });
const PATH = '/api/sessions/ses-native/turns/turn-native/receipt';
const STATUS = {session_id:'ses-native', turn_id:'turn-native', state:'ready', message:'This completed turn has captured evidence ready to sign.', outcome:'completed', checks:{state:'not_configured',count:0}, review:{state:'not_configured'}};
async function fixture(engine, {status=STATUS, downloadFails=false, badContent=false, theme='dark', narrow=false, offscreen=false}={}) {
  const browser=await engine.launch();
  try {
    const context=await newAuthorizedContext(browser,{viewport:narrow?{width:390,height:844}:{width:1100,height:820},colorScheme:theme,reducedMotion:'reduce',acceptDownloads:true});
    const page=await context.newPage(), errors=[], calls=[], reads=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.route('**/receipt-fixture',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="${theme}"><head><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"></head><body>${offscreen ? '<div style="height:2400px">Earlier turns</div>' : ''}<ax-turn-receipt session-id="ses-native" turn-id="turn-native"></ax-turn-receipt><script type="module" src="/ui/turn-receipt.js"></script></body></html>`}));
    let current=structuredClone(status);
    await page.route(`**${PATH}/status`,route=>{reads.push(route.request().url());return route.fulfill({json:current});});
    await page.route(`**${PATH}`,route=>{
      calls.push(route.request().url());
      if(badContent)return route.fulfill({contentType:'text/html',body:'<h1>Sign in required</h1>'});
      if(downloadFails)return route.fulfill({status:503,json:{error:'The signing key could not be read. No receipt was created.'}});
      current={...current,state:'available',receipt:{key_id:'sha256:trusted-key',statement_sha256:'sha256:signed-statement',files:1,removed:0,source:'judged_capture',complete:true}};
      return route.fulfill({contentType:'application/vnd.dsse.envelope.v1+json',body:JSON.stringify({payloadType:'application/vnd.in-toto+json',payload:'captured-evidence',signatures:[]})});
    });
    await page.goto(`${runtime.baseUrl}/receipt-fixture`);
    const receipt=page.locator('ax-turn-receipt');
    if(!offscreen)await page.waitForFunction(()=>document.querySelector('ax-turn-receipt')?.dataset.state!=='loading');
    return {page,receipt,calls,reads,errors,setStatus:value=>{current=value;},close:async()=>{await context.close();await browser.close();}};
  } catch(error) {await browser.close();throw error;}
}
for(const engine of engines) {
  test(`${engine.name}: a native turn downloads its signed evidence and does not call unconfigured checks a pass`,async()=>{
    const f=await fixture(engine,{narrow:true});try {
      await f.receipt.locator('a.download').waitFor();
      const text=await f.receipt.evaluate(el=>el.shadowRoot.textContent);
      assert.match(text,/Checks: not configured \(0\)/);assert.match(text,/Review: not configured/);assert.match(text,/does not prove the change is correct/);
      const pending=f.page.waitForEvent('download');const link=f.receipt.getByRole('link',{name:'Download signed receipt',exact:true});await link.focus();await link.press('Enter');
      const download=await pending;assert.equal(download.suggestedFilename(),'turn-native.receipt.dsse.json');assert.equal(await download.failure(),null);assert.equal(f.calls.length,1);
      await f.page.waitForFunction(()=>document.querySelector('ax-turn-receipt').dataset.state==='available');
      assert.match(await f.receipt.evaluate(el=>el.shadowRoot.textContent),/Signing key: sha256:trusted-key/);
      await f.page.reload();await f.page.waitForFunction(()=>document.querySelector('ax-turn-receipt').dataset.state==='available');
      assert.equal(f.calls.length,1,'restoring history reads status without issuing another download');
      const bounds=await f.receipt.boundingBox();assert.ok(bounds.width<=390);assert.deepEqual(f.errors,[]);
    }finally{await f.close();}
  });
  test(`${engine.name}: signing failure stays visible and cannot masquerade as a downloaded receipt`,async()=>{
    const f=await fixture(engine,{downloadFails:true,theme:'light'});try{
      await f.receipt.getByRole('link',{name:'Download signed receipt',exact:true}).click();
      await f.receipt.getByText('Receipt unavailable: The signing key could not be read. No receipt was created.',{exact:true}).waitFor();
      assert.equal(await f.receipt.locator('a.download').count(),0);
      await f.receipt.getByRole('button',{name:'Refresh receipt',exact:true}).click();await f.receipt.locator('a.download').waitFor();
      assert.equal(f.calls.length,1);assert.deepEqual(f.errors,[]);
    }finally{await f.close();}
  });
  test(`${engine.name}: an unfinished evidence read refreshes without creating a receipt, and non-receipt downloads are rejected`,async()=>{
    const f=await fixture(engine,{status:{...STATUS,state:'pending',message:'Final evidence is being retained.'},badContent:true});try{
      await f.receipt.getByText('Final evidence is being retained.',{exact:true}).waitFor();assert.equal(await f.receipt.locator('a.download').count(),0);assert.equal(f.calls.length,0);
      f.setStatus({...STATUS});await f.receipt.locator('a.download').waitFor();assert.equal(f.calls.length,0);
      await f.receipt.getByRole('link',{name:'Download signed receipt',exact:true}).click();
      await f.receipt.getByText('Receipt unavailable: The server did not return a signed receipt.',{exact:true}).waitFor();assert.equal(await f.receipt.locator('a.download').count(),0);assert.deepEqual(f.errors,[]);
    }finally{await f.close();}
  });
  test(`${engine.name}: offscreen historical receipts do not read evidence until reached`,async()=>{
    const f=await fixture(engine,{offscreen:true});try{
      await f.page.screenshot();assert.equal(f.reads.length,0);assert.equal(f.calls.length,0);
      await f.receipt.scrollIntoViewIfNeeded();await f.receipt.locator('a.download').waitFor();
      assert.equal(f.reads.length,1);assert.equal(f.calls.length,0);assert.deepEqual(f.errors,[]);
    }finally{await f.close();}
  });
  test(`${engine.name}: a receipt for another turn is refused, and unavailable historical evidence has no download`,async()=>{
    const f=await fixture(engine,{status:{...STATUS,turn_id:'other-turn'}});try{
      await f.receipt.getByText('Receipt unavailable: The receipt status does not identify this Session turn.',{exact:true}).waitFor();
      assert.equal(await f.receipt.locator('a.download').count(),0);
      f.setStatus({...STATUS,state:'unavailable',message:'This historical turn has no captured file evidence.'});
      await f.receipt.getByRole('button',{name:'Refresh receipt',exact:true}).click();
      await f.receipt.getByText('This historical turn has no captured file evidence.',{exact:true}).waitFor();assert.equal(await f.receipt.locator('a.download').count(),0);
      assert.equal(f.calls.length,0);assert.deepEqual(f.errors,[]);
    }finally{await f.close();}
  });
}
