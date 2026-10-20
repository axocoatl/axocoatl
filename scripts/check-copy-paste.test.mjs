// Tests for scripts/check-copy-paste.mjs. Run: node --test scripts/check-copy-paste.test.mjs
// The YAML tests need js-yaml, a devDependency of sites/docs that `npm ci` there installs.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  checkBreaks,
  checkClaims,
  checkShell,
  checkYaml,
  classifyYaml,
  htmlBlocks,
  lintRepository,
  logicalLines,
  markdownBlocks,
  shellWords,
} from './check-copy-paste.mjs';

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const script = join(scriptDirectory, 'check-copy-paste.mjs');
const yaml = createRequire(join(scriptDirectory, '../sites/docs/package.json'))('js-yaml');
const subcommands = new Set(['onboard', 'doctor', 'dev', 'run', 'receipt', 'record', 'validate', 'connect', 'recipe']);

function shellProblems(markdown, file = 'page.md') {
  return checkShell(markdownBlocks(file, markdown), subcommands);
}

test('shell words keep quoted text together and report unbalanced quotes', () => {
  assert.deepEqual(shellWords('axocoatl receipt verify r.json --key "$(axocoatl receipt key)" --repo .').words,
    ['axocoatl', 'receipt', 'verify', 'r.json', '--key', '$(axocoatl receipt key)', '--repo', '.']);
  assert.deepEqual(shellWords("echo 'a b' c # comment").words, ['echo', 'a b', 'c']);
  assert.match(shellWords('axocoatl run fix --task "Paging skips').error, /unbalanced double quote/);
});

test('logical lines join backslash continuations and flag a dangling one', () => {
  const lines = logicalLines('axocoatl run fix \\\n  --receipt r.json\naxocoatl doctor \\');
  assert.equal(lines.length, 2);
  assert.equal(lines[0].text, 'axocoatl run fix --receipt r.json');
  assert.equal(lines[1].dangling, true);
});

test('a flag on a line of its own is refused: a pasted copy runs it as a command', () => {
  const problems = shellProblems('```sh\naxocoatl run fix --task "Paging"\n--model reviewer=ollama:gpt-oss:20b\n```\n');
  assert.equal(problems.length, 1);
  assert.match(problems[0], /page\.md:3: this line starts with a flag/);
});

test('a continued flag line is accepted', () => {
  assert.deepEqual(shellProblems('```sh\naxocoatl run fix --task "Paging" \\\n  --model reviewer=ollama:gpt-oss:20b\n```\n'), []);
});

test('a backslash at the end of a block, or followed by spaces, is refused', () => {
  assert.match(shellProblems('```sh\naxocoatl doctor \\\n```\n')[0], /ends in a backslash with no command line after it/);
  assert.match(shellProblems('```sh\naxocoatl run fix \\  \n  --task "x"\n```\n')[0], /backslash followed by spaces/);
});

test('an unknown axocoatl subcommand is refused', () => {
  const problems = shellProblems('```sh\naxocoatl verify receipt.json\naxocoatl receipt verify receipt.json --key "$(axocoatl receipt key)"\n```\n');
  assert.equal(problems.length, 1);
  assert.match(problems[0], /axocoatl has no subcommand 'verify'/);
});

test('--keys needs a file an earlier command on the page creates', () => {
  assert.match(shellProblems('```sh\naxocoatl receipt verify r.json --keys trusted-keys --repo .\n```\n')[0],
    /--keys trusted-keys, but no earlier command on this page creates trusted-keys/);
  assert.deepEqual(shellProblems('```sh\naxocoatl receipt key > trusted-keys\n```\n\ntext\n\n```sh\naxocoatl receipt verify r.json --keys trusted-keys --repo .\n```\n'), []);
  assert.deepEqual(shellProblems('```sh\nmkdir -p .axocoatl && axocoatl receipt key >> .axocoatl/receipt-keys\naxocoatl receipt verify r.json --keys .axocoatl/receipt-keys\n```\n'), []);
  // A placeholder names no file.
  assert.deepEqual(shellProblems('```sh\naxocoatl receipt verify - --keys "$KEYS_FILE"\n```\n'), []);
});

