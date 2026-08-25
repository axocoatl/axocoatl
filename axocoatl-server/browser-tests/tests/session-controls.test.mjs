import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { readFile } from 'node:fs/promises';
import { chromium } from 'playwright';

import { launchTestDaemon, resolveChromiumExecutable } from '../support/daemon.mjs';

let runtime;
let browser;

function beginEvent({
  id,
  sessionId,
  input,
  agentId = 'browser-test-coder',
  context = [],
  metadata = {},
  recordedAt,
}) {
  return {
    schema_version: 1,
    operation_id: `begin:${id}`,
    recorded_at: recordedAt,
    kind: 'begin',
    turn: {
      id,
      session_id: sessionId,
      user_input: input,
      agent_id: agentId,
      model: 'browser-test-model',
      context,
      status: 'running',
      partial_output: '',
      created_at: recordedAt,
      updated_at: recordedAt,
      idempotency_key: id,
      metadata,
      execution_events: [],
      agent_outputs: [],
      superseded: false,
    },
  };
}

function transitionEvent({ id, status, output = null, error = null, recordedAt }) {
  return {
    schema_version: 1,
    operation_id: `terminal:${id}`,
    recorded_at: recordedAt,
    kind: 'transition',
    turn_id: id,
    transition: {
      status,
      final_output: output,
      error,
    },
  };
}

function toolEvent({ id, phase, recordedAt }) {
  const started = phase === 'tool_started';
  return {
    schema_version: 1,
    operation_id: `${id}:${phase}`,
    recorded_at: recordedAt,
    kind: 'execution',
    turn_id: id,
    execution: {
      kind: phase,
      execution_id: 'safe-boundary-edit',
      metadata: {
        agent_id: 'browser-test-coder',
        call_id: 'safe-boundary-edit',
        call_id_sha256: 'a'.repeat(64),
        occurrence: 0,
        tool_name: 'edit_file',
        ...(started
          ? { arguments: { path: 'src/index.js', old: 'before', new: 'after' } }
          : { is_error: false, result: { changed: true, path: 'src/index.js' } }),
      },
    },
  };
}

before(async () => {
  runtime = await launchTestDaemon();
  const alpha = runtime.fixtures.alpha.sessions[0];
  const beta = runtime.fixtures.beta.sessions[0];
  const launchContext = [{
    reference_id: 'context:history-failed:0',
    display_name: 'launch-contract.md',
    kind: 'code_selection',
    scope: 'this_turn',
    origin: 'docs/launch-contract.md',
    metadata: { content: 'Launch contract evidence' },
  }];
  await runtime.restartWithSessionTurnEvents([
    beginEvent({
      id: 'history-completed',
      sessionId: alpha.id,
      input: 'Establish the durable baseline',
      metadata: { session_name: alpha.name },
      recordedAt: 1_000,
    }),
    transitionEvent({
      id: 'history-completed',
      status: 'completed',
      output: 'Baseline is durable.',
      recordedAt: 1_100,
    }),
    beginEvent({
      id: 'history-cancelled',
      sessionId: alpha.id,
      input: 'Apply the safe-boundary edit',
      metadata: { session_name: alpha.name },
      recordedAt: 2_000,
    }),
    toolEvent({ id: 'history-cancelled', phase: 'tool_started', recordedAt: 2_050 }),
    toolEvent({ id: 'history-cancelled', phase: 'tool_result', recordedAt: 2_075 }),
    transitionEvent({
      id: 'history-cancelled',
      status: 'cancelled',
      output: 'Stopped after the edit completed.',
      recordedAt: 2_100,
    }),
    beginEvent({
      id: 'history-failed',
      sessionId: alpha.id,
      input: 'Diagnose the launch blocker',
      context: launchContext,
      metadata: { session_name: alpha.name },
      recordedAt: 3_000,
    }),
    transitionEvent({
      id: 'history-failed',
      status: 'failed',
      error: 'Launch blocker remained unresolved.',
      recordedAt: 3_100,
    }),
    beginEvent({
      id: 'history-other-session',
      sessionId: beta.id,
      input: 'Find the cross-session marker',
      metadata: { session_name: beta.name },
      recordedAt: 4_000,
    }),
    transitionEvent({
      id: 'history-other-session',
      status: 'completed',
      output: 'CROSS-SESSION MARKER is present.',
      recordedAt: 4_100,
    }),
  ]);
  const executablePath = await resolveChromiumExecutable();
  browser = await chromium.launch({ headless: true, ...(executablePath ? { executablePath } : {}) });
});

