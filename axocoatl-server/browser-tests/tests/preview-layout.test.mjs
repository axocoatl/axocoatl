import assert from 'node:assert/strict';
import {after, before, test} from 'node:test';
import {chromium} from 'playwright';
import {launchTestDaemon, resolveChromiumExecutable, newAuthorizedContext} from '../support/daemon.mjs';

let runtime, browser, previewMarkup, togglePickerSource, pickerShellSource, tapSource;
before(async()=>{
  runtime=process.env.AXOCOATL_COMPONENT_BASE_URL
    ? {baseUrl:process.env.AXOCOATL_COMPONENT_BASE_URL,stop:async()=>{}}
    : await launchTestDaemon();
  const source=await(await fetch(`${runtime.baseUrl}/${process.env.AXOCOATL_COMPONENT_BASE_URL?'index.html':''}`)).text();
  const start=source.indexOf('<div class="cockpit-pane cockpit-browser-pane"');
  const end=source.indexOf('<!-- Session-scoped Agent graph.',start);
  assert.ok(start>=0&&end>start,'use the actual embedded Preview subtree');
  previewMarkup=source.slice(start,end);
  togglePickerSource=source.slice(source.indexOf('function toggleBrowserPick()'),source.indexOf('// Hierarchy panel state'));
  assert.ok(togglePickerSource.startsWith('function toggleBrowserPick()'));
  pickerShellSource=source.slice(source.indexOf('function getBrowserIframe()'),source.indexOf('function termStatusClass('));
  assert.ok(pickerShellSource.includes('function confirmDomHier()'));
  tapSource=await(await fetch(`${runtime.baseUrl}/axo-tap.js`)).text();
  const executablePath=await resolveChromiumExecutable();
  browser=await chromium.launch({headless:true,...(executablePath?{executablePath}:{})});
});

test('picker references exclude transient classes and still identify the clicked element after Add to chat',async()=>{
  const context=await newAuthorizedContext(browser, {viewport:{width:1280,height:720}});
  const page=await context.newPage(),errors=[];page.on('pageerror',error=>errors.push(error.message));
  try{
    const previewOrigin=`http://ses-picker-test-p8765.localhost:${new URL(runtime.baseUrl).port}`;
    await context.route(url=>url.origin===previewOrigin,route=>route.fulfill(route.request().url().endsWith('/picker.js')
      ?{contentType:'text/javascript',body:tapSource}
      :{contentType:'text/html',body:'<!doctype html><html><body><main class="catalog-panel"><header><h1 data-fixture="plain">Order review</h1></header><button class="checkout primary" data-fixture="styled">Save order</button></main><script src="/picker.js"></script></body></html>'}));
    await page.route('**/preview-clean-reference',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html><link rel="stylesheet" href="/ui/tokens.css"><link rel="stylesheet" href="/ui/shell.css"><style>#cockpit-browser{height:648px}</style>${previewMarkup}<div id="references"></div><script type="module">
      import '/ui/browser.js';
      const $=selector=>document.querySelector(selector);
      const S={browser:{url:null,picking:false}};
      const el=(tag,classes,text)=>{const n=document.createElement(tag);n.className=classes;if(text!==undefined)n.textContent=text;return n;};
      function toast(){}
      window.pickReferences=[];window.pickMessages=[];
      function addRef(ref){window.pickReferences.push(ref);$('#references').textContent=JSON.stringify(window.pickReferences);}
      ${pickerShellSource}
      window.addEventListener('message',event=>{if(event.source===$('#browser').frame?.contentWindow&&event.origin===browserFrameOrigin())window.pickMessages.push(event.data);});
      $('#bx-pick').addEventListener('click',toggleBrowserPick);
      $('#dom-hier-confirm').addEventListener('click',confirmDomHier);
      $('#dom-hier-cancel').addEventListener('click',()=>closeDomHier(false));
      $('#browser').session='ses-picker-test';$('#browser').go('http://localhost:8765/fixture');
    </script></html>`}));
    await page.goto(runtime.baseUrl+'/preview-clean-reference');
    await page.waitForFunction(()=>window.pickMessages?.some(message=>message.kind==='axo-tap:ready'));
    const frame=page.frames().find(frame=>frame.url().startsWith(previewOrigin));
    for(const name of ['plain','styled']){
      await page.locator('#bx-pick').click();
      await frame.locator('.axo-tap-banner').waitFor();
      await frame.locator(`[data-fixture="${name}"]`).click();
      await page.locator('#dom-hier').waitFor({state:'visible'});
      const picked=await page.evaluate(()=>window.pickMessages.findLast(message=>message.kind==='axo-tap:picked'));
      assert.doesNotMatch(JSON.stringify(picked.chain),/axo-tap-(?:hover|locked)/,'labels, selectors and class metadata contain only application classes');
      await page.locator('#dom-hier-confirm').click();
      await frame.waitForFunction(()=>!document.querySelector('.axo-tap-hover,.axo-tap-locked,.axo-tap-banner'));
      const ref=await page.evaluate(()=>window.pickReferences.at(-1));
      assert.doesNotMatch(ref.html,/axo-tap-(?:hover|locked)/);
      assert.equal(await frame.evaluate(({selector,name})=>document.querySelector(selector)===document.querySelector(`[data-fixture="${name}"]`),{selector:ref.selector,name}),true);
      if(name==='plain'){
        assert.match(ref.selector,/h1:nth-child\(1\)/,'when no application classes remain the existing structural fallback is used');
        assert.equal(ref.html,'<h1 data-fixture="plain">Order review</h1>');
      }else{
        assert.match(ref.selector,/button.checkout.primary/);
        assert.match(ref.html,/class="checkout primary"/);
        assert.equal(await frame.locator('[data-fixture="styled"]').getAttribute('class'),'checkout primary');
      }
    }
    // Selecting an ancestor serializes its descendants without transient decoration.
    await page.locator('#bx-pick').click();await frame.locator('.axo-tap-banner').waitFor();
    await frame.locator('[data-fixture="plain"]').click();
    await page.locator('#dom-hier-list button').filter({hasText:'header'}).click();
    await page.waitForFunction(()=>window.pickMessages.some(message=>message.kind==='axo-tap:level'));
    await page.locator('#dom-hier-confirm').click();
    await frame.waitForFunction(()=>!document.querySelector('.axo-tap-hover,.axo-tap-locked,.axo-tap-banner'));
    const ancestor=await page.evaluate(()=>window.pickReferences.at(-1));
    assert.equal(ancestor.html,'<header><h1 data-fixture="plain">Order review</h1></header>');
    assert.equal(await frame.evaluate(selector=>document.querySelector(selector)===document.querySelector('header'),ancestor.selector),true);
    assert.deepEqual(errors,[]);
  }finally{await context.close();}
});

test('the visible Inspect element button uses the component current URL and blocks external pages',async()=>{
  const context=await newAuthorizedContext(browser);
  const page=await context.newPage();
  try{
    await page.route('**/preview-picker',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html>${previewMarkup}<script type="module">
      import '/ui/browser.js';
      const $=selector=>document.querySelector(selector);
      const S={browser:{url:null,picking:false}};
      window.pickerEvents=[];
      function toast(title){window.pickerEvents.push({error:title});}
      function setBrowserPicking(on){S.browser.picking=on;window.pickerEvents.push({picking:on});}
      ${togglePickerSource}
      $('#bx-pick').addEventListener('click',toggleBrowserPick);
    </script></html>`}));
    await page.goto(runtime.baseUrl+'/preview-picker');
    await page.waitForFunction(()=>customElements.get('ax-browser')&&window.pickerEvents);
    await page.locator('#bx-pick').click();
    assert.deepEqual(await page.evaluate(()=>window.pickerEvents),[{error:'Open a page first'}]);
    await page.locator('#browser').evaluate(element=>{element.url='http://localhost:8765/app';});
    await page.locator('#bx-pick').click();
    assert.deepEqual(await page.evaluate(()=>window.pickerEvents.at(-1)),{picking:true},
      'address owned by the browser works without a duplicate shell URL');
    await page.locator('#bx-pick').click();
    assert.deepEqual(await page.evaluate(()=>window.pickerEvents.at(-1)),{picking:false});
    await page.locator('#browser').evaluate(element=>{element.url='https://example.com/';});
    await page.locator('#bx-pick').click();
    assert.deepEqual(await page.evaluate(()=>window.pickerEvents.at(-1)),{error:'Picker only works on local URLs'});
  }finally{await context.close();}
});
after(async()=>{await browser?.close();await runtime?.stop();});

