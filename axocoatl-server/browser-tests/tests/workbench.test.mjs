import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { chromium } from 'playwright';

import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';

let runtime;
let browser;

before(async () => {
  runtime = await launchTestDaemon();
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
});

after(async () => {
  await browser?.close();
  await runtime?.stop();
});

async function waitForTwoFrames(page) {
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}

async function openSession(
  session,
  viewport = { width: 1280, height: 800 },
  targetRuntime = runtime,
  beforeGoto = null,
) {
  const context = await browser.newContext({ viewport });
  const page = await context.newPage();
  const browserErrors = [];
  const browserRequests = [];
  const browserResponses = [];
  const requestFailures = [];
  const consoleMessages = [];
  page.on('pageerror', (error) => browserErrors.push(`pageerror: ${error.message}`));
  page.on('console', (message) => {
    if (message.type() === 'error' || message.type() === 'warning') {
      consoleMessages.push(`${message.type()}: ${message.text()}`);
    }
  });
  page.on('request', (request) => browserRequests.push({
    method: request.method(),
    url: request.url(),
  }));
  page.on('response', (response) => browserResponses.push({
    method: response.request().method(),
    status: response.status(),
    url: response.url(),
  }));
  page.on('requestfailed', (request) => requestFailures.push({
    method: request.method(),
    url: request.url(),
    error: request.failure()?.errorText || 'request failed',
  }));
  if (beforeGoto) await beforeGoto({ context, page });
  await page.goto(`${targetRuntime.baseUrl}/?session=${encodeURIComponent(session.id)}`, {
    waitUntil: 'domcontentloaded',
  });
  await page.waitForFunction((sessionId) => {
    const rail = document.querySelector('ax-rail');
    const cockpit = document.querySelector('#session-cockpit');
    return Boolean(
      customElements.get('ax-rail')
      && customElements.get('ax-session-home')
      && rail?.shadowRoot
      && rail.current === sessionId
      && cockpit?.style.display === 'flex',
    );
  }, session.id);
  return {
    context,
    page,
    browserErrors,
    browserRequests,
    browserResponses,
    requestFailures,
    consoleMessages,
    assertNoBrowserErrors() {
      assert.deepEqual(browserErrors, [], browserErrors.join('\n'));
    },
  };
}

async function railSessionLabels(page) {
  return page.locator('ax-rail .sessions .item .label').allTextContents();
}

async function editorSessionBinding(page) {
  return page.evaluate(() => globalThis.axEditor().session);
}

async function editorSuspended(page) {
  return page.evaluate(() => globalThis.axEditor().suspended);
}

async function waitForRailCollapsed(page, collapsed) {
  await page.waitForFunction((expected) =>
    document.querySelector('ax-rail')?.hasAttribute('collapsed') === expected, collapsed);
  await page.waitForFunction((expected) => {
    const width = document.querySelector('ax-rail')?.getBoundingClientRect().width || 0;
    return expected ? width <= 54 : width > 200;
  }, collapsed);
}

async function deepActiveMatches(page, selector) {
  return page.evaluate((expected) => {
    let active = document.activeElement;
    while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
    return Boolean(active?.matches?.(expected));
  }, selector);
}

function colorChannels(value) {
  const match = String(value).match(/^rgba?\(\s*([\d.]+)[, ]+\s*([\d.]+)[, ]+\s*([\d.]+)/i);
  assert.ok(match, `Expected a computed rgb() color, got ${value}`);
  return match.slice(1, 4).map(Number);
}

function contrastRatio(foreground, background) {
  const luminance = (value) => {
    const channels = colorChannels(value).map((channel) => {
      const normalized = channel / 255;
      return normalized <= 0.04045
        ? normalized / 12.92
        : ((normalized + 0.055) / 1.055) ** 2.4;
    });
    return (0.2126 * channels[0]) + (0.7152 * channels[1]) + (0.0722 * channels[2]);
  };
  const first = luminance(foreground);
  const second = luminance(background);
  return (Math.max(first, second) + 0.05) / (Math.min(first, second) + 0.05);
}

async function closeEnvironmentDialog(page, returnFocusSelector) {
  const cancel = page.locator('ax-session-home [data-action="picker-cancel"]');
  await cancel.waitFor({ state: 'visible' });
  await cancel.click();
  await cancel.waitFor({ state: 'hidden' });
  if (returnFocusSelector) {
    await page.waitForFunction((selector) => {
      let active = document.activeElement;
      while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
      return Boolean(active?.matches?.(selector));
    }, returnFocusSelector);
  }
}

async function expectEnvironmentDialog(page, sessionName) {
  const overlay = page.locator('ax-session-home .overlay');
  const dialog = page.locator('ax-session-home [role="dialog"]');
  await overlay.waitFor({ state: 'visible' });
  await dialog.waitFor({ state: 'visible' });
  assert.equal(await dialog.getAttribute('aria-modal'), 'true');
  await page.waitForFunction(() => {
    const home = document.querySelector('ax-session-home');
    return home?.shadowRoot?.activeElement?.matches?.('[role="dialog"]');
  });
  assert.equal(
    await page.locator('ax-session-home #session-picker-title').textContent(),
    `Environment for ${sessionName}`,
  );
  assert.equal(await page.locator('ax-session-home [data-field="setup-command"]').inputValue(), 'npm ci');
  assert.equal(await page.locator('ax-session-home [data-field="setup-approved"]').isChecked(), false);
}

function closedRecoveryResults(session, state) {
  const setId = `closed-recovery-${state}-${session.id}`;
  const lanes = [0, 1].map((index) => ({
    index,
    branch: `axo/closed-recovery/${index}`,
    worktree: `/tmp/closed-recovery/${index}`,
    model: `fixture-model-${index + 1}`,
    agent: `fixture-agent-${index + 1}`,
    provider: 'ollama',
  }));
  const attemptSet = {
    id: setId,
    session_id: session.id,
    task: 'Recover the durable closed-session decision',
    instruction: 'Use the persisted attempt evidence without reopening the Session.',
    base_sha: '1111111111111111111111111111111111111111',
    base_tree: '2222222222222222222222222222222222222222',
    state,
    created_at: 1,
    lanes,
  };
  if (state === 'applied') attemptSet.kept_index = 0;
  return {
    attempt_set: attemptSet,
    lanes,
    lane_states: lanes.map(({ index }) => ({ index, state: 'completed' })),
    verdicts: lanes.map(({ index }) => ({
      index,
      passed: true,
      exit_code: 0,
      output: 'fixture checks passed',
      changed_files: 1,
      touched_tests: [],
    })),
    usage: [],
    outputs: lanes.map(({ index }) => ({
      index,
      content: `Durable outcome from attempt ${index + 1}.`,
    })),
  };
}

function readySessionFixture(source) {
  const session = structuredClone(source);
  session.status = 'active';
  session.environment = {
    ...session.environment,
    state: 'ready',
    error: null,
    setup_results: [],
  };
  return session;
}

test('rail collapse, keyboard toggle, reload, and compact resizing preserve one reversible preference', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  try {
    const rail = page.locator('ax-rail');
    const collapse = page.locator('ax-rail #collapse');
    assert.equal(await rail.getAttribute('collapsed'), null);
    assert.ok((await rail.boundingBox()).width > 200);
    assert.equal(await collapse.getAttribute('aria-label'), 'Collapse the rail');

    await collapse.click();
    await waitForRailCollapsed(page, true);
    assert.equal(await page.evaluate(() => localStorage.getItem('axo.rail.collapsed.v1')), '1');
    assert.ok((await rail.boundingBox()).width <= 54, 'collapsed rail must finish at icon-strip width');
    assert.equal(await collapse.getAttribute('aria-label'), 'Expand the rail');

    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForRailCollapsed(page, true);
    await collapse.click();
    await waitForRailCollapsed(page, false);
    assert.equal(await page.evaluate(() => localStorage.getItem('axo.rail.collapsed.v1')), '0');

    await page.keyboard.down('Control');
    await page.keyboard.press('\\');
    await page.keyboard.up('Control');
    await waitForRailCollapsed(page, true);
    await page.keyboard.down('Control');
    await page.keyboard.press('\\');
    await page.keyboard.up('Control');
    await waitForRailCollapsed(page, false);

    await page.setViewportSize({ width: 1000, height: 760 });
    await waitForTwoFrames(page);
    await waitForRailCollapsed(page, false);
    assert.equal(await page.evaluate(() => localStorage.getItem('axo.rail.collapsed.v1')), '0');

    await collapse.click();
    await waitForRailCollapsed(page, true);
    await page.setViewportSize({ width: 1280, height: 800 });
    await waitForTwoFrames(page);
    await waitForRailCollapsed(page, true);
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitForRailCollapsed(page, true);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('enabled composer controls inherit readable light and dark theme tokens', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  try {
    await page.waitForFunction(() =>
      document.querySelector('#session-model')?.shadowRoot?.querySelector('.opt'));
    // The fixture intentionally awaits setup approval. Enable only the local
    // composer DOM so this visual contract exercises enabled styles without
    // approving or running the detected setup command.
    await page.evaluate(() => {
      document.querySelector('.session-input').removeAttribute('inert');
      document.querySelector('#session-text').disabled = false;
      document.querySelector('#fanout-btn').disabled = false;
      document.querySelector('#session-send').disabled = false;
    });

    for (const theme of ['dark', 'light']) {
      await page.evaluate((selectedTheme) => setThemePref(selectedTheme), theme);
      await page.waitForTimeout(200);
      const colors = await page.evaluate(() => {
        const resolveColor = (token) => {
          const probe = document.createElement('span');
          probe.style.color = `var(${token})`;
          document.body.appendChild(probe);
          const color = getComputedStyle(probe).color;
          probe.remove();
          return color;
        };
        const model = document.querySelector('#session-model');
        const trigger = model.shadowRoot.querySelector('.trig');
        const option = model.shadowRoot.querySelector('.opt');
        const menu = model.shadowRoot.querySelector('.menu');
        const fanout = document.querySelector('#fanout-btn');
        const textarea = document.querySelector('#session-text');
        const send = document.querySelector('#session-send');
        const fanoutDefault = {
          color: getComputedStyle(fanout).color,
          background: getComputedStyle(fanout).backgroundColor,
        };
        fanout.classList.add('on');
        const fanoutOnColor = getComputedStyle(fanout).color;
        const snapshot = {
          tokens: {
            text: resolveColor('--text'),
            muted: resolveColor('--muted'),
            input: resolveColor('--bg-3'),
            panel: resolveColor('--panel'),
            composer: resolveColor('--panel-2'),
            primary: resolveColor('--axo-jade'),
          },
          trigger: {
            color: getComputedStyle(trigger).color,
            background: getComputedStyle(trigger).backgroundColor,
          },
          option: {
            color: getComputedStyle(option).color,
            background: getComputedStyle(menu).backgroundColor,
          },
          fanout: { ...fanoutDefault, onColor: fanoutOnColor },
          textarea: {
            color: getComputedStyle(textarea).color,
            placeholder: getComputedStyle(textarea, '::placeholder').color,
          },
          send: {
            color: getComputedStyle(send).color,
            background: getComputedStyle(send).backgroundColor,
            backgroundImage: getComputedStyle(send).backgroundImage,
          },
        };
        fanout.classList.remove('on');
        return snapshot;
      });

      assert.equal(colors.trigger.color, colors.tokens.text, `${theme} model trigger token`);
      assert.equal(colors.trigger.background, colors.tokens.input, `${theme} model trigger surface`);
      assert.equal(colors.option.color, colors.tokens.text, `${theme} selected model option token`);
      assert.equal(colors.option.background, colors.tokens.panel, `${theme} model menu surface`);
      assert.equal(colors.fanout.color, colors.tokens.text, `${theme} answer-mode token`);
      assert.equal(colors.fanout.onColor, colors.tokens.text, `${theme} active answer-mode token`);
      assert.equal(colors.textarea.color, colors.tokens.text, `${theme} composer text token`);
      assert.equal(colors.textarea.placeholder, colors.tokens.muted, `${theme} composer placeholder token`);
      assert.equal(colors.send.background, colors.tokens.primary, `${theme} Send surface token`);
      assert.equal(colors.send.backgroundImage, 'none', `${theme} Send must not have a low-contrast gradient`);

      const pairs = [
        ['model trigger', colors.trigger.color, colors.trigger.background],
        ['selected model option', colors.option.color, colors.option.background],
        ['answer mode', colors.fanout.color, colors.fanout.background],
        ['composer text', colors.textarea.color, colors.tokens.composer],
        ['composer placeholder', colors.textarea.placeholder, colors.tokens.composer],
        ['Send', colors.send.color, colors.send.background],
      ];
      for (const [control, foreground, background] of pairs) {
        assert.ok(
          contrastRatio(foreground, background) >= 4.5,
          `${theme} ${control} contrast is ${contrastRatio(foreground, background).toFixed(2)}:1`,
        );
      }

      await page.locator('#session-send').hover();
      assert.equal(await page.locator('#session-send').evaluate((send) => getComputedStyle(send).filter), 'none');
    }
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('workspace selector exposes every named Workspace while the rail lists only its Sessions', async () => {
  const { alpha, beta } = runtime.fixtures;
  const { context, page, assertNoBrowserErrors } = await openSession(alpha.sessions[0]);
  try {
    const rail = page.locator('ax-rail');
    assert.equal(await rail.getAttribute('workspace'), alpha.workspace.id);
    assert.deepEqual((await railSessionLabels(page)).sort(), [
      'Alpha First Session',
      'Alpha Second Session',
    ]);
    assert.equal((await railSessionLabels(page)).includes('Beta Only Session'), false);

    await page.locator('ax-rail #open-workspace').click();
    const workspaceName = page.getByLabel('Workspace name');
    await workspaceName.waitFor({ state: 'visible' });
    assert.equal(await workspaceName.getAttribute('id'), 'workspace-name-input');
    await page.locator('ax-session-home label[for="workspace-name-input"]').click();
    assert.equal(await workspaceName.evaluate((input) => input.getRootNode().activeElement === input), true);
    await page.locator('ax-session-home [data-action="picker-cancel"]').click();

    await page.evaluate((workspaceId) =>
      document.querySelector('ax-session-home')?.newSession(workspaceId), alpha.workspace.id);
    const sessionName = page.getByLabel('Session name');
    await sessionName.waitFor({ state: 'visible' });
    assert.equal(await sessionName.getAttribute('id'), 'session-name-input');
    await page.locator('ax-session-home label[for="session-name-input"]').click();
    assert.equal(await sessionName.evaluate((input) => input.getRootNode().activeElement === input), true);
    await page.locator('ax-session-home [data-action="picker-cancel"]').click();

    await page.locator('ax-rail #switch').click();
    const alphaChoice = page.locator('ax-rail [aria-label="Open Workspace Alpha Workspace"]');
    const betaChoice = page.locator('ax-rail [aria-label="Open Workspace Beta Workspace"]');
    await betaChoice.waitFor({ state: 'visible' });
    assert.equal(await alphaChoice.count(), 1);
    assert.equal(await betaChoice.count(), 1);
    const menuPaths = await page.locator('ax-rail .menu .workspace-path').allTextContents();
    assert.ok(menuPaths.includes(alpha.workspace.canonical_path));
    assert.ok(menuPaths.includes(beta.workspace.canonical_path));

    await betaChoice.click();
    await page.waitForFunction(({ workspaceId, sessionId }) => {
      const railElement = document.querySelector('ax-rail');
      return railElement?.workspace === workspaceId && railElement?.current === sessionId;
    }, { workspaceId: beta.workspace.id, sessionId: beta.sessions[0].id });
    assert.deepEqual(await railSessionLabels(page), ['Beta Only Session']);
    assert.equal((await railSessionLabels(page)).some((label) => label.startsWith('Alpha ')), false);
    assert.equal(await page.locator('#cockpit-title').textContent(), 'Beta Only Session');

    await page.locator('ax-rail #switch').click();
    await page.locator('ax-rail [aria-label="Open Workspace Alpha Workspace"]').click();
    await page.waitForFunction((workspaceId) =>
      document.querySelector('ax-rail')?.workspace === workspaceId, alpha.workspace.id);
    assert.deepEqual((await railSessionLabels(page)).sort(), [
      'Alpha First Session',
      'Alpha Second Session',
    ]);
    assert.equal((await railSessionLabels(page)).includes('Beta Only Session'), false);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('an AwaitingApproval Session blocks work and routes runtime tools to the exact setup decision', async () => {
  const session = runtime.fixtures.alpha.sessions[1];
  const { context, page, browserRequests, assertNoBrowserErrors } = await openSession(session);
  try {
    const banner = page.locator('#session-environment');
    await banner.waitFor({ state: 'visible' });
    assert.equal(await banner.getAttribute('data-state'), 'awaiting_approval');
    assert.equal(await page.locator('#session-environment-title').textContent(), 'Project setup needs your approval');
    assert.equal(await page.locator('#session-environment-command').textContent(), 'npm ci');

    const send = page.locator('#session-send');
    const ways = page.locator('#fanout-btn');
    assert.equal(await send.isDisabled(), true);
    assert.equal(await send.textContent(), 'Needs setup');
    assert.equal(await ways.isDisabled(), true);
    assert.match(await ways.getAttribute('title'), /Prepare the project environment/);
    assert.equal(await page.locator('#source-control').getAttribute('session'), null);
    assert.equal(await page.locator('#browser').getAttribute('session'), null);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), true);

    const bannerAction = page.locator('#session-environment-action');
    await bannerAction.click();
    await expectEnvironmentDialog(page, session.name);
    await closeEnvironmentDialog(page, '#session-environment-action');
    assert.equal(await deepActiveMatches(page, '#session-environment-action'), true);

    await page.locator('#session-text').fill('This must not execute.');
    await page.locator('#session-text').press('Enter');
    await expectEnvironmentDialog(page, session.name);
    await closeEnvironmentDialog(page, '#session-text');

    await page.locator('[data-session-action="terminal"]').click();
    await expectEnvironmentDialog(page, session.name);
    assert.equal(await page.locator('#cockpit-terminals').evaluate((element) => element.classList.contains('open')), false);
    await closeEnvironmentDialog(page, '[data-session-action="terminal"]');

    await page.locator('[data-session-view="browser"]').click();
    await expectEnvironmentDialog(page, session.name);
    assert.notEqual(await page.locator('#cockpit-browser').evaluate((element) => element.style.display), 'flex');
    await closeEnvironmentDialog(page, '[data-session-view="browser"]');

    const files = page.locator('[data-session-view="files"]');
    await files.click();
    await expectEnvironmentDialog(page, session.name);
    assert.notEqual(await page.locator('#cockpit-files').evaluate((element) => element.style.display), 'flex');
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), true);
    await closeEnvironmentDialog(page, '[data-session-view="files"]');

    // Files is now unreachable before Ready, so its Source Control tab cannot
    // receive a real pointer click. Dispatching the tab's click still proves
    // the inner guard cannot become a bypass after a future routing change.
    await page.locator('#explorer-tabs [data-etab="git"]').dispatchEvent('click');
    await expectEnvironmentDialog(page, session.name);
    assert.equal(await page.locator('#source-control').getAttribute('session'), null);
    assert.equal(await page.locator('#source-control').evaluate((element) => element.classList.contains('hide')), true);
    await closeEnvironmentDialog(page);

    await page.locator('#session-text').focus();
    await page.keyboard.down('Control');
    await page.keyboard.press('p');
    await page.keyboard.up('Control');
    await expectEnvironmentDialog(page, session.name);
    assert.equal(await page.locator('.quick-open').count(), 0);
    await closeEnvironmentDialog(page, '#session-text');

    const runtimeRequests = browserRequests.flatMap((request) => {
      const pathname = new URL(request.url).pathname;
      const isMutation = request.method !== 'GET';
      const blocked = pathname.includes(`/api/sessions/${session.id}/execute`)
        || pathname.includes(`/api/sessions/${session.id}/git/`)
        || pathname.includes(`/api/sessions/${session.id}/preview`)
        || pathname === `/api/sessions/${session.id}/tree`
        || pathname === `/api/sessions/${session.id}/file`
        || (isMutation && pathname.includes(`/api/sessions/${session.id}/tasks`));
      return blocked ? [`${request.method} ${pathname}`] : [];
    });
    assert.deepEqual(runtimeRequests, [], `Blocked controls reached runtime endpoints:\n${runtimeRequests.join('\n')}`);
    assert.equal(
      browserRequests.filter((request) => request.method === 'GET'
        && new URL(request.url).pathname === `/api/sessions/${session.id}/tasks`).length,
      0,
      'a Session that is not Ready must not poll the executable task surface',
    );
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('restored unresolved Ways keep primary runtime surfaces unbound', async () => {
  const source = runtime.fixtures.alpha.sessions[0];
  const session = structuredClone(source);
  session.status = 'active';
  session.environment = {
    ...session.environment,
    state: 'ready',
    error: null,
    setup_results: [],
  };
  const results = closedRecoveryResults(session, 'ready');
  let taskGets = 0;
  let taskPosts = 0;
  const opened = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page }) => {
      await page.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify([session]),
        }),
      );
      await page.route('**/api/sessions', (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([session]),
      }));
      await page.route(`**/api/sessions/${session.id}/turns`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: '[]',
      }));
      await page.route(`**/api/sessions/${session.id}/active-turn`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ run: null }),
      }));
      await page.route(`**/api/sessions/${session.id}/tasks`, (route) => {
        if (route.request().method() === 'GET') {
          taskGets += 1;
          return route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
        }
        taskPosts += 1;
        return route.fulfill({
          status: 409,
          contentType: 'application/json',
          body: JSON.stringify({ error: 'the unresolved Ways decision owns this Workspace' }),
        });
      });
      await page.route(`**/api/sessions/${session.id}/variants/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        if (pathname.endsWith('/results')) {
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(results),
          });
        }
        if (pathname.endsWith('/status')) {
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(results.lanes.map((lane) => ({
              index: lane.index,
              branch: lane.branch,
              status: { branch: lane.branch, clean: false, files: [] },
            }))),
          });
        }
        return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
      });
    },
  );
  const {
    context, page, browserRequests, browserResponses, requestFailures, consoleMessages,
    assertNoBrowserErrors,
  } = opened;
  try {
    await page.waitForFunction(({ sessionId, setId }) =>
      S.session.id === sessionId
      && S.session.attemptRestorePending === false
      && S.threadVariants?.attemptSetId === setId,
    { sessionId: session.id, setId: results.attempt_set.id });
    await page.evaluate(() => {
      setExplorerTab('git');
      openSessionTerminal();
      browserGo('http://localhost:3000');
      void openQuickOpen();
    });
    const detachedPreview = await page.locator('#browser').evaluate((element) => {
      element.go('http://localhost:3999/direct-method');
      const input = element.shadowRoot.querySelector('input');
      input.value = 'http://localhost:3999/address-enter';
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
      return { disabled: input.disabled, frame: Boolean(element.frame), url: element.url };
    });
    await page.waitForTimeout(4_200);
    assert.equal(taskGets, 0, 'Ways ownership must suppress primary task polling');
    assert.equal(taskPosts, 0, 'attempt restoration must suppress implicit terminal creation');
    assert.equal(await page.evaluate(() => S.session.autoTerminalDone), false);
    assert.equal(await page.locator('#source-control').getAttribute('session'), null);
    assert.equal(await page.locator('#browser').getAttribute('session'), null);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), true);
    assert.equal(await page.locator('#session-send').isDisabled(), true);
    assert.equal(await page.locator('#session-send').textContent(), 'Finish Ways');
    assert.equal(await page.locator('#session-history').getAttribute('rewind-enabled'), 'false');
    assert.deepEqual(detachedPreview, { disabled: true, frame: false, url: '' });
    const primaryRuntimeReads = browserRequests.filter((request) => {
      const url = new URL(request.url);
      const pathname = url.pathname;
      return pathname === `/api/sessions/${session.id}/tree`
        || pathname.startsWith(`/api/sessions/${session.id}/git/`)
        || pathname.startsWith(`/api/sessions/${session.id}/preview`)
        || url.hostname.startsWith(`${session.id}-p`);
    });
    assert.deepEqual(primaryRuntimeReads, [], 'Ways restoration must not touch primary runtime surfaces');
    assert.deepEqual(
      browserResponses.filter((response) => response.status >= 400),
      [],
      'Ways restoration must not produce failed HTTP responses',
    );
    assert.deepEqual(requestFailures, [], 'Ways restoration must not produce failed browser requests');
    assert.deepEqual(consoleMessages, [], 'Ways restoration must leave the console clean');
    assert.equal(await page.locator('.toast', { hasText: 'Auto terminal failed' }).count(), 0);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('a stale task snapshot cannot repopulate runtime state after Ways takes ownership', async () => {
  const source = runtime.fixtures.alpha.sessions[0];
  const session = structuredClone(source);
  session.status = 'active';
  session.environment = { ...session.environment, state: 'ready', error: null, setup_results: [] };
  const results = closedRecoveryResults(session, 'ready');
  let exposeSet = false;
  let taskGets = 0;
  let taskPosts = 0;
  let releaseTaskSnapshot;
  let observeTaskSnapshot;
  const taskSnapshotHeld = new Promise((resolve) => { observeTaskSnapshot = resolve; });
  const taskSnapshotRelease = new Promise((resolve) => { releaseTaskSnapshot = resolve; });
  const opened = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page }) => {
      const sessions = JSON.stringify([session]);
      await page.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({ status: 200, contentType: 'application/json', body: sessions }),
      );
      await page.route('**/api/sessions', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: sessions,
      }));
      await page.route(`**/api/sessions/${session.id}/turns`, (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: '[]',
      }));
      await page.route(`**/api/sessions/${session.id}/active-turn`, (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify({ run: null }),
      }));
      await page.route(`**/api/sessions/${session.id}/tasks`, async (route) => {
        if (route.request().method() !== 'GET') {
          taskPosts += 1;
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify({ id: 'must-not-start' }),
          });
        }
        taskGets += 1;
        if (taskGets === 1) {
          observeTaskSnapshot();
          await taskSnapshotRelease;
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify([{
              id: 'stale-shell', kind: 'terminal', command: 'sh', status: 'running',
            }]),
          });
        }
        return route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
      });
      await page.route(`**/api/sessions/${session.id}/variants/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        if (pathname.endsWith('/results')) {
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(exposeSet ? results : {
              attempt_set: null, lanes: [], lane_states: [], verdicts: [], usage: [], outputs: [],
            }),
          });
        }
        if (pathname.endsWith('/status')) {
          return route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
        }
        return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
      });
    },
  );
  const {
    context, page, browserResponses, requestFailures, consoleMessages, assertNoBrowserErrors,
  } = opened;
  try {
    await taskSnapshotHeld;
    exposeSet = true;
    await page.evaluate(({ sessionId, setId }) => handleWsFrame({
      kind: 'lane-started',
      session: sessionId,
      run: `${sessionId}#0`,
      workflow: `${sessionId}#0`,
      attempt_set_id: setId,
    }), { sessionId: session.id, setId: results.attempt_set.id });
    await page.waitForFunction(({ sessionId, setId }) =>
      S.session.id === sessionId
      && S.session.attemptRestorePending === false
      && S.threadVariants?.attemptSetId === setId,
    { sessionId: session.id, setId: results.attempt_set.id });
    releaseTaskSnapshot();
    await page.waitForTimeout(150);

    assert.equal(taskGets, 1);
    assert.equal(taskPosts, 0);
    assert.deepEqual(await page.evaluate(() => S.session.tasks), []);
    assert.equal(await page.evaluate(() => S.xterms.size), 0);
    assert.equal(await page.evaluate(() => S.session.autoTerminalDone), false);
    assert.equal(await page.locator('#source-control').getAttribute('session'), null);
    assert.equal(await page.locator('#browser').getAttribute('session'), null);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), true);
    assert.deepEqual(browserResponses.filter((response) => response.status >= 400), []);
    const unexpectedFailures = requestFailures.filter((failure) => {
      const pathname = new URL(failure.url).pathname;
      return pathname !== `/api/sessions/${session.id}/tasks`;
    });
    assert.deepEqual(
      unexpectedFailures,
      [],
      'only the deliberately invalidated task snapshot may be aborted',
    );
    assert.deepEqual(consoleMessages, []);
    assertNoBrowserErrors();
  } finally {
    releaseTaskSnapshot?.();
    await context.close();
  }
});

