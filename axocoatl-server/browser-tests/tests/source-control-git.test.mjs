import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';

import { browserEngines, launchTestDaemon, newAuthorizedContext } from '../support/daemon.mjs';

// Source Control says who a commit is by, refuses one it cannot make as the
// person, shows Git's own refusal, and offers to initialize Git instead of
// doing it on a read. The Git routes are fixtures; the component is the real
// one served by the daemon.
const engines = await browserEngines();
let runtime;

before(async () => {
  runtime = process.env.AXOCOATL_COMPONENT_BASE_URL
    ? { baseUrl: process.env.AXOCOATL_COMPONENT_BASE_URL, stop: async () => {} }
    : await launchTestDaemon();
});

after(async () => {
  await runtime?.stop();
});

const STAGED = {
  branch: 'main', clean: false,
  files: [{ path: 'calc.py', state: 'modified', staged: true, unstaged: false, last_turn: true, added: 1, removed: 1 }],
};

async function fixture(engine, options) {
  const browser = await engine.launch();
  // A fixture that does not open closes its browser: a browser left running
  // keeps this file's process, and so the suite, waiting forever.
  try {
    return await openFixture(browser, options);
  } catch (error) {
    await browser.close();
    throw error;
  }
}

async function openFixture(browser, { identity, status = STAGED, commit = null, init = null, scope = null }) {
  const context = await newAuthorizedContext(browser, { viewport: { width: 420, height: 720 } });
  const page = await context.newPage();
  const errors = [];
  const calls = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.route('**/source-control-fixture', (route) => route.fulfill({
    contentType: 'text/html',
    body: `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="stylesheet" href="/ui/tokens.css"></head>
<body style="display:flex;height:100vh;margin:0"><script>window.notices=[];</script>
<script type="module">
import '/ui/source-control.js';
const element = document.createElement('ax-source-control');
element.addEventListener('notify', (event) => window.notices.push(event.detail));
${scope ? `element.setAttribute('scope', ${JSON.stringify(scope)});` : ''}
element.setAttribute('session', 'fixture-session');
document.body.append(element);
</script></body></html>`,
  }));
  let currentStatus = status;
  await page.route('**/api/sessions/fixture-session/git/**', async (route) => {
    const url = new URL(route.request().url());
    const suffix = url.pathname.split('/git/')[1];
    calls.push({ method: route.request().method(), suffix, body: route.request().postDataJSON?.() ?? null });
    if (suffix === 'status') {
      return typeof currentStatus === 'function' ? currentStatus(route) : route.fulfill({ json: currentStatus });
    }
    if (suffix === 'branches') return route.fulfill({ json: { current: 'main', branches: ['main'] } });
    if (suffix === 'identity') return route.fulfill({ json: identity });
    if (suffix === 'commit' && commit) return commit(route);
    if (suffix === 'init' && init) {
      currentStatus = STAGED;
      return init(route);
    }
    return route.fulfill({ status: 404, json: { error: `unexpected ${suffix}` } });
  });
  await page.goto(`${runtime.baseUrl}/source-control-fixture`);
  const element = page.locator('ax-source-control');
  await element.waitFor();
  return {
    page, element, errors, calls,
    async close() { await context.close(); await browser.close(); },
  };
}