after(async () => {
  await browser?.close();
  await runtime?.stop();
});

async function openControlledSession({ turns, attachments = [] } = {}) {
  const session = runtime.fixtures.alpha.sessions[0];
  const readySession = structuredClone(session);
  readySession.status = 'active';
  readySession.environment = {
    ...readySession.environment,
    state: 'ready',
    setup_reviewed: true,
    setup_results: [],
    error: null,
  };
  const context = await browser.newContext({
    viewport: { width: 1280, height: 800 },
    acceptDownloads: true,
  });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(`pageerror: ${error.message}`));

  await page.route('**/api/sessions', (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify([readySession]),
  }));
  await page.route(
    `**/api/workspaces/${encodeURIComponent(session.workspace_id)}/sessions`,
    (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify([readySession]),
    }),
  );
  await page.route(`**/api/sessions/${session.id}/tasks`, async (route) => {
    if (route.request().method() === 'POST') {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          id: 'session-controls-shell',
          kind: 'terminal',
          status: 'running',
          command: 'sh',
        }),
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: '[]',
    });
  });
  await page.route(`**/api/sessions/${session.id}/tree**`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: '[]',
  }));
  await page.route(`**/api/sessions/${session.id}/git/**`, (route) => {
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
  if (turns) {
    await page.route(`**/api/sessions/${session.id}/turns`, (route) => route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(turns.value),
    }));
  }
  await page.route(`**/api/sessions/${session.id}/attachments`, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify(attachments.filter(
      (reference) => reference.consumed?.status !== 'consumed',
    )),
  }));

  await page.goto(`${runtime.baseUrl}/?session=${encodeURIComponent(session.id)}`, {
    waitUntil: 'domcontentloaded',
  });
  await page.waitForFunction((sessionId) => Boolean(
    customElements.get('ax-session-context')
    && customElements.get('ax-session-history')
    && S.session.id === sessionId
    && S.session.historyState === 'ready'
    && document.querySelector('#session-cockpit')?.style.display === 'flex'
  ), session.id);
  await page.waitForFunction(() => S.session.attemptRestorePending === false);
  await page.evaluate((sessionId) => activateOpenedSessionRuntime(sessionId), session.id);
  await page.waitForFunction(() => sessionRuntimeSurfaceReady(S.session));
  await page.evaluate(() => {
    if (S.session.taskTimer) clearInterval(S.session.taskTimer);
    S.session.taskTimer = null;
    S.session.environment = {
      ...S.session.environment,
      state: 'ready',
      setup_reviewed: true,
      setup_results: [],
      error: null,
    };
    S.session.attemptRestorePending = false;
    S.session.pendingTurn = null;
    S.session.activeTurnId = null;
    S.threadVariants = null;
    syncSessionComposerState();
  });
  await page.waitForFunction(() => !document.querySelector('#session-context')?.disabled);

  return {
    context,
    page,
    session,
    assertNoErrors() {
      assert.deepEqual(errors, [], errors.join('\n'));
    },
  };
}