test('Ways suspension cancels delayed primary reads and preserves dirty editor drafts', async () => {
  const session = readySessionFixture(runtime.fixtures.alpha.sessions[0]);
  const results = closedRecoveryResults(session, 'ready');
  let exposeSet = false;
  let holdPrimaryReads = false;
  let releasePrimaryReads;
  let observePrimaryReads;
  let branchReads = 0;
  const heldKinds = new Set();
  const primaryReadsHeld = new Promise((resolve) => { observePrimaryReads = resolve; });
  const primaryReadsRelease = new Promise((resolve) => { releasePrimaryReads = resolve; });
  const noteHeld = (kind) => {
    heldKinds.add(kind);
    if (heldKinds.size === 3) observePrimaryReads();
  };
  const fulfillAfterRelease = async (route, kind, body) => {
    if (holdPrimaryReads) {
      noteHeld(kind);
      await primaryReadsRelease;
    }
    try {
      await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) });
    } catch {
      // AbortController may already have cancelled the intercepted request.
    }
  };

  const opened = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page }) => {
      const sessions = JSON.stringify([session]);
      await page.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({
          status: 200, contentType: 'application/json', body: sessions,
        }),
      );
      await page.route('**/api/sessions', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: sessions,
      }));
      await page.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: route.request().method() === 'GET'
          ? '[]'
          : JSON.stringify({ id: 'ways-suspension-shell' }),
      }));
      await page.route(`**/api/sessions/${session.id}/tree**`, (route) => fulfillAfterRelease(
        route,
        'tree',
        [
          { name: 'index.js', path: 'src/index.js', kind: 'file', size: 28 },
          { name: 'second.js', path: 'src/second.js', kind: 'file', size: 29 },
        ],
      ));
      await page.route(`**/api/sessions/${session.id}/file?*`, (route) => {
        const path = new URL(route.request().url()).searchParams.get('path') || '';
        const body = {
          path,
          content: path === 'src/second.js'
            ? 'export const second = "late";\n'
            : 'export const first = "before";\n',
          lang: 'js',
          truncated: false,
        };
        return fulfillAfterRelease(route, 'file', body);
      });
      await page.route(`**/api/sessions/${session.id}/git/status`, (route) => fulfillAfterRelease(
        route,
        'git',
        { branch: 'main', files: [], clean: true },
      ));
      await page.route(`**/api/sessions/${session.id}/git/branches`, (route) => {
        branchReads += 1;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ current: 'main', branches: ['main'] }),
        });
      });
      await page.route(`**/api/sessions/${session.id}/variants/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        if (pathname.endsWith('/results')) {
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(exposeSet ? results : {
              attempt_set: null, lanes: [], lane_states: [], verdicts: [], usage: [], outputs: [],
            }),
          });
        }
        if (pathname.endsWith('/status')) {
          return route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
        }
        return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
      });
    },
  );
  const {
    context, page, browserResponses, requestFailures, consoleMessages, assertNoBrowserErrors,
  } = opened;
  try {
    await page.waitForFunction(() => S.session.historyState === 'ready'
      && sessionRuntimeSurfaceReady(S.session));
    await page.evaluate(() => {
      clearInterval(S.session.taskTimer);
      S.session.taskTimer = null;
    });
    await page.waitForFunction((sessionId) => {
      const source = document.querySelector('#source-control');
      const tree = document.querySelector('#file-tree');
      return source?.session === sessionId && source.status?.branch === 'main'
        && tree?.session === sessionId
        && tree.shadowRoot?.querySelectorAll('[role="treeitem"]').length === 2
        && !globalThis.axEditor().suspended;
    }, session.id);

    await page.evaluate(() => openFile('src/index.js'));
    await page.waitForFunction(() => globalThis.axEditor().active === 'src/index.js'
      && window.monaco?.editor?.getModels?.().length > 0);
    await page.evaluate(() => {
      const model = window.monaco.editor.getModels()
        .find((candidate) => candidate.uri?.path?.endsWith('/src/index.js'))
        || window.monaco.editor.getModels()[0];
      model.setValue('export const first = "unsaved dirty draft";\n');
    });
    await page.waitForFunction(() => globalThis.axEditor().files
      .some((file) => file.path === 'src/index.js' && file.dirty));

    const branchesBeforeSuspension = branchReads;
    holdPrimaryReads = true;
    await page.evaluate(() => {
      void document.querySelector('#file-tree').reload();
      void globalThis.axEditor().open('src/second.js');
      void document.querySelector('#source-control').refresh({ branches: true });
    });
    await primaryReadsHeld;

    exposeSet = true;
    await page.evaluate(({ sessionId, setId }) => handleWsFrame({
      kind: 'lane-started',
      session: sessionId,
      run: `${sessionId}#0`,
      workflow: `${sessionId}#0`,
      attempt_set_id: setId,
    }), { sessionId: session.id, setId: results.attempt_set.id });
    await page.waitForFunction(({ sessionId, setId }) =>
      S.session.id === sessionId
      && S.session.attemptRestorePending === false
      && S.threadVariants?.attemptSetId === setId
      && globalThis.axEditor().suspended,
    { sessionId: session.id, setId: results.attempt_set.id });
    releasePrimaryReads();
    await page.waitForTimeout(200);

    assert.equal(branchReads, branchesBeforeSuspension,
      'a stale status response must not launch a branches read after suspension');
    assert.deepEqual(await page.evaluate(() => ({
      session: globalThis.axEditor().session,
      suspended: globalThis.axEditor().suspended,
      files: globalThis.axEditor().files.map((file) => ({ path: file.path, dirty: file.dirty })),
      content: globalThis.axEditor().contentOf('src/index.js'),
      treeSession: document.querySelector('#file-tree').session,
      treeRows: document.querySelector('#file-tree').shadowRoot.querySelectorAll('[role="treeitem"]').length,
      sourceSession: document.querySelector('#source-control').session,
      sourceStatus: document.querySelector('#source-control').status,
    })), {
      session: session.id,
      suspended: true,
      files: [{ path: 'src/index.js', dirty: true }],
      content: 'export const first = "unsaved dirty draft";\n',
      treeSession: '',
      treeRows: 0,
      sourceSession: '',
      sourceStatus: null,
    });
    assert.deepEqual(
      browserResponses.filter((response) => response.status >= 400),
      [],
      'suspension must not turn delayed primary reads into failed HTTP responses',
    );
    const unexpectedFailures = requestFailures.filter((failure) => {
      const pathname = new URL(failure.url).pathname;
      return pathname !== `/api/sessions/${session.id}/tree`
        && pathname !== `/api/sessions/${session.id}/file`
        && pathname !== `/api/sessions/${session.id}/git/status`
        && (pathname !== `/api/sessions/${session.id}/turns`
          || failure.error !== 'net::ERR_ABORTED');
    });
    assert.deepEqual(
      unexpectedFailures,
      [],
      'only deliberately aborted primary or superseded transcript reads may fail',
    );
    assert.deepEqual(consoleMessages, []);
    assertNoBrowserErrors();
  } finally {
    releasePrimaryReads?.();
    await context.close();
  }
});

