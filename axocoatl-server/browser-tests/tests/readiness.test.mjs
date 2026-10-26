import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'node:http';
import { constants } from 'node:fs';
import { access, chmod, mkdir, mkdtemp, readFile, readdir, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { browserEngines, isWebKitLoadDiagnostic, launchTestDaemon, newAuthorizedContext } from '../support/daemon.mjs';

// The workbench's first screen says what the first Session still needs.
// The daemon here sees a Podman it cannot use, and an Ollama server that
// reports 0.20.6, a version native Sessions refuse. On macOS that Podman is
// installed but has no machine (a shim on its PATH that runs nothing). On
// Linux Podman needs no machine, and a Podman that does not answer
// `podman info` stops the daemon's start (it cannot prove that no old
// Session container exposes its data), so the state a new user reaches this
// screen in is Podman not installed: the daemon's PATH is the test's with
// every `podman` left out.
const engines = await browserEngines();
const macos = process.platform === 'darwin';
let runtime;
let ollama;
let shims;
let podmanFix;

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

/**
 * What onboarding does about a missing Podman on this Linux, from
 * `/etc/os-release` (`InstallRoute::for_linux` in axocoatl-isolation): the
 * readiness item's whole fix, so a repeated sentence or an onboarding that
 * claims to install Podman where it cannot fails here.
 */
function linuxInstallFix(osRelease) {
  const field = (key) => {
    const line = osRelease.split('\n').find((entry) => entry.startsWith(`${key}=`));
    return (line ? line.slice(key.length + 1) : '').trim().replace(/^["']|["']$/g, '');
  };
  const id = field('ID').toLowerCase();
  const like = field('ID_LIKE').toLowerCase().split(/\s+/).filter(Boolean);
  const is = (family) => id === family || like.includes(family);
  const onboard = (command) => `Run \`axocoatl onboard\` (it installs Podman after asking), or: \`${command}\`.`;
  if (id === 'ubuntu' && Number.parseInt(field('VERSION_ID').split('.')[0], 10) <= 22) {
    return "This Ubuntu's Podman is too old (22.04 ships 3.4.4; Axocoatl runs need 4.3 or later, 4.9 tested): use Ubuntu 24.04 or newer.";
  }
  if (is('debian') || is('ubuntu')) return onboard('sudo apt-get update && sudo apt-get install -y podman');
  if (is('fedora') || is('rhel') || is('centos')) return onboard('sudo dnf install -y podman');
  if (is('arch')) return onboard('sudo pacman -S --noconfirm podman');
  if (is('suse') || id.startsWith('opensuse') || is('sles')) return onboard('sudo zypper install -y podman');
  return 'Install Podman 4.9 or later (https://podman.io/docs/installation), then run `axocoatl onboard`.';
}

/** The Podman item's whole fix: on macOS the machine, sized for this computer. */
function assertPodmanFix(fix) {
  if (macos) {
    assert.match(fix, /^Run `axocoatl onboard` \(it creates the machine after asking\), or: `podman machine init --cpus \d+ --memory \d+ --disk-size 60 && podman machine start`\.$/);
  } else {
    assert.equal(fix, podmanFix);
  }
}

before(async () => {
  if (!macos) podmanFix = linuxInstallFix(await readFile('/etc/os-release', 'utf8'));
  ollama = await fakeOllama('0.20.6');
  shims = await mkdtemp(path.join(tmpdir(), 'axocoatl-readiness-shims-'));
  runtime = await launchTestDaemon({
    ollamaBaseUrl: `http://127.0.0.1:${ollama.address().port}`,
    env: { PATH: macos ? await pathWithMachinelessPodman(shims) : await pathWithoutPodman(shims) },
  });
});

/** The test's PATH behind a Podman that is installed but has no machine. */
async function pathWithMachinelessPodman(root) {
  await mkdir(path.join(root, 'bin'));
  const podman = path.join(root, 'bin', 'podman');
  await writeFile(podman, `#!/bin/sh
case "$*" in
  --version) echo 'podman version 5.6.0' ;;
  'machine list --format json') echo '[]' ;;
  *) echo 'Error: unable to connect to Podman socket' >&2; exit 125 ;;
esac
`);
  await chmod(podman, 0o755);
  return `${path.join(root, 'bin')}${path.delimiter}${process.env.PATH}`;
}

/**
 * The test's PATH with no `podman` on it: each directory that holds one is
 * replaced by a directory of links to everything else it holds.
 */
async function pathWithoutPodman(root) {
  const directories = [];
  for (const [index, directory] of (process.env.PATH || '').split(path.delimiter).entries()) {
    let entries;
    try { entries = await readdir(directory); } catch { continue; }
    if (!entries.includes('podman')) { directories.push(directory); continue; }
    const mirror = path.join(root, `path-${index}`);
    await mkdir(mirror);
    for (const entry of entries.filter((name) => name !== 'podman')) {
      await symlink(path.join(directory, entry), path.join(mirror, entry));
    }
    directories.push(mirror);
  }
  return directories.join(path.delimiter);
}

after(async () => {
  await runtime?.stop();
  await new Promise((resolve) => ollama?.close(resolve));
  if (shims) await rm(shims, { recursive: true, force: true });
});

test('GET /api/readiness names a missing Podman (no machine on macOS, not installed on Linux) and a refused Ollama version', async () => {
  const response = await fetch(`${runtime.baseUrl}/api/readiness`);
  assert.equal(response.status, 200);
  const report = await response.json();
  assert.equal(report.ready, false);
  const podman = report.sections.sandbox.find((item) => item.id === 'podman');
  assert.equal(podman.status, 'fail', JSON.stringify(podman));
  assertPodmanFix(podman.fix);
  assert.match(report.sessions_blocked, macos ? /has no machine/ : /Podman is not installed/);
  assert.equal(report.sessions_blocked, podman.detail);
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

/** What a Mac's first screen says about a Podman that is not installed. */
async function macInstallFix(searchPath) {
  if (process.arch !== 'arm64') return null;
  const candidates = [
    ...searchPath.split(path.delimiter).filter(Boolean).map((directory) => path.join(directory, 'brew')),
    '/opt/homebrew/bin/brew',
    '/usr/local/bin/brew',
  ];
  let homebrew = false;
  for (const candidate of candidates) {
    try { await access(candidate, constants.X_OK); homebrew = true; break; } catch {}
  }
  return homebrew
    ? 'Run `axocoatl onboard` (it installs Podman after asking), or: `brew install podman`.'
    : 'Install Podman with the installer from https://podman.io (or install Homebrew and run `brew install podman`), then run `axocoatl onboard`.';
}

// The fixture daemon on macOS has a Podman with no machine; a second one
// here has none on its PATH, the state of a Mac before onboarding.
test('a Mac without Podman gets the install fix for this Mac', { skip: !macos && 'Linux: the daemon above has no Podman' }, async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'axocoatl-readiness-nopodman-'));
  let notInstalled;
  try {
    const searchPath = await pathWithoutPodman(root);
    notInstalled = await launchTestDaemon({
      ollamaBaseUrl: `http://127.0.0.1:${ollama.address().port}`,
      env: { PATH: searchPath },
    });
    const report = await (await fetch(`${notInstalled.baseUrl}/api/readiness`)).json();
    const podman = report.sections.sandbox.find((item) => item.id === 'podman');
    assert.equal(podman.status, 'fail', JSON.stringify(podman));
    assert.equal(podman.detail, 'Podman is not installed. Every Session runs in a Podman sandbox.');
    assert.equal(report.sessions_blocked, podman.detail);
    const expected = await macInstallFix(searchPath);
    if (expected) {
      assert.equal(podman.fix, expected);
    } else {
      assert.match(podman.fix, /^Podman 6 no longer supports Intel Macs and Homebrew refuses it: install the Podman 5\.8 amd64 package from https:\/\/github\.com\/containers\/podman\/releases \(Axocoatl has not been tested on Intel Macs\), then run `axocoatl onboard`\.$/);
    }
  } finally {
    await notInstalled?.stop();
    await rm(root, { recursive: true, force: true });
  }
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
      // The whole fix as the checklist shows it: each command once, copyable.
      const report = await page.evaluate(() => document.querySelector('#readiness').report);
      const fix = report.sections.sandbox.find((item) => item.id === 'podman').fix;
      assertPodmanFix(fix);
      assert.deepEqual(
        await podman.locator('.fix-text code').allTextContents(),
        fix.split('`').filter((_, index) => index % 2 === 1),
      );
      assert.equal(await podman.locator('.fix-text code').first().textContent(), 'axocoatl onboard');
      assert.equal(await podman.locator('.fix-text').evaluate((text) => {
        const copy = text.cloneNode(true);
        for (const button of copy.querySelectorAll('button')) button.remove();
        return copy.textContent;
      }), fix.replaceAll('`', ''));
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

      // Without a usable Podman, Create session is blocked with the fix.
      const workspace = runtime.fixtures.alpha.workspace;
      await page.evaluate((id) => document.querySelector('ax-session-home').newSession(id), workspace.id);
      const blocked = page.locator('ax-session-home .picker-blocked');
      await blocked.waitFor();
      assert.match(await blocked.textContent(), /No Session can run yet/);
      assert.match(await blocked.textContent(), macos ? /has no machine/ : /Podman is not installed/);
      assert.equal(await blocked.locator('code').textContent(), fix.replaceAll('`', ''));
      assert.equal(await page.locator('ax-session-home [data-action="picker-use"]').isDisabled(), true);
      assert.deepEqual(errors, []);
    } finally {
      await context?.close();
      await browser.close();
    }
  });

  // The New Session picker reads readiness again when it opens. Until that
  // read is in, nothing says whether Podman can run a Session, so Create
  // session waits for it, for at most ten seconds; a read that fails blocks
  // nothing (the daemon refuses a Session it cannot prepare), and a read that
  // comes late still blocks. Typing a name never enables a Create session
  // that readiness or anything else holds.
  test(`${engine.name}: Create session waits for the picker's readiness read, and typing a name does not enable a blocked one`, { timeout: 120_000 }, async () => {
    const browser = await engine.launch();
    let context;
    try {
      context = await newAuthorizedContext(browser, { viewport: { width: 1280, height: 860 } });
      const page = await context.newPage();
      const errors = [];
      // WebKit reports the readiness reads this journey aborts as page errors.
      page.on('pageerror', (error) => {
        if (!isWebKitLoadDiagnostic(error)) errors.push(error.message);
      });
      let readiness = 'fail';
      const held = [];
      await page.route('**/api/readiness', (route) => {
        if (readiness === 'fail') return route.abort();
        if (readiness === 'pass') return route.continue();
        held.push(route);
        return undefined;
      });
      await page.goto(`${runtime.baseUrl}/`, { waitUntil: 'domcontentloaded' });
      await page.waitForFunction(() => typeof closeCockpit === 'function'
        && document.querySelector('ax-session-home')?._wired
        && (document.querySelector('ax-session-home')?.workspaces?.length || 0) > 0);
      await page.evaluate(() => closeCockpit({ homeScope: 'all' }));
      const workspace = runtime.fixtures.alpha.workspace;
      const name = page.locator('ax-session-home [data-field="session-name"]');
      const create = page.locator('ax-session-home [data-action="picker-use"]');
      const openPicker = async () => {
        const reads = Promise.all([
          page.waitForResponse((response) => new URL(response.url()).pathname === '/api/fs/project'),
          page.waitForResponse((response) => new URL(response.url()).pathname === '/api/skills'),
        ]);
        await page.evaluate((id) => document.querySelector('ax-session-home').newSession(id), workspace.id);
        await name.waitFor();
        await reads;
      };

      // A readiness read that fails: nothing blocks.
      const failed = page.waitForEvent('requestfailed',
        (request) => new URL(request.url()).pathname === '/api/readiness');
      await openPicker();
      await failed;
      await name.fill('After a failed readiness read');
      await page.waitForFunction(() => document.querySelector('ax-session-home')?.shadowRoot
        ?.querySelector('[data-action="picker-use"]')?.disabled === false);
      assert.equal(await page.locator('ax-session-home .picker-blocked').count(), 0);
      await page.locator('ax-session-home [data-action="picker-cancel"]').click();

      // A read under way: Create session waits for it, with a name typed and
      // the project probe in, and stops waiting after ten seconds.
      readiness = 'hold';
      const pending = page.waitForRequest((request) => new URL(request.url()).pathname === '/api/readiness');
      const opened = Date.now();
      await openPicker();
      await pending;
      await name.fill('Before readiness');
      assert.equal(await create.isDisabled(), true, 'Create session must wait for readiness');
      assert.equal(await page.locator('ax-session-home .picker-blocked').count(), 0);
      await page.waitForFunction(() => document.querySelector('ax-session-home')?.shadowRoot
        ?.querySelector('[data-action="picker-use"]')?.disabled === false, null, { timeout: 30_000 });
      assert.ok(Date.now() - opened >= 9_500, `Create session waited only ${Date.now() - opened} ms`);

      // The read arrives late: Podman blocks Sessions here, and typing a name
      // leaves Create session blocked.
      readiness = 'pass';
      for (const route of held.splice(0)) await route.continue();
      const blocked = page.locator('ax-session-home .picker-blocked');
      await blocked.waitFor();
      assert.match(await blocked.textContent(), /No Session can run yet/);
      assert.equal(await create.isDisabled(), true);
      await name.fill('Typed after readiness');
      assert.equal(await create.isDisabled(), true, 'typing a name must not enable a blocked Create session');
      await name.press('Backspace');
      assert.equal(await create.isDisabled(), true);
      assert.deepEqual(errors, []);
    } finally {
      await context?.close();
      await browser.close();
    }
  });
}
