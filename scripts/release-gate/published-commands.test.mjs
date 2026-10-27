// node --test scripts/release-gate/published-commands.test.mjs
import assert from 'node:assert/strict';
import path from 'node:path';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  commandsOf, decodeEntities, firstCommand, htmlBlocks, installCommand, installPage, logicalLines, outputPath,
  missingCommands, readmeRunCommand, stripComment, yamlBlock,
} from './published-commands.mjs';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');

test('continuations join into one command with single spaces, and trailing comments go', () => {
  assert.deepEqual(logicalLines('axocoatl run fix --task "a # b" \\\n  --model x \\\n  --junit j   # writes JUnit\ncd ~/src/app\n'), [
    'axocoatl run fix --task "a # b" --model x --junit j',
    'cd ~/src/app',
  ]);
  assert.equal(stripComment("echo 'a#b' # c"), "echo 'a#b'");
  assert.equal(stripComment('curl -fsSL https://axocoatl.ai/install.sh | sh   # macOS 11+'), 'curl -fsSL https://axocoatl.ai/install.sh | sh');
  assert.equal(stripComment('url#fragment'), 'url#fragment');
});

test('HTML command blocks lose their markup, keep <br> as the line break a copy gets, and decode entities', () => {
  const blocks = htmlBlocks('<p><code>axocoatl inline</code></p><pre><code><span class="code-key">axocoatl run fix</span> --task &quot;x&quot; \\\n  --model a</code></pre><ax-cli-snippet>curl -fsSL https://axocoatl.ai/install.sh | sh</ax-cli-snippet><pre>axocoatl run fix --task "y"<br>--model b</pre>');
  assert.deepEqual(blocks.map((block) => block.lang), ['pre', 'snippet', 'pre']);
  assert.deepEqual(commandsOf('page.html', '<pre><code><span>axocoatl run fix</span> --task &quot;x&quot; \\\n  --model a</code></pre><pre>axocoatl run fix --task "y"<br>--model b</pre>'), [
    'axocoatl run fix --task "x" --model a',
    'axocoatl run fix --task "y"',
  ]);
  assert.equal(decodeEntities('&lt;&#39;&#x27;&hellip;&amp;'), "<''…&");
});

test('the README run command is the one a new user copies, on one line', () => {
  const command = readmeRunCommand(path.join(REPO, 'README.md'));
  assert.match(command, /^axocoatl run fix --task "[^"]+"/);
  assert.ok(!command.includes('\n') && !command.includes('\\'), command);
  assert.ok(!/^axocoatl run fix-/.test(command));
  assert.throws(() => readmeRunCommand('x.md', '# nothing\n\n```bash\naxocoatl dev\n```\n'), /no `axocoatl run fix` command/);
});

test('the home page links to /install, whose copyable commands are the actual setup sequence', () => {
  const page = installPage(path.join(REPO, 'sites/marketing/index.html'));
  assert.equal(page, path.join(REPO, 'sites/marketing/install/index.html'));
  assert.equal(firstCommand(page, 'curl'), 'curl -fsSL https://axocoatl.ai/install.sh | sh');
  assert.deepEqual(missingCommands(page, ['axocoatl onboard --start none', 'axocoatl doctor', 'axocoatl dev']), []);
});