test('Quick Open stops a directory-only cycle at its explicit depth bound', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  let treeRequests = 0;
  try {
    await page.route('**/api/sessions/**/tree**', async (route) => {
      treeRequests += 1;
      const url = new URL(route.request().url());
      const parent = url.searchParams.get('path') || '';
      const child = parent ? `${parent}/loop` : 'loop';
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([{ kind: 'dir', name: 'loop', path: child }]),
      });
    });
    const ready = readySessionFixture(session);
    await page.route(
      `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
      (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify([ready]),
      }),
    );
    await page.route('**/api/sessions', (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: JSON.stringify([ready]),
    }));
    await page.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: route.request().method() === 'GET'
        ? JSON.stringify([{
          id: 'quick-open-shell', kind: 'terminal', command: 'sh', status: 'running',
        }])
        : JSON.stringify({ id: 'unexpected-quick-open-shell' }),
    }));
    await page.route(`**/api/sessions/${session.id}/git/**`, (route) => {
      const pathname = new URL(route.request().url()).pathname;
      const body = pathname.endsWith('/git/status')
        ? { branch: 'main', files: [], clean: true }
        : pathname.endsWith('/git/branches')
          ? { current: 'main', branches: ['main'] }
          : {};
      return route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify(body),
      });
    });
    await page.evaluate(() => sessionHome().refresh());
    await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session));
    await page.waitForFunction(() =>
      !document.querySelector('#file-tree')?.shadowRoot?.querySelector('.empty.loading'));
    await page.evaluate(() => {
      clearInterval(S.session.taskTimer);
      S.session.taskTimer = null;
      S.quickOpen.index = null;
      S.quickOpen.indexFor = null;
    });
    treeRequests = 0;
    assert.equal(await page.evaluate(() => sessionEnvironmentReady(S.session)), true);
    // The AwaitingApproval journey above covers the physical Ctrl/Cmd+P gate.
    // Invoke the same browser function directly here so this controlled tree
    // fixture isolates traversal termination from keyboard delivery.
    await page.evaluate(() => openQuickOpen());
    await page.locator('#quick-open-input').waitFor({ state: 'visible' });
    await page.waitForFunction(() => !S.quickOpen.building);
    assert.ok(treeRequests > 10, 'fixture must behave like an otherwise-unbounded directory cycle');
    assert.ok(treeRequests <= 65, `Quick Open exceeded its depth bound with ${treeRequests} tree requests`);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Source Control exposes a normal turn absolute write in Last turn', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const ready = structuredClone(session);
  ready.status = 'active';
  ready.environment = {
    ...ready.environment,
    state: 'ready',
    error: null,
    setup_results: [],
  };
  const status = {
    branch: 'main',
    clean: false,
    files: [{
      path: 'lib/orders.js',
      state: 'modified',
      added: 1,
      removed: 1,
      staged: false,
      unstaged: true,
      last_turn: true,
    }],
  };
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: target }) => {
      await target.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({
          status: 200, contentType: 'application/json', body: JSON.stringify([ready]),
        }),
      );
      await target.route('**/api/sessions', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify([ready]),
      }));
      await target.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: '[]',
      }));
      await target.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: route.request().method() === 'GET'
          ? JSON.stringify([{
            id: 'source-control-shell', kind: 'terminal', command: 'sh', status: 'running',
          }])
          : JSON.stringify({ id: 'unexpected-source-control-shell' }),
      }));
      await target.route(`**/api/sessions/${session.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? status
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200, contentType: 'application/json', body: JSON.stringify(body),
        });
      });
    },
  );
  try {
    await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session));
    await page.locator('[data-session-view="files"]').click();
    await page.locator('[data-etab="git"]').click();
    await page.waitForFunction(() => {
      const root = document.querySelector('#source-control')?.shadowRoot;
      return root?.querySelector('[data-count="lastTurn"]')?.textContent === '1'
        && root?.querySelector('[data-count="all"]')?.textContent === '1';
    });
    const sourceControl = page.locator('#source-control');
    assert.equal(
      await sourceControl.locator('[data-scope="lastTurn"]').getAttribute('aria-pressed'),
      'true',
    );
    assert.equal(await sourceControl.locator('.file[data-path="lib/orders.js"]').count(), 1);
    assert.match(
      await sourceControl.locator('.file-main').getAttribute('aria-label'),
      /lib\/orders\.js.*attributed to last turn/,
    );
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('a failed Session renders bounded, text-only durable setup evidence in the banner and review', async () => {
  const evidenceSession = runtime.fixtures.alpha.sessions[1];
  const lazySession = runtime.fixtures.beta.sessions[0];
  const command = `printf '<img id="setup-command-injection">'`;
  const stdoutPrefix = '<img id="setup-stdout-injection" src=x onerror="globalThis.__setupEvidenceInjected=true">';
  const stderrPrefix = '<script>globalThis.__setupEvidenceInjected=true</script>';
  await runtime.restartWithSessionPatches([
    {
      id: evidenceSession.id,
      update(session) {
        session.environment = {
          ...session.environment,
          generation: Number(session.environment?.generation || 0) + 1,
          state: 'failed',
          setup_approved: true,
          setup_reviewed: true,
          setup_results: [{
            command,
            exit_code: 7,
            stdout: `${stdoutPrefix}\n${'o'.repeat(4000)}`,
            stderr: `${stderrPrefix}\n${'e'.repeat(4000)}`,
            completed_at: Math.floor(Date.now() / 1000),
          }],
          error: 'approved setup command exited with status 7',
          prepared_at: Math.floor(Date.now() / 1000),
        };
        return session;
      },
    },
    {
      id: lazySession.id,
      update(session) {
        const image = 'ghcr.io/axocoatl/browser-e2e-untrusted:latest';
        session.image = image;
        session.environment = {
          ...session.environment,
          generation: Number(session.environment?.generation || 0) + 1,
          state: 'ready',
          effective_image: image,
          runtime: {
            backend: 'podman',
            id: lazySession.id,
            remote_root: null,
            control_plane: null,
            data_plane_domain: null,
            authority_fingerprint: null,
            cleanup_confirmed: true,
          },
          setup_command: null,
          setup_approved: false,
          setup_reviewed: true,
          setup_results: [],
          error: null,
          prepared_at: Math.floor(Date.now() / 1000),
        };
        return session;
      },
    },
  ]);

  const { context, page, assertNoBrowserErrors } = await openSession(evidenceSession);
  try {
    const evidence = page.locator('#session-environment-evidence');
    await evidence.waitFor({ state: 'visible' });
    assert.equal(await evidence.getAttribute('open'), '');
    assert.equal(await page.locator('#session-environment-evidence-summary').textContent(), 'Setup evidence · exit 7');
    assert.equal(await page.locator('[data-evidence-field="command"]').textContent(), command);
    assert.equal(await page.locator('[data-evidence-field="exit"]').textContent(), '7');
    for (const field of ['stdout', 'stderr']) {
      const output = await page.locator(`[data-evidence-field="${field}"]`).textContent();
      assert.ok(output.length < 1700, `${field} evidence was not bounded`);
      assert.match(output, /output truncated in this view/);
    }
    assert.equal(await evidence.locator('img, script').count(), 0);
    assert.equal(await page.evaluate(() => globalThis.__setupEvidenceInjected), undefined);

    await page.locator('#session-environment-action').click();
    await page.locator('ax-session-home [role="dialog"]').waitFor({ state: 'visible' });
    assert.match(await page.locator('ax-session-home .setup-evidence summary').textContent(), /Setup evidence.*failed/);
    assert.equal(await page.locator('ax-session-home .setup-evidence img, ax-session-home .setup-evidence script').count(), 0);
    await closeEnvironmentDialog(page, '#session-environment-action');
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('a lazy terminal failure after a persisted Ready restart reconciles the exact Session to Failed', async () => {
  const session = runtime.fixtures.beta.sessions[0];
  await runtime.restartWithSessionPatches([{
    id: session.id,
    update(persisted) {
      const image = 'ghcr.io/axocoatl/browser-e2e-untrusted:latest';
      persisted.image = image;
      persisted.environment = {
        ...persisted.environment,
        generation: Number(persisted.environment?.generation || 0) + 1,
        state: 'ready',
        effective_image: image,
        runtime: {
          backend: 'podman',
          id: session.id,
          remote_root: null,
          control_plane: null,
          data_plane_domain: null,
          authority_fingerprint: null,
          cleanup_confirmed: true,
        },
        setup_command: null,
        setup_approved: false,
        setup_reviewed: true,
        setup_results: [],
        error: null,
        prepared_at: Math.floor(Date.now() / 1000),
      };
      return persisted;
    },
  }]);
  const { context, page, browserRequests, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: target }) => {
      // Keep Files from winning the lazy-activation race. The terminal POST
      // below remains a real request to the isolated daemon and is what causes
      // its persisted Ready record to be revalidated and failed.
      await target.route('**/api/sessions/**/tree**', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: '[]',
      }));
      await target.route(`**/api/sessions/${session.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? { branch: 'main', files: [], clean: true }
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(body),
        });
      });
      await target.route(`**/api/sessions/${session.id}/tasks`, (route) => {
        if (route.request().method() === 'GET') {
          return route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
        }
        return route.continue();
      });
    },
  );
  try {
    try {
      await page.waitForFunction(() =>
        document.querySelector('#session-environment')?.dataset.state === 'failed');
    } catch (error) {
      const visible = await page.evaluate(() => ({
        session: S.session,
        bannerState: document.querySelector('#session-environment')?.dataset.state || null,
        bannerTitle: document.querySelector('#session-environment-title')?.textContent || null,
      }));
      const persisted = (await runtime.listSessions()).find((item) => item.id === session.id);
      throw new Error([
        error.message,
        `visible=${JSON.stringify(visible)}`,
        `persisted=${JSON.stringify(persisted)}`,
        `requests=${JSON.stringify(browserRequests.filter((request) => new URL(request.url).pathname.includes(session.id)))}`,
        runtime.logs(),
      ].join('\n'));
    }
    const taskMutations = browserRequests.filter((request) => {
      const url = new URL(request.url);
      return request.method === 'POST'
        && url.pathname === `/api/sessions/${session.id}/tasks`;
    });
    assert.equal(taskMutations.length, 1, 'auto-terminal must exercise the real lazy runtime boundary once');

    const persisted = (await runtime.listSessions()).find((item) => item.id === session.id);
    assert.equal(persisted.environment.state, 'failed');
    assert.match(persisted.environment.error, /requires explicit trust/);
    assert.equal(await page.locator('#session-environment-title').textContent(), 'Project environment failed');
    assert.equal(await page.locator('#session-send').isDisabled(), true);
    assert.equal(await page.locator('#source-control').getAttribute('session'), null);
    assert.equal(await page.locator('#browser').getAttribute('session'), null);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), true);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('successful lazy runtime preparation leaves Preparing without a reload', async () => {
  const source = runtime.fixtures.alpha.sessions[0];
  const preparing = structuredClone(source);
  preparing.status = 'active';
  preparing.environment = {
    ...preparing.environment,
    state: 'preparing',
    runtime: null,
    runtime_creation: null,
    error: null,
  };
  const ready = structuredClone(preparing);
  ready.environment = {
    ...ready.environment,
    state: 'ready',
    runtime: {
      backend: 'podman',
      id: ready.id,
      remote_root: null,
      control_plane: null,
      data_plane_domain: null,
      authority_fingerprint: null,
      cleanup_confirmed: false,
    },
    prepared_at: Math.floor(Date.now() / 1000),
  };
  let serveReady = false;
  let taskGets = 0;
  let taskPosts = 0;
  const opened = await openSession(
    preparing,
    { width: 1280, height: 800 },
    runtime,
    async ({ page }) => {
      const sessions = () => JSON.stringify([serveReady ? ready : preparing]);
      await page.route(
        `**/api/workspaces/${encodeURIComponent(preparing.workspace_id)}/sessions`,
        (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: sessions(),
        }),
      );
      await page.route('**/api/sessions', (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: sessions(),
      }));
      await page.route(`**/api/sessions/${preparing.id}/tasks`, (route) => {
        if (route.request().method() === 'GET') {
          taskGets += 1;
          return route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
        }
        taskPosts += 1;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ id: 'lazy-ready-shell' }),
        });
      });
      await page.route(`**/api/sessions/${preparing.id}/tree**`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: '[]',
      }));
      await page.route(`**/api/sessions/${preparing.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? { branch: 'main', files: [], clean: true }
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(body),
        });
      });
    },
  );
  const { context, page, browserRequests, assertNoBrowserErrors } = opened;
  try {
    await page.waitForFunction(() =>
      document.querySelector('#session-environment')?.dataset.state === 'preparing');
    await page.evaluate(() => {
      clearInterval(S.session.taskTimer);
      S.session.taskTimer = null;
    });
    assert.equal(taskGets, 0, 'Preparing must not poll the executable task endpoint');
    assert.equal(taskPosts, 0, 'Preparing must not create an executable task');

    serveReady = true;
    await page.evaluate((sessionId) => refreshSessionTasks(sessionId), preparing.id);
    await page.waitForFunction(() =>
      sessionEnvironmentReady(S.session)
      && document.querySelector('#session-environment')?.classList.contains('hide')
      && !document.querySelector('#fanout-btn')?.disabled
      && S.session.autoTerminalDone
      && !S.session.autoTerminalStarting);
    assert.ok(taskGets <= 2, `Ready reconciliation issued ${taskGets} task snapshots`);
    assert.equal(taskPosts, 1, 'Ready recovery must resume exactly one automatic terminal');
    const activationPost = browserRequests.findIndex((request) => request.method === 'POST'
      && new URL(request.url).pathname === `/api/sessions/${preparing.id}/tasks`);
    const firstCompetingRead = browserRequests.findIndex((request) => {
      const pathname = new URL(request.url).pathname;
      return pathname === `/api/sessions/${preparing.id}/tree`
        || pathname.startsWith(`/api/sessions/${preparing.id}/git/`);
    });
    assert.ok(activationPost >= 0, 'the terminal must own lazy runtime activation');
    assert.ok(firstCompetingRead < 0 || firstCompetingRead > activationPost,
      'Files and Git must bind only after terminal activation completes');
    assert.equal(await page.locator('#source-control').getAttribute('session'), preparing.id);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), preparing.id);
    assert.equal(await editorSessionBinding(page), preparing.id);
    assert.equal(await editorSuspended(page), false);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('a superseded runtime activation cannot strand the next Session epoch', async () => {
  const session = readySessionFixture(runtime.fixtures.alpha.sessions[0]);
  let taskGets = 0;
  let taskPosts = 0;
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: target }) => {
      const sessions = JSON.stringify([session]);
      await target.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({ status: 200, contentType: 'application/json', body: sessions }),
      );
      await target.route('**/api/sessions', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: sessions,
      }));
      await target.route(`**/api/sessions/${session.id}/tasks`, async (route) => {
        if (route.request().method() === 'GET') {
          taskGets += 1;
          if (taskGets === 1) {
            await new Promise((resolve) => setTimeout(resolve, 800));
          }
          try {
            return await route.fulfill({
              status: 200, contentType: 'application/json', body: '[]',
            });
          } catch {
            return null;
          }
        }
        taskPosts += 1;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ id: `activation-shell-${taskPosts}` }),
        });
      });
      await target.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: '[]',
      }));
      await target.route(`**/api/sessions/${session.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? { branch: 'main', files: [], clean: true }
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200, contentType: 'application/json', body: JSON.stringify(body),
        });
      });
    },
  );
  try {
    await page.waitForFunction(() => S.session.runtimeActivationInFlight === true);
    const resumed = await page.evaluate(async (sessionId) => {
      markSessionRuntimeActivationRequired(sessionId);
      return activateOpenedSessionRuntime(sessionId);
    }, session.id);
    assert.equal(resumed, true);
    await page.waitForFunction(() => !S.session.runtimeActivationPending
      && !S.session.runtimeActivationInFlight
      && S.session.runtimeActivationController == null);
    assert.ok(taskGets >= 2, 'the replacement epoch must own a fresh task snapshot');
    assert.equal(taskPosts, 1, 'only the replacement epoch may start a terminal');
    assert.equal(await page.locator('#source-control').getAttribute('session'), session.id);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), session.id);
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), false);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('a rejected environment rebuild suspends before teardown and restores the Ready runtime', async () => {
  const session = readySessionFixture(runtime.fixtures.alpha.sessions[0]);
  let releaseRebuild;
  const rebuildGate = new Promise((resolve) => { releaseRebuild = resolve; });
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: target }) => {
      const sessions = JSON.stringify([session]);
      await target.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({ status: 200, contentType: 'application/json', body: sessions }),
      );
      await target.route('**/api/sessions', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: sessions,
      }));
      await target.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: route.request().method() === 'GET'
          ? JSON.stringify([{
            id: 'rebuild-shell', kind: 'terminal', command: 'sh', status: 'running',
          }])
          : JSON.stringify({ id: 'unexpected-rebuild-shell' }),
      }));
      await target.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: '[]',
      }));
      await target.route(`**/api/sessions/${session.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? { branch: 'main', files: [], clean: true }
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200, contentType: 'application/json', body: JSON.stringify(body),
        });
      });
      await target.route(`**/api/sessions/${session.id}/environment/rebuild`, async (route) => {
        await rebuildGate;
        return route.fulfill({
          status: 500,
          contentType: 'application/json',
          body: JSON.stringify({ error: 'controlled rebuild rejection' }),
        });
      });
    },
  );
  try {
    await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session));
    const rebuild = page.evaluate((sessionId) => sessionHome().rebuildEnvironment(sessionId), session.id);
    await page.waitForFunction(() => S.session.runtimeActivationPending === true);
    assert.equal(await page.locator('#source-control').getAttribute('session'), null);
    assert.equal(await page.locator('#browser').getAttribute('session'), null);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), true);

    releaseRebuild();
    assert.equal(await rebuild, false);
    await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session)
      && !S.session.runtimeActivationInFlight);
    assert.equal(await page.locator('#source-control').getAttribute('session'), session.id);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), session.id);
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), false);
    assertNoBrowserErrors();
  } finally {
    releaseRebuild?.();
    await context.close();
  }
});

test('a reconnect snapshot preserves a cross-tab runtime gate and detects a completed generation', async () => {
  const session = readySessionFixture(runtime.fixtures.alpha.sessions[0]);
  let canonical = structuredClone(session);
  let taskPosts = 0;
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: target }) => {
      const sessions = () => JSON.stringify([canonical]);
      await target.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({
          status: 200, contentType: 'application/json', body: sessions(),
        }),
      );
      await target.route('**/api/sessions', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: sessions(),
      }));
      await target.route(`**/api/sessions/${session.id}/tasks`, (route) => {
        if (route.request().method() === 'POST') taskPosts += 1;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: route.request().method() === 'GET'
            ? JSON.stringify([{
              id: 'cross-tab-shell', kind: 'terminal', command: 'sh', status: 'running',
            }])
            : JSON.stringify({ id: 'unexpected-cross-tab-shell' }),
        });
      });
      await target.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: '[]',
      }));
      await target.route(`**/api/sessions/${session.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? { branch: 'main', files: [], clean: true }
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200, contentType: 'application/json', body: JSON.stringify(body),
        });
      });
    },
  );
  try {
    await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session));
    await page.evaluate((sessionId) => handleWsFrame({
      kind: 'snapshot',
      runs: [],
      approvals: [],
      environment_transitions: [{
        session: sessionId,
        generation: Number(S.session.environment?.generation || 0),
      }],
    }), session.id);
    await page.waitForFunction(() => S.session.runtimeActivationPending === true
      && document.querySelector('#source-control')?.getAttribute('session') == null
      && document.querySelector('#file-tree')?.session === ''
      && globalThis.axEditor().suspended === true);

    canonical = structuredClone(canonical);
    canonical.environment = {
      ...canonical.environment,
      generation: Number(canonical.environment?.generation || 0) + 1,
      state: 'ready',
      error: null,
    };
    await page.evaluate(() => handleWsFrame({
      kind: 'snapshot', runs: [], approvals: [], environment_transitions: [],
    }));
    await page.waitForFunction((generation) =>
      Number(S.session.environment?.generation || 0) === generation
      && sessionRuntimeSurfaceReady(S.session)
      && !S.session.runtimeActivationInFlight,
    canonical.environment.generation);
    assert.equal(await page.locator('#source-control').getAttribute('session'), session.id);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), session.id);
    assert.equal(await editorSuspended(page), false);
    assert.equal(taskPosts, 0, 'reconnect must reuse the authoritative live terminal');

    await page.evaluate(({ workspaceId, ownerSession }) => handleWsFrame({
      kind: 'snapshot',
      runs: [],
      approvals: [],
      environment_transitions: [],
      attempt_ownerships: [{
        workspace: workspaceId,
        session: ownerSession,
        attempt_set_id: 'cross-tab-set',
      }],
    }), { workspaceId: session.workspace_id, ownerSession: 'other-session' });
    await page.waitForFunction((workspaceId) =>
      _workspaceAttemptOwnerships.get(workspaceId)?.attemptSetId === 'cross-tab-set'
      && S.session.runtimeActivationPending === true
      && document.querySelector('#source-control')?.getAttribute('session') == null
      && document.querySelector('#file-tree')?.session === ''
      && globalThis.axEditor().suspended === true,
    session.workspace_id);

    await page.evaluate(() => handleWsFrame({
      kind: 'snapshot', runs: [], approvals: [],
      environment_transitions: [], attempt_ownerships: [],
    }));
    await page.waitForFunction((workspaceId) =>
      !_workspaceAttemptOwnerships.has(workspaceId)
      && sessionRuntimeSurfaceReady(S.session)
      && !S.session.runtimeActivationInFlight,
    session.workspace_id);
    assert.equal(taskPosts, 0, 'Ways settlement must reuse the authoritative live terminal');
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('a cross-tab Keep settlement clears a judged comparison and restores the kept turn', async () => {
  const session = readySessionFixture(runtime.fixtures.alpha.sessions[0]);
  const results = closedRecoveryResults(session, 'judged');
  const setId = results.attempt_set.id;
  results.judgment = {
    winner: 0,
    candidates: [
      { index: 0, rank: 1, approach: 'One focused edit.', tradeoffs: 'Smallest route.' },
      { index: 1, rank: 2, approach: 'Equivalent repair.', tradeoffs: 'Longer route.' },
    ],
    reasoning: 'Both pass; the first route is more direct.',
  };
  let canonicalResults = results;
  let canonicalTurns = [];
  let gitStatus = { branch: 'main', files: [], clean: true };
  let resultsReads = 0;
  let turnReads = 0;
  let taskPosts = 0;
  const opened = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page }) => {
      const sessions = JSON.stringify([session]);
      await page.routeWebSocket(
        `**/api/sessions/${session.id}/terminals/*/ws`,
        (webSocket) => webSocket.onMessage(() => {}),
      );
      await page.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({ status: 200, contentType: 'application/json', body: sessions }),
      );
      await page.route('**/api/sessions', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: sessions,
      }));
      await page.route(`**/api/sessions/${session.id}/turns`, (route) => {
        turnReads += 1;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(canonicalTurns),
        });
      });
      await page.route(`**/api/sessions/${session.id}/active-turn`, (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify({ run: null }),
      }));
      await page.route(`**/api/sessions/${session.id}/tasks`, (route) => {
        if (route.request().method() === 'POST') taskPosts += 1;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: route.request().method() === 'GET'
            ? JSON.stringify([{
              id: 'cross-tab-keep-shell', kind: 'terminal', command: 'sh', status: 'running',
            }])
            : JSON.stringify({ id: 'unexpected-cross-tab-keep-shell' }),
        });
      });
      await page.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: '[]',
      }));
      await page.route(`**/api/sessions/${session.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? gitStatus
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200, contentType: 'application/json', body: JSON.stringify(body),
        });
      });
      await page.route(`**/api/sessions/${session.id}/variants/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        if (pathname.endsWith('/results')) {
          resultsReads += 1;
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(canonicalResults),
          });
        }
        if (pathname.endsWith('/status')) {
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(canonicalResults.lanes.map((lane) => ({
              index: lane.index,
              branch: lane.branch,
              status: { branch: lane.branch, clean: false, files: [] },
            }))),
          });
        }
        if (pathname.endsWith('/trajectories')) {
          return route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify({ baseline: 0, steps: [], diverged_steps: 0 }),
          });
        }
        return route.fulfill({ status: 200, contentType: 'application/json', body: '{}' });
      });
    },
  );
  const {
    context, page, browserResponses, requestFailures, consoleMessages, assertNoBrowserErrors,
  } = opened;
  try {
    await page.waitForFunction(({ sessionId, expectedSetId }) => {
      const compare = document.querySelector('#compare');
      return S.session.id === sessionId
        && S.session.attemptRestorePending === false
        && S.threadVariants?.attemptSetId === expectedSetId
        && S.threadVariants?.setState === 'judged'
        && compare?.attemptSetId === expectedSetId;
    }, { sessionId: session.id, expectedSetId: setId });

    await page.evaluate(({ workspaceId, sessionId, expectedSetId }) => handleWsFrame({
      kind: 'workspace-attempt-changing',
      workspace: workspaceId,
      session: sessionId,
      attempt_set_id: expectedSetId,
    }), { workspaceId: session.workspace_id, sessionId: session.id, expectedSetId: setId });
    await page.waitForFunction(({ workspaceId, expectedSetId }) =>
      _workspaceAttemptOwnerships.get(workspaceId)?.attemptSetId === expectedSetId
      && S.session.runtimeActivationPending === true,
    { workspaceId: session.workspace_id, expectedSetId: setId });

    canonicalResults = {
      attempt_set: null, lanes: [], lane_states: [], verdicts: [], usage: [], outputs: [],
    };
    canonicalTurns = [{
      id: `turn-attempt-${setId}-0`,
      session_id: session.id,
      user_input: 'Keep the checked repair.',
      final_output: 'Kept across tabs with durable evidence.',
      status: 'completed',
      agent_id: 'fixture-agent-1',
      model: 'fixture-model-1',
      context: [],
      execution_events: [{ kind: 'attempt_kept', attempt_id: setId }],
      agent_outputs: [],
      metadata: {
        source: 'kept_attempt',
        attempt_set_id: setId,
        attempt_index: 0,
        touched_paths: ['lib/orders.js'],
      },
    }];
    gitStatus = {
      branch: 'main',
      files: [{
        path: 'lib/orders.js', state: 'modified', added: 1, removed: 1,
        staged: false, unstaged: true, last_turn: true,
      }],
      clean: false,
    };

    const staleSettlement = await page.evaluate(({ workspaceId, sessionId }) => {
      const restoreReadId = _tvRestoreReadId;
      handleWsFrame({
        kind: 'workspace-attempt-settled',
        workspace: workspaceId,
        session: sessionId,
        attempt_set_id: 'older-set-that-does-not-own-the-workspace',
      });
      return {
        restoreReadId,
        restoreReadIdAfter: _tvRestoreReadId,
        ownedSetId: _workspaceAttemptOwnerships.get(workspaceId)?.attemptSetId || '',
        visibleSetId: S.threadVariants?.attemptSetId || '',
        restorePending: S.session.attemptRestorePending,
      };
    }, { workspaceId: session.workspace_id, sessionId: session.id });
    assert.deepEqual(staleSettlement, {
      restoreReadId: staleSettlement.restoreReadId,
      restoreReadIdAfter: staleSettlement.restoreReadId,
      ownedSetId: setId,
      visibleSetId: setId,
      restorePending: false,
    }, 'a stale settlement must not alter ownership or start an authoritative restore');

    const readsBeforeExactSettlement = resultsReads;
    await page.evaluate(({ workspaceId, sessionId, expectedSetId }) => handleWsFrame({
      kind: 'workspace-attempt-settled',
      workspace: workspaceId,
      session: sessionId,
      attempt_set_id: expectedSetId,
    }), { workspaceId: session.workspace_id, sessionId: session.id, expectedSetId: setId });
    await page.waitForFunction(({ workspaceId, sessionId }) => {
      const compare = document.querySelector('#compare');
      const source = document.querySelector('#source-control');
      return !_workspaceAttemptOwnerships.has(workspaceId)
        && !_attemptSetIds.has(sessionId)
        && S.threadVariants == null
        && S.session.attemptRestorePending === false
        && sessionRuntimeSurfaceReady(S.session)
        && compare?.attemptSetId === ''
        && source?.status?.files?.length === 1
        && source.status.files[0]?.last_turn === true
        && document.querySelector('#session-msgs')?.textContent
          .includes('Kept across tabs with durable evidence.');
    }, { workspaceId: session.workspace_id, sessionId: session.id });

    assert.ok(resultsReads > readsBeforeExactSettlement,
      'the matching settlement must re-read authoritative Results');
    assert.ok(turnReads >= 2, 'the kept canonical Turn must be reloaded');
    assert.equal(taskPosts, 0, 'settlement must reuse the authoritative live terminal');
    assert.deepEqual(browserResponses.filter((response) => response.status >= 400), []);
    const unexpectedFailures = requestFailures.filter((failure) => {
      const pathname = new URL(failure.url).pathname;
      return failure.error !== 'net::ERR_ABORTED'
        || (pathname !== `/api/sessions/${session.id}/turns`
          && pathname !== `/api/sessions/${session.id}/attachments`);
    });
    assert.deepEqual(
      unexpectedFailures,
      [],
      'only deliberately superseded transcript reads may be aborted during settlement',
    );
    assert.deepEqual(consoleMessages, []);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('cross-tab Close and Delete retire the visible Session instead of leaving a pending ghost', async () => {
  const session = readySessionFixture(runtime.fixtures.alpha.sessions[0]);
  for (const lifecycle of ['closed', 'deleted']) {
    let canonical = structuredClone(session);
    let retired = false;
    const { context, page, assertNoBrowserErrors } = await openSession(
      session,
      { width: 1280, height: 800 },
      runtime,
      async ({ page: target }) => {
        const sessions = () => JSON.stringify(
          retired && lifecycle === 'deleted' ? [] : [canonical],
        );
        await target.route(
          `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
          (route) => route.fulfill({
            status: 200, contentType: 'application/json', body: sessions(),
          }),
        );
        await target.route('**/api/sessions', (route) => route.fulfill({
          status: 200, contentType: 'application/json', body: sessions(),
        }));
        await target.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify([{
            id: `${lifecycle}-shell`, kind: 'terminal', command: 'sh', status: 'running',
          }]),
        }));
      },
    );
    try {
      await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session));
      await page.evaluate((sessionId) => handleWsFrame({
        kind: 'session-environment-changing', session: sessionId, generation: 1,
      }), session.id);
      await page.waitForFunction(() => S.session.runtimeActivationPending === true);
      if (lifecycle === 'closed') {
        canonical = structuredClone(canonical);
        canonical.status = 'closed';
      }
      retired = true;
      await page.evaluate((sessionId) => handleWsFrame({
        kind: 'session-environment-settled', session: sessionId,
      }), session.id);
      await page.waitForFunction(() => !S.session.id
        && document.querySelector('#session-cockpit')?.style.display === 'none');
      assertNoBrowserErrors();
    } finally {
      await context.close();
    }
  }
});

test('turn, task-poll, and Preview failures all reconcile a stale Ready cockpit', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  for (const scenario of ['turn', 'tasks', 'preview']) {
    const { context, page, assertNoBrowserErrors } = await openSession(session);
    try {
      const failed = structuredClone(session);
      failed.environment = {
        ...failed.environment,
        generation: Number(failed.environment?.generation || 0) + 1,
        state: 'failed',
        error: `${scenario} discovered that the persisted runtime cannot be restored`,
        setup_results: [],
      };
      await page.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({
          status: 200, contentType: 'application/json', body: JSON.stringify([failed]),
        }),
      );
      await page.route('**/api/sessions', (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify([failed]),
      }));
      await page.evaluate(() => {
        S.session.environment = { ...S.session.environment, state: 'ready', error: null };
        renderSessionEnvironment(S.session);
      });

      if (scenario === 'turn') {
        await page.evaluate((sessionId) => handleWsFrame({
          kind: 'session-error', session: sessionId, error: 'runtime activation failed',
        }), session.id);
      } else if (scenario === 'tasks') {
        await page.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
          status: 409,
          contentType: 'application/json',
          body: JSON.stringify({ error: 'runtime activation failed' }),
        }));
        await page.evaluate((sessionId) => refreshSessionTasks(sessionId), session.id);
      } else {
        await page.route(/ses-[a-z0-9-]+-p\d+\.localhost:\d+\//, (route) => route.fulfill({
          status: 409,
          contentType: 'application/json',
          body: JSON.stringify({ error: 'runtime activation failed' }),
        }));
        await page.evaluate((sessionId) => {
          document.querySelector('#browser').session = sessionId;
          browserGo('http://localhost:3000');
        }, session.id);
      }

      await page.waitForFunction(() =>
        document.querySelector('#session-environment')?.dataset.state === 'failed');
      assert.equal(await page.locator('#session-environment-title').textContent(), 'Project environment failed');
      assert.equal(await page.locator('#session-send').isDisabled(), true);
      assert.equal(await page.locator('#source-control').getAttribute('session'), null);
      assert.equal(await page.locator('#browser').getAttribute('session'), null);
      assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
      assert.equal(await editorSessionBinding(page), session.id);
      assert.equal(await editorSuspended(page), true);
      assertNoBrowserErrors();
    } finally {
      await context.close();
    }
  }
});

test('Preview app code cannot cross from its dedicated origin into the workbench', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const { context, page, browserRequests, assertNoBrowserErrors } = await openSession(session);
  try {
    const ready = structuredClone(session);
    ready.status = 'active';
    ready.environment = {
      ...ready.environment,
      generation: Number(ready.environment?.generation || 0) + 1,
      state: 'ready',
      error: null,
      setup_results: [],
    };
    const rebuildPath = `/api/sessions/${session.id}/environment/rebuild`;
    const controlResponses = [];
    page.on('response', (response) => {
      if (response.request().method() === 'POST'
          && new URL(response.url()).pathname === rebuildPath) {
        controlResponses.push(response.status());
      }
    });
    await page.route(
      `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
      (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify([ready]),
      }),
    );
    await page.route('**/api/sessions', (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: JSON.stringify([ready]),
    }));
    await page.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: '[]',
    }));
    await page.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: route.request().method() === 'GET'
        ? JSON.stringify([{
          id: 'preview-security-shell', kind: 'terminal', command: 'sh', status: 'running',
        }])
        : JSON.stringify({ id: 'unexpected-preview-security-shell' }),
    }));
    await page.route(`**/api/sessions/${session.id}/git/**`, (route) => {
      const pathname = new URL(route.request().url()).pathname;
      const body = pathname.endsWith('/git/status')
        ? { branch: 'main', files: [], clean: true }
        : pathname.endsWith('/git/branches')
          ? { current: 'main', branches: ['main'] }
          : {};
      return route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify(body),
      });
    });
    await page.route(/ses-[a-z0-9-]+-p\d+\.localhost:\d+\//, (route) => route.fulfill({
      status: 200,
      contentType: 'text/html',
      body: `<!doctype html><body><script>
        (async () => {
          let parentReadable = false;
          let apiReadable = false;
          try {
            parent.document.body.dataset.previewEscaped = 'true';
            parentReadable = parent.document.body.dataset.previewEscaped === 'true';
          } catch {}
          try {
            const response = await fetch(${JSON.stringify(`${runtime.baseUrl}${rebuildPath}`)}, { method: 'POST' });
            await response.text();
            apiReadable = true;
          } catch {}
          globalThis.__previewSecurityResult = { parentReadable, apiReadable };
        })();
      </script></body>`,
    }));
    await page.route('http://external-preview-fixture.test/**', (route) => route.fulfill({
      status: 200,
      contentType: 'text/html',
      body: `<!doctype html><body><script>
        parent.postMessage({
          kind: 'axo-tap:picked',
          chain: [{ tag: 'body', label: 'external-forged', selector: '#external-forged' }],
          selectedIndex: 0,
          selector: '#external-forged',
          html: '<button id="external-forged">forged</button>',
        }, '*');
      </script></body>`,
    }));
    await page.evaluate(() => sessionHome().refresh());
    await page.waitForFunction((sessionId) =>
      sessionHome().session(sessionId)?.environment?.state === 'ready', session.id);
    await page.waitForFunction(() => S.session.attemptRestorePending === false);
    const previewSandbox = await page.evaluate(async (value) => {
      clearInterval(S.session.taskTimer);
      S.session.taskTimer = null;
      applySessionEnvironmentUpdate(value, { activateRuntime: false });
      const activated = await activateOpenedSessionRuntime(value.id);
      if (!activated || !sessionRuntimeSurfaceReady(S.session)) {
        throw new Error('Preview security fixture did not activate the Session runtime');
      }
      const browser = document.querySelector('#browser');
      browser.go('http://localhost:3000/security-proof');
      return browser.frame?.getAttribute('sandbox') || '';
    }, ready);
    const iframeProperty = await page.locator('#browser').evaluateHandle((element) => element.frame);
    const iframe = iframeProperty.asElement();
    assert.ok(iframe, 'Preview must render inside its sandboxed iframe');
    const previewFrame = await iframe.contentFrame();
    assert.ok(previewFrame, 'Preview iframe must expose a browser frame');
    await previewFrame.waitForFunction(() => globalThis.__previewSecurityResult != null);
    await page.waitForTimeout(100);

    assert.deepEqual(
      await previewFrame.evaluate(() => globalThis.__previewSecurityResult),
      { parentReadable: false, apiReadable: false },
    );
    assert.equal(await page.locator('body').getAttribute('data-preview-escaped'), null);
    assert.equal(
      previewSandbox,
      'allow-scripts allow-same-origin allow-forms allow-popups allow-modals',
    );
    assert.equal(await page.locator('#browser').evaluate((element) =>
      element.shadowRoot.querySelector('[data-act="open"]') === null), true);

    // The parent accepts picker messages only from this exact Preview iframe
    // and its exact per-Session/per-port origin.
    // A same-page script, extension, or unrelated frame cannot forge a DOM
    // selection merely by using the public message kind.
    await page.evaluate(() => window.postMessage({
      kind: 'axo-tap:picked',
      chain: [{ tag: 'body', label: 'forged', selector: '#forged' }],
      selectedIndex: 0,
      selector: '#forged',
      html: '<button id="forged">forged</button>',
    }, '*'));
    await waitForTwoFrames(page);
    assert.deepEqual(await page.evaluate(() => ({
      hierarchy: S.browser.hier,
      hidden: document.querySelector('#dom-hier')?.classList.contains('hide'),
    })), { hierarchy: null, hidden: true });

    // A page outside the dedicated virtual Preview origin remains viewable,
    // but cannot become a trusted picker peer just because it occupies the
    // exact iframe. Source validation alone is insufficient here.
    await page.evaluate(() => document.querySelector('#browser')
      .go('http://external-preview-fixture.test/forge-picker'));
    await page.waitForTimeout(100);
    assert.deepEqual(await page.evaluate(() => ({
      hierarchy: S.browser.hier,
      hidden: document.querySelector('#dom-hier')?.classList.contains('hide'),
    })), { hierarchy: null, hidden: true });

    const controlWrites = browserRequests.filter((request) =>
      request.method === 'POST' && new URL(request.url).pathname === rebuildPath);
    assert.ok(controlWrites.length <= 1);
    assert.ok(controlResponses.every((status) => status === 403));
    const persisted = (await runtime.listSessions()).find((item) => item.id === session.id);
    assert.deepEqual(
      { state: persisted?.environment?.state, generation: persisted?.environment?.generation },
      { state: session.environment.state, generation: session.environment.generation },
    );

    // The response-level CSP is the second boundary: even if someone pastes
    // an authenticated proxy URL into a top-level tab, that document receives
    // an opaque origin and cannot read a control-plane response.
    const directPage = await context.newPage();
    const directProxyPath = `/api/sessions/${session.id}/proxy/3000/direct-security-proof`;
    const directControlResponses = [];
    directPage.on('response', (response) => {
      if (response.request().method() === 'POST'
          && new URL(response.url()).pathname === rebuildPath) {
        directControlResponses.push(response.status());
      }
    });
    await directPage.route(`**${directProxyPath}`, (route) => route.fulfill({
      status: 200,
      contentType: 'text/html',
      headers: {
        'content-security-policy': 'sandbox allow-scripts allow-forms allow-popups allow-modals',
        'x-content-type-options': 'nosniff',
        'referrer-policy': 'no-referrer',
      },
      body: `<!doctype html><body><script>
        (async () => {
          let apiReadable = false;
          try {
            const response = await fetch(${JSON.stringify(rebuildPath)}, { method: 'POST' });
            await response.text();
            apiReadable = true;
          } catch {}
          globalThis.__directPreviewSecurityResult = {
            opaqueOrigin: window.origin === 'null',
            apiReadable,
          };
        })();
      </script></body>`,
    }));
    const directResponse = await directPage.goto(`${runtime.baseUrl}${directProxyPath}`);
    assert.equal(
      directResponse?.headers()['content-security-policy'],
      'sandbox allow-scripts allow-forms allow-popups allow-modals',
    );
    await directPage.waitForFunction(() => globalThis.__directPreviewSecurityResult != null);
    assert.deepEqual(
      await directPage.evaluate(() => globalThis.__directPreviewSecurityResult),
      { opaqueOrigin: true, apiReadable: false },
    );
    assert.ok(directControlResponses.every((status) => status === 403));
    await directPage.close();
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Preview browser contract keeps modules, app APIs, storage, forms, assets, encoding negotiation, and HMR working', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const {
    context, page, browserErrors, browserRequests, assertNoBrowserErrors,
  } = await openSession(session);
  try {
    assert.equal(new URL(page.url()).hostname, 'localhost');
    assert.ok(browserRequests.some((request) =>
      new URL(request.url).hostname === '127.0.0.1'
      && new URL(request.url).pathname === '/'));
    const ready = structuredClone(session);
    ready.status = 'active';
    ready.environment = {
      ...ready.environment,
      generation: Number(ready.environment?.generation || 0) + 1,
      state: 'ready',
      error: null,
      setup_results: [],
    };
    const listenerPort = new URL(runtime.baseUrl).port;
    const previewHost = `${session.id}-p3000.localhost:${listenerPort}`;
    const previewOrigin = `http://${previewHost}`;
    const otherPreviewOrigin = `http://${session.id}-p5000.localhost:${listenerPort}`;
    const rebuildPath = `/api/sessions/${session.id}/environment/rebuild`;
    const tapSource = await readFile(new URL('../../static/axo-tap.js', import.meta.url), 'utf8');
    const fixtureRequests = [];
    const controlResponses = [];
    const websocketMessages = [];
    const otherPreviewCookieRequests = [];

    page.on('response', (response) => {
      if (response.request().method() === 'POST'
          && new URL(response.url()).pathname === rebuildPath) {
        controlResponses.push(response.status());
      }
    });
    await page.route(
      `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
      (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify([ready]),
      }),
    );
    await page.route('**/api/sessions', (route) => {
      if (route.request().headers().origin) return route.continue();
      return route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify([ready]),
      });
    });
    await page.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: '[]',
    }));
    await page.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: route.request().method() === 'GET'
        ? JSON.stringify([{
          id: 'preview-contract-shell', kind: 'terminal', command: 'sh', status: 'running',
        }])
        : JSON.stringify({ id: 'unexpected-preview-contract-shell' }),
    }));
    await page.route(`**/api/sessions/${session.id}/git/**`, (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: '{}',
    }));
    await page.routeWebSocket((url) => url.host === previewHost, (ws) => {
      websocketMessages.push(ws.url());
      ws.onMessage((message) => {
        websocketMessages.push(String(message));
        if (String(message) === 'vite-ready') ws.send('hot-update');
      });
    });
    await context.route((url) => url.origin === previewOrigin, async (route) => {
      const request = route.request();
      const url = new URL(request.url());
      fixtureRequests.push({
        path: url.pathname,
        method: request.method(),
        body: request.postData(),
        host: url.host,
        origin: request.headers().origin || null,
        authorization: request.headers().authorization || null,
        apiKey: request.headers()['x-api-key'] || null,
      });
      if (url.pathname === '/.axocoatl/preview-picker.js') {
        return route.fulfill({
          status: 200, contentType: 'text/javascript; charset=utf-8', body: tapSource,
        });
      }
      if (url.pathname === '/app') {
        return route.fulfill({
          status: 200,
          contentType: 'text/html; charset=utf-8',
          body: `<!doctype html><html><head><link rel="stylesheet" href="/fixture.css"></head><body>
            <script>
              if (window === top) {
                document.cookie = 'preview-host-only=from-a; Path=/';
                document.cookie = 'preview-parent-attempt=from-a; Domain=localhost; Path=/';
                globalThis.__topLevelCookieResult = {
                  cookies: document.cookie,
                  openerIsNull: window.opener === null,
                };
              }
            </script>
            <button id="pick-target">Pick target</button>
            <img id="relative-asset" src="/asset.svg" alt="fixture">
            <iframe name="form-result" hidden></iframe>
            <form id="native-form" method="post" action="/form-submit" target="form-result">
              <input name="fixture" value="native-form-ok">
            </form>
            <script src="/.axocoatl/preview-picker.js"></script>
            <script type="module">if (window !== top) await import('/app.js');</script>
          </body></html>`,
        });
      }
      if (url.pathname === '/app.js') {
        return route.fulfill({
          status: 200,
          contentType: 'text/javascript; charset=utf-8',
          body: `import { moduleValue } from '/dep.js';
            globalThis.__fixtureStage = 'module';
            const waitFor = async (read) => {
              for (let i = 0; i < 100; i += 1) {
                const value = read();
                if (value) return value;
                await new Promise((resolve) => setTimeout(resolve, 10));
              }
              throw new Error('fixture timed out');
            };
            let parentReadable = false;
            try { parent.document.body.dataset.previewEscaped = 'true'; parentReadable = true; } catch {}
            document.cookie = 'preview-host-only=from-a; Path=/';
            document.cookie = 'preview-parent-attempt=from-a; Domain=localhost; Path=/';
            const sourceHasHostCookie = document.cookie.includes('preview-host-only=from-a');
            localStorage.setItem('preview-storage', 'session-port-isolated');
            const fetched = await fetch('/api/data', {
              method: 'POST',
              headers: {
                'x-fixture': 'fetch',
                'authorization': 'Bearer app-token',
                'x-api-key': 'app-key',
              },
              body: 'fetch-body',
            }).then((response) => response.json());
            globalThis.__fixtureStage = 'fetch';
            const xhr = await new Promise((resolve, reject) => {
              const request = new XMLHttpRequest();
              request.open('GET', '/xhr');
              request.onload = () => resolve(request.responseText);
              request.onerror = reject;
              request.send();
            });
            globalThis.__fixtureStage = 'xhr';
            document.querySelector('#native-form').requestSubmit();
            const form = await waitFor(() => globalThis.__fixtureFormResult);
            globalThis.__fixtureStage = 'form';
            const asset = await waitFor(() => {
              const image = document.querySelector('#relative-asset');
              return image.complete && image.naturalWidth ? 'asset-ok' : null;
            });
            const compressed = await fetch('/compressed.json').then((response) => response.json());
            globalThis.__fixtureStage = 'encoding';
            let controlWritable = false;
            try {
              const response = await fetch(${JSON.stringify(`${runtime.baseUrl}${rebuildPath}`)}, {
                method: 'POST', mode: 'no-cors', body: 'blind-write',
              });
              await response.text();
              controlWritable = true;
            } catch {}
            globalThis.__fixtureStage = 'control-write';
            let controlReadable = false;
            try {
              await fetch(${JSON.stringify(`${runtime.baseUrl}/api/sessions`)}).then((response) => response.json());
              controlReadable = true;
            } catch {}
            globalThis.__fixtureStage = 'control-read';
            let crossPreviewWritable = false;
            try {
              const response = await fetch(${JSON.stringify(`${otherPreviewOrigin}/api/data`)}, {
                method: 'POST', body: 'cross-preview-write',
              });
              await response.text();
              crossPreviewWritable = true;
            } catch {}
            globalThis.__fixtureStage = 'cross-preview';
            const hmr = await new Promise((resolve, reject) => {
              const socket = new WebSocket('ws://' + location.host + '/hmr', 'vite-hmr');
              const timer = setTimeout(() => reject(new Error('HMR timed out')), 2000);
              socket.onopen = () => {
                socket.send('vite-ready');
                clearTimeout(timer);
                resolve('hmr-open');
                socket.close();
              };
              socket.onerror = reject;
            });
            globalThis.__fixtureStage = 'hmr';
            try {
              const storage = localStorage.getItem('preview-storage');
              globalThis.__fixtureStage = 'storage-read';
              const styled = getComputedStyle(document.body).getPropertyValue('--fixture-ready').trim();
              globalThis.__fixtureStage = 'style-read';
              globalThis.__previewFunctionalResult = {
                moduleValue, fetched, xhr, form, asset, compressed, hmr,
                storage, styled,
                sourceHasHostCookie,
                parentReadable, controlReadable, controlWritable, crossPreviewWritable,
                origin: location.origin,
              };
              globalThis.__fixtureStage = 'done';
            } catch (error) {
              globalThis.__fixtureAssignmentError = String(error && (error.stack || error));
            }`,
        });
      }
      if (url.pathname === '/dep.js') {
        return route.fulfill({
          status: 200, contentType: 'text/javascript', body: `export const moduleValue = 'module-ok';`,
        });
      }
      if (url.pathname === '/fixture.css') {
        return route.fulfill({
          status: 200, contentType: 'text/css', body: `body { --fixture-ready: css-ok; }`,
        });
      }
      if (url.pathname === '/asset.svg') {
        return route.fulfill({
          status: 200,
          contentType: 'image/svg+xml',
          body: `<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"></svg>`,
        });
      }
      if (url.pathname === '/api/data') {
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ method: request.method(), body: request.postData() }),
        });
      }
      if (url.pathname === '/xhr') {
        return route.fulfill({ status: 200, contentType: 'text/plain', body: 'xhr-ok' });
      }
      if (url.pathname === '/form-submit') {
        return route.fulfill({
          status: 200,
          contentType: 'text/html',
          body: `<script>parent.__fixtureFormResult = 'form-ok'</script>`,
        });
      }
      if (url.pathname === '/compressed.json') {
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          // The real proxy asks the upstream for identity bytes before HTML
          // inspection and preserves Content-Encoding on non-HTML streams.
          body: JSON.stringify({ decoded: 'identity-ok' }),
        });
      }
      return route.fulfill({ status: 404, contentType: 'text/plain', body: 'fixture only' });
    });
    await page.route((url) =>
      url.origin === otherPreviewOrigin && url.pathname === '/cookie-check', (route) => {
      otherPreviewCookieRequests.push(route.request().headers().cookie || '');
      return route.fulfill({
        status: 200,
        contentType: 'text/html; charset=utf-8',
        body: `<!doctype html><script>globalThis.__previewCookies = document.cookie;</script>`,
      });
    });

    await page.evaluate(() => sessionHome().refresh());
    await page.waitForFunction((sessionId) =>
      sessionHome().session(sessionId)?.environment?.state === 'ready', session.id);
    await page.waitForFunction(() => S.session.attemptRestorePending === false);
    const previewSandbox = await page.evaluate(async (value) => {
      clearInterval(S.session.taskTimer);
      S.session.taskTimer = null;
      applySessionEnvironmentUpdate(value, { activateRuntime: false });
      const activated = await activateOpenedSessionRuntime(value.id);
      if (!activated || !sessionRuntimeSurfaceReady(S.session)) {
        throw new Error('Preview contract fixture did not activate the Session runtime');
      }
      const preview = document.querySelector('#browser');
      preview.go('http://localhost:3000/app');
      return preview.frame?.getAttribute('sandbox') || '';
    }, ready);
    const iframeProperty = await page.locator('#browser').evaluateHandle((element) => element.frame);
    const iframe = iframeProperty.asElement();
    const previewFrame = await iframe?.contentFrame();
    assert.ok(previewFrame, 'Preview must expose the virtual-origin frame');
    try {
      await previewFrame.evaluate(() => new Promise((resolve, reject) => {
        const deadline = Date.now() + 10_000;
        const poll = () => {
          if (globalThis.__previewFunctionalResult != null) return resolve();
          if (Date.now() >= deadline) return reject(new Error('Preview fixture timed out'));
          setTimeout(poll, 25);
        };
        poll();
      }));
    } catch (error) {
      const frameDebug = await previewFrame.evaluate(() => ({
        stage: globalThis.__fixtureStage,
        form: globalThis.__fixtureFormResult,
        storage: localStorage.getItem('preview-storage'),
        assignmentError: globalThis.__fixtureAssignmentError,
      }));
      throw new Error([
        error.message,
        `frameDebug=${JSON.stringify(frameDebug)}`,
        `browserErrors=${JSON.stringify(browserErrors)}`,
        `fixtureRequests=${JSON.stringify(fixtureRequests)}`,
        `websocketMessages=${JSON.stringify(websocketMessages)}`,
      ].join('\n'));
    }

    assert.deepEqual(await previewFrame.evaluate(() => globalThis.__previewFunctionalResult), {
      moduleValue: 'module-ok',
      fetched: { method: 'POST', body: 'fetch-body' },
      xhr: 'xhr-ok',
      form: 'form-ok',
      asset: 'asset-ok',
      compressed: { decoded: 'identity-ok' },
      hmr: 'hmr-open',
      storage: 'session-port-isolated',
      styled: 'css-ok',
      sourceHasHostCookie: false,
      parentReadable: false,
      controlReadable: false,
      controlWritable: false,
      crossPreviewWritable: false,
      origin: previewOrigin,
    });
    assert.equal(previewSandbox, 'allow-scripts allow-same-origin allow-forms allow-popups allow-modals');
    assert.equal(new URL(await iframe.getAttribute('src')).origin, previewOrigin);
    assert.equal(await page.locator('body').getAttribute('data-preview-escaped'), null);
    assert.ok(fixtureRequests.some((request) =>
      request.path === '/api/data'
      && request.method === 'POST'
      && request.body === 'fetch-body'
      && request.host === previewHost
      && request.authorization === 'Bearer app-token'
      && request.apiKey === 'app-key'
      && request.origin === previewOrigin));
    assert.ok(fixtureRequests.some((request) =>
      request.path === '/form-submit'
      && request.method === 'POST'
      && request.body === 'fixture=native-form-ok'));
    assert.ok(websocketMessages.includes(`ws://${previewHost}/hmr`));
    assert.ok(websocketMessages.includes('vite-ready'));
    assert.equal(
      browserRequests.filter((request) =>
        request.method === 'POST' && new URL(request.url).pathname === rebuildPath).length,
      1,
      'the no-CORS blind write must reach the main-origin guard',
    );
    assert.ok(controlResponses.every((status) => status === 403));

    await page.evaluate(() => setBrowserPicking(true));
    await previewFrame.waitForSelector('.axo-tap-banner', { state: 'attached' });
    await previewFrame.evaluate(() => document.querySelector('#pick-target').click());
    assert.equal(
      await previewFrame.locator('#pick-target').evaluate((element) =>
        element.classList.contains('axo-tap-locked')),
      true,
      'the injected Pick bridge must capture the application element',
    );
    await page.waitForFunction(() =>
      !document.querySelector('#dom-hier')?.classList.contains('hide'));
    assert.match(await page.locator('#dom-hier-list').textContent(), /button#pick-target/);
    await page.evaluate(() => closeDomHier(false));

    // Modern Chromium blocks ordinary third-party cookies in the embedded
    // Preview. The explicit full-preview action opens this exact virtual URL
    // top-level (never the logical localhost address), where normal host-only
    // app cookies work without regaining access to the workbench origin.
    const fullPreviewButton = page.locator('ax-browser [data-act="open-full"]');
    assert.equal(await fullPreviewButton.getAttribute('href'), `${previewOrigin}/app`);
    assert.equal(await fullPreviewButton.getAttribute('target'), '_blank');
    assert.equal(await fullPreviewButton.getAttribute('rel'), 'noopener noreferrer');
    assert.equal(await fullPreviewButton.getAttribute('aria-disabled'), 'false');
    await page.locator('[data-session-view="browser"]').click();
    await fullPreviewButton.waitFor({ state: 'visible' });
    const pagesBeforeFullPreview = new Set(context.pages());
    await fullPreviewButton.evaluate((link) => link.addEventListener('click', () => {
      window.__fullPreviewLinkClicked = true;
    }, { once: true }));
    await fullPreviewButton.click();
    assert.equal(await page.evaluate(() => window.__fullPreviewLinkClicked), true);
    let fullPreviewPage = null;
    for (let attempt = 0; attempt < 100 && !fullPreviewPage; attempt += 1) {
      fullPreviewPage = context.pages().find((candidate) =>
        !pagesBeforeFullPreview.has(candidate)) || null;
      if (!fullPreviewPage) await new Promise((resolve) => setTimeout(resolve, 25));
    }
    assert.ok(fullPreviewPage, `full Preview did not open; pages=${context.pages()
      .map((candidate) => candidate.url()).join(',')}`);
    await fullPreviewPage.waitForLoadState('domcontentloaded');
    await fullPreviewPage.waitForFunction(() => globalThis.__topLevelCookieResult != null);
    assert.equal(fullPreviewPage.url(), `${previewOrigin}/app`);
    assert.notEqual(fullPreviewPage.url(), 'http://localhost:3000/app');
    const topLevelCookieResult = await fullPreviewPage.evaluate(() =>
      globalThis.__topLevelCookieResult);
    assert.equal(topLevelCookieResult.openerIsNull, true);
    assert.equal(topLevelCookieResult.cookies.includes('preview-host-only=from-a'), true);
    assert.equal(topLevelCookieResult.cookies.includes('preview-parent-attempt='), false);
    await fullPreviewPage.close();

    // Neither Preview A's host-only cookie nor its rejected parent-domain
    // attempt may cross into Preview B.
    await page.evaluate(() => document.querySelector('#browser')
      .go('http://localhost:5000/cookie-check'));
    const cookieIframeProperty = await page.locator('#browser')
      .evaluateHandle((element) => element.frame);
    const cookieIframe = cookieIframeProperty.asElement();
    const cookieFrame = await cookieIframe?.contentFrame();
    assert.ok(cookieFrame, 'the second Preview origin must load in the browser frame');
    await cookieFrame.waitForFunction(() => globalThis.__previewCookies != null);
    const cookiesAtOtherPreview = await cookieFrame.evaluate(() => globalThis.__previewCookies);
    assert.equal(cookiesAtOtherPreview.includes('preview-parent-attempt='), false);
    assert.equal(cookiesAtOtherPreview.includes('preview-host-only='), false);
    assert.ok(otherPreviewCookieRequests.length > 0);
    assert.ok(otherPreviewCookieRequests.every((cookie) =>
      !cookie.includes('preview-parent-attempt=') && !cookie.includes('preview-host-only=')));

    // The valid Preview Host boundary owns every path. None of these direct
    // URLs may fall through to the workbench when local auth is disabled.
    const directPage = await context.newPage();
    const missingOrigin = `http://ses-00000000-0000-0000-0000-000000000000-p3000.localhost:${listenerPort}`;
    for (const pathname of ['/', '/api/agents', '/ws', '/ui/shell.css']) {
      const response = await directPage.goto(`${missingOrigin}${pathname}`);
      assert.ok((response?.status() || 0) >= 400, `${pathname} must remain Preview-only`);
    }
    await directPage.close();
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Compare keeps surviving checked paths readable when one lane review is unavailable', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const controlAgents = [
    {
      id: session.mode.agent_id,
      name: 'Autonomous Judge',
      provider: 'ollama',
      model: 'qwen3:8b',
      role: 'autonomous',
      depends_on: [],
    },
    {
      id: 'browser-test-coordinator',
      name: 'Coordinator',
      provider: 'ollama',
      model: 'qwen3:8b',
      role: 'coordinator',
      depends_on: [],
    },
    {
      id: 'browser-test-worker',
      name: 'Worker',
      provider: 'ollama',
      model: 'qwen3:8b',
      role: 'worker',
      depends_on: [],
    },
  ];
  const setId = '0190aabb-ccdd-7eef-8899-aabbccddeeff';
  const lanes = [0, 1].map((index) => ({
    index,
    branch: `axo/test/${index}`,
    worktree: `/tmp/axo-test/${index}`,
    model: `fixture-model-${index + 1}`,
    agent: `fixture-agent-${index + 1}`,
    provider: 'ollama',
  }));
  const results = {
    attempt_set: {
      id: setId,
      session_id: session.id,
      task: 'Keep checked review evidence exact',
      instruction: 'Compare only the protected trees.',
      base_sha: '1111111111111111111111111111111111111111',
      base_tree: '2222222222222222222222222222222222222222',
      state: 'verified',
      created_at: 1,
      lanes,
    },
    lanes,
    lane_states: lanes.map(({ index }) => ({ index, state: 'completed' })),
    verdicts: lanes.map(({ index }) => ({
      index,
      passed: true,
      exit_code: 0,
      output: 'fixture checks passed',
      changed_files: 1,
      touched_tests: [],
      patch_sha256: 'a'.repeat(64),
    })),
    usage: [],
    outputs: [],
  };
  const statuses = [
    {
      index: 0,
      branch: lanes[0].branch,
      worktree: lanes[0].worktree,
      status: { branch: lanes[0].branch, files: [], clean: false },
      review_error: 'the protected checked tree is unavailable; run Checks again',
      model: lanes[0].model,
      agent: lanes[0].agent,
    },
    {
      index: 1,
      branch: lanes[1].branch,
      worktree: lanes[1].worktree,
      status: {
        branch: lanes[1].branch,
        clean: false,
        files: [{
          path: 'survivor.js',
          state: 'modified',
          added: 2,
          removed: 1,
          staged: false,
          unstaged: false,
          last_turn: false,
        }],
      },
      model: lanes[1].model,
      agent: lanes[1].agent,
    },
  ];
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: routedPage }) => {
      await routedPage.route('**/api/agents', (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(controlAgents),
      }));
      await routedPage.route(
        `**/api/sessions/${session.id}/variants/results`,
        (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(results),
        }),
      );
      await routedPage.route(
        `**/api/sessions/${session.id}/variants/status**`,
        (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(statuses),
        }),
      );
      await routedPage.route(
        `**/api/sessions/${session.id}/variants/trajectories**`,
        (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ baseline: 0, lanes: [] }),
        }),
      );
      await routedPage.route(
        `**/api/sessions/${session.id}/variants/judge`,
        (route) => route.fulfill({
          status: 422,
          contentType: 'application/json',
          body: JSON.stringify({
            error: 'Judge returned invalid structured output',
            control_usage: {
              calls: 2,
              input_tokens: 21,
              output_tokens: 8,
              reasoning_tokens: 5,
              token_usage_known: false,
            },
          }),
        }),
      );
    },
  );
  try {
    await page.locator('#panes-menu-btn').click();
    await page.locator('#panes-menu [role="menuitem"]', { hasText: 'Review attempts' }).click();
    await page.waitForFunction(() => S.cockpitLayout.center === 'compare');
    const cards = page.locator('ax-compare .lane-card');
    await cards.first().waitFor({ state: 'visible' });
    assert.match(
      await cards.nth(0).textContent(),
      /Changed paths unavailable: the protected checked tree is unavailable; run Checks again/,
    );
    assert.equal(await cards.nth(0).locator('[data-inspect]').count(), 0);
    assert.match(await cards.nth(1).textContent(), /survivor\.js/);
    assert.equal(await cards.nth(1).locator('[data-inspect]').count() > 0, true);
    const judgeAgent = page.locator('ax-compare [data-judge-agent]');
    await judgeAgent.waitFor({ state: 'visible' });
    assert.deepEqual(
      await judgeAgent.locator('option').evaluateAll((options) =>
        options.map((option) => option.value)),
      [session.mode.agent_id],
    );
    await page.locator('ax-compare [data-act="judge"]').click();
    const judgeError = page.locator('ax-compare [role="alert"]');
    await judgeError.waitFor({ state: 'visible' });
    assert.match(await judgeError.textContent(), /Judge returned invalid structured output/);
    assert.match(
      await judgeError.textContent(),
      /Judge · 2 calls · 21 in \/ 8 out \/ 5 reasoning · 34 total tokens · known lower bound · incomplete/,
    );
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Closed unresolved Attempts mount read-only and route setup-free recovery without Reopen', async () => {
  const cases = [
    {
      session: runtime.fixtures.alpha.sessions[0],
      state: 'applied',
      endpoint: 'adopt',
      control: '[data-keep="0"]',
      label: 'Finish Keep',
      expectedBody: (setId) => ({ attempt_set_id: setId, index: 0 }),
    },
    {
      session: runtime.fixtures.alpha.sessions[1],
      state: 'discarding',
      endpoint: 'discard',
      control: '[data-discard]',
      label: 'Finish cleanup',
      expectedBody: (setId) => ({ attempt_set_id: setId }),
    },
  ];

  for (const recovery of cases) {
    const closed = structuredClone(recovery.session);
    closed.status = 'closed';
    closed.environment = {
      ...closed.environment,
      generation: Number(closed.environment?.generation || 0) + 1,
      state: 'ready',
      error: null,
      setup_results: [],
    };
    const results = closedRecoveryResults(closed, recovery.state);
    const setId = results.attempt_set.id;
    let routedAction = null;
    let resolveRoutedAction;
    const routedActionPromise = new Promise((resolve) => { resolveRoutedAction = resolve; });

    const opened = await openSession(
      closed,
      { width: 1280, height: 800 },
      runtime,
      async ({ page }) => {
        await page.route(
          `**/api/workspaces/${encodeURIComponent(closed.workspace_id)}/sessions`,
          (route) => route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify([closed]),
          }),
        );
        await page.route('**/api/sessions', (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify([closed]),
        }));
        await page.route(`**/api/sessions/${closed.id}/turns`, (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify([{
            id: `closed-turn-${recovery.state}`,
            user_input: 'Durable request before Close',
            final_output: 'Durable answer remains readable after Close.',
            status: 'completed',
            agent_id: 'browser-test-coder',
            context: [],
            execution_events: [],
            agent_outputs: [],
          }]),
        }));
        await page.route(`**/api/sessions/${closed.id}/active-turn`, (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ run: null }),
        }));
        await page.route(
          `**/api/sessions/${closed.id}/variants/results`,
          (route) => route.fulfill({
            status: 200,
            contentType: 'application/json',
            body: JSON.stringify(results),
          }),
        );
        await page.route(
          `**/api/sessions/${closed.id}/variants/${recovery.endpoint}`,
          (route) => {
            routedAction = {
              method: route.request().method(),
              body: route.request().postDataJSON(),
            };
            resolveRoutedAction(routedAction);
            return route.fulfill({
              status: 200,
              contentType: 'application/json',
              body: JSON.stringify({ recovered: true }),
            });
          },
        );
        // If implicit activation ever returns, fail at the product seam without
        // letting this regression mutate the fixture daemon's real Session.
        await page.route(`**/api/sessions/${closed.id}/reopen`, (route) => route.fulfill({
          status: 409,
          contentType: 'application/json',
          body: JSON.stringify({ error: 'Reopen must be an explicit banner action' }),
        }));
      },
    );
    const { context, page, browserRequests, assertNoBrowserErrors } = opened;
    try {
      await page.waitForFunction(({ sessionId, expectedSetId }) => {
        const compare = document.querySelector('#compare');
        return S.session.id === sessionId
          && S.session.status === 'closed'
          && S.session.historyState === 'ready'
          && S.threadVariants?.attemptSetId === expectedSetId
          && compare?.attemptSetId === expectedSetId;
      }, { sessionId: closed.id, expectedSetId: setId });

      assert.match(await page.locator('#session-msgs').textContent(), /Durable request before Close/);
      assert.match(await page.locator('#session-msgs').textContent(), /Durable answer remains readable/);
      assert.equal(await page.locator('#session-environment-title').textContent(), 'Session is closed');
      assert.equal(await page.locator('#session-environment-action').textContent(), 'Reopen session');
      assert.equal(await page.locator('#session-send').textContent(), 'Session closed');
      assert.equal(await page.locator('#session-send').isDisabled(), true);
      assert.equal(await page.locator('.session-input').getAttribute('inert'), '');
      assert.equal(await page.locator('#source-control').getAttribute('session'), null);
      assert.equal(await page.locator('#browser').getAttribute('session'), null);
      assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
      assert.equal(await editorSessionBinding(page), closed.id);
      assert.equal(await editorSuspended(page), true);
      assert.equal(await page.locator('#compare').getAttribute('session'), closed.id);
      assert.equal(await page.locator('#attempts').getAttribute('session'), closed.id);
      assert.equal(await page.locator('#session-history').getAttribute('rewind-enabled'), 'false');
      assert.equal(await page.locator('#session-msgs .smsg-act', { hasText: 'Rewind' }).count(), 0);
      assert.equal(await page.evaluate(() => S.session.taskTimer == null), true);

      const reopenRequests = () => browserRequests.filter((request) => request.method === 'POST'
        && new URL(request.url).pathname === `/api/sessions/${closed.id}/reopen`);
      const runtimeRequests = () => browserRequests.filter((request) => {
        const pathname = new URL(request.url).pathname;
        return pathname.startsWith(`/api/sessions/${closed.id}/tasks`)
          || pathname.startsWith(`/api/sessions/${closed.id}/tree`)
          || pathname.startsWith(`/api/sessions/${closed.id}/file`)
          || pathname.startsWith(`/api/sessions/${closed.id}/git`)
          || pathname.startsWith(`/api/sessions/${closed.id}/preview`);
      });
      assert.equal(reopenRequests().length, 0);
      assert.deepEqual(runtimeRequests(), []);

      await page.locator('#panes-menu-btn').click();
      await page.locator('#panes-menu [role="menuitem"]', { hasText: 'Review attempts' }).click();
      await page.waitForFunction(() => S.cockpitLayout.center === 'compare');
      const control = page.locator(`ax-compare ${recovery.control}`);
      await control.waitFor({ state: 'visible' });
      assert.equal(await control.textContent(), recovery.label);
      assert.equal(await control.isEnabled(), true);
      const actionRequest = page.waitForRequest((request) => request.method() === 'POST'
        && new URL(request.url()).pathname
          === `/api/sessions/${closed.id}/variants/${recovery.endpoint}`);
      await control.click();
      await actionRequest;
      await routedActionPromise;
      assert.deepEqual(routedAction, {
        method: 'POST',
        body: recovery.expectedBody(setId),
      });
      assert.equal(reopenRequests().length, 0);
      assert.deepEqual(runtimeRequests(), []);
      assertNoBrowserErrors();
    } finally {
      await context.close();
    }
  }
});

