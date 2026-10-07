import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { REPOSITORY_ROOT, launchTestDaemon, newAuthorizedContext, resolveChromiumExecutable } from '../support/daemon.mjs';

// A stub of the audited local Ollama server: every chat answers "done".
const MODEL = 'browser-test-model:latest';
const DIGEST = 'a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72';
let runtime, modelServer, browser;
const chats = [], workspaces = [];
let finishedRun = null;

const SMOKE = `schema: axocoatl.loadout/1
id: smoke
version: 1
name: Smoke
description: One local writer and one required check, under network none.
kind: custom
agents:
  - id: writer
    role: writer
    model: { provider: ollama, model: ${MODEL} }
    tools: [read_file, list_dir, bash]
    instructions: Answer in one word.
checks:
  - { name: readme, run: { argv: [sh, -c, "test -f README.md"] }, timeout: 3m }
budgets:
  agent: { activations: 2, invocations: 40, tokens: 400000, cost_usd: 0 }
  wall_clock: 10m
prompt: "{task}"
sandbox: { network: none, workload: hardened }
environment: { image: docker.io/library/rust:bookworm }
`;

const binary = () => process.env.AXOCOATL_E2E_BINARY
  ? path.resolve(process.env.AXOCOATL_E2E_BINARY)
  : path.join(REPOSITORY_ROOT, 'target', 'debug', 'axocoatl');

function cli(args, env = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(binary(), args, { env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.once('error', reject);
    child.once('exit', (code) => resolve({ code, stdout, stderr }));
  });
}

before(async () => {
  modelServer = createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    if (req.url === '/api/chat') {
      chats.push(raw ? JSON.parse(raw) : null);
      const reply = { model: MODEL, message: { role: 'assistant', content: 'done' }, done: true, done_reason: 'stop', prompt_eval_count: 12, eval_count: 3 };
      res.writeHead(200, { 'content-type': 'application/x-ndjson' });
      res.end(`${JSON.stringify(reply)}\n`);
      return;
    }
    const responses = {
      '/api/version': { version: '0.20.6' }, '/api/status': { cloud: { disabled: true } },
      '/api/show': { details: { format: 'gguf' }, capabilities: ['completion', 'tools'], model_info: { 'general.context_length': 32768 } },
      '/api/tags': { models: [{ name: MODEL, model: MODEL, digest: DIGEST }] },
      '/api/ps': { models: [{ name: MODEL, model: MODEL, digest: DIGEST, details: { format: 'gguf' }, context_length: 32768 }] },
      '/api/generate': { model: MODEL, created_at: '2026-10-06T00:00:00Z', response: '', done: true, done_reason: 'load' },
    };
    res.writeHead(responses[req.url] ? 200 : 404, { 'content-type': 'application/json' });
    res.end(JSON.stringify(responses[req.url] || {}));
  });
  await new Promise((resolve) => modelServer.listen(0, '127.0.0.1', resolve));
  runtime = await launchTestDaemon({ nativeDataRoot: true, ollamaBaseUrl: `http://127.0.0.1:${modelServer.address().port}` });
  // User loadouts live beside the configuration file the daemon started with.
  const dir = path.join(runtime.runRoot, 'loadouts');
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, 'smoke.yaml'), SMOKE);
  await writeFile(path.join(dir, 'broken.yaml'), 'schema: axocoatl.loadout/1\nid: broken\nmystery: true\n');
  await writeFile(path.join(dir, 'fix.yaml'), SMOKE.replace('id: smoke', 'id: fix'));
  browser = await chromium.launch({ headless: true, executablePath: await resolveChromiumExecutable() });
});
after(async () => {
  await browser?.close();
  await runtime?.stop();
  for (const dir of workspaces) await rm(dir, { recursive: true, force: true });
  await new Promise((resolve) => modelServer?.close(resolve));
});

async function json(pathname, init) {
  const response = await fetch(`${runtime.baseUrl}${pathname}`, init);
  const text = await response.text();
  return { status: response.status, type: response.headers.get('content-type'), value: text ? JSON.parse(text) : null };
}