test('a copy button must hold one runnable line', () => {
  const html = '<ax-cli-snippet>axocoatl run fix --task "…"</ax-cli-snippet>\n<ax-cli-snippet>axocoatl run fix\n--task "x"</ax-cli-snippet>\n<ax-cli-snippet>curl -fsSL https://axocoatl.ai/install.sh | sh</ax-cli-snippet>';
  const problems = checkShell(htmlBlocks('index.html', html), subcommands);
  assert.equal(problems.filter((problem) => /placeholder/.test(problem)).length, 1);
  assert.equal(problems.filter((problem) => /more than one line/.test(problem)).length, 1);
  assert.ok(problems.some((problem) => /index\.html:3: this line starts with a flag/.test(problem)), problems.join('\n'));
});

test('a <br> inside code, a pre block or a copy button is refused', () => {
  const html = [
    '<p>Local: <code class="wrap">axocoatl run fix --task "x" --model writer=a<br>--model reviewer=b</code></p>',
    '<pre><code>axocoatl doctor</code></pre>',
    '<p>Prose<br>with a break.</p>',
  ].join('\n');
  const problems = checkBreaks(htmlBlocks('index.html', html));
  assert.equal(problems.length, 1);
  assert.match(problems[0], /index\.html:1: <br> inside a code element/);
});

test('HTML pre blocks are read as shell, YAML or text', () => {
  const html = [
    '<div class="code-pane"><div class="code-pane-head">config.yaml · two Agents</div><pre><code><span class="code-key">agents</span>:',
    '  - id: lead</code></pre></div>',
    '<div class="code-pane"><div class="code-pane-head">first run</div><pre><code><span class="code-key">axocoatl onboard</span>  <span class="code-comment"># set up</span>',
    'axocoatl receipt verify r.json --key <span class="code-string">"$(axocoatl receipt key)"</span> --repo .</code></pre></div>',
    '<div class="code-pane"><div class="code-pane-head">real output</div><pre><code>Verdict: pass (exit 0)</code></pre></div>',
  ].join('\n');
  const kinds = htmlBlocks('index.html', html).filter((block) => block.kind !== 'inline').map((block) => block.kind);
  assert.deepEqual(kinds, ['yaml', 'shell', 'text']);
  const shell = htmlBlocks('index.html', html).find((block) => block.kind === 'shell');
  assert.equal(shell.text.split('\n')[1], 'axocoatl receipt verify r.json --key "$(axocoatl receipt key)" --repo .');
});

test('YAML must parse, and configuration Agents need an id and a name', () => {
  const blocks = markdownBlocks('page.mdx', [
    '```yaml', 'agents:', '  - id: lead', '    provider: ollama', '```',
    '',
    '{/* copy-paste: excerpt */}', '```yaml', 'agents:', '  - id: reviewer', '```',
    '',
    '```yaml', 'agents: [', '```',
    '',
    '```yaml', 'on: pull_request', 'jobs:', '  receipt:', '    runs-on: ubuntu-24.04', '```',
  ].join('\n'));
  const problems = checkYaml(blocks, { yaml, binary: null });
  assert.equal(problems.length, 2, problems.join('\n'));
  assert.match(problems[0], /page\.mdx:2: agents\[0\] has no name/);
  assert.match(problems[1], /page\.mdx:14: YAML does not parse/);
  assert.equal(blocks[1].excerpt, true);
});