test('a cross-tab Close removes Ready runtime bindings until an explicit Reopen', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const { context, page, browserRequests, assertNoBrowserErrors } = await openSession(session);
  try {
    const active = structuredClone(session);
    active.status = 'active';
    active.environment = {
      ...active.environment,
      generation: Number(active.environment?.generation || 0) + 1,
      state: 'ready',
      error: null,
      setup_results: [],
    };
    const closed = structuredClone(active);
    closed.status = 'closed';
    let listed = closed;
    let reopened = false;

    await page.route(
      `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
      (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify([listed]),
      }),
    );
    await page.route('**/api/sessions', (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: JSON.stringify([listed]),
    }));
    await page.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: '[]',
    }));
    await page.route(`**/api/sessions/${session.id}/tasks`, async (route) => {
      if (!reopened) {
        return route.fulfill({
          status: 409,
          contentType: 'application/json',
          body: JSON.stringify({ error: 'Session is closed; reopen it explicitly' }),
        });
      }
      if (route.request().method() === 'POST') {
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ id: 'reopened-shell' }),
        });
      }
      return route.fulfill({ status: 200, contentType: 'application/json', body: '[]' });
    });
    await page.route(`**/api/sessions/${session.id}/reopen`, (route) => {
      reopened = true;
      listed = active;
      return route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify(active),
      });
    });

    // Supersede any Session-home refresh started while the cockpit was opening
    // before injecting the stale Ready state this test is meant to reconcile.
    await page.evaluate(() => sessionHome().refresh());
    await page.waitForFunction((sessionId) =>
      sessionHome().session(sessionId)?.status === 'closed', session.id);
    await page.evaluate((ready) => {
      clearInterval(S.session.taskTimer);
      S.session.taskTimer = null;
      applySessionEnvironmentUpdate(ready, { activateRuntime: false });
    }, active);
    const activeState = await page.evaluate(() => ({
      ready: sessionEnvironmentReady(),
      id: S.session.id,
      status: S.session.status,
      environment: S.session.environment,
    }));
    assert.equal(activeState.ready, true, JSON.stringify(activeState));

    await page.evaluate((sessionId) => refreshSessionTasks(sessionId), session.id);
    await page.waitForFunction(() => S.session.status === 'closed');
    assert.equal(await page.evaluate(() => sessionEnvironmentReady()), false);
    assert.equal(await page.locator('#session-environment-title').textContent(), 'Session is closed');
    assert.equal(await page.locator('#session-environment-action').textContent(), 'Reopen session');
    assert.equal(await page.locator('#session-send').textContent(), 'Session closed');
    assert.equal(await page.locator('#source-control').getAttribute('session'), null);
    assert.equal(await page.locator('#browser').getAttribute('session'), null);
    assert.equal(await page.locator('#file-tree').evaluate((element) => element.session), '');
    assert.equal(await editorSessionBinding(page), session.id);
    assert.equal(await editorSuspended(page), true);

    const terminalRequest = page.waitForRequest((request) => request.method() === 'POST'
      && new URL(request.url()).pathname === `/api/sessions/${session.id}/tasks`);
    await page.locator('#session-environment-action').click();
    await page.waitForFunction(() => S.session.status === 'active'
      && sessionEnvironmentReady()
      && sessionRuntimeSurfaceReady(S.session)
      && !S.session.runtimeActivationInFlight);
    assert.equal((await terminalRequest).postDataJSON()?.command, 'sh');
    assert.equal(await page.locator('#session-environment').isHidden(), true);
    assert.equal(await page.locator('#source-control').getAttribute('session'), session.id);
    assert.equal(
      browserRequests.filter((request) => request.method === 'POST'
        && new URL(request.url).pathname === `/api/sessions/${session.id}/reopen`).length,
      1,
    );
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('malformed devcontainer repair can explicitly retain the configured E2B template', async () => {
  const session = runtime.fixtures.alpha.sessions[1];
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  try {
    await page.route('**/api/fs/project?**', (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        devcontainer: { error: 'expected value at line 1 column 1' },
        runtime: {
          backend: 'e2b', supports_session_image: false, supports_preview: false, template: 'base',
        },
        suggested_setup: { source: 'package-lock', command: 'npm ci' },
        axocoatl_md: [],
      }),
    }));
    await page.evaluate((sessionId) => sessionHome().configureEnvironment(sessionId), session.id);
    const choose = page.locator('ax-session-home [data-action="confirm-e2b-template"]');
    const apply = page.locator('ax-session-home [data-action="picker-use"]');
    await choose.waitFor({ state: 'visible' });
    assert.equal(await choose.textContent(), 'Use E2B template');
    assert.equal(await apply.isDisabled(), true);

    await choose.click();
    assert.equal(await choose.textContent(), 'E2B template selected');
    assert.equal(await choose.isDisabled(), true);
    assert.equal(await apply.isEnabled(), true);
    assert.equal(await page.locator('ax-session-home .config-help', {
      hasText: 'You explicitly selected the daemon-configured E2B template',
    }).count(), 1);

    await page.locator('ax-session-home [data-action="picker-cancel"]').click();
    await page.evaluate((workspaceId) => sessionHome().newSession(workspaceId), session.workspace_id);
    await page.locator('ax-session-home .config-help', {
      hasText: 'Unavailable with the configured E2B runtime',
    }).waitFor({ state: 'visible' });
    assert.equal(await page.locator('ax-session-home [data-field="ports"]').count(), 0);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Session creation hides coordinator-owned Workers from Single and Custom controls', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const agents = [
    {
      id: 'browser-test-autonomous', name: 'Autonomous Agent', role: 'autonomous', depends_on: [],
    },
    {
      id: 'browser-test-worker', name: 'Coordinator Worker', role: 'worker', depends_on: [],
    },
    {
      id: 'browser-test-coordinator', name: 'Session Coordinator', role: 'coordinator', depends_on: [],
    },
  ];
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: target }) => {
      await target.route('**/api/agents', (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(agents),
      }));
    },
  );
  try {
    await page.waitForFunction(() => sessionHome().agents.length === 3);
    await page.evaluate((workspaceId) => sessionHome().newSession(workspaceId), session.workspace_id);

    const singleAgent = page.locator('ax-session-home [data-field="agent"]');
    await singleAgent.waitFor({ state: 'visible' });
    assert.deepEqual(
      await singleAgent.locator('option').evaluateAll((options) =>
        options.map((option) => ({ value: option.value, label: option.textContent }))),
      [
        { value: 'browser-test-autonomous', label: 'Autonomous Agent' },
        { value: 'browser-test-coordinator', label: 'Session Coordinator' },
      ],
    );
    assert.equal(await singleAgent.inputValue(), 'browser-test-autonomous');

    await page.locator('ax-session-home [data-field="mode"]').selectOption('custom');
    const customAgents = page.locator('ax-session-home [data-field="custom-agent"]');
    assert.deepEqual(
      await customAgents.evaluateAll((inputs) => inputs.map((input) => input.value)),
      ['browser-test-autonomous', 'browser-test-coordinator'],
    );
    assert.equal(
      await page.locator('ax-session-home [data-field="custom-agent"][value="browser-test-worker"]').count(),
      0,
    );
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Coordinator Sessions expose neither Ways nor rewind as autonomous-Agent actions', async () => {
  const fixture = runtime.fixtures.alpha.sessions[0];
  const session = structuredClone(fixture);
  session.status = 'active';
  session.environment = {
    ...session.environment,
    state: 'ready',
    setup_command: null,
    setup_approved: true,
    setup_reviewed: true,
    error: null,
  };
  const agentId = session.mode.agent_id;
  const coordinator = {
    id: agentId,
    name: 'Session Coordinator',
    role: 'coordinator',
    depends_on: [],
  };
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: target }) => {
      await target.route('**/api/sessions', (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([session]),
      }));
      await target.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify([session]),
        }),
      );
      await target.route('**/api/agents', (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([coordinator]),
      }));
      await target.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([{
          id: 'coordinator-shell', kind: 'terminal', command: 'sh', status: 'running',
        }]),
      }));
      await target.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: '[]',
      }));
      await target.route(`**/api/sessions/${session.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? { branch: 'main', files: [], clean: true }
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(body),
        });
      });
    },
  );
  try {
    await page.waitForFunction((expectedAgentId) =>
      S.agents.some((agent) => agent.id === expectedAgentId && agent.role === 'coordinator'), agentId);
    await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session));
    const ways = page.locator('#fanout-btn');
    assert.equal(await ways.isDisabled(), true);
    assert.match(await ways.getAttribute('title'), /requires an autonomous Agent/);
    assert.equal(await page.locator('#session-history').getAttribute('rewind-enabled'), 'false');

    await page.evaluate((expectedAgentId) => {
      S.agents = [{ id: expectedAgentId, name: 'Autonomous Agent', role: 'autonomous' }];
      syncSessionRoleCapabilities(S.session);
    }, agentId);
    assert.equal(await ways.isEnabled(), true);
    assert.equal(await page.locator('#session-history').getAttribute('rewind-enabled'), 'true');
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Ways preparation exposes only autonomous lane and planning Agents', async () => {
  const session = structuredClone(runtime.fixtures.alpha.sessions[0]);
  session.status = 'active';
  session.environment = {
    ...session.environment,
    state: 'ready',
    setup_command: null,
    setup_approved: true,
    setup_reviewed: true,
    error: null,
  };
  const autonomousId = session.mode.agent_id;
  const agents = [
    {
      id: autonomousId,
      name: 'Autonomous Agent',
      provider: 'ollama',
      model: 'qwen3:8b',
      role: 'autonomous',
      depends_on: [],
    },
    {
      id: 'browser-test-coordinator',
      name: 'Session Coordinator',
      provider: 'ollama',
      model: 'qwen3:8b',
      role: 'coordinator',
      depends_on: [],
    },
    {
      id: 'browser-test-worker',
      name: 'Coordinator Worker',
      provider: 'ollama',
      model: 'qwen3:8b',
      role: 'worker',
      depends_on: [],
    },
  ];
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: target }) => {
      await target.route('**/api/sessions', (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([session]),
      }));
      await target.route(
        `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
        (route) => route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify([session]),
        }),
      );
      await target.route('**/api/agents', (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(agents),
      }));
      await target.route(`**/api/sessions/${session.id}/tasks`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([{
          id: 'ways-preparation-shell', kind: 'terminal', command: 'sh', status: 'running',
        }]),
      }));
      await target.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: '[]',
      }));
      await target.route(`**/api/sessions/${session.id}/git/**`, (route) => {
        const pathname = new URL(route.request().url()).pathname;
        const body = pathname.endsWith('/git/status')
          ? { branch: 'main', files: [], clean: true }
          : pathname.endsWith('/git/branches')
            ? { current: 'main', branches: ['main'] }
            : {};
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(body),
        });
      });
      await target.route(
        `**/api/sessions/${session.id}/variants/plan`,
        (route) => route.fulfill({
          status: 422,
          contentType: 'application/json',
          body: JSON.stringify({
            error: 'Plan returned invalid structured output',
            control_usage: {
              calls: 2,
              input_tokens: 34,
              output_tokens: 13,
              reasoning_tokens: 7,
              token_usage_known: false,
            },
          }),
        }),
      );
    },
  );
  try {
    await page.waitForFunction((expectedId) =>
      S.agents.some((agent) => agent.id === expectedId && agent.role === 'autonomous'), autonomousId);
    await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session));
    assert.equal(await page.locator('#fanout-btn').isEnabled(), true);
    await page.locator('#fanout-btn').click();
    const fanout = page.locator('#fanout');
    await fanout.locator('.pop').waitFor({ state: 'visible' });
    const laneOptions = await fanout
      .locator('.rows .row')
      .first()
      .locator('select[title="Which agent runs this attempt"] option')
      .evaluateAll((options) => options.map((option) => option.value));
    assert.deepEqual(laneOptions, ['', autonomousId]);

    await fanout.locator('.sw').click();
    await fanout.getByRole('button', { name: 'Close answer mode configuration', exact: true }).click();
    await page.locator('#session-text').fill('Compare two safe implementations.');
    await page.locator('#attempts-dock-toggle').click();
    const planner = page.locator('#attempts [data-planner]');
    await planner.waitFor({ state: 'visible' });
    assert.deepEqual(
      await planner.locator('option').evaluateAll((options) =>
        options.map((option) => option.value)),
      [autonomousId],
    );
    await page.locator('#attempts [data-action="plan"]').click();
    const planError = page.locator('#attempts [role="alert"]');
    await planError.waitFor({ state: 'visible' });
    assert.match(await planError.textContent(), /Plan returned invalid structured output/);
    assert.match(
      await planError.textContent(),
      /Plan · 2 calls · 34 in \/ 13 out \/ 7 reasoning · 54 total tokens · known lower bound · incomplete/,
    );
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Coordinator run view makes every worker and Session terminal state explicit', async () => {
  const session = structuredClone(runtime.fixtures.alpha.sessions[0]);
  session.status = 'active';
  const coordinator = session.mode.agent_id;
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  try {
    await page.evaluate(({ sessionId, coordinatorId }) => {
      handleWsFrame({
        kind: 'coordinator-plan',
        workflow: sessionId,
        coordinator: coordinatorId,
        goal: 'Exercise every terminal boundary',
        subtasks: [
          { name: 'cancelled task', winner: 'worker-a', score: 1, bids: [] },
          { name: 'panicked task', winner: 'worker-b', score: 1, bids: [] },
        ],
      });
      handleWsFrame({
        kind: 'event', type: 'AgentActivated', workflow: sessionId, agent: 'worker-a',
      });
      handleWsFrame({
        kind: 'event', type: 'AgentActivated', workflow: sessionId, agent: 'worker-b',
      });
      handleWsFrame({
        kind: 'event', type: 'AgentCancelled', workflow: sessionId, agent: 'worker-a',
        output: 'partial worker evidence', tokens: 7,
      });
      handleWsFrame({
        kind: 'event', type: 'AgentPanicked', workflow: sessionId, agent: 'worker-b',
        output: 'worker panic evidence',
      });
    }, { sessionId: session.id, coordinatorId: coordinator });

    assert.deepEqual(await page.evaluate(() => ({
      cancelled: S.runView.workers['worker-a'],
      panicked: S.runView.workers['worker-b'],
    })), {
      cancelled: { status: 'cancelled', output: 'partial worker evidence', tokens: 7 },
      panicked: { status: 'error', output: 'worker panic evidence', tokens: 0 },
    });
    assert.match(await page.locator('#run-view-body').textContent(), /partial worker evidence/);
    assert.match(await page.locator('#run-view-body').textContent(), /worker panic evidence/);

    await page.evaluate((sessionId) => handleWsFrame({
      kind: 'session-cancelled', session: sessionId, input_tokens: 11, output_tokens: 5,
    }), session.id);
    assert.equal(await page.locator('#run-view-status').textContent(), 'stopped');
    assert.equal(await page.locator('#run-view-status').getAttribute('class'), 'run-view-status cancelled');

    await page.evaluate(({ sessionId, coordinatorId }) => {
      handleWsFrame({
        kind: 'coordinator-plan', workflow: sessionId, coordinator: coordinatorId,
        goal: 'Complete synthesis', subtasks: [],
      });
      handleWsFrame({
        kind: 'token', workflow: sessionId, agent: coordinatorId, delta: 'final ',
      });
      handleWsFrame({
        kind: 'token', workflow: sessionId, agent: coordinatorId, delta: 'answer',
      });
      handleWsFrame({
        kind: 'session-done', session: sessionId, input_tokens: 3, output_tokens: 2,
      });
    }, { sessionId: session.id, coordinatorId: coordinator });
    assert.equal(await page.locator('#run-view-status').textContent(), 'done');
    assert.match(await page.locator('#run-view-body').textContent(), /final answer/);
    assert.deepEqual(await page.evaluate(() => ({
      status: S.runView.status,
      synthesis: S.runView.synthesis,
      tokens: S.runView.tokens,
    })), { status: 'done', synthesis: 'final answer', tokens: 5 });

    await page.evaluate(({ sessionId, coordinatorId }) => {
      S.runView = null;
      handleWsFrame({
        kind: 'snapshot',
        approvals: [],
        runs: [{
          kind: 'session',
          workflow: sessionId,
          turn_id: 'turn-reconnected',
          coordinator: coordinatorId,
          goal: 'Resume the visible plan',
          subtasks: [
            { name: 'finished work', winner: 'worker-a', score: 1, bids: [] },
            { name: 'active work', winner: 'worker-b', score: 1, bids: [] },
          ],
          agents: [
            { agent: coordinatorId, status: 'running', output: 'partial synthesis', tokens: 2 },
            { agent: 'worker-a', status: 'done', output: 'finished evidence', tokens: 4 },
            { agent: 'worker-b', status: 'running', output: 'partial evidence', tokens: 1 },
          ],
        }],
      });
    }, { sessionId: session.id, coordinatorId: coordinator });
    assert.deepEqual(await page.evaluate(() => ({
      status: S.runView.status,
      goal: S.runView.goal,
      synthesis: S.runView.synthesis,
      finished: S.runView.workers['worker-a'],
      active: S.runView.workers['worker-b'],
    })), {
      status: 'running',
      goal: 'Resume the visible plan',
      synthesis: 'partial synthesis',
      finished: { status: 'done', output: 'finished evidence', tokens: 4 },
      active: { status: 'running', output: 'partial evidence', tokens: 1 },
    });
    assert.equal(await page.locator('#run-view-status').textContent(), 'running');
    assert.match(await page.locator('#run-view-body').textContent(), /finished evidence/);
    assert.match(await page.locator('#run-view-body').textContent(), /partial evidence/);

    const reconnectReplay = await page.evaluate((coordinatorId) => {
      const turnId = 'turn-ledger-ahead';
      S.session.activeTurnId = turnId;
      renderSessionTurns([{
        id: turnId,
        session_id: S.session.id,
        status: 'running',
        user_input: 'Reconnect safely',
        agent_id: coordinatorId,
        partial_output: 'already persisted',
        agent_outputs: [],
        execution_events: [{
          kind: 'tool_started',
          execution_id: 'call_0',
          metadata: {
            agent_id: coordinatorId,
            occurrence: 0,
            tool_name: 'echo',
            arguments: { value: 'once' },
          },
        }, {
          kind: 'tool_result',
          execution_id: 'call_0',
          metadata: {
            agent_id: coordinatorId,
            occurrence: 0,
            tool_name: 'echo',
            result: { value: 'once' },
            is_error: false,
          },
        }],
      }], {
        preserveLive: true,
        liveRun: {
          agents: [{
            agent: coordinatorId,
            status: 'running',
            output: 'already ',
            thinking: 'restored reasoning',
          }],
        },
      });
      const beforeCards = document.querySelectorAll('#session-msgs .toolcard').length;
      sessionAppendText('persisted', coordinatorId);
      sessionToolStart({
        turn_id: turnId,
        agent: coordinatorId,
        call_id: 'call_0',
        occurrence: 0,
        name: 'echo',
        arguments: { value: 'once' },
      });
      sessionToolResult({
        turn_id: turnId,
        agent: coordinatorId,
        call_id: 'call_0',
        occurrence: 0,
        name: 'echo',
        result: { value: 'once' },
        is_error: false,
      });
      return {
        output: sessionLiveStream(coordinatorId).streamBuf,
        replayCovered: sessionLiveStream(coordinatorId).replayCovered,
        reasoning: sessionLiveStream(coordinatorId).reasoningBuf,
        beforeCards,
        afterCards: document.querySelectorAll('#session-msgs .toolcard').length,
      };
    }, coordinator);
    assert.deepEqual(reconnectReplay, {
      output: 'already persisted',
      replayCovered: '',
      reasoning: 'restored reasoning',
      beforeCards: 1,
      afterCards: 1,
    });
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('Session History hydration preserves live frames that arrive while turns are in flight', async () => {
  const session = structuredClone(runtime.fixtures.alpha.sessions[0]);
  session.status = 'active';
  const agent = session.mode.agent_id;
  const turnId = 'turn-history-live-race';
  let noteTurnsRequested;
  const turnsRequested = new Promise((resolve) => { noteTurnsRequested = resolve; });
  let releaseTurns;
  const turnsMayReturn = new Promise((resolve) => { releaseTurns = resolve; });
  const activeRun = {
    kind: 'session',
    workflow: session.id,
    turn_id: turnId,
    agents: [{
      agent,
      status: 'running',
      output: 'before ',
      thinking: '',
      tokens: 0,
    }],
  };
  // Install a coherent active-turn + History boundary before navigation. The
  // real fixture daemon is idle; allowing any of its already-started active
  // reads to race a synthetic WebSocket owner would test contradictory server
  // state instead of the delayed-History merge below.
  const { context, page, assertNoBrowserErrors } = await openSession(
    session,
    { width: 1280, height: 800 },
    runtime,
    async ({ page: routedPage }) => {
      await routedPage.route(`**/api/sessions/${session.id}/active-turn`, (route) => route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ run: activeRun }),
      }));
      await routedPage.route(`**/api/sessions/${session.id}/turns`, async (route) => {
        noteTurnsRequested();
        await turnsMayReturn;
        await route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify([{
            id: turnId,
            session_id: session.id,
            status: 'running',
            user_input: 'Prove the hydration boundary',
            agent_id: agent,
            partial_output: 'before ',
            final_output: null,
            agent_outputs: [],
            execution_events: [],
            context: [],
          }]),
        });
      });
    },
  );
  try {
    await page.waitForFunction(() => _liveSessionSnapshotEpoch > 0);
    await turnsRequested;

    await page.evaluate(({ sessionId, activeTurnId, agentId }) => handleWsFrame({
      kind: 'snapshot',
      approvals: [],
      runs: [{
        kind: 'session',
        workflow: sessionId,
        turn_id: activeTurnId,
        agents: [{
          agent: agentId,
          status: 'running',
          output: 'before ',
          thinking: '',
          tokens: 0,
        }],
      }],
    }), { sessionId: session.id, activeTurnId: turnId, agentId: agent });
    await page.evaluate(({ sessionId, activeTurnId, agentId }) => {
      handleWsFrame({
        kind: 'token', workflow: sessionId, turn_id: activeTurnId,
        agent: agentId, delta: 'after',
      });
      handleWsFrame({
        kind: 'reasoning', workflow: sessionId, turn_id: activeTurnId,
        agent: agentId, delta: 'live thought',
      });
      handleWsFrame({
        kind: 'tool-call', workflow: sessionId, turn_id: activeTurnId,
        agent: agentId, call_id: 'call-race', occurrence: 0,
        name: 'echo', phase: 'start', arguments: { value: 'once' },
      });
      handleWsFrame({
        kind: 'tool-call', workflow: sessionId, turn_id: activeTurnId,
        agent: agentId, call_id: 'call-race', occurrence: 0,
        name: 'echo', phase: 'result', result: { value: 'once' }, is_error: false,
      });
    }, { sessionId: session.id, activeTurnId: turnId, agentId: agent });
    releaseTurns();

    await page.waitForFunction((agentId) => {
      const live = sessionLiveStream(agentId);
      return live.streamBuf === 'before after'
        && live.reasoningBuf === 'live thought'
        && document.querySelectorAll('#session-msgs .toolcard').length === 1
        && document.querySelector('#session-msgs .toolcard')?.dataset.resultApplied === 'true';
    }, agent);
    const hydrated = await page.evaluate((agentId) => ({
      output: sessionLiveStream(agentId).streamBuf,
      reasoning: sessionLiveStream(agentId).reasoningBuf,
      assistantText: Array.from(document.querySelectorAll('#session-msgs .smsg-body'))
        .map((node) => node.innerText).filter((text) => text.includes('before')).join(' '),
      cards: document.querySelectorAll('#session-msgs .toolcard').length,
      toolText: document.querySelector('#session-msgs .toolcard')?.textContent || '',
      reasoningCards: document.querySelectorAll('#session-msgs .chat-reasoning').length,
      reasoningSummary: document.querySelector('#session-msgs .chat-reasoning summary')?.textContent || '',
      reasoningText: document.querySelector('#session-msgs .chat-reasoning-body')?.textContent || '',
    }), agent);
    assert.deepEqual({
      output: hydrated.output,
      reasoning: hydrated.reasoning,
      assistantText: hydrated.assistantText,
      cards: hydrated.cards,
      reasoningCards: hydrated.reasoningCards,
      reasoningSummary: hydrated.reasoningSummary,
      reasoningText: hydrated.reasoningText,
    }, {
      output: 'before after',
      reasoning: 'live thought',
      assistantText: 'before after',
      cards: 1,
      reasoningCards: 1,
      reasoningSummary: 'Thinking…',
      reasoningText: 'live thought',
    });
    assert.match(hydrated.toolText, /once/);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('stale History response cannot resurrect a prior turn over a newer accepted turn', async () => {
  const session = structuredClone(runtime.fixtures.alpha.sessions[0]);
  session.status = 'active';
  const agent = session.mode.agent_id;
  const oldTurn = 'turn-history-old';
  const newTurn = 'turn-history-new';
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  try {
    await page.waitForFunction(() => _liveSessionSnapshotEpoch > 0);
    let noteTurnsRequested;
    const turnsRequested = new Promise((resolve) => { noteTurnsRequested = resolve; });
    let releaseTurns;
    const turnsMayReturn = new Promise((resolve) => { releaseTurns = resolve; });
    await page.route(`**/api/sessions/${session.id}/turns`, async (route) => {
      noteTurnsRequested();
      await turnsMayReturn;
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify([{
          id: oldTurn,
          session_id: session.id,
          status: 'running',
          user_input: 'The prior turn',
          agent_id: agent,
          partial_output: 'stale turn A output',
          final_output: null,
          agent_outputs: [],
          execution_events: [],
          context: [],
        }]),
      }).catch(() => {});
    });

    await page.evaluate(({ sessionId, turnId, agentId }) => handleWsFrame({
      kind: 'snapshot', approvals: [],
      runs: [{
        kind: 'session', workflow: sessionId, turn_id: turnId,
        agents: [{
          agent: agentId, status: 'running', output: 'stale turn A output',
          thinking: '', tokens: 0,
        }],
      }],
    }), { sessionId: session.id, turnId: oldTurn, agentId: agent });
    await turnsRequested;

    await page.evaluate(({ sessionId, firstTurn, secondTurn, agentId }) => {
      handleWsFrame({
        kind: 'session-done', session: sessionId, turn_id: firstTurn,
        input_tokens: 1, output_tokens: 1,
      });
      handleWsFrame({ kind: 'session-accepted', session: sessionId, turn_id: secondTurn });
      handleWsFrame({ kind: 'session-start', session: sessionId, turn_id: secondTurn });
      handleWsFrame({
        kind: 'token', workflow: sessionId, turn_id: secondTurn,
        agent: agentId, delta: 'new turn B output',
      });
    }, {
      sessionId: session.id, firstTurn: oldTurn, secondTurn: newTurn, agentId: agent,
    });
    releaseTurns();

    await page.waitForFunction(({ sessionId, turnId, agentId }) =>
      S.session.activeTurnId === turnId
        && _liveSessionRuns.get(sessionId)?.turn_id === turnId
        && sessionLiveStream(agentId).streamBuf === 'new turn B output', {
      sessionId: session.id, turnId: newTurn, agentId: agent,
    });
    const latest = await page.evaluate(({ sessionId, agentId }) => ({
      activeTurn: S.session.activeTurnId,
      cachedTurn: _liveSessionRuns.get(sessionId)?.turn_id || null,
      output: sessionLiveStream(agentId).streamBuf,
      stopVisible: !document.querySelector('#session-run-action')?.classList.contains('hide'),
      stopText: document.querySelector('#session-run-action')?.textContent,
      transcript: document.querySelector('#session-msgs')?.textContent || '',
    }), { sessionId: session.id, agentId: agent });
    assert.deepEqual({
      activeTurn: latest.activeTurn,
      cachedTurn: latest.cachedTurn,
      output: latest.output,
      stopVisible: latest.stopVisible,
      stopText: latest.stopText,
    }, {
      activeTurn: newTurn,
      cachedTurn: newTurn,
      output: 'new turn B output',
      stopVisible: true,
      stopText: 'Stop',
    });
    assert.match(latest.transcript, /new turn B output/);
    assert.doesNotMatch(latest.transcript, /stale turn A output/);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('active Session hydration orders authoritative HTTP state against newer socket state', async () => {
  const session = structuredClone(runtime.fixtures.alpha.sessions[0]);
  session.status = 'active';
  const agent = session.mode.agent_id;
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  try {
    await page.waitForFunction(() => _liveSessionSnapshotEpoch > 0);
    let responseRun = null;
    let requestGate = null;
    await page.route(`**/api/sessions/${session.id}/active-turn`, async (route) => {
      const response = structuredClone(responseRun);
      const gate = requestGate;
      gate?.started();
      if (gate) await gate.release;
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ run: response }),
      });
    });
    const run = (turnId, output = '') => ({
      kind: 'session',
      workflow: session.id,
      turn_id: turnId,
      agents: [{
        agent,
        status: 'running',
        output,
        thinking: '',
        tokens: 0,
      }],
    });
    const seedCache = (turnId, output = '') => page.evaluate((value) => {
      cacheSessionRunFrame({
        kind: 'session-accepted', session: value.sessionId, turn_id: value.turnId,
      });
      if (value.output) cacheSessionRunFrame({
        kind: 'token', workflow: value.sessionId, turn_id: value.turnId,
        agent: value.agentId, delta: value.output,
      });
      setSessionTurnRunning(value.turnId, true);
    }, { sessionId: session.id, turnId, agentId: agent, output });
    const liveState = () => page.evaluate((sessionId) => ({
      cached: _liveSessionRuns.get(sessionId)?.turn_id || null,
      active: S.session.activeTurnId,
    }), session.id);

    // No socket frame arrived during the request, so the authoritative active
    // endpoint must replace a stale pre-disconnect cache with its newer owner.
    await seedCache('turn-stale-a', 'stale A');
    responseRun = run('turn-authoritative-b', 'authoritative B');
    await page.evaluate((sessionId) => hydrateSessionConversation(sessionId), session.id);
    assert.deepEqual(await liveState(), {
      cached: 'turn-authoritative-b',
      active: 'turn-authoritative-b',
    });

    // The same unchanged-cache rule must also let authoritative idle clear a
    // ghost owner and its Stop action.
    await seedCache('turn-stale-idle', 'ghost');
    responseRun = null;
    await page.evaluate((sessionId) => hydrateSessionConversation(sessionId), session.id);
    assert.deepEqual(await liveState(), { cached: null, active: null });

    // A sequenced socket owner that lands after the request starts is newer
    // than the HTTP snapshot captured for that request.
    let noteStarted;
    let allowResponse;
    const started = new Promise((resolve) => { noteStarted = resolve; });
    const release = new Promise((resolve) => { allowResponse = resolve; });
    requestGate = { started: noteStarted, release };
    responseRun = run('turn-http-a', 'HTTP A');
    const hydration = page.evaluate(
      (sessionId) => hydrateSessionConversation(sessionId),
      session.id,
    );
    await started;
    await page.evaluate(({ sessionId, turnId, agentId }) => {
      cacheSessionRunFrame({ kind: 'session-accepted', session: sessionId, turn_id: turnId });
      cacheSessionRunFrame({
        kind: 'token', workflow: sessionId, turn_id: turnId,
        agent: agentId, delta: 'socket B',
      });
    }, { sessionId: session.id, turnId: 'turn-socket-b', agentId: agent });
    responseRun = run('turn-socket-b', 'authoritative reread B');
    allowResponse();
    await hydration;
    requestGate = null;
    assert.deepEqual(await liveState(), {
      cached: 'turn-socket-b',
      active: 'turn-socket-b',
    });

    // Deletion is ordered state too: a terminal socket frame after request
    // start must beat the older active response and leave no ghost Stop owner.
    await seedCache('turn-terminal-a', 'finishing');
    let noteTerminalStarted;
    let allowTerminalResponse;
    const terminalStarted = new Promise((resolve) => { noteTerminalStarted = resolve; });
    const terminalRelease = new Promise((resolve) => { allowTerminalResponse = resolve; });
    requestGate = { started: noteTerminalStarted, release: terminalRelease };
    responseRun = run('turn-terminal-a', 'still running');
    const terminalHydration = page.evaluate(
      (sessionId) => hydrateSessionConversation(sessionId),
      session.id,
    );
    await terminalStarted;
    await page.evaluate(({ sessionId, turnId }) => cacheSessionRunFrame({
      kind: 'session-done', session: sessionId, turn_id: turnId,
    }), { sessionId: session.id, turnId: 'turn-terminal-a' });
    responseRun = null;
    allowTerminalResponse();
    await terminalHydration;
    requestGate = null;
    assert.deepEqual(await liveState(), { cached: null, active: null });

    // Arrival timing alone is not recency. A delayed stale Snapshot may land
    // during a newer HTTP response; the boundary forces a second HTTP read,
    // which must restore the current B owner instead of accepting cached A.
    await seedCache('turn-before-delayed-snapshot');
    let noteSnapshotStarted;
    let allowSnapshotResponse;
    const snapshotStarted = new Promise((resolve) => { noteSnapshotStarted = resolve; });
    const snapshotRelease = new Promise((resolve) => { allowSnapshotResponse = resolve; });
    requestGate = { started: noteSnapshotStarted, release: snapshotRelease };
    responseRun = run('turn-current-after-snapshot', 'current B');
    const snapshotHydration = page.evaluate(
      (sessionId) => hydrateSessionConversation(sessionId),
      session.id,
    );
    await snapshotStarted;
    await page.evaluate(({ sessionId, turnId, agentId }) => cacheSessionRunFrame({
      kind: 'snapshot', approvals: [],
      runs: [{
        kind: 'session', workflow: sessionId, turn_id: turnId,
        agents: [{
          agent: agentId, status: 'running', output: 'delayed stale A',
          thinking: '', tokens: 0,
        }],
      }],
    }), { sessionId: session.id, turnId: 'turn-delayed-snapshot-a', agentId: agent });
    allowSnapshotResponse();
    await snapshotHydration;
    requestGate = null;
    assert.deepEqual(await liveState(), {
      cached: 'turn-current-after-snapshot',
      active: 'turn-current-after-snapshot',
    });

    // A delayed token is content, not an ownership boundary. It must not make
    // stale cached A outrank authoritative B or force a boundary reread.
    await seedCache('turn-delayed-token-a');
    let noteTokenStarted;
    let allowTokenResponse;
    const tokenStarted = new Promise((resolve) => { noteTokenStarted = resolve; });
    const tokenRelease = new Promise((resolve) => { allowTokenResponse = resolve; });
    requestGate = { started: noteTokenStarted, release: tokenRelease };
    responseRun = run('turn-current-after-token', 'current B');
    const tokenHydration = page.evaluate(
      (sessionId) => hydrateSessionConversation(sessionId),
      session.id,
    );
    await tokenStarted;
    await page.evaluate(({ sessionId, turnId, agentId }) => cacheSessionRunFrame({
      kind: 'token', workflow: sessionId, turn_id: turnId,
      agent: agentId, delta: ' delayed stale token',
    }), { sessionId: session.id, turnId: 'turn-delayed-token-a', agentId: agent });
    allowTokenResponse();
    await tokenHydration;
    requestGate = null;
    assert.deepEqual(await liveState(), {
      cached: 'turn-current-after-token',
      active: 'turn-current-after-token',
    });

    // Same-turn HTTP state can be newer even when ownership is unchanged. A
    // stale AgentActivated cache must not shorten output/reasoning or regress a
    // terminal Agent and its final token count back to running/zero.
    const sameTurn = 'turn-same-owner-newer-http';
    await page.evaluate(({ sessionId, turnId, agentId }) => {
      cacheSessionRunFrame({ kind: 'session-accepted', session: sessionId, turn_id: turnId });
      cacheSessionRunFrame({
        kind: 'event', workflow: sessionId, agent: agentId, type: 'AgentActivated',
      });
      cacheSessionRunFrame({
        kind: 'token', workflow: sessionId, turn_id: turnId, agent: agentId, delta: 'a',
      });
      cacheSessionRunFrame({
        kind: 'reasoning', workflow: sessionId, turn_id: turnId,
        agent: agentId, delta: 'thought',
      });
      setSessionTurnRunning(turnId, true);
    }, { sessionId: session.id, turnId: sameTurn, agentId: agent });
    responseRun = {
      ...run(sameTurn, 'ab'),
      agents: [{
        agent, status: 'done', output: 'ab', thinking: 'thought complete', tokens: 37,
      }],
    };
    await page.evaluate((sessionId) => hydrateSessionConversation(sessionId), session.id);
    const mergedAgent = await page.evaluate(({ sessionId, agentId }) => {
      const agentState = _liveSessionRuns.get(sessionId)?.agents
        ?.find((candidate) => candidate.agent === agentId);
      return agentState ? {
        output: agentState.output,
        thinking: agentState.thinking,
        status: agentState.status,
        tokens: agentState.tokens,
      } : null;
    }, { sessionId: session.id, agentId: agent });
    assert.deepEqual(mergedAgent, {
      output: 'ab',
      thinking: 'thought complete',
      status: 'done',
      tokens: 37,
    });
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('a rejected pending send cannot clear a different active Session turn', async () => {
  const session = structuredClone(runtime.fixtures.alpha.sessions[0]);
  session.status = 'active';
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  try {
    const state = await page.evaluate((sessionId) => {
      const activeTurn = 'turn-owned-by-another-send';
      const rejectedTurn = 'turn-rejected-before-ownership';
      setSessionTurnRunning(activeTurn, true);
      const userEl = el('div', 'smsg user', 'rejected prompt');
      document.querySelector('#session-msgs').appendChild(userEl);
      S.session.pendingTurn = {
        id: rejectedTurn,
        userEl,
        composerText: 'retry this prompt',
        inlineRefs: [],
        referenceIds: [],
      };
      _pendingSessionTurns.set(sessionId, S.session.pendingTurn);
      handleWsFrame({
        kind: 'session-error', session: sessionId, turn_id: rejectedTurn,
        error: 'another turn already owns this Session',
      });
      return {
        activeTurn: S.session.activeTurnId,
        pending: S.session.pendingTurn,
        pendingCache: _pendingSessionTurns.has(sessionId),
        composer: document.querySelector('#session-text').value,
        rejectedPromptStillVisible: document.body.contains(userEl),
        stopVisible: !document.querySelector('#session-run-action').classList.contains('hide'),
      };
    }, session.id);
    assert.deepEqual(state, {
      activeTurn: 'turn-owned-by-another-send',
      pending: null,
      pendingCache: false,
      composer: 'retry this prompt',
      rejectedPromptStillVisible: false,
      stopVisible: true,
    });
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('manual E2B cleanup confirmation requires the exact retained runtime affirmation', async () => {
  const session = runtime.fixtures.beta.sessions[0];
  const { context, page, browserRequests, assertNoBrowserErrors } = await openSession(session);
  try {
    const runtimeId = 'e2b-runtime-exact-123';
    const retained = structuredClone(session);
    retained.environment = {
      ...retained.environment,
      runtime: {
        backend: 'e2b',
        id: runtimeId,
        control_plane: 'https://api.e2b.dev',
        authority_fingerprint: 'test-fingerprint',
        cleanup_confirmed: false,
      },
    };
    const confirmed = structuredClone(retained);
    confirmed.environment.runtime.cleanup_confirmed = true;
    let listed = retained;
    let confirmationResponse = confirmed;

    await page.route('**/api/sessions', (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: JSON.stringify([listed]),
    }));
    await page.route(
      `**/api/sessions/${session.id}/environment/confirm-runtime-cleanup`,
      (route) => {
        listed = confirmationResponse;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(confirmationResponse),
        });
      },
    );
    await page.evaluate(({ sessionId, value }) => {
      Object.assign(sessionHome().session(sessionId), value);
      sessionHome().configureEnvironment(sessionId);
    }, { sessionId: session.id, value: retained });

    const checkbox = page.locator('ax-session-home [data-field="runtime-cleanup-confirmed"]');
    const confirm = page.locator('ax-session-home [data-action="confirm-runtime-cleanup"]');
    await checkbox.waitFor({ state: 'visible' });
    assert.equal(await checkbox.isChecked(), false);
    assert.equal(await confirm.isDisabled(), true);
    assert.equal(
      await checkbox.locator('xpath=..').textContent(),
      `I deleted E2B runtime ${runtimeId} outside Axocoatl`,
    );
    assert.equal(browserRequests.filter((request) =>
      new URL(request.url).pathname.endsWith('/environment/confirm-runtime-cleanup')).length, 0);

    await checkbox.check();
    assert.equal(await confirm.isEnabled(), true);
    const confirmationRequest = page.waitForRequest((request) => request.method() === 'POST'
      && new URL(request.url()).pathname
        === `/api/sessions/${session.id}/environment/confirm-runtime-cleanup`);
    await confirm.click();
    const request = await confirmationRequest;
    assert.deepEqual(request.postDataJSON(), { runtime_id: runtimeId, confirmed: true });
    await page.waitForFunction(() =>
      S.session.environment?.runtime?.cleanup_confirmed === true);
    assert.equal(await page.locator('ax-session-home [role="dialog"]').isHidden(), true);

    const creationToken = 'create-token-exact-456';
    const retainedCreation = structuredClone(session);
    retainedCreation.environment = {
      ...retainedCreation.environment,
      runtime: null,
      runtime_creation: {
        backend: 'e2b',
        token: creationToken,
        discovered_ids: ['e2b-discovered-1', 'e2b-discovered-2'],
      },
    };
    const creationConfirmed = structuredClone(retainedCreation);
    creationConfirmed.environment.runtime_creation = null;
    listed = retainedCreation;
    confirmationResponse = creationConfirmed;
    const requestsBeforeCreationReview = browserRequests.filter((candidate) =>
      new URL(candidate.url).pathname
        === `/api/sessions/${session.id}/environment/confirm-runtime-cleanup`).length;

    await page.evaluate(({ sessionId, value }) => {
      Object.assign(sessionHome().session(sessionId), value);
      applySessionEnvironmentUpdate(value, { activateRuntime: false });
      sessionHome().configureEnvironment(sessionId);
    }, { sessionId: session.id, value: retainedCreation });

    await checkbox.waitFor({ state: 'visible' });
    assert.deepEqual(await page.evaluate(() => S.session.environment.runtime_creation), {
      backend: 'e2b',
      token: creationToken,
      discovered_ids: ['e2b-discovered-1', 'e2b-discovered-2'],
    });
    assert.equal(await checkbox.isChecked(), false);
    assert.equal(await confirm.isDisabled(), true);
    assert.equal(
      await page.locator('ax-session-home .runtime-cleanup code').textContent(),
      `axocoatl_creation_token=${creationToken}`,
    );
    assert.equal(
      await checkbox.locator('xpath=..').textContent(),
      `I deleted every E2B sandbox with metadata axocoatl_creation_token=${creationToken}`,
    );
    assert.equal(browserRequests.filter((candidate) =>
      new URL(candidate.url).pathname
        === `/api/sessions/${session.id}/environment/confirm-runtime-cleanup`).length,
    requestsBeforeCreationReview);

    await checkbox.check();
    assert.equal(await confirm.isEnabled(), true);
    const creationConfirmationRequest = page.waitForRequest((candidate) =>
      candidate.method() === 'POST'
      && new URL(candidate.url()).pathname
        === `/api/sessions/${session.id}/environment/confirm-runtime-cleanup`);
    await confirm.click();
    const creationRequest = await creationConfirmationRequest;
    assert.deepEqual(creationRequest.postDataJSON(), {
      creation_token: creationToken,
      confirmed_all_matching_sandboxes_deleted: true,
    });
    await page.waitForFunction((sessionId) =>
      S.session.environment?.runtime_creation == null
      && sessionHome().session(sessionId)?.environment?.runtime_creation == null,
    session.id);
    assert.equal(await page.locator('ax-session-home [role="dialog"]').isHidden(), true);
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('E2B Close explains exact-runtime pause while local Close keeps its existing promise', async () => {
  const session = runtime.fixtures.alpha.sessions[0];
  const { context, page, assertNoBrowserErrors } = await openSession(session);
  try {
    const remote = structuredClone(session);
    remote.environment = {
      ...remote.environment,
      runtime: {
        backend: 'e2b',
        id: 'e2b-close-copy-proof',
        cleanup_confirmed: false,
      },
    };
    let listed = remote;
    await page.route('**/api/sessions', (route) => route.fulfill({
      status: 200, contentType: 'application/json', body: JSON.stringify([listed]),
    }));
    await page.route(
      `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
      (route) => route.fulfill({
        status: 200, contentType: 'application/json', body: JSON.stringify([listed]),
      }),
    );
    await page.evaluate(() => closeCockpit({ homeScope: 'all' }));
    await page.waitForFunction((sessionId) =>
      sessionHome().session(sessionId)?.environment?.runtime?.backend === 'e2b', session.id);
    const row = page.locator(`ax-session-home [data-session-id="${session.id}"]`);
    await row.waitFor({ state: 'visible' });
    await row.click({ button: 'right' });
    await page.locator('ax-session-home [data-action="menu-action"]', { hasText: 'Close' }).click();
    const copy = await page.locator('ax-session-home .dialog-body').textContent();
    assert.match(copy, /E2B VM will pause/);
    assert.match(copy, /remote working tree preserved/);
    assert.match(copy, /Reopen resumes that exact runtime, including uncommitted work/);
    assert.match(copy, /Delete session or Change runtime is the explicit destructive boundary/);
    assert.doesNotMatch(copy, /container will stop/);
    await page.locator('ax-session-home [data-action="dialog-cancel"]').click();

    listed = structuredClone(session);
    listed.environment.runtime = {
      backend: 'podman', id: `axocoatl-session-${session.id}`, cleanup_confirmed: false,
    };
    await page.evaluate(() => sessionHome().refresh());
    await row.click({ button: 'right' });
    await page.locator('ax-session-home [data-action="menu-action"]', { hasText: 'Close' }).click();
    const localCopy = await page.locator('ax-session-home .dialog-body').textContent();
    assert.equal(
      localCopy,
      `“${session.name}”'s container will stop. You can reopen it later — its history stays.`,
    );
    await page.locator('ax-session-home [data-action="dialog-cancel"]').click();
    assertNoBrowserErrors();
  } finally {
    await context.close();
  }
});

