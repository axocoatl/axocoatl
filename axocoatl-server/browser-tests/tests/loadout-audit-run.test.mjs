import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { REPOSITORY_ROOT, launchTestDaemon } from '../support/daemon.mjs';

// The opt-in audit loadout end to end against a stub of the audited local
// Ollama server: the planner splits the scope into two areas, one read-only
// worker per area reports its findings in one turn, and the integrator
// merges them in a third turn; each turn has its own Team Apply.
const MODEL = 'browser-test-model:latest';
const DIGEST = 'a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72';
let runtime, modelServer;
const chats = [], workspaces = [];

const binary = () => process.env.AXOCOATL_E2E_BINARY
  ? path.resolve(process.env.AXOCOATL_E2E_BINARY)
  : path.join(REPOSITORY_ROOT, 'target', 'debug', 'axocoatl');

function run(command, args, env = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.once('error', reject);
    child.once('exit', (code) => resolve({ code, stdout, stderr }));
  });
}

const fence = (value) => `\n\`\`\`json\n${JSON.stringify(value)}\n\`\`\`\n`;

/** What the stub answers to one chat request. */
function answer(body) {
  const text = (body?.messages || []).map((message) => typeof message.content === 'string' ? message.content : '').join('\n');
  if (/The area workers of this audit have finished/.test(text)) {
    return { role: 'assistant', content: `FINDINGS${fence([{ id: 'A1', title: 'README has no usage section', severity: 'low', location: 'README.md:1', area: 'docs' }])}` };
  }
  const area = text.match(/Your area: ([a-z0-9-]+)/);
  if (area?.[1] === 'docs') {
    const findings = [{ id: 'F1', title: 'README has no usage section', severity: 'low', location: 'README.md:1' }];
    return { role: 'assistant', content: `FINDINGS${fence(findings)}NOT_REACHED${fence([])}` };
  }
  if (area) {
    // As the 1.3.0 re-smoke's notify worker answered: one unfenced JSON
    // object with uppercase block keys, listing another planned area and a
    // path that does not exist as not reached.
    return { role: 'assistant', content: JSON.stringify({ FINDINGS: [], NOT_REACHED: ['docs', 'src/main.rs'] }, null, 2) };
  }
  return { role: 'assistant', content: `AREAS${fence({ areas: [
    { name: 'docs', scope: 'the README', paths: ['README.md'] },
    { name: 'code', scope: 'the source', paths: ['src/**'] },
  ] })}` };
}

before(async () => {
  modelServer = createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    if (req.url === '/api/chat') {
      const body = raw ? JSON.parse(raw) : null;
      chats.push(body);
      const reply = { model: MODEL, message: answer(body), done: true, done_reason: 'stop', prompt_eval_count: 12, eval_count: 3 };
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
});
after(async () => {
  await runtime?.stop();
  for (const dir of workspaces) await rm(dir, { recursive: true, force: true });
  await new Promise((resolve) => modelServer?.close(resolve));
});

async function git(repo, ...args) {
  const result = await run('git', ['-C', repo, ...args]);
  assert.equal(result.code, 0, `git ${args.join(' ')}: ${result.stderr}`);
  return result.stdout.trim();
}

test('the audit loadout plans two areas, runs a read-only worker per area and integrates their findings', { timeout: 900_000 }, async () => {
  const projects = await mkdtemp(path.join(tmpdir(), 'axocoatl-audit-repo-'));
  workspaces.push(projects);
  const repo = await realpath(projects);
  await mkdir(path.join(repo, 'src'));
  await writeFile(path.join(repo, 'README.md'), '# fixture\n');
  await writeFile(path.join(repo, 'src', 'lib.rs'), 'pub fn one() -> u32 { 1 }\n');
  await git(repo, 'init', '-q', '-b', 'main');
  await git(repo, 'add', '.');
  await git(repo, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'Add a fixture');
  const model = `ollama:${MODEL}`;
  const out = await mkdtemp(path.join(runtime.runRoot, 'out-'));
  const junitPath = path.join(out, 'junit.xml');
  const result = await run(binary(), ['run', 'audit', '--task', 'Find defects.', '--repo', repo,
    '--model', `planner=${model}`, '--model', `worker=${model}`, '--model', `integrator=${model}`,
    '--junit', junitPath, '--json', '--url', runtime.baseUrl], { AXOCOATL_TOKEN: runtime.token });
  const outcome = JSON.parse(result.stdout.slice(0, result.stdout.lastIndexOf('}') + 1));
  assert.equal(outcome.exit_code, 0, `${JSON.stringify(outcome, null, 2)}\n${result.stderr}`);
  assert.equal(result.code, 0);
  assert.deepEqual(outcome.turns.map((turn) => [turn.purpose, turn.state]),
    [['audit_plan', 'completed'], ['audit_areas', 'completed'], ['audit_integrate', 'completed']]);
  // The code worker's answer was read; neither the other area nor the
  // missing path is a gap, and both are notes in the run's progress.
  assert.deepEqual(outcome.not_covered, []);
  assert.match(result.stderr, /note: worker-code listed other planned areas as not reached \(docs\)/);
  assert.match(result.stderr, /note: worker-code listed src\/main\.rs as not reached, and no such path exists/);
  assert.equal(outcome.findings.length, 1, JSON.stringify(outcome.findings));
  assert.equal(outcome.findings[0].title, 'README has no usage section');
  assert.equal(outcome.findings[0].area, 'docs');
  // Each area got a worker with its own request; nothing was written.
  const workers = chats.filter((body) => /Your area: /.test(JSON.stringify(body)));
  const areas = new Set(workers.map((body) => JSON.stringify(body).match(/Your area: ([a-z0-9-]+)/)[1]));
  assert.deepEqual([...areas].sort(), ['code', 'docs']);
  assert.equal(await git(repo, 'status', '--porcelain'), '');
  assert.match(await readFile(junitPath, 'utf8'), /<testsuite name="findings"/);
});
