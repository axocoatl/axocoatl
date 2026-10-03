import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { launchTestDaemon, newAuthorizedContext, resolveChromiumExecutable } from '../support/daemon.mjs';

let runtime;
let browser;
before(async () => {
  runtime = process.env.AXOCOATL_COMPONENT_BASE_URL
    ? { baseUrl: process.env.AXOCOATL_COMPONENT_BASE_URL, stop: async () => {} }
    : await launchTestDaemon();
  browser = await chromium.launch({ headless: true, executablePath: await resolveChromiumExecutable() });
});
after(async () => {
  await browser?.close();
  await runtime?.stop();
});

const agent = { kind: 'agent', invocation_id: 'tool-1', activation_id: 'activation', agent: 'writer', process: 'tool-1:0' };

function line(seq, event) {
  return { v: 1, seq, ts_ms: 1790000000000 + seq * 1000, event };
}

function record(extraRules = []) {
  return [
    line(1, { kind: 'policy', scope: 'session', revision: 1, digest: 'a'.repeat(64), source: 'config', rules: ['registry.npmjs.org:443 (preset npm)'] }),
    line(2, { kind: 'sidecar', state: 'ready', generation: 1 }),
    line(3, { kind: 'bind', token: '0123456789abcdef', binding: agent, scope: 'session' }),
    line(4, { kind: 'open', conn: 'g1:1', decision: 'allow', rule: 'preset:npm/registry.npmjs.org', host: 'registry.npmjs.org', port: 443, conn_kind: 'connect', addrs: ['104.16.0.35'], token: '0123456789abcdef', binding: agent, scope: 'session', policy_revision: 1 }),
    line(5, { kind: 'close', conn: 'g1:1', ip: '104.16.0.35', up: 900, down: 1258291, ms: 40, outcome: 'closed' }),
    line(6, { kind: 'open', conn: 'g1:2', decision: 'deny', reason: 'not_allowed', status: 403, host: 'api.example.com', port: 443, conn_kind: 'connect', addrs: [], token: '0123456789abcdef', binding: agent, scope: 'session', policy_revision: 1 }),
    line(7, { kind: 'open', conn: 'g1:3', decision: 'deny', reason: 'no_credential', status: 407, host: 'upstream.test', port: 8000, conn_kind: 'http', method: 'GET', path: '/', addrs: [] }),
    line(8, { kind: 'open', conn: 'g1:4', decision: 'deny', reason: 'not_allowed', status: 403, host: '1.1.1.1', port: 443, conn_kind: 'connect', addrs: [], token: '0123456789abcdef', binding: agent, scope: 'session', policy_revision: 1 }),
    line(9, { kind: 'web', tool: 'web_fetch', invocation_id: 'tool-2', activation_id: 'activation', agent: 'writer', decision: 'allow', url: 'https://docs.example.com/page', redirects: [], unresponsive_engines: [], source_ids: ['S1a2b3c4d'], retrieved_at_ms: 1790000009000, ms: 200 }),
    ...extraRules,
  ];
}

