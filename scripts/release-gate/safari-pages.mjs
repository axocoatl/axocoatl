#!/usr/bin/env node
// The pages the macOS gate's Safari steps open (docs/RELEASE_GATE.md, Tier
// C, the workbench path): a page on another `*.localhost` address than the
// workbench and its Previews, as a development server the person runs would
// be. newuser-mac.sh starts it and the person opens its pages in Safari:
//
//   /go?to=URL       a 302 redirect to URL, which must be a Preview address
//                    (http://<session>-p<port>.localhost:<port>/...): the
//                    redirect from another site that Safari must not follow
//                    to a Preview running top-level
//   /plant?path=P    sets, from the page's script, for localhost with Path=P
//                    (/api or /ws), 51 cookies of 4,000 bytes and then 9 from
//                    2,048 down to 8 bytes, as signed-out.test.mjs does: the
//                    Domain=localhost cookie flood the docs describe
//   /clear?path=P    expires the same cookies again
//
// It binds only the loopback addresses, sends nothing anywhere, and writes
// each request (method, path, Sec-Fetch-Dest) to --log.
//
// Usage: node safari-pages.mjs --port-file FILE [--port N] [--log FILE]
// Node 18 or later, no dependencies.

import { appendFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** A Preview address, as the daemon names them (routes.rs, preview hosts). */
export const PREVIEW_URL = /^http:\/\/[a-z0-9][a-z0-9-]*-p[1-9][0-9]{0,4}\.localhost(:[1-9][0-9]{0,4})?\/[^\s]*$/;

/** The script that sets (or, with `expire`, removes) the cookie flood. */
export function plantScript(cookiePath, expire = false) {
  const attributes = `; Domain=localhost; Path=${cookiePath}; Max-Age=${expire ? 0 : 3600}`;
  return `(() => {
  const attributes = ${JSON.stringify(attributes)};
  let n = 0;
  for (let i = 0; i < 51; i += 1) { document.cookie = 'fill-' + i + '=' + 'x'.repeat(4000) + attributes; n += 1; }
  [2048, 1024, 512, 256, 128, 64, 32, 16, 8].forEach((size, i) => { document.cookie = 'fit-' + i + '=' + 'y'.repeat(size) + attributes; n += 1; });
  document.getElementById('done').textContent = n + ' cookies ${expire ? 'expired' : 'set'} for localhost with Path=${cookiePath}';
})();`;
}

function page(title, body, script = '') {
  return `<!doctype html><meta charset="utf-8"><title>${title}</title><h1>${title}</h1>${body}${script ? `<script>${script}</script>` : ''}\n`;
}

/** The answer to one request: `{ status, headers, body }`. */
export function answer(method, target) {
  const url = new URL(target, 'http://gate-pages.localhost');
  const cookiePath = url.searchParams.get('path') ?? '/api';
  if (method !== 'GET') return { status: 405, headers: {}, body: 'GET only\n' };
  if (url.pathname === '/go') {
    const to = url.searchParams.get('to') ?? '';
    if (!PREVIEW_URL.test(to)) return { status: 400, headers: {}, body: 'to= must be a Preview address\n' };
    return { status: 302, headers: { location: to }, body: '' };
  }
  if (url.pathname === '/plant' || url.pathname === '/clear') {
    if (!['/api', '/ws', '/'].includes(cookiePath)) return { status: 400, headers: {}, body: 'path= is /api, /ws or /\n' };
    const expire = url.pathname === '/clear';
    return {
      status: 200,
      headers: { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' },
      body: page(expire ? 'Gate: cookies expired' : 'Gate: cookies planted', '<p id="done">…</p>', plantScript(cookiePath, expire)),
    };
  }
  return { status: 404, headers: {}, body: 'not found\n' };
}

function parse(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 2) {
    const [flag, value] = [argv[index], argv[index + 1]];
    if (!['--port', '--port-file', '--log'].includes(flag) || value === undefined) throw new Error(`unexpected argument ${flag}`);
    options[flag.slice(2).replace('-file', 'File')] = value;
  }
  if (!options.portFile) throw new Error('--port-file is required');
  return options;
}

async function main() {
  const options = parse(process.argv.slice(2));
  const handler = (req, res) => {
    if (options.log) appendFileSync(options.log, `${req.method} ${req.url} ${req.headers['sec-fetch-dest'] ?? '-'}\n`);
    const { status, headers, body } = answer(req.method, req.url);
    res.writeHead(status, headers);
    res.end(body);
  };
  const v4 = createServer(handler);
  await new Promise((resolve, reject) => v4.once('error', reject).listen(Number(options.port ?? 0), '127.0.0.1', resolve));
  const { port } = v4.address();
  // Safari may try ::1 first for a *.localhost name; the same port there.
  const v6 = createServer(handler);
  await new Promise((resolve) => v6.once('error', resolve).listen(port, '::1', resolve));
  writeFileSync(options.portFile, `${port}\n`);
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => { v4.close(); v6.close(); process.exit(0); });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(`safari-pages: ${error.message}`);
    process.exit(2);
  });
}