test('a GitHub workflow snippet is a whole workflow file unless marked as an excerpt', () => {
  const blocks = markdownBlocks('receipts.mdx', [
    '```yaml', 'jobs:', '  receipt:', '    if: startsWith(github.head_ref, \'axocoatl/\')', '```',
    '',
    '```yaml', 'name: Receipt', 'on:', '  pull_request:', '```',
    '',
    '{/* copy-paste: excerpt */}', '```yaml', 'jobs:', '  receipt:', '    runs-on: ubuntu-24.04', '```',
    '',
    '```yaml', 'name: Receipt', 'on: pull_request', 'jobs:', '  receipt:', '    runs-on: ubuntu-24.04', '```',
  ].join('\n'));
  const problems = checkYaml(blocks, { yaml, binary: null });
  assert.equal(problems.length, 2, problems.join('\n'));
  assert.match(problems[0], /receipts\.mdx:2: a GitHub workflow without on:, which GitHub refuses/);
  assert.match(problems[1], /receipts\.mdx:8: a GitHub workflow without jobs:/);
});

test('YAML snippets are classified by what they configure', () => {
  assert.equal(classifyYaml({ schema: 'axocoatl.loadout/1', id: 'x' }), 'loadout');
  assert.equal(classifyYaml({ environment: { recipes: ['codex'] } }), 'loadout-fragment');
  assert.equal(classifyYaml({ on: { pull_request: {} }, jobs: {} }), 'workflow');
  assert.equal(classifyYaml({ agents: [] }), 'config');
  assert.equal(classifyYaml('text'), 'other');
});

test('"stronger" next to the 48 GB reviewer is refused outside measured blocks', () => {
  const unmeasured = 'Use `gpt-oss:20b` to review.\nA stronger reviewer catches more.\n';
  assert.match(checkClaims('README.md', unmeasured)[0], /README\.md:2: "stronger" within five lines of gpt-oss:20b/);
  const measured = '<!-- measured: x -->\nA stronger local model, gpt-oss 120B, helped.\n<!-- /measured -->\n`gpt-oss:20b` has not been measured.\n';
  assert.deepEqual(checkClaims('README.md', measured), []);
  assert.deepEqual(checkClaims('README.md', 'A stronger reviewer helped.\n\n\n\n\n\n\nUse gpt-oss:20b.\n'), []);
});

test('gpt-oss:120b needs its memory note', () => {
  assert.match(checkClaims('a.mdx', 'Pull `gpt-oss:120b` for review.\n')[0], /without its memory note/);
  assert.deepEqual(checkClaims('a.mdx', 'Pull `gpt-oss:120b` (65 GB),\nwhich needs about 96 GB of memory.\n'), []);
  assert.deepEqual(checkClaims('a.mdx', 'A cloud model such as `gpt-oss:120b-cloud` is refused.\n'), []);
});

test('withdrawn claims are refused', () => {
  for (const claim of [
    'Nothing leaves your machine.',
    'If the project stopped tomorrow, it would keep running.',
    'One command per task, from a shell or a CI job.',
    'Every finished run gets a receipt.',
  ]) {
    assert.equal(checkClaims('index.html', `${claim}\n`).length, 1, claim);
  }
  assert.deepEqual(checkClaims('index.html', 'Every finished loadout run gets a receipt.\n'), []);
});

test('unverified platforms need their label on the install surfaces', () => {
  assert.match(checkClaims('README.md', 'Runs on macOS, Linux or WSL2.\n', { platform: true })[0], /WSL2 named without "not yet verified"/);
  assert.deepEqual(checkClaims('README.md', 'Runs inside WSL2 (not yet verified).\n', { platform: true }), []);
  const section = '## Windows through WSL2\n\nWSL2 is not yet verified.\n\n1. a\n2. b\n3. c\n4. d\n5. Run it inside WSL2.\n\n## Next\n\nWSL2 again.\n';
  const problems = checkClaims('install.mdx', section, { platform: true });
  assert.equal(problems.length, 1, problems.join('\n'));
  assert.match(problems[0], /install\.mdx:13:/);
  // Other pages may name WSL2 freely, as in a table of configuration paths.
  assert.deepEqual(checkClaims('config.mdx', 'Linux and WSL2: ~/.config/axocoatl\n'), []);
});