test('the install link must be a real local anchor and its staged target must exist', () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'axo-published-link-'));
  try {
    const home = path.join(dir, 'index.html');
    const install = path.join(dir, 'install/index.html');
    assert.throws(() => installPage(home, '<a href="/install">Install</a>'), /ENOENT/);
    mkdirSync(path.dirname(install));
    writeFileSync(install, '<pre>curl -fsSL https://axocoatl.ai/install.sh | sh</pre>');
    for (const href of ['/install', '/install/', '/install/index.html', '/install#what-it-needs']) {
      assert.equal(installPage(home, `<a class="btn" href="${href}"><span>Install</span></a>`), install);
    }
    for (const source of [
      '<a href="https://other.example/install">Install</a>', '<a href="/installing">Install</a>',
      '<a data-href="/install">Install</a>', '<a href="/install"></a>', '<p>/install</p>',
      '<!-- <a href="/install">Install</a> -->', '<script>"<a href="/install">Install</a>"</script>',
      '<template><a href="/install">Install</a></template>',
    ]) assert.throws(() => installPage(home, source), /no local link to \/install/);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('the Mac staged-site prerequisite refuses missing links, installers or exact setup commands before installing', () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'axo-published-flow-'));
  const gateDir = path.join(REPO, 'scripts/release-gate');
  const shellFunction = (file, name) => {
    const source = readFileSync(path.join(gateDir, file), 'utf8');
    const found = source.match(new RegExp('^' + name + '\\(\\) \\{[\\s\\S]*?^\\}', 'm'))?.[0];
    assert.ok(found, `actual ${name} function must be exercised`);
    return found;
  };
  const checker = shellFunction('newuser-mac.sh', 'site_setup_commands');
  const check = shellFunction('lib.sh', 'gate_check');
  const script = `${check}\n${checker}\n` + String.raw`
set -u
site_page=$1/index.html
site_install_page=$1/install/index.html
gate_commands_js=$2/published-commands.mjs
install_command=$(node "$gate_commands_js" install "$site_install_page" 2>/dev/null) || install_command=''
gate_record() { printf '%s: %s\n' "$2" "$1"; [ "$2" = pass ]; }
site_setup_commands || exit $?
printf 'INSTALL_REACHED\n'
`;
  const setup = '<pre>curl -fsSL https://axocoatl.ai/install.sh | sh\naxocoatl onboard --start none\naxocoatl doctor\naxocoatl dev</pre>';
  try {
    mkdirSync(path.join(dir, 'install'));
    for (const [name, home, page, passes] of [
      ['linked setup', '<a href="/install">Install</a>', setup, true],
      ['no link', '<p>Install</p>', setup, false],
      ['no installer', '<a href="/install">Install</a>', setup.replace('curl -fsSL https://axocoatl.ai/install.sh | sh', ''), false],
      ['foreground onboarding', '<a href="/install">Install</a>', setup.replace(' --start none', ''), false],
      ['missing doctor', '<a href="/install">Install</a>', setup.replace('axocoatl doctor', ''), false],
      ['different dev flags', '<a href="/install">Install</a>', setup.replace('axocoatl dev', 'axocoatl dev --port 9000'), false],
    ]) {
      writeFileSync(path.join(dir, 'index.html'), home);
      writeFileSync(path.join(dir, 'install/index.html'), page);
      const result = spawnSync('/bin/bash', ['-c', script, 'published-flow', dir, gateDir], { encoding: 'utf8' });
      assert.equal(result.status, passes ? 0 : 1, `${name}: ${result.stdout} ${result.stderr}`);
      assert.equal(result.stdout.includes('INSTALL_REACHED'), passes, name);
    }
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test('the install command is the curl piped to sh that the README and the site show', () => {
  assert.equal(installCommand(path.join(REPO, 'README.md')), 'curl -fsSL https://axocoatl.ai/install.sh | sh');
  assert.equal(installCommand(installPage(path.join(REPO, 'sites/marketing/index.html'))), 'curl -fsSL https://axocoatl.ai/install.sh | sh');
  // Another download first, or an install line that saves the script, is not the command.
  assert.equal(installCommand('a.md', '```bash\ncurl -fsSLO https://example.com/x.tar.gz\ncurl -fsSL https://axocoatl.ai/install.sh | bash\n```\n'),
    'curl -fsSL https://axocoatl.ai/install.sh | bash');
  assert.throws(() => installCommand('a.md', '```bash\ncurl -fsSL https://axocoatl.ai/install.sh -o install.sh\n```\n'), /no `curl … install.sh \| sh`/);
});

test('missing commands are named, and a command must be shown exactly', () => {
  const source = '```bash\naxocoatl onboard\naxocoatl doctor   # check\naxocoatl dev --port 9000\n```\n';
  assert.deepEqual(missingCommands('a.md', ['axocoatl onboard', 'axocoatl doctor', 'axocoatl dev'], source), ['axocoatl dev']);
  assert.deepEqual(missingCommands(path.join(REPO, 'README.md'), ['axocoatl onboard --start none', 'axocoatl doctor', 'axocoatl dev']), []);
});

test('Markdown fences of other languages are not commands, and a yaml block can be taken by its content', () => {
  const source = '```yaml\nname: Verify receipt\nuses: axocoatl/axocoatl/actions/verify-receipt@v1.5.0\n```\n\n```text\naxocoatl not a command here\n```\n\n```bash\n$ axocoatl doctor\n```\n';
  assert.deepEqual(commandsOf('doc.mdx', source), ['axocoatl doctor']);
  assert.match(yamlBlock('doc.mdx', 'verify-receipt', source), /^name: Verify receipt\n/);
  assert.throws(() => yamlBlock('doc.mdx', 'missing', source), /no yaml block/);
});


test('run output paths resolve only the documented evidence variable, never arbitrary shell expressions', () => {
  const repo = '/tmp/example checkout';
  const evidence = '/tmp/example evidence';
  assert.equal(outputPath('axocoatl run fix --record "$AXO_EVIDENCE/run.jsonl"', '--record', repo, evidence), `${evidence}/run.jsonl`);
  assert.equal(outputPath('axocoatl run fix --record "${AXO_EVIDENCE}/run.jsonl"', '--record', repo, evidence), `${evidence}/run.jsonl`);
  assert.equal(outputPath("axocoatl run fix --record 'a record.jsonl'", '--record', repo, evidence), `${repo}/a record.jsonl`);
  assert.equal(outputPath('axocoatl run fix --record r.jsonl', '--record', repo, evidence), `${repo}/r.jsonl`);
  for (const command of [
    'axocoatl run fix', 'axocoatl run fix --record a --record b',
    'axocoatl run fix --record "$OTHER/r.jsonl"', 'axocoatl run fix --record "$(touch /tmp/not-run)"',
    'axocoatl run fix --record "`touch /tmp/not-run`"', "axocoatl run fix --record '$AXO_EVIDENCE/r.jsonl'",
  ]) assert.throws(() => outputPath(command, '--record', repo, evidence), /expected exactly one|unsupported shell expression/);
});

test('the actual fixture runner binds README evidence outside the checkout and verifies those exact files', () => {
  const dir = mkdtempSync(path.join(tmpdir(), 'axo published output '));
  const gateDir = path.join(REPO, 'scripts/release-gate');
  const repo = path.join(dir, 'fixture checkout');
  const calls = path.join(dir, 'calls.jsonl');
  const published = readmeRunCommand(path.join(REPO, 'README.md'));
  try {
    mkdirSync(path.join(dir, 'bin'));
    // This stand-in writes only requested outputs and checks verifier inputs;
    // it cannot contact a daemon, model provider or account.
    const fake = `#!${process.execPath}\n` + String.raw`
const fs = require('node:fs');
const path = require('node:path');
const args = process.argv.slice(2);
fs.appendFileSync(process.env.FAKE_CALLS, JSON.stringify({args, cwd: process.cwd(), evidence: process.env.AXO_EVIDENCE}) + '\n');
const flag = name => args[args.indexOf(name) + 1];
if (args[0] === 'run') {
  for (const name of ['--junit', '--record', '--receipt']) {
    const output = flag(name);
    if (!output || !path.isAbsolute(output) || output.startsWith(process.cwd() + path.sep)) process.exit(91);
    fs.writeFileSync(output, 'synthetic ' + name + '\n');
  }
} else if (args[0] === 'receipt' && args[1] === 'key') {
  console.log('synthetic public key');
} else if (args[0] === 'receipt' && args[1] === 'verify') {
  if (!fs.existsSync(args[2]) || !fs.existsSync(flag('--record'))) process.exit(92);
} else if (args[0] === 'record' && args[1] === 'verify') {
  if (!fs.existsSync(args[2])) process.exit(93);
} else process.exit(94);
`;
    writeFileSync(path.join(dir, 'bin/axocoatl'), fake, { mode: 0o755 });
    const script = String.raw`
set -u
. "$1/lib.sh"
export PATH="$2/bin:$PATH" FAKE_CALLS="$2/calls.jsonl"
gate_begin "$2/evidence" 1.5.0 cccccccccccccccccccccccccccccccccccccccc A-published A
gate_fixture_expect() { :; }
gate_markup_check() { test -f "$3" && test -f "$4" && test -f "$5"; }
gate_run_fixture node-deps "$4" "$3" readme-node-deps
`;
    const result = spawnSync('/bin/bash', ['-c', script, 'published-outputs', gateDir, dir, repo, published], { encoding: 'utf8' });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    const observed = readFileSync(calls, 'utf8').trim().split('\n').map(JSON.parse);
    const run = observed.find(call => call.args[0] === 'run');
    assert.ok(run.evidence && !run.evidence.startsWith(repo + path.sep));
    const receipt = path.join(run.evidence, 'receipt.json');
    const record = path.join(run.evidence, 'run.axorecord.jsonl');
    const verifications = observed.filter(call => call.args[0] === 'receipt' && call.args[1] === 'verify');
    assert.equal(verifications.length, 2);
    for (const call of verifications) {
      assert.equal(call.args[2], receipt);
      assert.equal(call.args[call.args.indexOf('--record') + 1], record);
    }
    assert.ok(verifications[0].args.includes('--repo') && verifications[0].args.includes('--require-pass'));
    assert.equal(observed.find(call => call.args[0] === 'record').args[2], record);
    const kept = path.join(dir, 'evidence/1.5.0/cccccccccccccccccccccccccccccccccccccccc/A-published/readme-node-deps');
    assert.equal(readFileSync(path.join(kept, 'command.txt'), 'utf8').trim(), published);
    for (const file of ['junit.xml', 'run.axorecord.jsonl', 'receipt.json']) {
      assert.equal(readFileSync(path.join(kept, file), 'utf8'), readFileSync(path.join(run.evidence, file), 'utf8'));
    }
    assert.equal(readFileSync(path.join(kept, 'changes.txt'), 'utf8'), '');
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
