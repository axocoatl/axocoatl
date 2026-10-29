import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, readFile, realpath, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { after, before, test } from 'node:test';

import { chromium } from 'playwright';
import { REPOSITORY_ROOT, newAuthorizedContext, resolveChromiumExecutable, launchTestDaemon } from '../support/daemon.mjs';
import { answerContractProbe, OLLAMA_VERSION } from '../support/ollama.mjs';

// Actual native dashboard Send -> real repository edit, required host check and
// review -> downloadable signed receipt. Only the model server is scripted;
// admission, execution, capture, signing and the HTTP routes are real. The fixture
// setup uses the API; the user-facing task and receipt use the actual app controls.
const IMAGE = process.env.AXOCOATL_NATIVE_TURN_IMAGE || 'docker.io/library/rust:bookworm';
const MODEL = 'browser-test-model:latest';
const DIGEST = 'a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72';
let runtime, modelServer, browser;
const chats = [];

function run(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { ...options, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.once('error', reject);
    child.once('exit', (code) => resolve({ code, stdout, stderr }));
  });
}

const imageMissing = (await run('podman', ['image', 'exists', IMAGE]).catch(() => ({ code: 1 }))).code !== 0
  ? `${IMAGE} is not on this machine; pull it or set AXOCOATL_NATIVE_TURN_IMAGE to a curated image with Git`
  : false;

function answer(body) {
  const messages = body?.messages || [];
  if (messages.some(message => typeof message.content === 'string' && message.content.includes('VERDICT: APPROVE'))) return {role:'assistant',content:'VERDICT: APPROVE\nThe addition uses the requested operator.'};
  const tools = (body?.tools || []).map((tool) => tool.function?.name);
  if (messages.at(-1)?.role !== 'tool' && tools.includes('edit_file')) {
    return { role: 'assistant', content: '', tool_calls: [{ id: 'call-1', function: { name: 'edit_file', arguments: { path: 'calc.py', old: 'return a - b', new: 'return a + b' } } }] };
  }
  return { role: 'assistant', content: 'Fixed add in calc.py.' };
}

async function git(repo, ...args) {
  // The host's own Git configuration is not consulted here.
  const result = await run('git', ['-C', repo, ...args], { env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  assert.equal(result.code, 0, `git ${args.join(' ')}: ${result.stderr}`);
  return result.stdout.trim();
}

async function api(method, pathname, body) {
  const response = await fetch(`${runtime.baseUrl}${pathname}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch { json = text; }
  return { status: response.status, json };
}

before(async () => {
  if (imageMissing) return;
  modelServer = createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    if (answerContractProbe(req.url, raw ? JSON.parse(raw) : null, res)) return;
    if (req.url === '/api/chat') {
      const body = raw ? JSON.parse(raw) : null;
      chats.push(body);
      // The fixture Agent names the model without its tag; the reply names
      // the model it was asked for.
      const reply = { model: body?.model, message: answer(body), done: true, done_reason: 'stop', prompt_eval_count: 12, eval_count: 3 };
      res.writeHead(200, { 'content-type': 'application/x-ndjson' });
      res.end(`${JSON.stringify(reply)}\n`);
      return;
    }
    const responses = {
      '/api/version': { version: OLLAMA_VERSION }, '/api/status': { cloud: { disabled: true } },
      '/api/show': { details: { format: 'gguf' }, capabilities: ['completion', 'tools'], model_info: { 'general.context_length': 32768 } },
      '/api/tags': { models: [{ name: MODEL, model: MODEL, digest: DIGEST }] },
      '/api/ps': { models: [{ name: MODEL, model: MODEL, digest: DIGEST, details: { format: 'gguf' }, context_length: 32768, runner: 'llamacpp' }] },
      '/api/generate': { model: MODEL, created_at: '2026-10-19T00:00:00Z', response: '', done: true, done_reason: 'load' },
    };
    res.writeHead(responses[req.url] ? 200 : 404, { 'content-type': 'application/json' });
    res.end(JSON.stringify(responses[req.url] || {}));
  });
  await new Promise((resolve) => modelServer.listen(0, '127.0.0.1', resolve));
  runtime = await launchTestDaemon({
    nativeDataRoot: true,
    extraAgents:[{id:'receipt-reviewer',name:'Receipt Reviewer',role:'worker',tools:['read_file'],writes:[]}],
    ollamaBaseUrl: `http://127.0.0.1:${modelServer.address().port}`,
    // The tools an onboarded Lead has; its shell is what captures the
    // repository around each turn.
    agentTools: ['read_file', 'list_dir', 'grep', 'glob', 'write_file', 'edit_file', 'bash'],
  });
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({headless:true,...(executablePath ? {executablePath} : {})});
});

