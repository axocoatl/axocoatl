import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { chmod, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { REPOSITORY_ROOT, launchTestDaemon } from '../support/daemon.mjs';

// The opt-in audit loadout end to end against a stub of the audited local
// Ollama server: the planner splits the scope into two areas, one read-only
// worker per area reads its area's file and reports its findings in one
// turn, and the integrator merges them in a third turn; each turn has its
// own Team Apply. Read-only Agents run as the hardened container's helper
// user, which reads the repository through its view of it, whatever the
// repository's file modes.
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

/** The file each area's worker reads before it answers. */
const READS = { docs: 'README.md', code: 'src/lib.rs' };

/** What the stub answers to one chat request. */
function answer(body) {
  const text = (body?.messages || []).map((message) => typeof message.content === 'string' ? message.content : '').join('\n');
  if (/The area workers of this audit have finished/.test(text)) {
    return { role: 'assistant', content: `FINDINGS${fence([{ id: 'A1', title: 'README has no usage section', severity: 'low', location: 'README.md:1', area: 'docs' }])}` };
  }
  const area = text.match(/Your area: ([a-z0-9-]+)/);
  // A worker reads its area's file first: the run judges from the recorded
  // tool calls whether a worker examined its area.
  if (area && !(body?.messages || []).some((message) => message.role === 'tool')) {
    return { role: 'assistant', content: '', tool_calls: [
      { id: `call_${area[1]}`, function: { index: 0, name: 'read_file', arguments: { path: READS[area[1]] } } },
    ] };
  }
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

/**
 * A committed fixture repository whose directory has `mode`; its entries are
 * readable to other users (as git clone makes them), or with `ownerOnly`
 * readable to their owner alone (as under `umask 077`).
 */
async function fixtureRepo(prefix, mode, { ownerOnly = false } = {}) {
  const projects = await mkdtemp(path.join(tmpdir(), prefix));
  workspaces.push(projects);
  const repo = await realpath(projects);
  const [dirMode, fileMode] = ownerOnly ? [0o700, 0o600] : [0o755, 0o644];
  await mkdir(path.join(repo, 'src'), { mode: dirMode });
  await writeFile(path.join(repo, 'README.md'), '# fixture\n', { mode: fileMode });
  await writeFile(path.join(repo, 'src', 'lib.rs'), 'pub fn one() -> u32 { 1 }\n', { mode: fileMode });
  for (const [entry, entryMode] of [['src', dirMode], ['README.md', fileMode], ['src/lib.rs', fileMode]]) {
    await chmod(path.join(repo, entry), entryMode);
  }
  await git(repo, 'init', '-q', '-b', 'main');
  await git(repo, 'add', '.');
  await git(repo, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'Add a fixture');
  if (ownerOnly) assert.equal((await run('chmod', ['-R', 'go-rwx', path.join(repo, '.git')])).code, 0);
  await chmod(repo, mode);
  return repo;
}

const cut = (text, max = 2000) => (text.length > max ? `${text.slice(0, max)}… (${text.length} bytes)` : text);

/**
 * Each area worker's tool calls as its Session recorded them, with their
 * outcomes, and the tool results the stub model received, so a failed run
 * can be judged from the test's log alone.
 */
async function workerCalls(outcome) {
  const lines = [];
  for (const turn of (outcome?.turns || []).filter((item) => item.purpose === 'audit_areas')) {
    try {
      const response = await fetch(`${runtime.baseUrl}/api/sessions/${outcome.session_id}/turns/${turn.turn_id}/control-plane`);
      const view = await response.json();
      const labels = new Map((view.nodes || []).map((node) => [node.node_id, node.label]));
      lines.push(`recorded tool calls of turn ${turn.turn_id} (HTTP ${response.status}, ${view.invocations?.status}):`);
      for (const call of view.invocations?.value || []) {
        const final = call.final_evidence;
        const node = call.activation?.node_id;
        lines.push(`  ${labels.get(node) || node}: ${call.intent?.tool_name} ${cut(call.intent?.redacted_preview || '')} -> ${final?.outcome || final?.kind || call.disposition}: ${cut(final?.redacted_preview || '')}`);
      }
    } catch (error) {
      lines.push(`recorded tool calls of turn ${turn.turn_id}: ${error.message}`);
    }
  }
  lines.push('tool calls and results the model sent and received:');
  for (const body of chats) {
    const area = JSON.stringify(body).match(/Your area: ([a-z0-9-]+)/)?.[1];
    for (const message of area ? body.messages || [] : []) {
      for (const call of message.role === 'assistant' ? message.tool_calls || [] : []) {
        lines.push(`  worker-${area} called ${call.function?.name} ${JSON.stringify(call.function?.arguments)}`);
      }
      if (message.role === 'tool') {
        lines.push(`  worker-${area} got: ${cut(typeof message.content === 'string' ? message.content : JSON.stringify(message.content))}`);
      }
    }
  }
  return lines.join('\n');
}

test('the audit loadout plans two areas, runs a read-only worker per area and integrates their findings', { timeout: 900_000 }, async () => {
  // mkdtemp makes the directory 0700; other users, the helper among them,
  // may read a repository that git clone made (0755).
  const repo = await fixtureRepo('axocoatl-audit-repo-', 0o755);
  const model = `ollama:${MODEL}`;
  const out = await mkdtemp(path.join(runtime.runRoot, 'out-'));
  const junitPath = path.join(out, 'junit.xml');
  const result = await run(binary(), ['run', 'audit', '--task', 'Find defects.', '--repo', repo,
    '--model', `planner=${model}`, '--model', `worker=${model}`, '--model', `integrator=${model}`,
    '--junit', junitPath, '--json', '--url', runtime.baseUrl], { AXOCOATL_TOKEN: runtime.token });
  const outcome = JSON.parse(result.stdout.slice(0, result.stdout.lastIndexOf('}') + 1));
  const failed = outcome.exit_code !== 0 || outcome.not_covered?.length > 0
    ? `${JSON.stringify(outcome, null, 2)}\n${result.stderr}\n${await workerCalls(outcome)}`
    : '';
  assert.equal(outcome.exit_code, 0, failed);
  assert.equal(result.code, 0);
  assert.deepEqual(outcome.turns.map((turn) => [turn.purpose, turn.state]),
    [['audit_plan', 'completed'], ['audit_areas', 'completed'], ['audit_integrate', 'completed']]);
  // The code worker's answer was read; neither the other area nor the
  // missing path is a gap, and both are notes in the run's progress.
  assert.deepEqual(outcome.not_covered, [], failed);
  assert.match(result.stderr, /note: worker-code listed other planned areas as not reached \(docs\)/);
  assert.match(result.stderr, /note: worker-code listed src\/main\.rs as not reached, and no such path exists/);
  // The notes are in the Outcome and the JUnit verdict too, not only in
  // the progress lines.
  assert.equal(outcome.notes.length, 2, JSON.stringify(outcome.notes));
  assert.match(outcome.notes.join('\n'), /worker-code listed src\/main\.rs as not reached/);
  assert.match(outcome.notes.join('\n'), /worker-code listed other planned areas as not reached \(docs\)/);
  assert.match(await readFile(junitPath, 'utf8'), /<testcase classname="axocoatl.run" name="verdict">\n {6}<system-out>note: worker-code listed /);
  assert.equal(outcome.findings.length, 1, JSON.stringify(outcome.findings));
  assert.equal(outcome.findings[0].title, 'README has no usage section');
  assert.equal(outcome.findings[0].area, 'docs');
  // Each area got a worker with its own request; nothing was written.
  const workers = chats.filter((body) => /Your area: /.test(JSON.stringify(body)));
  const areas = new Set(workers.map((body) => JSON.stringify(body).match(/Your area: ([a-z0-9-]+)/)[1]));
  assert.deepEqual([...areas].sort(), ['code', 'docs']);
  // Each worker's second call carried its read's result.
  assert.equal(workers.filter((body) => body.messages.some((message) => message.role === 'tool')).length, 2);
  assert.equal(await git(repo, 'status', '--porcelain'), '');
  assert.match(await readFile(junitPath, 'utf8'), /<testsuite name="findings"/);
});

// A repository only its owner may enter, as `mktemp -d` and `umask 077`
// make one (directories 0700, files 0600): the read-only Agents run as the
// helper user, one of the other users, and still read it through their view
// of it, on a Linux host as on a macOS Podman machine; they change nothing.
test('an audit of a repository only its owner may enter reads it and changes nothing', { timeout: 900_000 }, async () => {
  const repo = await fixtureRepo('axocoatl-audit-private-', 0o700, { ownerOnly: true });
  const model = `ollama:${MODEL}`;
  const asked = chats.length;
  const result = await run(binary(), ['run', 'audit', '--task', 'Find defects.', '--repo', repo,
    '--model', `planner=${model}`, '--model', `worker=${model}`, '--model', `integrator=${model}`,
    '--json', '--url', runtime.baseUrl], { AXOCOATL_TOKEN: runtime.token });
  const outcome = JSON.parse(result.stdout.slice(0, result.stdout.lastIndexOf('}') + 1));
  const shown = `${JSON.stringify(outcome, null, 2)}\n${result.stderr}\n${await workerCalls(outcome)}`;
  assert.equal(outcome.exit_code, 0, shown);
  assert.equal(result.code, 0, shown);
  assert.deepEqual(outcome.turns.map((turn) => [turn.purpose, turn.state]),
    [['audit_plan', 'completed'], ['audit_areas', 'completed'], ['audit_integrate', 'completed']], shown);
  assert.deepEqual(outcome.not_covered, [], shown);
  assert.equal(outcome.findings.length, 1, shown);
  // Each worker read its area's private file: the content came back.
  const workers = chats.slice(asked).filter((body) => /Your area: /.test(JSON.stringify(body)));
  const results = workers.flatMap((body) => (body.messages || []).filter((message) => message.role === 'tool'))
    .map((message) => (typeof message.content === 'string' ? message.content : JSON.stringify(message.content)));
  assert.ok(results.some((text) => text.includes('# fixture')), `${shown}\n${results.join('\n')}`);
  assert.ok(results.some((text) => text.includes('pub fn one() -> u32')), `${shown}\n${results.join('\n')}`);
  assert.equal(await git(repo, 'status', '--porcelain'), '');
  const mode = await run('stat', process.platform === 'linux' ? ['-c', '%a', repo] : ['-f', '%Lp', repo]);
  assert.equal(mode.stdout.trim(), '700');
});