function fixtureRoot(files) {
  const root = mkdtempSync(join(tmpdir(), 'check-copy-paste-'));
  const all = {
    'axocoatl-cli/src/main.rs': 'enum Commands {\n    Onboard,\n    Doctor,\n    Dev {\n    },\n    Run(RunArgs),\n    Receipt {\n    },\n}\n',
    ...files,
  };
  for (const [path, contents] of Object.entries(all)) {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    writeFileSync(join(root, path), contents);
  }
  return root;
}

test('the repository check finds each problem with its file and line, and passes once they are fixed', () => {
  const broken = fixtureRoot({
    'README.md': '```bash\ncurl -fsSL https://axocoatl.ai/install.sh | sh   # macOS, Linux, or WSL2\naxocoatl run fix --task "Paging"\n--model reviewer=ollama:gpt-oss:20b\n```\n',
    'sites/marketing/index.html': '<pre><code>axocoatl receipt verify receipt.json --keys trusted-keys --repo .</code></pre>\n',
  });
  const fixed = fixtureRoot({
    'README.md': '```bash\ncurl -fsSL https://axocoatl.ai/install.sh | sh   # macOS, Linux, or inside WSL2 (not yet verified)\naxocoatl run fix --task "Paging" \\\n  --model reviewer=ollama:gpt-oss:20b\n```\n',
    'sites/marketing/index.html': '<pre><code>axocoatl receipt verify receipt.json --key "$(axocoatl receipt key)" --repo .</code></pre>\n',
  });
  try {
    const { problems } = lintRepository(broken, { yaml });
    assert.equal(problems.length, 3, problems.join('\n'));
    assert.ok(problems.some((problem) => problem.startsWith('README.md:4: this line starts with a flag')));
    assert.ok(problems.some((problem) => problem.startsWith('sites/marketing/index.html:1: --keys trusted-keys')));
    assert.ok(problems.some((problem) => problem.startsWith('README.md:2: WSL2 named without')));
    assert.deepEqual(lintRepository(fixed, { yaml }).problems, []);

    const failing = spawnSync(process.execPath, [script, '--root', broken], { encoding: 'utf8' });
    assert.equal(failing.status, 1);
    assert.match(failing.stderr, /Copy-paste check failed \(3\)/);
    const passing = spawnSync(process.execPath, [script, '--root', fixed, '--json'], { encoding: 'utf8' });
    assert.equal(passing.status, 0, passing.stderr);
    const report = JSON.parse(passing.stdout);
    assert.deepEqual(report.problems, []);
    const readme = report.blocks.find((block) => block.file === 'README.md');
    assert.deepEqual(readme.commands, [
      'curl -fsSL https://axocoatl.ai/install.sh | sh   # macOS, Linux, or inside WSL2 (not yet verified)',
      'axocoatl run fix --task "Paging" --model reviewer=ollama:gpt-oss:20b',
    ]);
  } finally {
    rmSync(broken, { recursive: true, force: true });
    rmSync(fixed, { recursive: true, force: true });
  }
});

test('run through a symlinked path, the program still checks and fails', () => {
  const broken = fixtureRoot({
    'README.md': '```sh\naxocoatl receipt verify r.json --keys nowhere-file\naxocoatl frobnicate\n```\n',
  });
  const links = mkdtempSync(join(tmpdir(), 'check-copy-paste-link-'));
  try {
    const linked = join(links, 'check-copy-paste.mjs');
    symlinkSync(script, linked);
    const failing = spawnSync(process.execPath, [linked, '--root', broken], { encoding: 'utf8' });
    assert.equal(failing.status, 1, failing.stdout + failing.stderr);
    assert.match(failing.stderr, /Copy-paste check failed \(2\)/);
    const json = spawnSync(process.execPath, [linked, '--root', broken, '--json'], { encoding: 'utf8' });
    assert.equal(json.status, 1);
    assert.equal(JSON.parse(json.stdout).problems.length, 2);
  } finally {
    rmSync(broken, { recursive: true, force: true });
    rmSync(links, { recursive: true, force: true });
  }
});

test('the repository itself passes', () => {
  const { problems } = lintRepository(join(scriptDirectory, '..'), { yaml });
  assert.deepEqual(problems, []);
});
