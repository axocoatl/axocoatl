#!/usr/bin/env node
// A stand-in for axocoatl that follows the 1.5.0 shared contract just far
// enough to drive newuser-linux.sh end to end in test-release-gate.sh
// (GATE_SIMULATION=1): onboard's non-interactive flags and exit codes (and
// --update, Step 6's Always-On Service and Step 7's restart of it), doctor
// --json, a daemon that answers /health with its version and start time and
// /health/live; `run fix` as a tiny writer/reviewer loop against the gate's
// stub model server, executing read_file and edit_file itself, whose record
// is shaped as a 1.5.0 run writes it (testdata/record-bundle.mjs: the
// commit it started from, its verdict, and the writer's and reviewer's
// accepted answers in the Session history); `service`; and `secret list`.
// It is test scaffolding: it proves the gate script's own logic, never the
// product.
import { spawn, spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import {
  appendFileSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync,
} from 'node:fs';
import { createServer } from 'node:http';
import { homedir } from 'node:os';
import path from 'node:path';

import { history, record } from './record-bundle.mjs';

const VERSION = process.env.FAKE_AXOCOATL_VERSION || '0.0.0';
const darwin = process.platform === 'darwin';
// Where the product keeps config.yaml and its data (axocoatl-cli/src/user_paths.rs).
const configPath = darwin
  ? path.join(homedir(), 'Library', 'Application Support', 'Axocoatl', 'config.yaml')
  : path.join(process.env.XDG_CONFIG_HOME || path.join(homedir(), '.config'), 'axocoatl', 'config.yaml');
const dataDir = darwin
  ? path.join(homedir(), 'Library', 'Application Support', 'Axocoatl', 'data')
  : path.join(process.env.XDG_DATA_HOME || path.join(homedir(), '.local', 'share'), 'axocoatl');
// The Always-On Service's unit (crates/axocoatl-service).
const unitPath = darwin
  ? path.join(homedir(), 'Library', 'LaunchAgents', 'ai.axocoatl.daemon.plist')
  : path.join(process.env.XDG_CONFIG_HOME || path.join(homedir(), '.config'), 'systemd', 'user', 'axocoatl.service');
const servicePid = path.join(dataDir, 'fake-service.pid');
if (process.env.FAKE_ARGV_LOG) {
  appendFileSync(process.env.FAKE_ARGV_LOG, `${JSON.stringify({ cwd: process.cwd(), argv: process.argv.slice(2) })}\n`);
}
const [command, ...args] = process.argv.slice(2);
const out = (text) => process.stdout.write(`${text}\n`);
const err = (text) => process.stderr.write(`${text}\n`);

function flag(name) {
  const at = args.indexOf(name);
  return at >= 0 ? args[at + 1] : undefined;
}
function flags(name) {
  return args.flatMap((value, index) => (value === name ? [args[index + 1]] : []));
}
function onPath(program) {
  return (process.env.PATH || '').split(':').some((dir) => dir && existsSync(path.join(dir, program)));
}
function readConfig() {
  if (!existsSync(configPath)) return null;
  const text = readFileSync(configPath, 'utf8');
  const pick = (key) => text.match(new RegExp(`^\\s*${key}:\\s*"?([^"\\n]+)"?`, 'm'))?.[1];
  return { port: Number(pick('port')), baseUrl: pick('base_url'), writer: pick('writer'), reviewer: pick('reviewer') };
}
function writeConfig({ port, baseUrl, writer, reviewer }) {
  mkdirSync(path.dirname(configPath), { recursive: true });
  writeFileSync(configPath, [
    'server:', `  port: ${port}`, '  host: "127.0.0.1"',
    'providers:', '  ollama:', `    base_url: "${baseUrl}"`,
    'run:', '  models:', `    writer: "${writer}"`, `    reviewer: "${reviewer}"`, '',
  ].join('\n'));
}
function servicePidRunning() {
  try {
    const pid = Number(readFileSync(servicePid, 'utf8'));
    process.kill(pid, 0);
    return pid;
  } catch {
    return 0;
  }
}
async function health(port) {
  const response = await fetch(`http://127.0.0.1:${port}/health`).catch(() => null);
  return response?.ok ? response.json() : null;
}
async function waitFor(check) {
  for (let i = 0; i < 100; i += 1) {
    if (await check()) return true;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return false;
}
async function startService() {
  const child = spawn(process.execPath, [process.argv[1], 'serve'], { detached: true, stdio: 'ignore', env: process.env });
  child.unref();
  mkdirSync(dataDir, { recursive: true });
  writeFileSync(servicePid, String(child.pid));
  return waitFor(async () => Boolean(await health(readConfig().port)));
}
async function stopService() {
  const pid = servicePidRunning();
  if (pid) process.kill(pid, 'SIGTERM');
  rmSync(servicePid, { force: true });
  return waitFor(async () => !servicePidRunning() && !(await health(readConfig().port)));
}
async function chat(baseUrl, model, messages, tools) {
  const response = await fetch(`${baseUrl}/api/chat`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ model, messages, stream: true, tools: tools.map((name) => ({ type: 'function', function: { name } })) }),
  });
  if (!response.ok) throw new Error(`model call failed: HTTP ${response.status}`);
  return JSON.parse((await response.text()).trim().split('\n').at(-1)).message;
}

