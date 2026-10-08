import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { chmod, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { REPOSITORY_ROOT, launchTestDaemon } from '../support/daemon.mjs';

// The built-in fix loadout end to end against a stub of the audited local
// Ollama server: the writer writes one file with write_file, the required
// check passes on it, the reviewer approves, and Keep as branch commits it
// with host git without touching the checkout. Writer and reviewer run the
// same model, so the run carries the same-model warning. The reviewer is
// read-only and runs as the hardened container's helper user, which reads
// only what the file modes let any user read, so each repository is made
// readable to other users as git clone makes one (mkdtemp makes it 0700).
const MODEL = 'browser-test-model:latest';
const DIGEST = 'a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72';
let runtime, modelServer;
const chats = [], workspaces = [];
let calls = 0;
// A chat whose request names SLOW-TASK is held until the test releases it.
let releaseSlow = () => {};
const slow = new Promise((resolve) => { releaseSlow = resolve; });

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

/** What the stub answers to one chat request. */
function answer(body) {
  const messages = body?.messages || [];
  const text = messages.map((message) => typeof message.content === 'string' ? message.content : '').join('\n');
  if (/VERDICT: APPROVE/.test(text)) {
    return { role: 'assistant', content: 'VERDICT: APPROVE\nNothing must change.' };
  }
  const tools = (body?.tools || []).map((tool) => tool.function?.name);
  if (messages.at(-1)?.role !== 'tool' && tools.includes('write_file')) {
    calls += 1;
    return { role: 'assistant', content: '', tool_calls: [{ id: `call-${calls}`, function: { name: 'write_file', arguments: { path: 'NOTES.md', content: 'kept by the fix run\n' } } }] };
  }
  return { role: 'assistant', content: 'Wrote NOTES.md.' };
}

before(async () => {
  modelServer = createServer(async (req, res) => {
    let raw = ''; for await (const chunk of req) raw += chunk;
    if (req.url === '/api/chat') {
      const body = raw ? JSON.parse(raw) : null;
      chats.push(body);
      if (raw.includes('SLOW-TASK')) await slow;
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
  releaseSlow();
  await runtime?.stop();
  for (const dir of workspaces) await rm(dir, { recursive: true, force: true });
  await new Promise((resolve) => modelServer?.close(resolve));
});

async function git(repo, ...args) {
  const result = await run('git', ['-C', repo, ...args]);
  assert.equal(result.code, 0, `git ${args.join(' ')}: ${result.stderr}`);
  return result.stdout.trim();
}

test('the built-in fix loadout runs under egress, passes, warns about the same model and keeps its change as a branch', { timeout: 900_000 }, async () => {
  const projects = await mkdtemp(path.join(tmpdir(), 'axocoatl-fix-repo-'));
  workspaces.push(projects);
  const repo = await realpath(projects);
  await chmod(repo, 0o755);
  await writeFile(path.join(repo, 'README.md'), '# fixture\n');
  await git(repo, 'init', '-q', '-b', 'main');
  // Keep commits as the repository's git identity. Set one here so the test does not
  // depend on the host's: a CI runner has none, and git then refuses to commit.
  await git(repo, 'config', 'user.name', 'Fixture');
  await git(repo, 'config', 'user.email', 'fixture@example.invalid');
  await git(repo, 'add', 'README.md');
  await git(repo, 'commit', '-qm', 'Add a README');
  const head = await git(repo, 'rev-parse', 'HEAD');
  const out = await mkdtemp(path.join(runtime.runRoot, 'out-'));
  const junitPath = path.join(out, 'junit.xml'), recordPath = path.join(out, 'run.axorecord.jsonl');
  const model = `ollama:${MODEL}`;
  const result = await run(binary(), ['run', 'fix', '--task', 'Add NOTES.md.', '--repo', repo,
    '--model', `writer=${model}`, '--model', `reviewer=${model}`, '--check', 'test -f NOTES.md',
    '--keep', 'branch', '--junit', junitPath, '--record', recordPath, '--json', '--url', runtime.baseUrl],
  { AXOCOATL_TOKEN: runtime.token });
  // With --json, standard output is exactly one Outcome document; Keep's lines go to stderr.
  const outcome = JSON.parse(result.stdout);
  assert.equal(outcome.exit_code, 0, `${result.stdout}\n${result.stderr}`);
  assert.equal(result.code, 0, `${result.stdout}\n${result.stderr}`);
  assert.equal(outcome.verdict, 'pass');
  assert.deepEqual(outcome.checks.map((check) => [check.name, check.state]), [['tests', 'passed']]);
  assert.equal(outcome.review.passed, true, JSON.stringify(outcome.review));
  assert.equal(outcome.usage.complete, true, JSON.stringify(outcome.usage));
  assert.equal(outcome.review.rounds[0].verdict, 'approve');
  assert.deepEqual(outcome.adjudications, [], 'an approving first round sends nothing back');
  assert.ok(outcome.warnings.some((warning) => warning.code === 'same_model_reviewer'), JSON.stringify(outcome.warnings));
  assert.equal(result.stderr.match(/^! warning same_model_reviewer:/gm)?.length, 1, `the same-model warning prints once:\n${result.stderr}`);
  // The run's Session is bound to the loadout under egress, hardened.
  const session = await (await fetch(`${runtime.baseUrl}/api/sessions/${outcome.session_id}`, { headers: { authorization: `Bearer ${runtime.token}` } })).json();
  assert.equal(session.loadout.network, 'egress');
  assert.equal(session.loadout.workload, 'hardened');
  // Keep as branch: a new branch holds exactly NOTES.md; the checkout is untouched.
  assert.match(result.stderr, /Kept: branch axocoatl\/fix-[0-9a-f]{8} at [0-9a-f]{40}/);
  const branch = result.stderr.match(/Kept: branch (\S+)/)[1];
  assert.equal(await git(repo, 'rev-parse', 'HEAD'), head);
  assert.equal(await git(repo, 'branch', '--show-current'), 'main');
  assert.equal(await git(repo, 'show', `${branch}:NOTES.md`), 'kept by the fix run');
  assert.equal(await git(repo, 'diff', '--name-only', head, branch), 'NOTES.md');
  assert.equal(await git(repo, 'rev-parse', `${branch}^`), head);
  assert.equal(await git(repo, 'log', '-1', '--format=%an <%ae> / %cn <%ce>', branch),
    'Fixture <fixture@example.invalid> / Fixture <fixture@example.invalid>', 'Keep commits as the repository\'s identity');
  const status = await (await fetch(`${runtime.baseUrl}/api/runs/${outcome.run_id}`, { headers: { authorization: `Bearer ${runtime.token}` } })).json();
  assert.equal(status.keep?.branch, branch, JSON.stringify({ ...status, outcome: undefined }));
  // JUnit and the record bundle are written, and the bundle verifies.
  assert.match(await readFile(junitPath, 'utf8'), /<property name="axocoatl.warning.same_model_reviewer"/);
  // Keep ran before the record was written, so the bundle holds its event.
  const recordLines = (await readFile(recordPath, 'utf8')).trim().split('\n').map((line) => JSON.parse(line));
  assert.ok(recordLines.some((line) => line.section === 'run_event' && line.data?.event?.phase === 'keep'), 'the record holds the keep event');
  const verified = await run(binary(), ['record', 'verify', recordPath]);
  assert.equal(verified.code, 0, verified.stdout + verified.stderr);
  assert.ok(chats.some((body) => /VERDICT/.test(JSON.stringify(body))), 'the reviewer was asked');
});

test('Ctrl-C stops a running loadout run: exit 6, the run interrupted, and its JUnit and record still written', { timeout: 600_000 }, async () => {
  const projects = await mkdtemp(path.join(tmpdir(), 'axocoatl-fix-stop-'));
  workspaces.push(projects);
  const repo = await realpath(projects);
  await chmod(repo, 0o755);
  await writeFile(path.join(repo, 'README.md'), '# fixture\n');
  await git(repo, 'init', '-q', '-b', 'main');
  await git(repo, 'add', 'README.md');
  await git(repo, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'Add a README');
  const out = await mkdtemp(path.join(runtime.runRoot, 'stop-'));
  const junitPath = path.join(out, 'junit.xml'), recordPath = path.join(out, 'run.axorecord.jsonl');
  const model = `ollama:${MODEL}`;
  const child = spawn(binary(), ['run', 'fix', '--task', 'SLOW-TASK: add NOTES.md.', '--repo', repo,
    '--model', `writer=${model}`, '--model', `reviewer=${model}`, '--check', 'test -f NOTES.md',
    '--junit', junitPath, '--record', recordPath, '--url', runtime.baseUrl],
  { env: { ...process.env, AXOCOATL_TOKEN: runtime.token }, stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = '';
  child.stdout.on('data', (chunk) => { stdout += chunk; });
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const exited = new Promise((resolve) => child.once('exit', (code) => resolve(code)));
  // Interrupt once the writer's model call is in flight.
  for (let waited = 0; waited < 600 && !chats.some((body) => JSON.stringify(body).includes('SLOW-TASK')); waited += 1) {
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  assert.ok(chats.some((body) => JSON.stringify(body).includes('SLOW-TASK')), stderr);
  child.kill('SIGINT');
  const code = await exited;
  releaseSlow();
  assert.equal(code, 6, `${stdout}\n${stderr}`);
  const runId = stderr.match(/run-[0-9a-f-]{36}/)?.[0];
  assert.ok(runId, stderr);
  const status = await (await fetch(`${runtime.baseUrl}/api/runs/${runId}`, { headers: { authorization: `Bearer ${runtime.token}` } })).json();
  assert.equal(status.outcome?.verdict, 'interrupted', JSON.stringify(status));
  assert.equal(status.outcome.exit_code, 6);
  assert.match(await readFile(junitPath, 'utf8'), /<property name="axocoatl.exit_code" value="6"\/>/);
  assert.equal((await run(binary(), ['record', 'verify', recordPath])).code, 0);
});