test('operator devcontainer policy is visible and reversible while package-lock setup stays unapproved', async () => {
  const policyRuntime = await launchTestDaemon({ allowPostCreateCommand: true });
  const markerName = '.axocoatl-policy-marker';
  const postCreateCommand = `touch ${markerName}`;
  const policyProject = await policyRuntime.createProjectWorkspace(
    'operator-policy-project',
    'Operator Policy Workspace',
    { packageLock: false, devcontainer: { postCreateCommand } },
  );
  const { context, page, browserRequests, assertNoBrowserErrors } = await openSession(
    policyRuntime.fixtures.alpha.sessions[0],
    { width: 1280, height: 800 },
    policyRuntime,
  );
  try {
    // A detected package-lock command is only a suggestion even when the
    // operator has enabled the separate devcontainer post-create policy.
    await page.locator('ax-rail .section-new').click();
    const setupCommand = page.locator('ax-session-home [data-field="setup-command"]');
    await page.waitForFunction(() =>
      document.querySelector('ax-session-home')?.shadowRoot
        ?.querySelector('[data-field="setup-command"]')?.value === 'npm ci');
    assert.equal(await setupCommand.inputValue(), 'npm ci');
    assert.equal(await page.locator('ax-session-home [data-field="setup-approved"]').isChecked(), false);
    assert.equal(await page.locator('ax-session-home .config-help', { hasText: 'Suggested from package-lock' }).count(), 1);
    await page.locator('ax-session-home [data-action="picker-cancel"]').click();

    await page.locator('ax-rail #switch').click();
    await page.locator('ax-rail [aria-label="Open Workspace Operator Policy Workspace"]').click();
    await page.waitForFunction((workspaceId) =>
      document.querySelector('ax-rail')?.workspace === workspaceId, policyProject.workspace.id);
    await page.locator('ax-rail .section-new').click();
    await page.waitForFunction((expected) =>
      document.querySelector('ax-session-home')?.shadowRoot
        ?.querySelector('[data-field="setup-command"]')?.value === expected, postCreateCommand);

    const approved = page.locator('ax-session-home [data-field="setup-approved"]');
    assert.equal(await setupCommand.inputValue(), postCreateCommand);
    assert.equal(await approved.isChecked(), true);
    assert.equal(await page.locator('ax-session-home .config-help', {
      hasText: 'Daemon policy defaults this exact devcontainer setup to approved',
    }).count(), 1);
    await page.locator('ax-session-home [data-field="session-name"]').fill('Policy Declined Session');
    await approved.uncheck();
    assert.equal(await approved.isChecked(), false);
    assert.equal(await policyRuntime.pathExists(path.join(policyProject.projectPath, markerName)), false);
    await page.locator('ax-session-home [data-action="picker-use"]').click();
    await page.waitForFunction(() =>
      document.querySelector('#cockpit-title')?.textContent === 'Policy Declined Session'
      && document.querySelector('#session-environment')?.dataset.state === 'awaiting_approval');

    const created = (await policyRuntime.listSessions())
      .find((session) => session.name === 'Policy Declined Session');
    assert.ok(created, 'the declined-policy Session must be durably created');
    assert.equal(created.environment.setup_command, postCreateCommand);
    assert.equal(created.environment.setup_approved, false);
    assert.equal(created.environment.setup_reviewed, true);
    assert.equal(created.environment.state, 'awaiting_approval');
    assert.equal(await policyRuntime.pathExists(path.join(policyProject.projectPath, markerName)), false);

    const runtimeRequests = browserRequests.filter((request) => {
      if (!created?.id) return false;
      const pathname = new URL(request.url).pathname;
      return pathname.startsWith(`/api/sessions/${created.id}/`)
        && (pathname.endsWith('/tree') || pathname.endsWith('/file')
          || pathname.includes('/git/')
          || (request.method !== 'GET' && pathname.endsWith('/tasks')));
    });
    assert.deepEqual(runtimeRequests, [], 'declining policy must not touch the project runtime');
    assertNoBrowserErrors();
  } finally {
    await context.close();
    await policyRuntime.stop();
  }
});
