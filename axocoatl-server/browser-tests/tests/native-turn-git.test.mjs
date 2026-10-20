import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, readFile, realpath, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { after, before, test } from 'node:test';

import { launchTestDaemon } from '../support/daemon.mjs';
import { answerContractProbe, OLLAMA_VERSION } from '../support/ollama.mjs';

// A new user's first workbench turn, end to end on local Podman against a
// stub of the audited Ollama server: the suggested Team and budget limits
// apply as they are, the Agent fixes calc.py with edit_file, Source
// Control's Last turn marks calc.py before and after a daemon restart, and
// a commit from Source Control is the repository's own Git identity.
// It needs Podman and a curated image with Git already pulled (Alpine, the
// default, gets Git from the network, which the fixture has none of); it is
// skipped when that image is not on the machine, rather than pulling it.
const IMAGE = process.env.AXOCOATL_NATIVE_TURN_IMAGE || 'docker.io/library/rust:bookworm';
const MODEL = 'browser-test-model:latest';
const DIGEST = 'a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72';
let runtime, modelServer;
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
    ollamaBaseUrl: `http://127.0.0.1:${modelServer.address().port}`,
    // The tools an onboarded Lead has; its shell is what captures the
    // repository around each turn.
    agentTools: ['read_file', 'list_dir', 'grep', 'glob', 'write_file', 'edit_file', 'bash'],
  });
});

after(async () => {
  await runtime?.stop();
  await new Promise((resolve) => modelServer?.close(resolve));
});

test('the first native turn on the suggested budget fills Last turn, across a restart, and commits as the person', { timeout: 600_000, skip: imageMissing }, async () => {
  const repo = path.join(runtime.projectsDirectory, 'tinycalc');
  await mkdir(repo);
  await writeFile(path.join(repo, 'calc.py'), 'def add(a, b):\n    return a - b\n');
  await writeFile(path.join(repo, 'README.md'), '# tinycalc\n');
  await git(repo, 'init', '-q', '-b', 'main');
  await git(repo, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'add', '-A');
  await git(repo, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'tinycalc');
  const canonical = await realpath(repo);

  const workspace = await api('POST', '/api/workspaces', { path: canonical, name: 'tinycalc' });
  assert.equal(workspace.status, 200, JSON.stringify(workspace.json));
  const created = await api('POST', `/api/workspaces/${workspace.json.id}/sessions`, {
    name: 'First fix', mode: { kind: 'single_agent', agent_id: 'browser-test-coder' },
    enabled_skills: [], exposed_ports: [], image: IMAGE,
    setup_command: null, setup_approved: false, setup_reviewed: true,
  });
  assert.equal(created.status, 200, JSON.stringify(created.json));
  const session = created.json;
  assert.equal(session.environment.state, 'ready', JSON.stringify(session.environment));

  // The suggested budget applies as it is. The fixture Agent has no output
  // maximum, so the edit gives it the one an onboarded Lead has (4096).
  const view = (await api('GET', `/api/sessions/${session.id}/team`)).json;
  assert.equal(view.approved, false);
  const budget = view.suggested_budget;
  const edit = {
    command_id: `team-edit-${Date.now()}`, expected_configuration_revision: view.configuration_revision,
    slots: view.slots.map((slot) => ({
      ...slot,
      max_output_tokens: 4096,
      limits: { ...budget.agent, cost_microunits: budget.free_providers.includes(slot.provider) ? 0 : budget.agent.cost_microunits },
      expires_at_ms: Date.now() + budget.expires_in_ms,
    })),
    dependencies: view.dependencies, layout: view.layout,
  };
  const preview = await api('POST', `/api/sessions/${session.id}/team/preview`, edit);
  assert.equal(preview.status, 200, JSON.stringify(preview.json));
  const applied = await api('POST', `/api/sessions/${session.id}/team/apply`, { edit, review_digest: preview.json.review_digest });
  assert.equal(applied.status, 200, JSON.stringify(applied.json));

  const turn = await api('POST', `/api/sessions/${session.id}/execute`, { input: 'Fix add in calc.py.' });
  assert.equal(turn.status, 200, JSON.stringify(turn.json));
  assert.ok(chats.some((body) => body?.tools?.some((tool) => tool.function?.name === 'edit_file')), 'the Agent was offered edit_file');
  assert.equal(await readFile(path.join(canonical, 'calc.py'), 'utf8'), 'def add(a, b):\n    return a + b\n');

  const lastTurn = async () => {
    const status = await api('GET', `/api/sessions/${session.id}/git/status`);
    assert.equal(status.status, 200, JSON.stringify(status.json));
    return Object.fromEntries(status.json.files.map((file) => [file.path, file.last_turn]));
  };
  assert.equal((await lastTurn())['calc.py'], true, 'Last turn marks the file the turn edited');
  await runtime.restart();
  assert.equal((await lastTurn())['calc.py'], true, 'and still does after a restart');

  // Source Control commits as the repository's own identity, set the way a
  // refused commit asks for, while the Session runs. Host Git replaces
  // .git/config, and the commit right after it still reads Git.
  await git(canonical, 'config', 'user.name', 'Ada Lovelace');
  await git(canonical, 'config', 'user.email', 'ada@example.invalid');
  const identity = await api('GET', `/api/sessions/${session.id}/git/identity`);
  assert.deepEqual(identity.json, { name: 'Ada Lovelace', email: 'ada@example.invalid', signing: false, problem: null });
  const commit = await api('POST', `/api/sessions/${session.id}/git/commit`, { message: 'Fix add', stage_all: true });
  assert.equal(commit.status, 200, JSON.stringify(commit.json));
  assert.equal(await git(canonical, 'log', '-1', '--format=%an <%ae>|%cn <%ce>|%s'), 'Ada Lovelace <ada@example.invalid>|Ada Lovelace <ada@example.invalid>|Fix add');
  assert.doesNotMatch(await readFile(path.join(canonical, '.git', 'config'), 'utf8'), /axocoatl/i);
});