test('Once context is consumed on durable acceptance while Session context reaches the next turn', async () => {
  const turns = { value: [] };
  const attachments = [
    {
      reference_id: 'ctx-once',
      session_id: runtime.fixtures.alpha.sessions[0].id,
      blob_id: `sha256:${'1'.repeat(64)}`,
      display_name: 'once.txt',
      declared_mime: 'text/plain',
      size: 12,
      scope: 'this_turn',
      active: true,
      consumed: { status: 'available' },
    },
    {
      reference_id: 'ctx-session',
      session_id: runtime.fixtures.alpha.sessions[0].id,
      blob_id: `sha256:${'2'.repeat(64)}`,
      display_name: 'session.txt',
      declared_mime: 'text/plain',
      size: 24,
      scope: 'session',
      active: true,
      consumed: { status: 'available' },
    },
  ];
  const { context, page, session, assertNoErrors } = await openControlledSession({
    turns,
    attachments,
  });
  try {
    await page.waitForFunction(() =>
      document.querySelector('#session-context')?.selectedReferenceIds?.().length === 2);
    assert.deepEqual(
      await page.locator('#session-context .scope').allTextContents(),
      ['Once', 'Session'],
    );
    await page.evaluate(() => {
      globalThis.__sessionControlFrames = [];
      globalThis.wsSend = (frame) => {
        globalThis.__sessionControlFrames.push(structuredClone(frame));
        return true;
      };
    });

    await page.locator('#session-text').fill('Use both context files');
    await page.locator('#session-send').click();
    const first = await page.evaluate(() => globalThis.__sessionControlFrames.at(-1));
    assert.equal(first.cmd, 'session');
    assert.equal(first.id, session.id);
    assert.deepEqual(first.reference_ids, ['ctx-once', 'ctx-session']);

    attachments[0].consumed = { status: 'consumed', turn_id: first.turn_id };
    await page.evaluate(({ sessionId, turnId }) => handleWsFrame({
      kind: 'session-accepted', session: sessionId, turn_id: turnId,
    }), { sessionId: session.id, turnId: first.turn_id });
    await page.waitForFunction(() => {
      const control = document.querySelector('#session-context');
      return JSON.stringify(control?.selectedReferenceIds?.()) === JSON.stringify(['ctx-session']);
    });
    assert.deepEqual(await page.locator('#session-context .scope').allTextContents(), ['Session']);

    await page.evaluate((turnId) => {
      setSessionTurnRunning(turnId, false);
      S.session.pendingTurn = null;
      setSessionTurnPending(false);
    }, first.turn_id);
    await page.locator('#session-text').fill('Use retained Session context again');
    await page.evaluate(() => sendSessionMessage());
    const second = await page.evaluate(() => globalThis.__sessionControlFrames.at(-1));
    assert.equal(second.cmd, 'session');
    assert.notEqual(second.turn_id, first.turn_id);
    assert.deepEqual(second.reference_ids, ['ctx-session']);
    assertNoErrors();
  } finally {
    await context.close();
  }
});

