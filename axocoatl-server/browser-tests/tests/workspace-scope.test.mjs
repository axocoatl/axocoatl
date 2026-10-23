import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { mkdir } from 'node:fs/promises';
import path from 'node:path';

import { browserEngines, launchTestDaemon, newAuthorizedContext } from '../support/daemon.mjs';

// Open workspace starts in the person's home folder. The daemon here has a
// fixture home with one Git repository and one plain folder, so the journey
// never lists the real one.
const engines = await browserEngines();
let runtime;

before(async () => {
  runtime = await launchTestDaemon({ home: true });
  await mkdir(path.join(runtime.homeDirectory, 'project', '.git'), { recursive: true });
  await mkdir(path.join(runtime.homeDirectory, 'notes'), { recursive: true });
});

after(async () => {
  await runtime?.stop();
});

async function workspaces() {
  const response = await fetch(`${runtime.baseUrl}/api/workspaces`);
  assert.equal(response.status, 200);
  return response.json();
}

async function removeByPath(folder) {
  for (const workspace of await workspaces()) {
    if (workspace.canonical_path !== folder) continue;
    await fetch(`${runtime.baseUrl}/api/workspaces/${encodeURIComponent(workspace.id)}`, { method: 'DELETE' });
  }
}

async function openHome(engine) {
  const browser = await engine.launch();
  // A home that does not open closes its browser, so a failed journey cannot
  // keep this file's process (and the suite) waiting on it.
  try {
    const context = await newAuthorizedContext(browser, { viewport: { width: 1280, height: 800 } });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`));
    page.on('console', (message) => {
      // The refusals this journey exercises are 409s, which the browser logs.
      if (message.type() === 'error' && !/status of 409/.test(message.text())) {
        errors.push(`console: ${message.text()}`);
      }
    });
    // Provider health is outside this journey; keep its toast away.
    await page.route('**/api/llm-health', (route) => route.fulfill({
      json: { ollama: { base_url: 'http://127.0.0.1:9', reachable: true, configured: true, missing_models: [] } },
    }));
    await page.goto(`${runtime.baseUrl}/`, { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(() => customElements.get('ax-rail') && customElements.get('ax-session-home')
      && document.querySelector('ax-rail')?.shadowRoot);
    return {
      browser, page, errors,
      async close() { await context.close(); await browser.close(); },
    };
  } catch (error) {
    await browser.close();
    throw error;
  }
}

async function openPicker(page) {
  await page.locator('ax-rail #open-workspace').click();
  await page.locator('ax-session-home .picker-host .modal').waitFor({ state: 'visible' });
}

for (const engine of engines) {
  test(`${engine.name}: the folder picker marks Git repositories and opening the home folder needs its typed name`, async () => {
    const home = runtime.homeDirectory;
    const { page, errors, close } = await openHome(engine);
    try {
      await openPicker(page);
      const pathLine = page.locator('ax-session-home .picker-path');
      await page.waitForFunction((expected) => document.querySelector('ax-session-home')
        ?.shadowRoot?.querySelector('.picker-path')?.textContent?.startsWith(expected), home);
      assert.equal(await pathLine.locator('.git-badge').count(), 0, 'the home folder is not a repository');
      const project = page.locator('ax-session-home .picker-row', { hasText: 'project' });
      const notes = page.locator('ax-session-home .picker-row', { hasText: 'notes' });
      assert.equal(await project.locator('.git-badge').count(), 1);
      assert.equal(await project.getAttribute('aria-label'), 'project (Git repository)');
      assert.equal(await notes.locator('.git-badge').count(), 0);
      const scope = page.locator('ax-session-home .picker-scope');
      assert.match(await scope.textContent(), /is your home folder/);
      assert.match(await scope.textContent(), /type its name/);

      // Open is refused by the daemon; the person is asked for the name.
      const use = page.locator('ax-session-home [data-action="picker-use"]');
      await use.click();
      const dialog = page.locator('ax-session-home .dialog-host .modal');
      await dialog.waitFor({ state: 'visible' });
      assert.match(await dialog.textContent(), /is your home folder/);
      assert.match(await dialog.textContent(), /type its name: home/);
      await dialog.locator('.dialog-input').fill('Home ');
      await dialog.locator('[data-action="dialog-ok"]').click();
      await page.locator('ax-session-home .notice', { hasText: 'Workspace not opened' }).waitFor();
      assert.equal((await workspaces()).some((workspace) => workspace.canonical_path === home), false);
      assert.equal(await use.isDisabled(), false, 'the picker stays open for another choice');

      // Cancelling the confirmation opens nothing either.
      await use.click();
      await dialog.waitFor({ state: 'visible' });
      await dialog.locator('[data-action="dialog-cancel"]').click();
      await dialog.waitFor({ state: 'detached' });
      assert.equal((await workspaces()).some((workspace) => workspace.canonical_path === home), false);

      // The exact name opens it.
      await use.click();
      await dialog.waitFor({ state: 'visible' });
      await dialog.locator('.dialog-input').fill('home');
      await dialog.locator('[data-action="dialog-ok"]').click();
      await page.locator('ax-session-home .picker-host .modal').waitFor({ state: 'detached' });
      assert.equal((await workspaces()).some((workspace) => workspace.canonical_path === home), true);

      // A repository opens with no confirmation.
      await openPicker(page);
      await project.click();
      await page.waitForFunction((expected) => document.querySelector('ax-session-home')
        ?.shadowRoot?.querySelector('.picker-path')?.textContent?.startsWith(expected),
      path.join(home, 'project'));
      assert.equal(await pathLine.locator('.git-badge').count(), 1);
      assert.equal(await page.locator('ax-session-home .picker-scope').count(), 0);
      await use.click();
      await page.locator('ax-session-home .picker-host .modal').waitFor({ state: 'detached' });
      assert.equal(await dialog.count(), 0);
      assert.equal((await workspaces()).some((workspace) => workspace.canonical_path === path.join(home, 'project')), true);
      assert.deepEqual(errors, []);
    } finally {
      await close();
      await removeByPath(home);
      await removeByPath(path.join(home, 'project'));
    }
  });

  test(`${engine.name}: Remove from list forgets a Workspace and refuses one with open Sessions`, async () => {
    const created = await runtime.createProjectWorkspace(`removable-${engine.name}`, `Removable ${engine.name}`);
    const { page, errors, close } = await openHome(engine);
    try {
      // The workbench reopens the last Session of the most recently active
      // Workspace. The Workspace created above can tie with Alpha, seeded the
      // same second, and the tie goes to Alpha by name, so an Alpha Session
      // can cover the home surface that lists the Workspaces. Wait for that
      // restore, then go home.
      await page.waitForFunction(() => Boolean(document.querySelector('ax-rail')?.workspace));
      await page.evaluate(() => closeCockpit({ homeScope: 'all' }));
      await page.evaluate(() => document.querySelector('ax-session-home')?.showWorkspaces());
      const home = page.locator('ax-session-home');
      await home.locator(`.side-row[data-workspace-id="${created.workspace.id}"]`).waitFor({ state: 'visible' });

      // A Workspace with open Sessions stays, with the reason shown.
      const alpha = runtime.fixtures.alpha.workspace;
      await home.locator(`.side-row[data-workspace-id="${alpha.id}"] [data-action="workspace-menu"]`).click();
      await home.locator('.menu-item', { hasText: 'Remove from list…' }).click();
      const dialog = home.locator('.dialog-host .modal');
      await dialog.waitFor({ state: 'visible' });
      assert.match(await dialog.textContent(), /Nothing in .* is deleted/);
      await dialog.locator('[data-action="dialog-ok"]').click();
      const refusal = home.locator('.notice', { hasText: 'Workspace not removed' });
      await refusal.waitFor();
      assert.match(await refusal.textContent(), /open Sessions/);
      assert.equal((await workspaces()).some((workspace) => workspace.id === alpha.id), true);

      // An empty Workspace leaves the list; its folder stays.
      await home.locator(`.side-row[data-workspace-id="${created.workspace.id}"]`).click({ button: 'right' });
      await home.locator('.menu-item', { hasText: 'Remove from list…' }).click();
      await dialog.waitFor({ state: 'visible' });
      await dialog.locator('[data-action="dialog-ok"]').click();
      await home.locator('.notice', { hasText: 'Workspace removed from the list' }).waitFor();
      await home.locator(`.side-row[data-workspace-id="${created.workspace.id}"]`).waitFor({ state: 'detached' });
      assert.equal((await workspaces()).some((workspace) => workspace.id === created.workspace.id), false);
      assert.equal(await runtime.pathExists(created.projectPath), true);
      assert.deepEqual(errors, []);
    } finally {
      await close();
    }
  });

  // The rail reads the Workspaces itself, so it can offer New session for one
  // the Session browser has not read yet: right after the workbench opens
  // (its own read still under way), or one opened in another tab or with the
  // CLI since. The picker then opens once the browser has read it; only a
  // Workspace that is not there is refused.
  test(`${engine.name}: New session for a Workspace the Session browser has not read yet opens once it is read`, async () => {
    const { page, errors, close } = await openHome(engine);
    let late = null;
    try {
      await page.waitForFunction(() => Boolean(document.querySelector('ax-rail')?.workspace));
      await page.evaluate(() => closeCockpit({ homeScope: 'all' }));
      await page.waitForFunction(() => (document.querySelector('ax-session-home')?.workspaces?.length || 0) > 0);
      late = await runtime.createProjectWorkspace(`late-${engine.name}`, `Late ${engine.name}`);
      assert.equal(await page.evaluate((id) =>
        document.querySelector('ax-session-home').workspace(id), late.workspace.id), null);

      assert.equal(await page.evaluate((id) =>
        document.querySelector('ax-session-home').newSession(id), late.workspace.id), true);
      const dialog = page.getByRole('dialog', { name: `New session in Late ${engine.name}` });
      await dialog.waitFor({ state: 'visible' });
      assert.equal(await page.locator('ax-session-home .picker-path').textContent(), late.workspace.canonical_path);
      const refusal = page.locator('ax-session-home .notice', { hasText: 'Open a Workspace first' });
      assert.equal(await refusal.count(), 0);
      await page.locator('ax-session-home [data-action="picker-cancel"]').click();
      await dialog.waitFor({ state: 'detached' });

      assert.equal(await page.evaluate(() =>
        document.querySelector('ax-session-home').newSession('wsp-not-a-workspace')), false);
      await refusal.waitFor();
      assert.equal(await page.locator('ax-session-home .picker-host .modal').count(), 0);
      assert.deepEqual(errors, []);
    } finally {
      await close();
      if (late) await fetch(`${runtime.baseUrl}/api/workspaces/${encodeURIComponent(late.workspace.id)}`, { method: 'DELETE' });
    }
  });
}
