import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'node:http';
import { chmod, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { browserEngines, launchTestDaemon, newAuthorizedContext } from '../support/daemon.mjs';

// The workbench's first screen says what the first Session still needs.
// The daemon here sees a Podman that is installed but has no machine (a
// shim on its PATH that runs nothing), and an Ollama server that reports
// 0.20.6, a version native Sessions refuse.
const engines = await browserEngines();
let runtime;
let ollama;
let shims;

async function fakeOllama(version) {
  const server = createServer((req, res) => {
    const json = (body) => { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
    if (req.url === '/api/version') return json({ version });
    if (req.url === '/api/status') return json({ cloud: { disabled: true, source: 'config' } });
    if (req.url === '/api/tags') return json({ models: [{ name: 'browser-test-model:latest' }] });
    res.writeHead(404); res.end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return server;
}

before(async () => {
  ollama = await fakeOllama('0.20.6');
  shims = await mkdtemp(path.join(tmpdir(), 'axocoatl-readiness-shims-'));
  await mkdir(path.join(shims, 'bin'));
  const podman = path.join(shims, 'bin', 'podman');
  await writeFile(podman, `#!/bin/sh
case "$*" in
  --version) echo 'podman version 5.6.0' ;;
  'machine list --format json') echo '[]' ;;
  *) echo 'Error: unable to connect to Podman socket' >&2; exit 125 ;;
esac
`);
  await chmod(podman, 0o755);
  runtime = await launchTestDaemon({
    ollamaBaseUrl: `http://127.0.0.1:${ollama.address().port}`,
    env: { PATH: `${path.join(shims, 'bin')}:${process.env.PATH}` },
  });
});

after(async () => {
  await runtime?.stop();
  await new Promise((resolve) => ollama?.close(resolve));
  if (shims) await rm(shims, { recursive: true, force: true });
});

test('GET /api/readiness names a missing Podman machine and a refused Ollama version', async () => {
  const response = await fetch(`${runtime.baseUrl}/api/readiness`);
  assert.equal(response.status, 200);
  const report = await response.json();
  assert.equal(report.ready, false);
  const podman = report.sections.sandbox.find((item) => item.id === 'podman');
  if (process.platform === 'darwin') {
    assert.equal(podman.status, 'fail');
    assert.match(podman.fix, /axocoatl onboard/);
    assert.match(report.sessions_blocked, /no machine/);
  } else {
    assert.equal(podman.status, 'fail', 'the shim answers no readiness probe');
  }
  const version = report.sections.models.find((item) => item.id === 'ollama.version');
  assert.equal(version.status, 'fail');
  assert.match(version.detail, /reports version 0\.20\.6; native Sessions refuse it/);
  assert.ok(['sandbox', 'models', 'writers', 'daemon'].every((key) => Array.isArray(report.sections[key])));
  const health = await (await runtime.fetchWithoutToken('/health')).json();
  assert.match(health.version, /^\d+\.\d+\.\d+/);
  assert.equal(report.version, health.version);
  // The readiness report is not public.
  assert.equal((await runtime.fetchWithoutToken('/api/readiness')).status, 401);
});

for (const engine of engines) {
  test(`${engine.name}: the first screen lists what is left, with the exact commands, and blocks Create session`, async () => {
    const browser = await engine.launch();
    let context;
    try {
      context = await newAuthorizedContext(browser, { viewport: { width: 1280, height: 860 } });
      const page = await context.newPage();
      const errors = [];
      page.on('pageerror', (error) => errors.push(error.message));
      await page.goto(`${runtime.baseUrl}/`, { waitUntil: 'domcontentloaded' });
      const checklist = page.locator('#readiness');
      await page.waitForFunction(() => document.querySelector('#readiness')?.report);
      // The fixture has Sessions, so the app reopens one; the checklist
      // belongs to the home surface a new user lands on.
      await page.waitForFunction(() => typeof closeCockpit === 'function' && customElements.get('ax-rail'));
      await page.evaluate(() => closeCockpit({ homeScope: 'all' }));
      await checklist.locator('.inline h2').waitFor();
      assert.match(await checklist.locator('.inline h2').textContent(), /steps? left before your first Session/);
      const version = checklist.locator('.inline li[data-id="ollama.version"]');
      assert.equal(await version.getAttribute('data-status'), 'fail');
      assert.match(await version.textContent(), /reports version 0\.20\.6; native Sessions refuse it/);
      assert.match(await version.locator('.fix-text').textContent(), /Install Ollama/);
      assert.equal(await version.locator('.fix-text code').textContent(), 'axocoatl doctor');
      const podman = checklist.locator('.inline li[data-id="podman"]');
      assert.equal(await podman.getAttribute('data-status'), 'fail');
      if (process.platform === 'darwin') {
        assert.equal(await podman.locator('.fix-text code').first().textContent(), 'axocoatl onboard');
      }
      // No default model is suggested anywhere.
      assert.doesNotMatch(await checklist.textContent(), /llama3\.2/);

      // The rail pill is red and opens the same checklist.
      const pill = page.locator('#status-pearls .pearl[data-key="health"]');
      await page.waitForFunction(() => /setup steps? left/.test(document.querySelector('#status-pearls .pearl[data-key="health"]')?.textContent || ''));
      assert.equal(await pill.locator('.pearl-dot').getAttribute('class'), 'pearl-dot err');
      await pill.click();
      const dialog = checklist.locator('dialog');
      await dialog.waitFor({ state: 'visible' });
      assert.equal(await dialog.locator('li[data-id="ollama.version"]').getAttribute('data-status'), 'fail');
      await dialog.getByRole('button', { name: 'Close', exact: true }).click();
      await dialog.waitFor({ state: 'hidden' });

      if (process.platform === 'darwin') {
        // Without a Podman machine, Create session is blocked with the fix.
        const workspace = runtime.fixtures.alpha.workspace;
        await page.evaluate((id) => document.querySelector('ax-session-home').newSession(id), workspace.id);
        const blocked = page.locator('ax-session-home .picker-blocked');
        await blocked.waitFor();
        assert.match(await blocked.textContent(), /No Session can run yet/);
        assert.match(await blocked.textContent(), /axocoatl onboard/);
        assert.equal(await page.locator('ax-session-home [data-action="picker-use"]').isDisabled(), true);
      }
      assert.deepEqual(errors, []);
    } finally {
      await context?.close();
      await browser.close();
    }
  });
}
