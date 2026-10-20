// node --test scripts/release-gate/published-commands.test.mjs
import assert from 'node:assert/strict';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  commandsOf, decodeEntities, firstCommand, htmlBlocks, logicalLines, readmeRunCommand,
  stripComment, yamlBlock,
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

test('the published install command is found on the home page and /install', () => {
  for (const page of ['sites/marketing/index.html', 'sites/marketing/install/index.html']) {
    assert.equal(firstCommand(path.join(REPO, page), 'curl'), 'curl -fsSL https://axocoatl.ai/install.sh | sh', page);
  }
});

test('Markdown fences of other languages are not commands, and a yaml block can be taken by its content', () => {
  const source = '```yaml\nname: Verify receipt\nuses: axocoatl/axocoatl/actions/verify-receipt@v1.5.0\n```\n\n```text\naxocoatl not a command here\n```\n\n```bash\n$ axocoatl doctor\n```\n';
  assert.deepEqual(commandsOf('doc.mdx', source), ['axocoatl doctor']);
  assert.match(yamlBlock('doc.mdx', 'verify-receipt', source), /^name: Verify receipt\n/);
  assert.throws(() => yamlBlock('doc.mdx', 'missing', source), /no yaml block/);
});