test('the loadout API lists built-in and user loadouts, shows a graph and validates text, behind the API token', async () => {
  const anonymous = await runtime.fetchWithoutToken('/api/loadouts');
  assert.equal(anonymous.status, 401);
  for (const pathname of ['/api/runs', '/api/loadouts/fix']) {
    assert.equal((await runtime.fetchWithoutToken(pathname)).status, 401, pathname);
  }
  const list = await json('/api/loadouts');
  assert.equal(list.status, 200, JSON.stringify(list.value));
  const ids = list.value.map((row) => row.id);
  for (const id of ['fix', 'qa', 'audit', 'smoke', 'broken']) assert.ok(ids.includes(id), id);
  assert.ok(list.value.find((row) => row.id === 'audit').opt_in);
  const broken = list.value.find((row) => row.id === 'broken');
  assert.match(broken.error, /mystery/);
  assert.equal(broken.path, path.join(await realpath(runtime.runRoot), 'loadouts', 'broken.yaml'));
  const shadow = list.value.find((row) => row.path?.endsWith('fix.yaml'));
  assert.match(shadow.error, /built-in/, 'a user file cannot take a built-in id');
  const fix = await json('/api/loadouts/fix');
  assert.equal(fix.status, 200);
  assert.deepEqual(fix.value.graph.nodes.map((node) => node.id), ['agent:writer', 'check:tests', 'review']);
  assert.match(fix.value.text, /^schema: axocoatl\.loadout\/1$/m);
  assert.equal((await json('/api/loadouts/nope')).status, 404);
  const valid = await json('/api/loadouts/validate', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ text: SMOKE.replace('id: smoke', 'id: other') }) });
  assert.equal(valid.value.valid, true, JSON.stringify(valid.value));
  const invalid = await json('/api/loadouts/validate', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ text: 'schema: x' }) });
  assert.equal(invalid.value.valid, false);
  const unknown = await json('/api/runs', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ loadout: 'nope', task: 't', repo: runtime.runRoot, request_id: 'unknown-loadout' }) });
  assert.equal(unknown.status, 404);
  const missing = await json('/api/runs', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ loadout: 'fix', task: 't', repo: runtime.runRoot, request_id: 'missing-model' }) });
  assert.equal(missing.status, 422);
  assert.match(missing.value.error, /writer_model/);
  assert.deepEqual((await json('/api/runs')).value, []);
});