// The daemon (`dev`, `serve`, the service): /health and /health/live.
async function daemon() {
  const config = readConfig();
  mkdirSync(dataDir, { recursive: true });
  const started = Date.now();
  const server = createServer((req, res) => {
    const json = (status, body) => { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
    if (req.url === '/health/live') return json(200, { status: 'ok' });
    if (req.url === '/health') return json(200, { status: 'ok', version: VERSION, started_at_ms: started });
    return json(404, { error: 'not found' });
  });
  server.listen(config.port, '127.0.0.1');
  await new Promise((resolve) => { process.once('SIGINT', resolve); process.once('SIGTERM', resolve); });
  server.close();
  return 0;
}

async function onboard() {
  // FAKE_ONBOARD_OPERATOR=1 stands for a person answering every prompt
  // (the gate's --live mode); otherwise, with no terminal, refuse.
  const interactive = !args.includes('--yes');
  if (interactive && !process.stdin.isTTY && process.env.FAKE_ONBOARD_OPERATOR !== '1') {
    err('axocoatl onboard: no terminal; pass --yes and the flags in --help');
    return 3;
  }
  // Step 6: --start, or the person's choice (FAKE_STEP6_CHOICE=service);
  // its default answer runs `axocoatl dev` in the foreground.
  const start = flag('--start') ?? (interactive ? process.env.FAKE_STEP6_CHOICE : 'none');
  if (!start) {
    err('fake axocoatl: interactive onboarding without --start would run axocoatl dev in the foreground');
    return 9;
  }
  if (!onPath('podman')) {
    out('Not ready yet, 1 step left:\n  1) Install Podman: sudo apt-get install -y podman');
    return 1;
  }
  if (process.env.FAKE_NO_SUBUID) {
    out('Not ready yet, 1 step left:\n  1) sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER && podman system migrate');
    return 1;
  }
  // A person's onboarding adds a missing range of subordinate ids, the
  // first free one from 100000 (FAKE_SUBID_FILES: the files to use).
  if (interactive && process.env.FAKE_SUBID_FILES) {
    const user = process.env.USER || 'user';
    for (const file of process.env.FAKE_SUBID_FILES.split(':')) {
      const lines = readFileSync(file, 'utf8').split('\n').filter(Boolean);
      if (lines.some((line) => line.split(':')[0] === user)) continue;
      const end = Math.max(100000, ...lines.map((line) => Number(line.split(':')[1]) + Number(line.split(':')[2])));
      out(`No range of subordinate ids for ${user}. Add a range with \`sudo usermod --add-subuids ${end}-${end + 65535}\`? yes`);
      appendFileSync(file, `${user}:${end}:65536\n`);
    }
  }
  const existing = args.includes('--update') ? readConfig() : null;
  const before = existsSync(configPath) ? statSync(configPath).mtimeMs : 0;
  // Onboarding's port: --port, else the configured one, else 8080 when
  // free (FAKE_DEFAULT_PORT stands for that choice, so tests never bind 8080).
  const next = {
    port: Number(flag('--port') || existing?.port || process.env.FAKE_DEFAULT_PORT || 8080),
    baseUrl: flag('--ollama-url') || existing?.baseUrl || process.env.FAKE_OLLAMA_URL,
    writer: flag('--writer-model') || existing?.writer || 'ollama:qwen3-coder:30b',
    reviewer: flag('--reviewer') || existing?.reviewer || 'ollama:gpt-oss:20b',
  };
  if (!existing || JSON.stringify(existing) !== JSON.stringify(next)) writeConfig(next);
  // A person's browser sign-in stores the key (FAKE_SIGN_IN=openrouter).
  if (interactive && process.env.FAKE_SIGN_IN) {
    mkdirSync(path.join(dataDir, 'secrets'), { recursive: true });
    writeFileSync(path.join(dataDir, 'secrets', process.env.FAKE_SIGN_IN), 'stored\n');
  }
  if (start === 'service') {
    mkdirSync(path.dirname(unitPath), { recursive: true });
    writeFileSync(unitPath, '# fake unit\n');
    if (!servicePidRunning() && !(await startService())) return 1;
    out('✓ Always-On Service installed and started');
  }
  // Step 7: a service that started before the configuration changed runs
  // the old one; onboarding asks to restart it.
  if (args.includes('--smoke-test') && servicePidRunning()) {
    const running = await health(next.port);
    if (running && statSync(configPath).mtimeMs > running.started_at_ms && statSync(configPath).mtimeMs > before) {
      out(`The Always-On Service started before ${configPath} changed, so the first run would use the old configuration. Restart it now? yes`);
      await stopService();
      if (!(await startService())) return 1;
    }
  }
  out('Ready for: axocoatl run fix\n  ✓ axocoatl run fix (ollama)\nReady: yes');
  return 0;
}

async function service(action) {
  switch (action) {
    case 'status': {
      const installed = existsSync(unitPath);
      const running = Boolean(servicePidRunning());
      out(`Always-On Service (fake backend)\n  installed: ${installed ? 'yes' : 'no'}\n  running:   ${running ? 'yes' : 'no'}\n  detail:    fake`);
      return 0;
    }
    case 'stop':
      return (await stopService()) ? 0 : 1;
    case 'uninstall':
      await stopService();
      rmSync(unitPath, { force: true });
      return 0;
    default:
      return 2;
  }
}

async function run() {
  const config = readConfig();
  const models = Object.fromEntries(flags('--model').map((entry) => entry.split(/=(.*)/s).slice(0, 2)));
  const writer = (models.writer || config?.writer || '').replace(/^ollama:/, '');
  const reviewer = (models.reviewer || config?.reviewer || '');
  if (!reviewer.startsWith('ollama:')) {
    err(`axocoatl run: ${reviewer} cannot review a run; exit 3, no Session created`);
    return 3;
  }
  const manifest = ['package.json', 'pyproject.toml', 'Cargo.toml', 'go.mod'].find((file) => existsSync(file));
  // The plan line the product printed in CI on the fixture this
  // repository is (run-plan-lines.txt), as the CLI prints it.
  const pyproject = manifest === 'pyproject.toml' ? readFileSync('pyproject.toml', 'utf8') : '';
  const python = [['[tool.poetry]', 'python-poetry'], ['[tool.uv]', 'python-uv'], ['[tool.pdm.', 'python-pdm'],
    ['[tool.hatch.envs.', 'python-hatch']].find(([marker]) => pyproject.includes(marker))?.[1];
  const node = [['pnpm-lock.yaml', 'node-pnpm'], ['.yarnrc.yml', 'node-yarn-berry'], ['yarn.lock', 'node-yarn'],
    ['package-lock.json', 'node-deps']].find(([file]) => existsSync(file))?.[1];
  const fixture = {
    'package.json': node ?? 'node-plain',
    'pyproject.toml': python ?? 'python-pytest',
    'Cargo.toml': 'rust',
    'go.mod': 'go',
  }[manifest];
  const plan = readFileSync(new URL('run-plan-lines.txt', import.meta.url), 'utf8')
    .split('\n')
    .filter((line) => line && !line.startsWith('#'))
    .map((line) => line.split('\t'))
    .find(([name]) => name === fixture)?.[1];
  if (plan) err(`· note: ${plan}`);
  if (manifest === 'package.json' && readFileSync('package.json', 'utf8').includes('axocoatl-gate-absent')) {
    err('axocoatl run: the check program is not in the image; exit 3 before any model call');
    return 3;
  }
  const live = await fetch(`http://127.0.0.1:${config.port}/health/live`).catch(() => null);
  if (!live?.ok) return 4;
  const session = `ses-${randomUUID()}`;
  const repoHead = spawnSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).stdout.trim();
  const messages = [{ role: 'user', content: flag('--task') }];
  let answer = '';
  for (let turn = 0; turn < 32; turn += 1) {
    const message = await chat(config.baseUrl, writer, messages, ['read_file', 'edit_file', 'write_file']);
    messages.push(message);
    if (!message.tool_calls?.length) {
      answer = message.content ?? '';
      break;
    }
    for (const call of message.tool_calls) {
      const { name, arguments: callArgs } = call.function;
      let result;
      if (name === 'read_file') {
        result = existsSync(callArgs.path) ? JSON.stringify({ content: readFileSync(callArgs.path, 'utf8') }) : `Tool error: ${callArgs.path}: not found`;
      } else if (name === 'edit_file') {
        const before = readFileSync(callArgs.path, 'utf8');
        writeFileSync(callArgs.path, before.replace(callArgs.old, callArgs.new));
        result = JSON.stringify({ ok: true, path: callArgs.path, replacements: 1 });
      }
      messages.push({ role: 'tool', content: result });
    }
  }
  // FAKE_FINAL_TEXT stands for a turn that ended on text (a slipped tool
  // call) and that a broken product accepted as the writer's answer.
  if (process.env.FAKE_FINAL_TEXT) answer = `${answer}\n${process.env.FAKE_FINAL_TEXT}`;
  const review = await chat(config.baseUrl, reviewer.replace(/^ollama:/, ''), [{ role: 'user', content: 'Give VERDICT: APPROVE or VERDICT: CHANGES' }], ['read_file']);
  const changed = spawnSync('git', ['status', '--porcelain'], { encoding: 'utf8' }).stdout.split('\n').filter((line) => /^ M /.test(line));
  const pass = /VERDICT: APPROVE/.test(review.content) && changed.length === 1;
  const bundle = record({
    runId: `run-${randomUUID()}`, sessionId: session, repoHead, verdict: pass ? 'pass' : 'checks_failed',
    entries: history(answer, review.content ?? ''),
  });
  for (const [option, body] of [['--junit', '<testsuites/>\n'], ['--record', bundle], ['--receipt', '{"payloadType":"fake"}\n']]) {
    if (flag(option)) writeFileSync(flag(option), body);
  }
  out(`Verdict: ${pass ? 'pass (exit 0)' : 'checks failed (exit 1)'}`);
  if (plan) out(`Notes:\n  ${plan}`);
  return pass ? 0 : 1;
}