async function setup({ full = false, sidecar = { state: 'ready', generation: 1, restarts: 0 }, loseFirstAllow = false, extra = [], warnings, proposals = [], loseFirstDecision = false } = {}) {
  const context = await newAuthorizedContext(browser, { viewport: { width: 390, height: 844 }, colorScheme: 'dark', reducedMotion: 'reduce' });
  const page = await context.newPage();
  const calls = [];
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  const sessionRules = [{ id: 'preset:npm/registry.npmjs.org', text: 'registry.npmjs.org:443 (preset npm)', source: 'preset' }];
  let revision = 1;
  const applied = new Set();
  await page.route('**/network-fixture', (route) => route.fulfill({
    contentType: 'text/html',
    body: '<!doctype html><html data-theme="dark"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/ui/tokens.css"><ax-session-network></ax-session-network><script type="module" src="/ui/session-network.js"></script></html>',
  }));
  await page.route('**/api/sessions/session/network**', async (route) => {
    const url = new URL(route.request().url());
    const decision = url.pathname.match(/\/network\/proposals\/([^/]+)\/(approve|reject)$/);
    if (decision) {
      const [, id, action] = decision;
      const body = route.request().postDataJSON();
      calls.push({ path: url.pathname, ...body });
      const proposal = proposals.find((entry) => entry.id === decodeURIComponent(id));
      if (applied.has(body.command_id) || proposal.state !== 'pending') {
        return route.fulfill({ status: 409, json: { error: `proposal ${proposal.id} was already ${proposal.state}` } });
      }
      applied.add(body.command_id);
      proposal.state = action === 'approve' ? 'approved' : 'rejected';
      proposal.actor = 'human';
      if (action === 'approve') {
        revision += 1;
        proposal.revision = revision;
        sessionRules.push({ id: `session#rev${revision}`, text: `${proposal.host}:${proposal.ports.join(',')} (allowed for this Session)`, source: 'session' });
      }
      if (loseFirstDecision) { loseFirstDecision = false; return route.abort('failed'); }
      return route.fulfill({ json: { proposal_id: proposal.id, state: proposal.state, ...(action === 'approve' ? { revision, digest: 'b'.repeat(64) } : {}) } });
    }
    if (url.pathname.endsWith('/allow')) {
      const body = route.request().postDataJSON();
      calls.push(body);
      if (applied.has(body.command_id)) return route.fulfill({ status: 409, json: { error: `command ${body.command_id} was already applied` } });
      applied.add(body.command_id);
      revision += 1;
      sessionRules.push({ id: `session#rev${revision}`, text: `${body.host}:${body.ports.join(',')} (allowed for this Session)`, source: 'session' });
      if (loseFirstAllow) { loseFirstAllow = false; return route.abort('failed'); }
      return route.fulfill({ json: { revision, digest: 'b'.repeat(64) } });
    }
    const events = record(extra);
    return route.fulfill({
      json: {
        session_id: 'session', mode: 'egress', sidecar, ...(warnings ? { warnings } : {}),
        policies: [
          { scope: 'provisioning', revision: 1, digest: 'c'.repeat(64), rules: [{ id: 'preset:alpine/dl-cdn.alpinelinux.org', text: 'dl-cdn.alpinelinux.org:80,443 (preset alpine)', source: 'preset' }] },
          { scope: 'session', revision, digest: 'd'.repeat(64), rules: sessionRules.slice() },
        ],
        private_destinations: [],
        record: { events: events.length, bytes: 4096, max_events: 50000, full, gaps: 0 },
        events, next_after: events.length,
        proposals: proposals.map((proposal) => ({ ...proposal })),
      },
    });
  });
  await page.goto(`${runtime.baseUrl}/network-fixture`);
  await page.evaluate(async () => {
    await customElements.whenDefined('ax-session-network');
    await document.querySelector('ax-session-network').open({ sessionId: 'session' });
  });
  return { context, page, calls, errors };
}