test('Stop targets the exact turn, reloads durable safe-boundary evidence, and leaves a clean next send', async () => {
  const turns = { value: [] };
  const { context, page, session, assertNoErrors } = await openControlledSession({ turns });
  try {
    await page.evaluate(() => {
      globalThis.__sessionControlFrames = [];
      globalThis.wsSend = (frame) => {
        globalThis.__sessionControlFrames.push(structuredClone(frame));
        return true;
      };
      setSessionTurnRunning('turn-live-safe-boundary', true);
    });
    const stop = page.locator('#session-run-action');
    await stop.waitFor({ state: 'visible' });
    await stop.click();
    assert.deepEqual(await page.evaluate(() => globalThis.__sessionControlFrames.at(-1)), {
      cmd: 'session-stop',
      id: session.id,
      turn_id: 'turn-live-safe-boundary',
    });
    assert.equal(await stop.textContent(), 'Stopping…');
    assert.equal(await stop.isDisabled(), true);

    turns.value = [{
      id: 'turn-live-safe-boundary',
      session_id: session.id,
      user_input: 'Apply the edit, then stop',
      agent_id: 'browser-test-coder',
      status: 'cancelled',
      partial_output: 'Stopped after the edit completed.',
      final_output: 'Stopped after the edit completed.',
      context: [],
      execution_events: [
        {
          operation_id: 'safe-start',
          kind: 'tool_started',
          execution_id: 'safe-edit',
          metadata: {
            agent_id: 'browser-test-coder', occurrence: 0, tool_name: 'edit_file',
            arguments: { path: 'src/index.js', old: 'before', new: 'after' },
          },
        },
        {
          operation_id: 'safe-result',
          kind: 'tool_result',
          execution_id: 'safe-edit',
          metadata: {
            agent_id: 'browser-test-coder', occurrence: 0, tool_name: 'edit_file',
            is_error: false, result: { changed: true, path: 'src/index.js' },
          },
        },
      ],
      agent_outputs: [],
      superseded: false,
    }];
    await page.evaluate((sessionId) => handleWsFrame({
      kind: 'session-cancelled',
      session: sessionId,
      turn_id: 'turn-live-safe-boundary',
      input_tokens: 9,
      output_tokens: 3,
    }), session.id);
    await page.waitForFunction(() =>
      document.querySelector('#session-msgs')?.textContent?.includes('Stopped by you')
      && document.querySelector('#session-msgs')?.textContent?.includes('src/index.js'));
    assert.equal(await page.evaluate(() => S.session.activeTurnId), null);
    assert.equal(await page.locator('#session-send').isEnabled(), true);
    assert.match(await page.locator('#session-msgs').textContent(), /edit_file/);
    assert.match(await page.locator('#session-msgs').textContent(), /Stopped after the edit completed/);

    await page.locator('#session-text').fill('Start a clean next turn');
    await page.evaluate(() => sendSessionMessage());
    const next = await page.evaluate(() => globalThis.__sessionControlFrames.at(-1));
    assert.equal(next.cmd, 'session');
    assert.equal(next.id, session.id);
    assert.notEqual(next.turn_id, 'turn-live-safe-boundary');
    assert.equal(next.input, 'Start a clean next turn');
    assertNoErrors();
  } finally {
    await context.close();
  }
});