test('a custom loadout runs headless in a hardened Session: 202, events, JUnit and a verifiable record from axocoatl run', { timeout: 600_000 }, async () => {
  // A Workspace must lie outside the daemon's control-plane directories.
  const projects = await mkdtemp(path.join(tmpdir(), 'axocoatl-loadout-repo-'));
  workspaces.push(projects);
  const repo = await realpath(projects);
  await writeFile(path.join(repo, 'README.md'), '# fixture\n');
  for (const args of [['init', '-q'], ['add', 'README.md'], ['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'Add a README']]) {
    const git = spawn('git', ['-C', repo, ...args], { stdio: 'ignore' });
    assert.equal(await new Promise((resolve) => git.once('exit', resolve)), 0, args.join(' '));
  }
  // POST /api/runs answers 202 and a repeat of the request returns the same run.
  const request = { loadout: 'smoke', task: 'Say done.', repo, request_id: `direct-${Date.now()}` };
  const post = (body) => fetch(`${runtime.baseUrl}/api/runs`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
  const first = await post(request);
  const accepted = await first.json();
  assert.equal(first.status, 202, JSON.stringify(accepted));
  const again = await post(request);
  assert.equal(again.status, 202);
  assert.equal((await again.json()).run_id, accepted.run_id);
  let after = 0, finished = false, events = [];
  for (let polls = 0; polls < 600 && !finished; polls += 1) {
    const page = await json(`/api/runs/${accepted.run_id}/events?after=${after}`);
    assert.equal(page.status, 200);
    events.push(...page.value.events.map(([, event]) => event));
    after = page.value.next_after ?? after;
    finished = page.value.finished;
  }
  assert.ok(finished, JSON.stringify(events));
  const status = await json(`/api/runs/${accepted.run_id}`);
  assert.equal(status.value.state, 'finished', JSON.stringify(status.value));
  const outcome = status.value.outcome;
  if (outcome.exit_code !== 0) {
    const turn = outcome.turns[0]?.turn_id;
    const plane = turn ? await json(`/api/sessions/${accepted.session_id}/turns/${turn}/control-plane`) : null;
    assert.fail(`${JSON.stringify(outcome, null, 2)}\n${JSON.stringify({checks: plane?.value?.required_checks, readiness: plane?.value?.required_check_readiness, state: plane?.value?.state}, null, 2)}`);
  }
  assert.deepEqual(outcome.checks.map((check) => [check.name, check.state]), [['readme', 'passed']]);
  assert.ok(events.some((event) => event.kind === 'turn_started'));
  assert.equal(events.at(-1).kind, 'ended', JSON.stringify(events.map((event) => [event.kind, event.phase, event.detail])));
  // The Session the run created is bound to its loadout and runs hardened.
  const session = await json(`/api/sessions/${accepted.session_id}`);
  assert.equal(session.value.loadout.run_id, accepted.run_id);
  assert.equal(session.value.loadout.network, 'none');
  assert.equal(session.value.loadout.workload, 'hardened');
  const junit = await fetch(`${runtime.baseUrl}/api/runs/${accepted.run_id}/junit`);
  assert.equal(junit.headers.get('content-type'), 'application/xml');
  assert.match(await junit.text(), /<testsuites name="axocoatl"/);
  const record = await fetch(`${runtime.baseUrl}/api/runs/${accepted.run_id}/record`);
  assert.equal(record.headers.get('content-type'), 'application/vnd.axocoatl.record+jsonl');
  const lines = (await record.text()).trim().split('\n').map((line) => JSON.parse(line));
  assert.equal(lines[0].schema, 'axocoatl.record-bundle/1');
  assert.equal(lines.at(-1).section, 'end');
  assert.ok(lines.some((line) => line.section === 'turn'));
  assert.ok(chats.length > 0, 'the writer called the stub provider');
  finishedRun = accepted;

  // axocoatl run against the same daemon writes --junit and --record, and
  // axocoatl record verify accepts the bundle.
  const out = await mkdtemp(path.join(runtime.runRoot, 'out-'));
  const junitPath = path.join(out, 'junit.xml'), recordPath = path.join(out, 'run.axorecord.jsonl');
  const run = await cli(['run', 'smoke', '--task', 'Say done again.', '--repo', repo,
    '--junit', junitPath, '--record', recordPath, '--url', runtime.baseUrl], { AXOCOATL_TOKEN: runtime.token });
  assert.equal(run.code, 0, `${run.stdout}\n${run.stderr}`);
  assert.match(run.stdout, /Verdict: pass \(exit 0\)/);
  assert.match(run.stdout, /readme\s+passed/);
  assert.match(run.stdout, /Record: run-/);
  assert.match(await readFile(junitPath, 'utf8'), /<property name="axocoatl.exit_code" value="0"\/>/);
  const verified = await cli(['record', 'verify', recordPath]);
  assert.equal(verified.code, 0, verified.stdout + verified.stderr);
  assert.match(verified.stdout, /^valid: run run-/);
  // A changed byte is detected.
  const bytes = await readFile(recordPath);
  const at = bytes.indexOf(Buffer.from('"verdict":"pass"'));
  bytes[at + 12] = 'P'.charCodeAt(0);
  await writeFile(recordPath, bytes);
  assert.equal((await cli(['record', 'verify', recordPath])).code, 2);
  // Usage errors exit 3 and an unreachable daemon 4.
  assert.equal((await cli(['run', 'smoke', '--repo', repo, '--url', runtime.baseUrl], { AXOCOATL_TOKEN: runtime.token })).code, 3);
  assert.equal((await cli(['run', 'nope', '--task', 't', '--repo', repo, '--url', runtime.baseUrl], { AXOCOATL_TOKEN: runtime.token })).code, 3);
  assert.equal((await cli(['run', 'smoke', '--task', 't', '--repo', repo, '--url', runtime.baseUrl], { AXOCOATL_TOKEN: 'wrong' })).code, 4);
  assert.equal((await cli(['run', 'smoke', '--task', 't', '--repo', repo, '--url', 'http://127.0.0.1:9'])).code, 4);
});

test('Settings → Loadouts lists loadouts, shows invalid files and keeps the graph read-only', async () => {
  const context = await newAuthorizedContext(browser), page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.route('**/loadouts-fixture', (route) => route.fulfill({ contentType: 'text/html', body:
    '<!doctype html><html><head><link rel="stylesheet" href="/ui/tokens.css"></head><body style="height:900px;display:flex"><ax-settings-loadouts></ax-settings-loadouts><script type="module" src="/ui/settings-loadouts.js"></script></body></html>' }));
  try {
    await page.goto(`${runtime.baseUrl}/loadouts-fixture`);
    const settings = page.locator('ax-settings-loadouts');
    await settings.locator('.side-row[data-key="fix"]').waitFor();
    for (const key of ['fix', 'qa', 'audit', 'smoke']) await settings.locator(`.side-row[data-key="${key}"]`).waitFor();
    assert.equal(await settings.locator('.side-row .badge.optin').count(), 1, 'audit is opt-in');
    // An invalid user file is listed with its path and error.
    await settings.locator('.side-row').filter({ hasText: 'broken.yaml' }).click();
    await settings.locator('.invalid-error').waitFor();
    assert.match(await settings.locator('.invalid-error').textContent(), /mystery/);
    assert.match(await settings.locator('.detail').textContent(), /loadouts\/broken\.yaml/);
    // A loadout shows its YAML, parameters, the run command and the graph.
    await settings.locator('.side-row[data-key="fix"]').click();
    await settings.locator('pre.yaml').filter({ hasText: 'id: fix' }).waitFor();
    assert.equal(await settings.locator('input[aria-label="axocoatl run command"]').inputValue(),
      'axocoatl run fix --task "..." --model reviewer=provider:model --model writer=provider:model');
    assert.ok(await settings.locator('table.params').textContent().then((text) => text.includes('writer_model')));
    const graph = settings.locator('ax-loadout-graph');
    await graph.locator('ax-node#agent-writer').waitFor();
    const lattice = graph.locator('ax-lattice');
    assert.equal(await lattice.getAttribute('readonly'), '');
    assert.equal(await lattice.getAttribute('aria-readonly'), 'true');
    const snapshot = () => lattice.evaluate((canvas) => JSON.stringify({
      nodes: [...canvas.querySelectorAll('ax-node')].map((node) => [node.id, node.getAttribute('data-x'), node.getAttribute('data-y')]),
      edges: [...canvas.querySelectorAll('ax-edge')].map((edge) => [edge.getAttribute('from'), edge.getAttribute('to')]),
    }));
    const before = await snapshot();
    assert.equal(JSON.parse(before).nodes.length, 3);
    const events = await lattice.evaluate((canvas) => {
      window.loadoutEdits = [];
      for (const kind of ['edge-connect', 'node-moving', 'nodes-delete-request', 'edges-delete-request']) {
        canvas.addEventListener(kind, () => window.loadoutEdits.push(kind));
      }
      canvas.mode = 'edit';
      return { mode: canvas.mode, edge: canvas.addEdge({ from: 'agent-writer', to: 'review' }) };
    });
    assert.deepEqual(events, { mode: 'view', edge: null }, 'read-only refuses Edit and new edges');
    // Dragging a node and pulling a connection from a handle change nothing.
    const box = await graph.locator('ax-node#agent-writer').boundingBox();
    await page.mouse.move(box.x + 40, box.y + 30); await page.mouse.down();
    await page.mouse.move(box.x + 160, box.y + 140, { steps: 5 }); await page.mouse.up();
    await page.mouse.move(box.x + box.width - 1, box.y + box.height / 2); await page.mouse.down();
    const target = await graph.locator('ax-node#review').boundingBox();
    await page.mouse.move(target.x + 5, target.y + target.height / 2, { steps: 5 }); await page.mouse.up();
    await lattice.focus(); await page.keyboard.press('Delete');
    assert.equal(await snapshot(), before);
    assert.deepEqual(await page.evaluate(() => window.loadoutEdits), []);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});

test('a loadout Session shows its loadout badge and the run outcome panel in the workbench', async (t) => {
  if (!finishedRun) { t.skip('the headless run did not finish'); return; }
  const context = await newAuthorizedContext(browser, { viewport: { width: 1280, height: 800 } }), page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.route('**/api/llm-health', (route) => route.fulfill({ json: { ollama: { base_url: 'http://127.0.0.1:9', reachable: true, configured: true, missing_models: [] } } }));
  try {
    await page.goto(`${runtime.baseUrl}/?session=${encodeURIComponent(finishedRun.session_id)}`, { waitUntil: 'domcontentloaded' });
    const badge = page.locator('#session-active .sa-chip.loadout');
    await badge.waitFor({ timeout: 30_000 });
    // The team view loads after the Session opens and renders the row again.
    await page.locator('#session-active .sa-chip:not(.loadout)').first().waitFor();
    assert.equal((await badge.textContent()).trim(), 'Loadout smoke@1');
    await badge.click();
    const panel = page.locator('#session-run-outcome');
    assert.equal(await panel.evaluate((node) => node.open), true);
    const outcome = page.locator('ax-run-outcome');
    assert.equal(await outcome.getAttribute('run-id'), finishedRun.run_id);
    await outcome.locator('.verdict[data-verdict="pass"]').waitFor();
    assert.match(await outcome.locator('details[data-check="readme"]').textContent(), /passed/);
    assert.equal(await outcome.locator('a.download').getAttribute('href'), `/api/runs/${finishedRun.run_id}/record`);
    assert.deepEqual(errors, []);
  } finally { await context.close(); }
});