for (const engine of engines) {
  test(`${engine.name}: the commit box names the person a commit is by`, async () => {
    const { page, element, errors, calls, close } = await fixture(engine, {
      identity: { name: 'Ada Lovelace', email: 'ada@example.com', signing: false, problem: null },
      commit: (route) => route.fulfill({ json: { branch: 'main', clean: true, files: [] } }),
    });
    try {
      const identity = element.locator('.commit-identity');
      await page.waitForFunction(() => document.querySelector('ax-source-control')
        ?.shadowRoot?.querySelector('.commit-identity')?.textContent?.startsWith('Commits as'));
      assert.equal(await identity.textContent(),
        "Commits as Ada Lovelace <ada@example.com> · not signed · repository hooks don't run");
      await element.locator('#commit-message').fill('Fix the sum');
      const commit = element.locator('[data-action="commit"]');
      assert.equal(await commit.isDisabled(), false);
      await commit.click();
      await page.waitForFunction(() => window.notices.some((notice) => notice.title === 'Committed'));
      assert.deepEqual(calls.find((call) => call.suffix === 'commit')?.body, { message: 'Fix the sum', stage_all: false });
      assert.deepEqual(errors, []);
    } finally { await close(); }
  });

  test(`${engine.name}: no Git identity disables Commit and says how to set one`, async () => {
    const problem = 'Git has no name and email for your commits in this repository. Set them in a terminal with `git config --global user.name "Your Name"` and `git config --global user.email you@example.com`, then commit again.';
    const { page, element, errors, calls, close } = await fixture(engine, {
      identity: { name: null, email: null, signing: false, problem },
    });
    try {
      await page.waitForFunction(() => document.querySelector('ax-source-control')
        ?.shadowRoot?.querySelector('.commit-identity.problem'));
      assert.equal(await element.locator('.commit-identity').textContent(), problem);
      await element.locator('#commit-message').fill('Fix the sum');
      assert.equal(await element.locator('[data-action="commit"]').isDisabled(), true);
      assert.equal(await element.locator('[data-action="commit"]').getAttribute('title'), problem);
      assert.equal(calls.some((call) => call.suffix === 'commit'), false);
      assert.deepEqual(errors, []);
    } finally { await close(); }
  });

  test(`${engine.name}: a refused commit shows Git's message instead of Committed`, async () => {
    const refusal = "Nothing was committed: fatal: Unable to create '/w/.git/index.lock': File exists.";
    const { page, element, errors, close } = await fixture(engine, {
      identity: { name: 'Ada Lovelace', email: 'ada@example.com', signing: false, problem: null },
      commit: (route) => route.fulfill({ status: 400, json: { error: refusal } }),
    });
    try {
      await page.waitForFunction(() => document.querySelector('ax-source-control')
        ?.shadowRoot?.querySelector('.commit-identity')?.textContent?.startsWith('Commits as'));
      await element.locator('#commit-message').fill('Fix the sum');
      await element.locator('[data-action="commit"]').click();
      await page.waitForFunction(() => window.notices.some((notice) => notice.title === 'Not committed'));
      const notices = await page.evaluate(() => window.notices);
      assert.equal(notices.some((notice) => notice.title === 'Committed'), false);
      assert.deepEqual(notices.find((notice) => notice.title === 'Not committed'),
        { title: 'Not committed', body: refusal, kind: 'err' });
      assert.deepEqual(errors, []);
    } finally { await close(); }
  });

  test(`${engine.name}: a folder that is not a repository is offered Initialize Git here`, async () => {
    const { page, element, errors, calls, close } = await fixture(engine, {
      identity: { name: 'Ada Lovelace', email: 'ada@example.com', signing: false, problem: null },
      status: (route) => route.fulfill({ status: 409, json: {
        error: "This Session's folder is not a Git repository. Choose Initialize Git here in Source Control, or run `git init` in the folder yourself.",
        code: 'not_a_repository',
      } }),
      init: (route) => route.fulfill({ json: STAGED }),
    });
    try {
      const init = element.locator('[data-action="git-init"]');
      await init.waitFor();
      assert.match(await element.locator('.files').textContent(), /not a Git repository/);
      assert.equal(await element.locator('[data-action="commit"]').isDisabled(), true);
      assert.equal(calls.some((call) => call.suffix === 'init'), false, 'reading never initializes');
      await init.click();
      await page.waitForFunction(() => window.notices.some((notice) => notice.title === 'Git initialized'));
      await element.locator('.file[data-path="calc.py"]').first().waitFor();
      assert.equal(calls.filter((call) => call.suffix === 'init' && call.method === 'POST').length, 1);
      assert.deepEqual(errors, []);
    } finally { await close(); }
  });

  test(`${engine.name}: Last turn says when the last turn's files are not known`, async () => {
    const reason = 'Axocoatl cannot tell which files part of the last turn changed: lead (generation 1) in turn t: it has no After capture. Review All changes.';
    const { page, element, errors, close } = await fixture(engine, {
      identity: { name: 'Ada Lovelace', email: 'ada@example.com', signing: false, problem: null },
      scope: 'last-turn',
      status: {
        branch: 'main', clean: false, last_turn_unattributed: reason,
        files: [{ path: 'calc.py', state: 'modified', staged: false, unstaged: true, last_turn: false, added: 1, removed: 1 }],
      },
    });
    try {
      await page.waitForFunction(() => document.querySelector('ax-source-control')
        ?.shadowRoot?.querySelector('.files .empty'));
      const text = await element.locator('.files .empty').textContent();
      assert.match(text, /cannot tell which files part of the last turn changed/);
      assert.doesNotMatch(text, /No current changes remain/);
      assert.deepEqual(errors, []);
    } finally { await close(); }
  });
}
