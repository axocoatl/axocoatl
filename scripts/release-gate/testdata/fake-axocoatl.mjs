#!/usr/bin/env node
// A stand-in for axocoatl that follows the 1.5.0 shared contract just far
// enough to drive newuser-linux.sh end to end in test-release-gate.sh
// (GATE_SIMULATION=1): onboard's non-interactive flags and exit codes,
// doctor --json, a daemon that answers /health/live, and `run fix` as a
// tiny writer/reviewer loop against the gate's stub model server, executing
// read_file and edit_file itself. It is test scaffolding: it proves the gate
// script's own logic, never the product.
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { homedir } from 'node:os';
import path from 'node:path';

const VERSION = process.env.FAKE_AXOCOATL_VERSION || '0.0.0';
const configPath = path.join(homedir(), '.config', 'axocoatl', 'config.yaml');
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
async function chat(baseUrl, model, messages, tools) {
  const response = await fetch(`${baseUrl}/api/chat`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ model, messages, stream: true, tools: tools.map((name) => ({ type: 'function', function: { name } })) }),
  });
  if (!response.ok) throw new Error(`model call failed: HTTP ${response.status}`);
  return JSON.parse((await response.text()).trim().split('\n').at(-1)).message;
}

async function main() {
  switch (command) {
    case '--version':
      out(`axocoatl ${VERSION}`);
      return 0;
    case 'onboard': {
      if (!args.includes('--yes') && !process.stdin.isTTY) {
        err('axocoatl onboard: no terminal; pass --yes and the flags in --help');
        return 3;
      }
      if (!onPath('podman')) {
        out('Not ready yet, 1 step left:\n  1) Install Podman: sudo apt-get install -y podman');
        return 1;
      }
      if (process.env.FAKE_NO_SUBUID) {
        out('Not ready yet, 1 step left:\n  1) sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER && podman system migrate');
        return 1;
      }
      mkdirSync(path.dirname(configPath), { recursive: true });
      writeFileSync(configPath, [
        'server:', `  port: ${flag('--port') || 8080}`, '  host: "127.0.0.1"',
        'providers:', '  ollama:', `    base_url: "${flag('--ollama-url')}"`,
        'run:', '  models:', `    writer: "${flag('--writer-model')}"`, `    reviewer: "${flag('--reviewer')}"`, '',
      ].join('\n'));
      out('Ready for:\n  ✓ axocoatl run fix');
      return 0;
    }
    case 'doctor': {
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
      out(args.includes('--json') ? JSON.stringify(doc) : (ready ? 'Ready for: axocoatl run fix' : 'Not ready'));
      return ready ? 0 : 1;
    }
    case 'dev': {
      const config = readConfig();
      const server = createServer((req, res) => {
        res.writeHead(req.url === '/health/live' ? 200 : 404);
        res.end();
      });
      server.listen(config.port, '127.0.0.1');
      await new Promise((resolve) => process.once('SIGINT', resolve));
      server.close();
      return 0;
    }
    case 'session':
      out('No sessions.');
      return 0;
    case 'receipt':
      if (args[0] === 'key') out(`ed25519 ${Buffer.alloc(32).toString('base64')} fake`);
      return args[0] === 'key' || existsSync(args[1] ?? '') ? 0 : 1;
    case 'record':
      return existsSync(args[1] ?? '') ? 0 : 1;
    case 'run': {
      const config = readConfig();
      const models = Object.fromEntries(flags('--model').map((entry) => entry.split(/=(.*)/s).slice(0, 2)));
      const writer = (models.writer || config?.writer || '').replace(/^ollama:/, '');
      const reviewer = (models.reviewer || config?.reviewer || '');
      if (!reviewer.startsWith('ollama:')) {
        err(`axocoatl run: ${reviewer} cannot review a run; exit 3, no Session created`);
        return 3;
      }
      const manifest = ['package.json', 'pyproject.toml', 'Cargo.toml', 'go.mod'].find((file) => existsSync(file));
      const image = { 'package.json': 'node:20-slim', 'pyproject.toml': 'python:3.12-slim', 'Cargo.toml': 'rust:bookworm', 'go.mod': 'golang:1.23' }[manifest];
      const setup = existsSync('package-lock.json') ? 'npm ci' : manifest === 'pyproject.toml' ? "pip install --user -e '.[test]'" : 'none';
      err(`image: ${image} · check: detected · setup: ${setup}`);
      if (manifest === 'package.json' && readFileSync('package.json', 'utf8').includes('axocoatl-gate-absent')) {
        err('axocoatl run: the check program is not in the image; exit 3 before any model call');
        return 3;
      }
      const health = await fetch(`http://127.0.0.1:${config.port}/health/live`).catch(() => null);
      if (!health?.ok) return 4;
      const messages = [{ role: 'user', content: flag('--task') }];
      for (let turn = 0; turn < 12; turn += 1) {
        const message = await chat(config.baseUrl, writer, messages, ['read_file', 'edit_file', 'write_file']);
        messages.push(message);
        if (!message.tool_calls?.length) break;
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
      const review = await chat(config.baseUrl, reviewer.replace(/^ollama:/, ''), [{ role: 'user', content: 'Give VERDICT: APPROVE or VERDICT: CHANGES' }], ['read_file']);
      const changed = spawnSync('git', ['status', '--porcelain'], { encoding: 'utf8' }).stdout.split('\n').filter((line) => /^ M /.test(line));
      const pass = /VERDICT: APPROVE/.test(review.content) && changed.length === 1;
      for (const [option, body] of [['--junit', '<testsuites/>\n'], ['--record', '{"section":"header"}\n'], ['--receipt', '{"payloadType":"fake"}\n']]) {
        if (flag(option)) writeFileSync(flag(option), body);
      }
      out(`Verdict: ${pass ? 'pass (exit 0)' : 'checks failed (exit 1)'}`);
      return pass ? 0 : 1;
    }
    default:
      err(`fake axocoatl: unsupported command ${command}`);
      return 2;
  }
}

process.exitCode = await main();