test('terminal resync clears stale xterm state and exact identity changes cancel retries', async () => {
  const { context, page, session, assertNoErrors } = await openControlledSession();
  try {
    const result = await page.evaluate(async (sessionId) => {
      const NativeWebSocket = window.WebSocket;
      const NativeTerminal = window.Terminal;
      const NativeFitAddon = window.FitAddon;
      const sockets = [];
      const terminals = [];
      const decode = new TextDecoder();

      class FakeTerminal {
        constructor() {
          this.buffer = '';
          this.resetCount = 0;
          this.clearCount = 0;
          this.rows = 30;
          this.cols = 100;
          this.dataListeners = new Set();
          terminals.push(this);
        }
        loadAddon() {}
        open() {}
        write(value) {
          this.buffer += value instanceof Uint8Array ? decode.decode(value) : String(value);
        }
        onData(listener) {
          this.dataListeners.add(listener);
          return { dispose: () => this.dataListeners.delete(listener) };
        }
        reset() { this.resetCount += 1; this.buffer = ''; }
        clear() { this.clearCount += 1; this.buffer = ''; }
        focus() {}
        dispose() { this.dataListeners.clear(); }
      }

      class FakeTerminalSocket {
        static CONNECTING = 0;
        static OPEN = 1;
        static CLOSING = 2;
        static CLOSED = 3;
        constructor(url) {
          this.url = String(url);
          this.readyState = FakeTerminalSocket.CONNECTING;
          this.binaryType = '';
          this.sent = [];
          sockets.push(this);
          queueMicrotask(() => {
            if (this.readyState !== FakeTerminalSocket.CONNECTING) return;
            this.readyState = FakeTerminalSocket.OPEN;
            this.onopen?.({});
          });
        }
        send(value) { this.sent.push(value); }
        close(code = 1000, reason = '') {
          if (this.readyState === FakeTerminalSocket.CLOSED) return;
          this.readyState = FakeTerminalSocket.CLOSED;
          this.onclose?.({ code, reason, wasClean: true });
        }
        output(text) {
          const bytes = new TextEncoder().encode(text);
          this.onmessage?.({ data: bytes.buffer });
        }
        drop(code, reason) {
          this.readyState = FakeTerminalSocket.CLOSED;
          this.onclose?.({ code, reason, wasClean: code === 1000 });
        }
      }

      const waitFor = async (predicate, label) => {
        for (let attempt = 0; attempt < 200; attempt += 1) {
          if (predicate()) return;
          await new Promise((resolve) => setTimeout(resolve, 10));
        }
        throw new Error(`timed out waiting for ${label}`);
      };

      let element;
      try {
        window.WebSocket = FakeTerminalSocket;
        window.Terminal = FakeTerminal;
        window.FitAddon = undefined;
        element = document.createElement('ax-terminal');
        element.session = sessionId;
        element.task = 'term-stable';
        document.body.append(element);

        await waitFor(() => sockets.length === 1
          && sockets[0].readyState === FakeTerminalSocket.OPEN, 'initial terminal attach');
        sockets[0].output('before');
        await waitFor(() => terminals[0]?.buffer === 'before', 'initial terminal output');
        sockets[0].drop(4001, 'terminal-resync-required: missed 1 output chunks');
        await waitFor(() => sockets.length === 2
          && sockets[1].readyState === FakeTerminalSocket.OPEN, 'terminal resync attach');
        const clearedBeforeSnapshot = terminals[0].buffer === ''
          && terminals[0].resetCount === 1
          && terminals[0].clearCount === 1;
        sockets[1].output('beforeduring');
        await waitFor(() => terminals[0].buffer === 'beforeduring', 'resynced snapshot');
        const resyncedBuffer = terminals[0].buffer;

        const stableUrls = sockets.slice(0, 2).map((socket) => socket.url);
        sockets[1].drop(4001, 'terminal-resync-required: missed 2 output chunks');
        element.task = 'term-new';
        await waitFor(() => sockets.some((socket) => socket.url.includes('/term-new/ws')),
          'replacement terminal identity');
        await new Promise((resolve) => setTimeout(resolve, 250));
        const stableSocketCountAfterTaskChange = sockets.filter(
          (socket) => socket.url.includes('/term-stable/ws'),
        ).length;
        const socketCountBeforeDestroy = sockets.length;
        sockets.at(-1).drop(4001, 'terminal-resync-required: missed 3 output chunks');
        element.destroy();
        await new Promise((resolve) => setTimeout(resolve, 250));

        return {
          clearedBeforeSnapshot,
          resyncedBuffer,
          stableUrls,
          stableSocketCountAfterTaskChange,
          replacementUrl: sockets[2]?.url,
          socketCountBeforeDestroy,
          socketCountAfterDestroy: sockets.length,
          session: element.session,
          task: element.task,
        };
      } finally {
        element?.destroy();
        element?.remove();
        window.WebSocket = NativeWebSocket;
        window.Terminal = NativeTerminal;
        window.FitAddon = NativeFitAddon;
      }
    }, session.id);

    assert.equal(result.clearedBeforeSnapshot, true);
    assert.equal(result.resyncedBuffer, 'beforeduring');
    assert.equal(result.stableUrls.length, 2);
    assert.equal(result.stableUrls[0], result.stableUrls[1]);
    assert.match(result.stableUrls[0], new RegExp(`/api/sessions/${session.id}/terminals/term-stable/ws$`));
    assert.equal(result.stableSocketCountAfterTaskChange, 2);
    assert.match(result.replacementUrl, /\/term-new\/ws$/);
    assert.equal(result.socketCountAfterDestroy, result.socketCountBeforeDestroy);
    assert.equal(result.session, session.id);
    assert.equal(result.task, 'term-new');
    assertNoErrors();
  } finally {
    await context.close();
  }
});