test('the network panel shows mode, counts, policy and refused rows, and allows a refused host', async () => {
  const { context, page, calls, errors } = await setup();
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    await dialog.getByText('1 allowed', { exact: true }).waitFor();
    await dialog.getByText('3 refused', { exact: true }).waitFor();
    await dialog.getByText('1.2 MB in · 900 B out', { exact: true }).waitFor();
    await dialog.getByText('Per-Agent host limits are not enforced inside one Session container', { exact: false }).waitFor();
    await dialog.getByText('Egress proxy: ready (generation 1, 0 restarts).', { exact: true }).waitFor();
    await dialog.getByText('registry.npmjs.org:443 (preset npm)', { exact: true }).waitFor();
    // Refused rows: the agent's not_allowed host, the unattributed no_credential
    // attempt, and an IP literal that cannot be allowed by name.
    const rows = dialog.locator('table.refused tbody tr');
    assert.equal(await rows.count(), 3);
    await dialog.getByRole('cell', { name: 'api.example.com:443', exact: true }).waitFor();
    await dialog.getByRole('cell', { name: 'no credential', exact: true }).waitFor();
    await dialog.getByText('The process had no egress credential', { exact: false }).waitFor();
    assert.equal(await dialog.getByRole('button', { name: 'Allow 1.1.1.1:443 for this Session', exact: true }).count(), 0);
    assert.equal(await dialog.getByRole('button', { name: /Allow upstream\.test/ }).count(), 0);
    // Web events show their source ids.
    await dialog.getByText('S1a2b3c4d', { exact: true }).waitFor();

    await dialog.getByRole('button', { name: 'Allow api.example.com:443 for this Session', exact: true }).click();
    await dialog.getByText('api.example.com:443 is allowed for this Session. New connections use it now.', { exact: true }).waitFor();
    await dialog.locator('li.session-rule', { hasText: 'api.example.com:443 (allowed for this Session)' }).waitFor();
    assert.equal(calls.length, 1);
    assert.equal(calls[0].scope, 'session');
    assert.equal(calls[0].host, 'api.example.com');
    assert.deepEqual(calls[0].ports, [443]);
    assert.match(calls[0].command_id, /^[0-9a-f-]{36}$/);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('a provisioning refusal cannot widen the Session policy, and Session warnings are shown', async () => {
  const warning = "Axocoatl's config file is inside this Workspace; Agents can read it. Keep secrets in environment variables.";
  const extra = [line(10, { kind: 'open', conn: 'g1:5', decision: 'deny', reason: 'not_allowed', status: 403, host: 'mirror.example.org', port: 443, conn_kind: 'connect', addrs: [], token: 'fedcba9876543210', binding: { kind: 'provisioning' }, scope: 'provisioning', policy_revision: 1 })];
  const { context, page, calls, errors } = await setup({ extra, warnings: [warning] });
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    await dialog.getByText('4 refused', { exact: true }).waitFor();
    await dialog.getByRole('cell', { name: 'mirror.example.org:443', exact: true }).waitFor();
    assert.equal(await dialog.getByRole('button', { name: /Allow mirror\.example\.org/ }).count(), 0);
    await dialog.getByText('Provisioning reaches only the distribution mirrors of its presets', { exact: false }).waitFor();
    // The Session's own refusal keeps its button.
    await dialog.getByRole('button', { name: 'Allow api.example.com:443 for this Session', exact: true }).waitFor();
    await dialog.getByText(warning, { exact: true }).waitFor();
    assert.equal(calls.length, 0);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('requests on egress routes show what each asked for and how it ended', async () => {
  const route = { conn: 'g1:6', host: 'api.github.com' };
  const extra = [
    line(10, { kind: 'open', conn: route.conn, decision: 'allow', rule: 'route#0', host: route.host, port: 443, conn_kind: 'connect', addrs: ['140.82.112.6'], token: '0123456789abcdef', binding: agent, scope: 'session', policy_revision: 1 }),
    line(11, { kind: 'request', conn: route.conn, seq_in_conn: 1, method: 'GET', path: '/repos/acme/app', host: route.host, rule: 'route#0.rules[0]', decision: 'allow', credential: 'github' }),
    line(12, { kind: 'response', conn: route.conn, seq_in_conn: 1, status: 200, up: 0, down: 5120, ms: 80, outcome: 'completed' }),
    line(13, { kind: 'request', conn: route.conn, seq_in_conn: 2, method: 'DELETE', path: '/repos/acme/app', host: route.host, decision: 'deny', reason: 'route_denied' }),
    line(14, { kind: 'open', conn: 'g1:7', decision: 'deny', reason: 'route_not_for_binding', status: 403, rule: 'route#0', host: route.host, port: 443, conn_kind: 'connect', addrs: [], token: 'fedcba9876543210', binding: { kind: 'terminal', terminal_id: 'term-1' }, scope: 'session', policy_revision: 1 }),
    line(15, { kind: 'request', conn: route.conn, seq_in_conn: 3, method: 'POST', path: '/login', host: route.host, rule: 'route#0.rules[1]', decision: 'allow', credential: 'github' }),
    line(16, { kind: 'response', conn: route.conn, seq_in_conn: 3, status: 200, up: 64, down: 128, ms: 30, outcome: 'completed', cookies_dropped: 2 }),
  ];
  const { context, page, calls, errors } = await setup({ extra });
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    await dialog.getByRole('heading', { name: 'Requests on routes', exact: true }).waitFor();
    const rows = dialog.locator('table.requests tbody tr');
    assert.equal(await rows.count(), 3);
    // Newest first: the POST whose cookies were removed, the refused
    // DELETE, then the GET that went out.
    await rows.nth(0).getByText('route#0.rules[1] · 200 (completed) · credential github · 2 Set-Cookie removed', { exact: true }).waitFor();
    assert.equal(await rows.nth(1).getAttribute('data-decision'), 'deny');
    await rows.nth(1).getByRole('cell', { name: 'DELETE api.github.com/repos/acme/app', exact: true }).waitFor();
    await rows.nth(1).getByText('route_denied', { exact: true }).waitFor();
    await rows.nth(1).getByText('No rule of the route allows this request.', { exact: true }).waitFor();
    await rows.nth(2).getByRole('cell', { name: 'writer', exact: true }).waitFor();
    await rows.nth(2).getByText('route#0.rules[0] · 200 (completed) · credential github', { exact: true }).waitFor();
    // A process kind the route does not serve is refused at the connection,
    // with no button to widen the policy.
    await dialog.getByText('An egress route that does not serve this kind of process.', { exact: true }).waitFor();
    assert.equal(await dialog.getByRole('button', { name: /Allow api\.github\.com/ }).count(), 0);
    assert.equal(calls.length, 0);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('connections are listed newest first, and in a hardened Session each names its program', async () => {
  const curl = { pid: 412, uid: 1000, gid: 1000, exe: '/usr/bin/curl', exe_sha256: 'ab'.repeat(32), ancestors: ['/bin/bash', '/axocoatl-exec-supervisor'] };
  const git = { pid: 413, uid: 1000, gid: 1000, exe: '/usr/libexec/git-core/git-remote-http', exe_sha256: 'cd'.repeat(32), ancestors: ['/usr/bin/git', '/bin/bash'] };
  const extra = [
    line(10, { kind: 'open', conn: 'g1:8', peer: git, decision: 'allow', rule: 'route#0', host: 'git.example.com', port: 443, conn_kind: 'connect', addrs: ['192.0.2.7'], token: '0123456789abcdef', binding: agent, scope: 'session', policy_revision: 1 }),
    line(11, { kind: 'request', conn: 'g1:8', seq_in_conn: 1, method: 'GET', path: '/acme/app.git/info/refs', host: 'git.example.com', rule: 'route#0.rules[0]', decision: 'allow', credential: 'git' }),
    line(12, { kind: 'open', conn: 'g1:9', peer: curl, decision: 'deny', reason: 'not_allowed', status: 403, host: 'paste.example.net', port: 443, conn_kind: 'connect', addrs: [], token: '0123456789abcdef', binding: agent, scope: 'session', policy_revision: 1 }),
    line(13, { kind: 'open', conn: 'g1:10', peer: { pid: 77, uid: 1000, error: 'foreign_namespace', exe_sha256: 'ef'.repeat(32) }, decision: 'allow', rule: 'preset:npm/registry.npmjs.org', host: 'registry.npmjs.org', port: 443, conn_kind: 'connect', addrs: ['104.16.0.35'], token: '0123456789abcdef', binding: agent, scope: 'session', policy_revision: 1 }),
  ];
  const { context, page, calls, errors } = await setup({ extra });
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    await dialog.getByRole('heading', { name: 'Connections', exact: true }).waitFor();
    const connections = dialog.locator('table.connections');
    assert.deepEqual(await connections.locator('th').allTextContents(), ['Time', 'Agent', 'Program', 'Destination', 'Traffic']);
    const rows = connections.locator('tbody tr');
    // Newest first: the unnamed npm connection, the git route, then the
    // first connection, recorded before identities and closed.
    assert.equal(await rows.count(), 3);
    assert.deepEqual(await rows.evaluateAll((all) => all.map((row) => row.dataset.conn)), ['g1:10', 'g1:8', 'g1:1']);
    await rows.nth(0).getByText('unknown program', { exact: true }).waitFor();
    await rows.nth(0).getByText('mount namespace of its own', { exact: false }).waitFor();
    await rows.nth(1).getByText('/usr/libexec/git-core/git-remote-http', { exact: true }).waitFor();
    await rows.nth(1).getByText('user 1000:1000 · started by /usr/bin/git (under /bin/bash) · SHA-256 cdcdcdcdcdcd…', { exact: true }).waitFor();
    assert.equal(await rows.nth(1).locator('code').getAttribute('title'), `SHA-256 ${'cd'.repeat(32)}`);
    await rows.nth(1).getByText('open', { exact: true }).waitFor();
    await rows.nth(2).getByText('not named', { exact: true }).waitFor();
    await rows.nth(2).getByText('1.2 MB received, 900 B sent', { exact: true }).waitFor();
    // The refused row and the route's request name their programs too.
    const refused = dialog.locator('table.refused');
    assert.deepEqual(await refused.locator('th').allTextContents(), ['Time', 'Agent', 'Program', 'Destination', 'Reason', '']);
    const paste = refused.locator('tbody tr', { hasText: 'paste.example.net:443' });
    await paste.getByText('/usr/bin/curl', { exact: true }).waitFor();
    await paste.getByText('started by /bin/bash (under /axocoatl-exec-supervisor)', { exact: false }).waitFor();
    const request = dialog.locator('table.requests tbody tr');
    assert.equal(await request.count(), 1);
    await request.getByText('/usr/libexec/git-core/git-remote-http', { exact: true }).waitFor();
    assert.equal(calls.length, 0);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('without program identities the panel has no program column', async () => {
  const { context, page, errors } = await setup();
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    const connections = dialog.locator('table.connections');
    await connections.waitFor();
    assert.deepEqual(await connections.locator('th').allTextContents(), ['Time', 'Agent', 'Destination', 'Traffic']);
    assert.deepEqual(await dialog.locator('table.refused th').allTextContents(), ['Time', 'Agent', 'Destination', 'Reason', '']);
    assert.equal(await dialog.locator('td.program').count(), 0);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('a lost allow is resent with the same command id and a 409 for it counts as applied', async () => {
  const { context, page, calls, errors } = await setup({ loseFirstAllow: true });
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    const allow = dialog.getByRole('button', { name: 'Allow api.example.com:443 for this Session', exact: true });
    await allow.click();
    await dialog.getByText('The answer was lost', { exact: false }).waitFor();
    await allow.click();
    await dialog.getByText('api.example.com:443 is allowed for this Session. New connections use it now.', { exact: true }).waitFor();
    assert.equal(calls.length, 2);
    assert.equal(calls[0].command_id, calls[1].command_id);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

function proposal(id, host, ports, state = 'pending') {
  return {
    id, state, host, ports, reason: `The build reads its schema from ${host}.`, agent: 'writer',
    invocation_id: 'tool-9', activation_id: 'activation',
  };
}

test('Agents\' host requests wait for a person, who approves or rejects each', async () => {
  const proposals = [
    proposal('prop_0123456789abcdef', 'api.example.com', [443]),
    proposal('prop_fedcba9876543210', 'cdn.example.net', [443, 8443]),
    { ...proposal('prop_00000000000000aa', 'old.example.org', [443], 'approved'), revision: 1, actor: 'human' },
  ];
  const { context, page, calls, errors } = await setup({ proposals });
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    await dialog.getByText('2 host requests from Agents wait for you below.', { exact: true }).waitFor();
    const rows = dialog.locator('table.proposals tbody tr');
    assert.equal(await rows.count(), 3);
    const requests = dialog.locator('table.proposals');
    await requests.getByRole('cell', { name: 'api.example.com:443', exact: true }).waitFor();
    await requests.getByRole('cell', { name: 'cdn.example.net:443,8443', exact: true }).waitFor();
    await dialog.getByText('The build reads its schema from api.example.com.', { exact: true }).waitFor();
    await dialog.getByText('Only you can approve a request.', { exact: false }).waitFor();
    // A decided request has no buttons.
    await dialog.locator('tr[data-proposal="prop_00000000000000aa"]').getByText('Approved · revision 1', { exact: true }).waitFor();
    assert.equal(await dialog.locator('tr[data-proposal="prop_00000000000000aa"] button').count(), 0);

    await dialog.getByRole('button', { name: 'Approve api.example.com:443 for this Session', exact: true }).click();
    await dialog.getByText('api.example.com:443 is allowed for this Session. The Agent\'s call continues.', { exact: true }).waitFor();
    await dialog.locator('tr[data-proposal="prop_0123456789abcdef"]').getByText('Approved · revision 2', { exact: true }).waitFor();
    await dialog.locator('li.session-rule', { hasText: 'api.example.com:443 (allowed for this Session)' }).waitFor();
    await dialog.getByText('1 host request from Agents waits for you below.', { exact: true }).waitFor();

    await dialog.getByRole('button', { name: 'Reject cdn.example.net:443,8443', exact: true }).click();
    await dialog.getByText('cdn.example.net:443,8443 was rejected.', { exact: false }).waitFor();
    await dialog.locator('tr[data-proposal="prop_fedcba9876543210"]').getByText('Rejected', { exact: true }).waitFor();
    assert.equal(await dialog.locator('p.proposals-waiting').count(), 0);
    assert.equal(await dialog.locator('li.session-rule', { hasText: 'cdn.example.net' }).count(), 0);

    assert.equal(calls.length, 2);
    assert.equal(calls[0].path, '/api/sessions/session/network/proposals/prop_0123456789abcdef/approve');
    assert.equal(calls[1].path, '/api/sessions/session/network/proposals/prop_fedcba9876543210/reject');
    for (const call of calls) {
      assert.match(call.command_id, /^[0-9a-f-]{36}$/);
      assert.deepEqual(Object.keys(call).sort(), ['command_id', 'path']);
    }
    assert.notEqual(calls[0].command_id, calls[1].command_id);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('a lost approval is resent with the same command id and a 409 for it counts as applied', async () => {
  const proposals = [proposal('prop_0123456789abcdef', 'api.example.com', [443])];
  const { context, page, calls, errors } = await setup({ proposals, loseFirstDecision: true });
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    const approve = dialog.getByRole('button', { name: 'Approve api.example.com:443 for this Session', exact: true });
    await approve.click();
    await dialog.getByText('The answer was lost; select Approve again', { exact: false }).waitFor();
    await approve.click();
    await dialog.getByText('api.example.com:443 is allowed for this Session. The Agent\'s call continues.', { exact: true }).waitFor();
    assert.equal(calls.length, 2);
    assert.equal(calls[0].command_id, calls[1].command_id);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('a refused browser request is allowed in the browser scope', async () => {
  const browserRow = line(10, { kind: 'open', conn: 'g1:6', decision: 'deny', reason: 'not_allowed', status: 403, host: 'fonts.example.com', port: 443, conn_kind: 'connect', addrs: [], token: 'fedcba9876543210', binding: { kind: 'browser', invocation_id: 'tool-b', agent: 'qa' }, scope: 'browser', policy_revision: 1 });
  const { context, page, calls, errors } = await setup({ extra: [browserRow] });
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    await dialog.getByRole('button', { name: 'Allow fonts.example.com:443 for this Session', exact: true }).click();
    await dialog.getByText('fonts.example.com:443 is allowed for this Session. New connections use it now.', { exact: true }).waitFor();
    assert.equal(calls.length, 1);
    assert.equal(calls[0].scope, 'browser');
    assert.equal(calls[0].host, 'fonts.example.com');
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('the reload and proposal routes of the real daemon answer as documented', { skip: !!process.env.AXOCOATL_COMPONENT_BASE_URL }, async () => {
  const post = async (pathname, body) => {
    const response = await fetch(`${runtime.baseUrl}${pathname}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, body: await response.json() };
  };
  const configPath = path.join(runtime.runRoot, 'axocoatl.e2e.yaml');
  const original = await readFile(configPath, 'utf8');
  try {
    let reload = await post('/api/network/reload');
    assert.equal(reload.status, 200, JSON.stringify(reload.body));
    assert.deepEqual(reload.body.applied, []);
    assert.deepEqual(reload.body.restart_required, []);
    assert.deepEqual(reload.body.unchanged, [
      'sandbox.egress.allow', 'sandbox.egress.private_destinations', 'sandbox.egress.routes',
      'credentials', 'browser.allow', 'browser.private_destinations',
    ]);

    await writeFile(configPath, original.replace('  network: none\n', '  network: none\n  egress:\n    allow: [npm]\n'));
    reload = await post('/api/network/reload');
    assert.equal(reload.status, 200, JSON.stringify(reload.body));
    assert.deepEqual(reload.body.applied, ['sandbox.egress.allow']);
    assert.deepEqual(reload.body.restart_required, []);
    assert.deepEqual(reload.body.revisions, []);
    assert.deepEqual(reload.body.changes, [{ key: 'sandbox.egress.allow', added: ['registry.npmjs.org:443 (preset npm)'] }]);
    assert.equal(reload.body.failed, undefined);

    await writeFile(configPath, original.replace('  network: none\n', '  network: bridge\n'));
    reload = await post('/api/network/reload');
    assert.equal(reload.status, 200, JSON.stringify(reload.body));
    assert.deepEqual(reload.body.restart_required, ['sandbox.network']);

    await writeFile(configPath, original.replace('  network: none\n', '  network: everywhere\n'));
    reload = await post('/api/network/reload');
    assert.equal(reload.status, 400, JSON.stringify(reload.body));
    assert.match(reload.body.error, /nothing was changed/);
  } finally {
    await writeFile(configPath, original);
  }

  const sessionId = runtime.fixtures.alpha.sessions[0].id;
  const decide = (session, proposal, action) => post(
    `/api/sessions/${encodeURIComponent(session)}/network/proposals/${proposal}/${action}`,
    { command_id: 'c-route' },
  );
  // This daemon does not run Sessions under egress, so nothing can be decided.
  let decided = await decide(sessionId, 'prop_0123456789abcdef', 'approve');
  assert.equal(decided.status, 400, JSON.stringify(decided.body));
  decided = await decide(sessionId, 'not-a-proposal', 'reject');
  assert.equal(decided.status, 400, JSON.stringify(decided.body));
  decided = await decide('no-such-session', 'prop_0123456789abcdef', 'approve');
  assert.equal(decided.status, 404, JSON.stringify(decided.body));
  const view = await fetch(`${runtime.baseUrl}/api/sessions/${encodeURIComponent(sessionId)}/network`).then((response) => response.json());
  assert.deepEqual(view.proposals, []);
});

test('a full record and a failed proxy are shown as banners', async () => {
  const { context, page, errors } = await setup({ full: true, sidecar: { state: 'failed', generation: 6, restarts: 5 } });
  try {
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    await dialog.getByText('The network record is full, so new connections are refused.', { exact: true }).waitFor();
    await dialog.getByText('The egress proxy stopped after repeated failures', { exact: false }).waitFor();
    await dialog.getByText('Egress proxy: failed (generation 6, 5 restarts).', { exact: true }).waitFor();
    for (const theme of ['light', 'dark']) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      const background = await dialog.evaluate((node) => getComputedStyle(node).backgroundColor);
      assert.notEqual(background, 'rgba(0, 0, 0, 0)');
    }
    await page.keyboard.press('Escape');
    assert.equal(await dialog.isVisible(), false);
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});

test('the activation inspector shows network evidence and opens the Session network panel', async () => {
  const context = await newAuthorizedContext(browser, { viewport: { width: 1000, height: 850 } });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  try {
    await page.route('**/inspector-fixture', (route) => route.fulfill({
      contentType: 'text/html',
      body: '<!doctype html><html data-theme="light"><head><link rel="stylesheet" href="/ui/tokens.css"></head><body><ax-activation-inspector></ax-activation-inspector><script type="module" src="/ui/activation-inspector.js"></script></body></html>',
    }));
    await page.route('**/api/sessions/session/network**', (route) => {
      const events = record();
      return route.fulfill({ json: {
        session_id: 'session', mode: 'egress', sidecar: { state: 'ready', generation: 1, restarts: 0 }, policies: [],
        private_destinations: [], record: { events: events.length, bytes: 4096, max_events: 50000, full: false, gaps: 0 },
        events, next_after: events.length,
      } });
    });
    await page.goto(`${runtime.baseUrl}/inspector-fixture`);
    await page.waitForFunction(() => customElements.get('ax-activation-inspector'));
    await page.locator('ax-activation-inspector').evaluate((element) => {
      const unrecorded = { status: 'not_recorded' };
      const activation = {
        reference: { kind: 'exact', activation: { session_id: 'session', turn_id: 'turn', execution_epoch_id: 'epoch', node_id: 'writer', activation_id: 'act-1', generation: 1 } },
        generation: { status: 'available', value: 1 }, state: 'accepted', reason: unrecorded, started_at: unrecorded,
        completed_at: unrecorded, input: unrecorded, output: unrecorded, partial_outputs: [], usage: unrecorded, capabilities: {},
        evidence: [{
          kind: 'network', reference: unrecorded,
          summary: { status: 'available', value: 'registry.npmjs.org:443 allowed ×3 (1.2 MB in); evil.test:443 refused (not_allowed)' },
          recorded_at: { status: 'available', value: 1790000000000 },
          details: { status: 'available', value: { events: [], truncated: false } },
        }],
      };
      const nodes = [{ node_id: 'writer', label: 'Writer', dependencies: [], activations: [activation] }];
      element.model = { turnId: 'turn', sessionId: 'session', historyVersion: 'execution_v2', nodes, warnings: [], controlPlane: {
        schema_version: 1, history_version: 'execution_v2', state: 'completed', session_id: 'session', turn_id: 'turn',
        turn_revision: { status: 'available', value: 4 }, graph_revision: { status: 'available', value: 1 },
        epochs: { status: 'available', value: [{ id: 'epoch', state: 'completed' }] }, nodes } };
      element.nodeId = 'writer';
    });
    await page.getByText('registry.npmjs.org:443 allowed ×3 (1.2 MB in); evil.test:443 refused (not_allowed)', { exact: true }).waitFor();
    await page.getByText('network', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Session network', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Session network', exact: true });
    await dialog.getByText('3 refused', { exact: true }).waitFor();
    assert.deepEqual(errors, []);
  } finally {
    await context.close();
  }
});