after(async () => {
  await browser?.close();
  await runtime?.stop();
  await new Promise((resolve) => modelServer?.close(resolve));
});

test('a dashboard task downloads a trusted signed receipt of its real checks, review and captured files', {timeout:600_000,skip:imageMissing}, async()=>{
  const repo = path.join(runtime.projectsDirectory,'dashboard-receipt');
  await mkdir(repo);
  await writeFile(path.join(repo,'calc.py'),'def add(a, b):\n    return a - b\n');
  await writeFile(path.join(repo,'README.md'),'# Receipt regression\n');
  await git(repo,'init','-q','-b','main');
  await git(repo,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','add','-A');
  await git(repo,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','Fixture');
  const canonical=await realpath(repo);
  const workspace=await api('POST','/api/workspaces',{path:canonical,name:'Receipt regression'});
  assert.equal(workspace.status,200,JSON.stringify(workspace.json));
  const created=await api('POST',`/api/workspaces/${workspace.json.id}/sessions`,{
    name:'Dashboard receipt',mode:{kind:'single_agent',agent_id:'browser-test-coder'},
    enabled_skills:[],exposed_ports:[],image:IMAGE,setup_command:null,setup_approved:false,setup_reviewed:true});
  assert.equal(created.status,200,JSON.stringify(created.json));
  assert.equal(created.json.environment.state,'ready');
  const session=created.json;
  const context=await newAuthorizedContext(browser,{viewport:{width:1280,height:900},colorScheme:'dark',reducedMotion:'reduce',acceptDownloads:true});
  const page=await context.newPage(),errors=[];
  page.on('pageerror',error=>errors.push(error.message));
  try {
    await page.goto(`${runtime.baseUrl}/?session=${session.id}`,{waitUntil:'domcontentloaded'});
    await page.getByRole('button',{name:'Team and budget',exact:true}).click();
    const team=page.locator('ax-session-team');
    await team.getByRole('button',{name:'Edit',exact:true}).click();
    await team.getByLabel('Maximum output tokens per request',{exact:true}).fill('4096');
    await team.locator('.checks summary').click();
    await team.getByLabel('Required checks, one command per line',{exact:true}).fill("grep -q 'return a + b' calc.py");
    await team.locator('.review-setting summary').click();
    await team.getByLabel('Required reviewer',{exact:true}).selectOption('receipt-reviewer');
    await team.getByLabel('Review rounds',{exact:true}).fill('1');
    await team.getByRole('button',{name:'Preview changes',exact:true}).click();
    await page.getByText('Review these changes.',{exact:false}).waitFor();
    await team.getByRole('button',{name:'Apply to this Session',exact:true}).click();
    await page.getByText('Saved Session configuration 1.',{exact:false}).waitFor();
    await team.getByRole('button',{name:'Close team review',exact:true}).click();
    await page.locator('#session-text').fill('Fix add in calc.py. Keep the function and use addition.');
    await page.locator('#session-send').click();
    const receipt=page.locator('ax-turn-receipt').last();
    const downloadLink=receipt.getByRole('link',{name:'Download signed receipt',exact:true});
    await downloadLink.waitFor({state:'visible',timeout:180_000});
    const turnId=await receipt.getAttribute('turn-id');
    for(const suffix of ['', '/status']) {
      const unsigned=await runtime.fetchWithoutToken(`/api/sessions/${session.id}/turns/${turnId}/receipt${suffix}`);
      assert.equal(unsigned.status,401,'receipt evidence requires dashboard authentication');
    }
    assert.equal(await readFile(path.join(canonical,'calc.py'),'utf8'),'def add(a, b):\n    return a + b\n');
    const downloadPromise=page.waitForEvent('download');
    await downloadLink.click();
    const download=await downloadPromise;
    const receiptPath=path.join(runtime.runRoot,'dashboard-receipt.json');
    await download.saveAs(receiptPath);
    const original=await readFile(receiptPath,'utf8');
    const envelope=JSON.parse(original),statement=JSON.parse(Buffer.from(envelope.payload,'base64').toString('utf8'));
    assert.equal(statement.predicate.run.session_id,session.id);
    assert.equal(statement.predicate.native_turn.turn_id,turnId);
    assert.equal(statement.predicate.run.run_id,turnId);
    assert.equal(statement.predicate.loadout,undefined);
    assert.equal(statement.predicate.verdict,'pass');
    assert.ok(statement.subject.some(subject=>subject.name==='calc.py'));
    assert.equal(statement.predicate.files.complete,true,JSON.stringify(statement.predicate.files));
    assert.deepEqual(statement.predicate.files.read_at_finish||[],[]);
    assert.equal(statement.predicate.checks.length,1);
    assert.equal(statement.predicate.checks[0].state,'passed');
    assert.equal(statement.predicate.checks[0].exit_code,0);
    assert.match(JSON.stringify(statement.predicate.checks[0].argv),/grep -q 'return a \+ b' calc\.py/);
    assert.equal(statement.predicate.review.verdict,'approved');
    assert.equal(statement.predicate.review.passed,true);
    assert.ok(chats.some(body=>body.messages.some(message=>String(message.content).includes('VERDICT: APPROVE'))),'the real host ran the required reviewer');
    const keyResponse=await api('GET','/api/receipts/key');
    assert.equal(keyResponse.status,200);
    const key=typeof keyResponse.json==='string'?keyResponse.json:keyResponse.json.line;
    assert.equal(typeof key,'string',JSON.stringify(keyResponse.json));
    const binary=process.env.AXOCOATL_E2E_BINARY||path.join(REPOSITORY_ROOT,'target/debug/axocoatl');
    const verify=()=>run(binary,['receipt','verify',receiptPath,'--key',key,'--repo',canonical,'--require-pass','--json']);
    let verified=await verify();assert.equal(verified.code,0,verified.stdout+verified.stderr);
    await writeFile(path.join(canonical,'calc.py'),'def add(a, b):\n    return a * b\n');
    verified=await verify();assert.equal(verified.code,4,'later edits must fail the captured-file comparison');
    await writeFile(path.join(canonical,'calc.py'),'def add(a, b):\n    return a + b\n');
    await page.reload({waitUntil:'domcontentloaded'});
    await downloadLink.waitFor({state:'visible'});
    const reloadDownload=page.waitForEvent('download');
    await downloadLink.click();
    await (await reloadDownload).saveAs(path.join(runtime.runRoot,'receipt-reloaded.json'));
    const reloaded=await readFile(path.join(runtime.runRoot,'receipt-reloaded.json'),'utf8');
    assert.equal(reloaded,original,'reload retains the same signed bytes');
    await runtime.restart();
    await page.reload({waitUntil:'domcontentloaded'});
    await downloadLink.waitFor({state:'visible'});
    const restartDownload=page.waitForEvent('download');
    await downloadLink.click();
    const restartedPath=path.join(runtime.runRoot,'receipt-restarted.json');
    await (await restartDownload).saveAs(restartedPath);
    assert.equal(await readFile(restartedPath,'utf8'),original,'daemon restart retains the same signed evidence');
    verified=await verify();assert.equal(verified.code,0,verified.stdout+verified.stderr);
    assert.deepEqual(errors,[]);
    if(process.env.AXOCOATL_P1_SCREENSHOT_DIR)await page.screenshot({path:path.join(process.env.AXOCOATL_P1_SCREENSHOT_DIR,'native-turn-receipt.png'),fullPage:true});
  } finally {await context.close();}
});
