// node --test scripts/release-gate/safari-pages.test.mjs
//
// The pages the macOS gate's Safari steps open: a redirect that goes only
// to a Preview address, and the Domain=localhost cookie flood (and its
// removal) that signed-out.test.mjs sets, from another *.localhost page.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

import { PREVIEW_URL, answer, plantScript } from './safari-pages.mjs';

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

test('the flood is the one the journeys set: 51 cookies of 4,000 bytes, then 9 down to 8 bytes, for localhost', () => {
  const cookies = [];
  const document = {
    set cookie(value) { cookies.push(value); },
    getElementById: () => ({ set textContent(text) { document.said = text; } }),
  };
  vm.runInNewContext(plantScript('/api'), { document });
  assert.equal(cookies.length, 60);
  assert.equal(cookies.filter((c) => /^fill-\d+=x{4000}; Domain=localhost; Path=\/api; Max-Age=3600$/.test(c)).length, 51);
  const fits = cookies.slice(51).map((c) => c.match(/^fit-\d+=(y+);/)[1].length);
  assert.deepEqual(fits, [2048, 1024, 512, 256, 128, 64, 32, 16, 8]);
  assert.equal(document.said, '60 cookies set for localhost with Path=/api');
  const removed = [];
  vm.runInNewContext(plantScript('/ws', true), { document: { set cookie(value) { removed.push(value); }, getElementById: () => ({}) } });
  assert.ok(removed.every((c) => c.endsWith('; Domain=localhost; Path=/ws; Max-Age=0')));
  const page = answer('GET', '/plant?path=/api');
  assert.equal(page.status, 200);
  assert.match(page.body, /Domain=localhost; Path=\/api/);
  assert.equal(answer('GET', '/plant?path=/etc').status, 400);
  assert.equal(answer('GET', '/clear?path=/ws').status, 200);
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
    assert.equal(readFileSync(log, 'utf8'), `GET /go?to=${encodeURIComponent(preview)} document\n`);
  } finally {
    child.kill();
  }
});