async function main() {
  switch (command) {
    case '--version':
      out(`axocoatl ${VERSION}`);
      return 0;
    case 'onboard':
      return onboard();
    case 'doctor': {
      // Log lines go to standard error, as the real daemon's tracing does;
      // the gate must read the JSON document from standard output alone.
      err('INFO axocoatl doctor: checking the sandbox, models, writers and daemon');
      const config = readConfig();
      const ready = Boolean(config) && onPath('podman') && !process.env.FAKE_NO_SUBUID;
      const doc = {
        ready,
        sections: {
          sandbox: [{ name: 'podman', status: onPath('podman') ? 'ok' : 'fail', detail: '', fix: 'sudo apt-get install -y podman' }],
          models: [{ name: 'ollama', status: config ? 'ok' : 'fail', detail: config?.baseUrl ?? '', fix: '' }],
          writers: [],
          daemon: [{ name: 'daemon', status: 'warn', detail: 'not running', fix: 'axocoatl dev' }],
        },
      };
      if (process.env.FAKE_NO_SUBUID) doc.sections.sandbox.push({ name: 'subuid', status: 'fail', detail: 'no range', fix: 'sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER' });
      const text = ready
        ? 'Axocoatl doctor\n  [OK] Podman\n\nReady for: axocoatl run fix\n  ✓ axocoatl run fix (ollama)\nReady: yes'
        : 'Axocoatl doctor\n  [FAIL] Podman\n\nNot ready yet, 1 step left:\n  1) Podman\nReady: no';
      out(args.includes('--json') ? JSON.stringify(doc) : text);
      return ready ? 0 : 1;
    }
    case 'dev':
    case 'serve':
      return daemon();
    case 'service':
      return service(args[0]);
    case 'secret': {
      const dir = path.join(dataDir, 'secrets');
      if (args[0] === 'list' && existsSync(dir)) for (const name of readdirSync(dir)) out(name);
      return args[0] === 'list' ? 0 : 2;
    }
    case 'session':
      out('No sessions.');
      return 0;
    case 'receipt':
      if (args[0] === 'key') out(`ed25519 ${Buffer.alloc(32).toString('base64')} fake`);
      return args[0] === 'key' || existsSync(args[1] ?? '') ? 0 : 1;
    case 'record':
      return existsSync(args[1] ?? '') ? 0 : 1;
    case 'run':
      return run();
    default:
      err(`fake axocoatl: unsupported command ${command}`);
      return 2;
  }
}

process.exitCode = await main();