test('History searches and exports the durable ledger, then Rewind restores transcript and composer without touching the workspace', async () => {
  const { context, page, session, assertNoErrors } = await openControlledSession();
  const errors = [];
  page.on('console', (message) => {
    if (message.type() === 'error') errors.push(`console: ${message.text()}`);
  });
  const workspaceFile = `${session.working_dir}/src/index.js`;
  const beforeWorkspace = await readFile(workspaceFile, 'utf8');
  try {
    await page.waitForFunction((sessionId) => Boolean(
      customElements.get('ax-session-history')
      && S.session.id === sessionId
      && S.session.historyState === 'ready'
      && document.querySelectorAll('#session-msgs .smsg.user').length === 3
    ), session.id);
    await page.evaluate(() => {
      if (S.session.taskTimer) clearInterval(S.session.taskTimer);
      S.session.taskTimer = null;
    });

    await page.locator('#session-history-btn').click();
    await page.locator('#session-history .turn').first().waitFor({ state: 'visible' });
    assert.equal(await page.locator('#session-history .turn').count(), 3);
    const historyText = await page.locator('#session-history .dialog').textContent();
    assert.match(historyText, /Completed/);
    assert.match(historyText, /Cancelled/);
    assert.match(historyText, /Failed/);
    assert.match(historyText, /edit_file/);

    await page.locator('#session-history .search').fill('LAUNCH-CONTRACT.MD');
    await page.locator('#session-history .search-submit').click();
    await page.waitForFunction(() => {
      const history = document.querySelector('#session-history')?.shadowRoot;
      return history?.querySelectorAll('.turn').length === 1
        && history?.querySelector('.matches')?.textContent?.includes('context');
    });
    assert.match(await page.locator('#session-history .turn').textContent(), /Diagnose the launch blocker/);

    await page.locator('#session-history .search').fill('cross-session marker');
    await page.locator('#session-history .search-submit').click();
    await page.waitForFunction(() =>
      document.querySelector('#session-history')?.shadowRoot?.querySelector('.announcement')?.textContent === '0 turns');
    await page.locator('#session-history .scope').selectOption('all');
    await page.waitForFunction(() => {
      const history = document.querySelector('#session-history')?.shadowRoot;
      return history?.querySelectorAll('.turn').length === 1
        && history?.querySelector('.turn')?.textContent?.includes('Beta Only Session');
    });

    const markdownDownload = page.waitForEvent('download');
    await page.locator('#session-history .export-markdown').click();
    const markdown = await markdownDownload;
    assert.equal(markdown.suggestedFilename(), `session-${session.id}.md`);
    const markdownPath = await markdown.path();
    assert.ok(markdownPath);
    const markdownBody = await readFile(markdownPath, 'utf8');
    assert.match(markdownBody, /# Alpha First Session/);
    assert.match(markdownBody, /_Turn status: Cancelled_/);
    assert.match(markdownBody, /launch-contract\.md/);

    const jsonDownload = page.waitForEvent('download');
    await page.locator('#session-history .export-json').click();
    const json = await jsonDownload;
    assert.equal(json.suggestedFilename(), `session-${session.id}.json`);
    const jsonPath = await json.path();
    assert.ok(jsonPath);
    const exportedTurns = JSON.parse(await readFile(jsonPath, 'utf8'));
    assert.deepEqual(exportedTurns.map((turn) => turn.status), ['completed', 'cancelled', 'failed']);
    await page.locator('#session-history .close').click();

    const failedTurn = page.locator('#session-msgs .smsg.user').nth(2);
    const rewind = failedTurn.locator('.smsg-act', { hasText: 'Rewind' });
    await rewind.click();
    await page.waitForFunction(() =>
      document.querySelectorAll('#session-msgs .smsg.user').length === 2
      && document.querySelector('#session-text')?.value === 'Diagnose the launch blocker');
    assert.doesNotMatch(await page.locator('#session-msgs').textContent(), /Diagnose the launch blocker/);
    assert.deepEqual(
      await page.evaluate(() => document.querySelector('#session-context')?.selectedReferenceIds?.()),
      [],
      'historical context must not be silently reattached after rewind',
    );
    assert.equal(await readFile(workspaceFile, 'utf8'), beforeWorkspace);

    const visible = await fetch(`${runtime.baseUrl}/api/sessions/${session.id}/turns`).then((response) => response.json());
    assert.deepEqual(visible.map((turn) => turn.id), ['history-completed', 'history-cancelled']);
    assertNoErrors();
    assert.deepEqual(errors, [], errors.join('\n'));
  } finally {
    await context.close();
  }
});