for(const [theme,width] of [['light',1280],['dark',390]])test(`Preview fills its body with a floating element inspector (${theme}, ${width}px)`,async()=>{
  const context=await newAuthorizedContext(browser, {viewport:{width,height:720},colorScheme:theme,reducedMotion:'reduce'});
  const page=await context.newPage(),errors=[];page.on('pageerror',error=>errors.push(error.message));
  try{
    await page.route('**/preview-layout',route=>route.fulfill({contentType:'text/html',body:`<!doctype html><html data-theme="${theme}"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"><link rel="stylesheet" href="/ui/shell.css"><style>body{margin:0}#cockpit-browser{height:648px}</style>${previewMarkup}<script type="module" src="/ui/browser.js"></script></html>`}));
    await page.goto(runtime.baseUrl+'/preview-layout');
    await page.waitForFunction(()=>customElements.get('ax-browser'));
    const measure=()=>page.evaluate(()=>{
      const rect=selector=>{const r=document.querySelector(selector).getBoundingClientRect();return{top:r.top,bottom:r.bottom,left:r.left,right:r.right,width:r.width,height:r.height};};
      return{pane:rect('#cockpit-browser'),body:rect('#browser-body'),browser:rect('#browser'),picker:rect('#dom-hier')};
    });
    const before=await measure();
    assert.ok(before.browser.height>500,'the browser uses the remaining pane height');
    assert.ok(Math.abs(before.browser.height-before.body.height)<2,'no empty sibling consumes half the Preview');
    await page.evaluate(()=>document.querySelector('#dom-hier').classList.remove('hide'));
    const shown=await measure();
    assert.equal(shown.browser.height,before.browser.height,'opening inspection does not resize the Preview');
    assert.ok(shown.picker.top>=shown.browser.top&&shown.picker.bottom<=shown.browser.bottom
      &&shown.picker.left>=shown.browser.left&&shown.picker.right<=shown.browser.right,
    'the inspector floats within the Preview at desktop and narrow widths');
    await page.locator('#dom-hier-confirm').click();
    assert.equal(await page.locator('#dom-hier-confirm').isVisible(),true);
    assert.deepEqual(errors,[]);
  }finally{await context.close();}
});
