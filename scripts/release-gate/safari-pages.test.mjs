// node --test scripts/release-gate/safari-pages.test.mjs
//
// The pages the macOS gate's Safari steps open: a redirect that goes only
// to a Preview address, and the Domain=localhost cookie flood (and its
// removal) that signed-out.test.mjs sets, from another *.localhost page,
// with a count of the cookies the browser sends back.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

import { PREVIEW_URL, answer, plantScript, plantedIn } from './safari-pages.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const scratch = mkdtempSync(path.join(tmpdir(), 'axocoatl-safari-pages-'));
after(() => rmSync(scratch, { recursive: true, force: true }));

const preview = 'http://ses-123e4567-e89b-12d3-a456-426614174000-p5173.localhost:8080/';

test('the redirect goes only to a Preview address', () => {
  const go = answer('GET', `/go?to=${encodeURIComponent(preview)}`);
  assert.equal(go.status, 302);
  assert.equal(go.headers.location, preview);
  for (const to of ['https://example.com/', 'http://localhost:8080/', 'http://evil.localhost:8080/', `${preview} x`, '']) {
    assert.equal(answer('GET', `/go?to=${encodeURIComponent(to)}`).status, 400, to);
  }
  assert.ok(PREVIEW_URL.test('http://ses-1-p3000.localhost/next'));
  assert.equal(answer('POST', '/go').status, 405);
  assert.equal(answer('GET', '/other').status, 404);
});

test('the flood is the one the journeys set: 51 cookies of 4,000 bytes, then 9 down to 8 bytes, for localhost', async () => {
  // run SCRIPT: the cookies it sets, the count URL it asks, what it shows.
  const run = async (script, sent = { planted: 57, fill: 48, fit: 9 }) => {
    const seen = { cookies: [], asked: [] };
    const document = {
      set cookie(value) { seen.cookies.push(value); },
      getElementById: () => ({ set textContent(text) { seen.said = text; } }),
    };
    const fetch = async (url) => { seen.asked.push(url); return { json: async () => sent }; };
    await vm.runInNewContext(plantScript(...script), { document, fetch });
    return seen;
  };
  const planted = await run(['/api']);
  assert.equal(planted.cookies.length, 60);
  assert.equal(planted.cookies.filter((c) => /^fill-\d+=x{4000}; Domain=localhost; Path=\/api; Max-Age=3600$/.test(c)).length, 51);
  const fits = planted.cookies.slice(51).map((c) => c.match(/^fit-\d+=(y+);/)[1].length);
  assert.deepEqual(fits, [2048, 1024, 512, 256, 128, 64, 32, 16, 8]);
  // What it shows is the count its server received, not what it tried.
  assert.deepEqual(planted.asked, ['/api/gate-cookie-count']);
  assert.equal(planted.said, "Set 60 cookies for localhost with Path=/api. Safari sends 57 of them back to this page's server (48 of the 51 large, 9 of the 9 small).");
  const removed = await run(['/ws', 'clear'], { planted: 0, fill: 0, fit: 0 });
  assert.ok(removed.cookies.length === 60 && removed.cookies.every((c) => c.endsWith('; Domain=localhost; Path=/ws; Max-Age=0')));
  assert.deepEqual(removed.asked, ['/ws/gate-cookie-count']);
  assert.match(removed.said, /Safari sends 0 of them back/);
  const counted = await run(['/', 'count'], { planted: 3, fill: 0, fit: 3 });
  assert.deepEqual([counted.cookies, counted.asked], [[], ['/gate-cookie-count']]);
  assert.match(counted.said, /^This page sets no cookie; it counts the 60 for localhost with Path=\/\. Safari sends 3 of them back/);
  const page = answer('GET', '/plant?path=/api');
  assert.equal(page.status, 200);
  assert.match(page.body, /Domain=localhost; Path=\/api/);
  assert.equal(answer('GET', '/plant?path=/etc').status, 400);
  assert.equal(answer('GET', '/clear?path=/ws').status, 200);
  assert.equal(answer('GET', '/count?path=/api').status, 200);
  // The count reads the Cookie header: only the planted names count.
  assert.deepEqual(plantedIn('fill-0=x; fit-8=y; axocoatl-token-8080=z; fill-50=x'), { planted: 3, fill: 2, fit: 1, bytes: 51 });
  const count = answer('GET', '/api/gate-cookie-count', 'fill-0=x; other=1');
  assert.deepEqual([count.status, JSON.parse(count.body).planted, count.planted.planted], [200, 1, 1]);
});

test('the server binds loopback only, writes its port and logs each request with its Sec-Fetch-Dest', async () => {
  const portFile = path.join(scratch, 'port');
  const log = path.join(scratch, 'requests.log');
  const child = spawn(process.execPath, [path.join(HERE, 'safari-pages.mjs'), '--port-file', portFile, '--log', log], { stdio: 'ignore' });
  try {
    let port = '';
    for (let i = 0; i < 100 && !port; i += 1) {
      await new Promise((resolve) => setTimeout(resolve, 50));
      try { port = readFileSync(portFile, 'utf8').trim(); } catch { /* not yet */ }
    }
    assert.match(port, /^\d+$/);
    const response = await fetch(`http://127.0.0.1:${port}/go?to=${encodeURIComponent(preview)}`, {
      redirect: 'manual', headers: { 'sec-fetch-dest': 'document' },
    });
    assert.equal(response.status, 302);
    // A Cookie header as large as WebKit sends (about 200 KB) is counted,
    // and the count is logged for the gate.
    const cookie = [...Array.from({ length: 51 }, (_, i) => `fill-${i}=${'x'.repeat(4000)}`), 'fit-0=yy'].join('; ');
    const counted = await fetch(`http://127.0.0.1:${port}/api/gate-cookie-count`, { headers: { cookie } });
    assert.deepEqual([counted.status, (await counted.json()).planted], [200, 52]);
    assert.equal(readFileSync(log, 'utf8'),
      `GET /go?to=${encodeURIComponent(preview)} document\nGET /api/gate-cookie-count - planted=52\n`);
  } finally {
    child.kill();
  }
});
