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
//                    (/api, /ws or /), 51 cookies of 4,000 bytes and then 9
//                    from 2,048 down to 8 bytes, as signed-out.test.mjs does:
//                    the Domain=localhost cookie flood the docs describe
//   /clear?path=P    expires the same cookies again
//   /count?path=P    sets nothing
// Each of these pages then asks its own server how many of the 60 cookies
// Safari sends back with Path=P (P/gate-cookie-count, which counts the
// ones in its Cookie header) and shows the number: the cookies Safari kept
// and sends, not the ones the script tried to set.
//
// It binds only the loopback addresses, sends nothing anywhere, and writes
// each request (method, path, Sec-Fetch-Dest) to --log, and after a count
// ` planted=N` (the gate reads it).
//
// Usage: node safari-pages.mjs --port-file FILE [--port N] [--log FILE]
// Node 18 or later, no dependencies.

import { appendFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** A Preview address, as the daemon names them (routes.rs, preview hosts). */
export const PREVIEW_URL = /^http:\/\/[a-z0-9][a-z0-9-]*-p[1-9][0-9]{0,4}\.localhost(:[1-9][0-9]{0,4})?\/[^\s]*$/;

/** Where a page asks how many planted cookies Safari sends with Path=P. */
export function countPath(cookiePath) {
  return `${cookiePath === '/' ? '' : cookiePath}/gate-cookie-count`;
}

/** How many of the planted cookies a Cookie header holds. */
export function plantedIn(cookieHeader = '') {
  const names = cookieHeader.split(';').map((pair) => pair.trim().split('=')[0]);
  const fill = names.filter((name) => /^fill-\d+$/.test(name)).length;
  const fit = names.filter((name) => /^fit-\d+$/.test(name)).length;
  return { planted: fill + fit, fill, fit, bytes: cookieHeader.length };
}

/** The script that sets (`plant`) or removes (`clear`) the cookie flood,
 *  or neither (`count`), and then shows how many of its cookies Safari
 *  sends back with Path=`cookiePath`. */
export function plantScript(cookiePath, mode = 'plant') {
  const attributes = `; Domain=localhost; Path=${cookiePath}; Max-Age=${mode === 'clear' ? 0 : 3600}`;
  const lead = {
    plant: `Set 60 cookies for localhost with Path=${cookiePath}.`,
    clear: `Expired the 60 cookies for localhost with Path=${cookiePath}.`,
    count: `This page sets no cookie; it counts the 60 for localhost with Path=${cookiePath}.`,
  }[mode];
  return `(async () => {
  const attributes = ${JSON.stringify(attributes)};
  if (${JSON.stringify(mode)} !== 'count') {
    for (let i = 0; i < 51; i += 1) document.cookie = 'fill-' + i + '=' + 'x'.repeat(4000) + attributes;
    [2048, 1024, 512, 256, 128, 64, 32, 16, 8].forEach((size, i) => { document.cookie = 'fit-' + i + '=' + 'y'.repeat(size) + attributes; });
  }
  const said = document.getElementById('done');
  const lead = ${JSON.stringify(lead)};
  try {
    const count = await (await fetch(${JSON.stringify(countPath(cookiePath))}, { cache: 'no-store' })).json();
    said.textContent = lead + " Safari sends " + count.planted + " of them back to this page's server (" + count.fill + " of the 51 large, " + count.fit + " of the 9 small).";
  } catch (error) {
    said.textContent = lead + " Counting what Safari sends back failed: " + error.message;
  }
})();`;
}

function page(title, body, script = '') {
  return `<!doctype html><meta charset="utf-8"><title>${title}</title><h1>${title}</h1>${body}${script ? `<script>${script}</script>` : ''}\n`;
}

/** The answer to one request: `{ status, headers, body }`, and `planted`
 *  (plantedIn) for a count. */
export function answer(method, target, cookieHeader = '') {
  const url = new URL(target, 'http://gate-pages.localhost');
  const cookiePath = url.searchParams.get('path') ?? '/api';
  if (method !== 'GET') return { status: 405, headers: {}, body: 'GET only\n' };
  if (['/api', '/ws', '/'].some((p) => url.pathname === countPath(p))) {
    const planted = plantedIn(cookieHeader);
    return { status: 200, headers: { 'content-type': 'application/json', 'cache-control': 'no-store' }, body: `${JSON.stringify(planted)}\n`, planted };
  }
  if (url.pathname === '/go') {
    const to = url.searchParams.get('to') ?? '';
    if (!PREVIEW_URL.test(to)) return { status: 400, headers: {}, body: 'to= must be a Preview address\n' };
    return { status: 302, headers: { location: to }, body: '' };
  }
  const mode = { '/plant': 'plant', '/clear': 'clear', '/count': 'count' }[url.pathname];
  if (mode) {
    if (!['/api', '/ws', '/'].includes(cookiePath)) return { status: 400, headers: {}, body: 'path= is /api, /ws or /\n' };
    const title = { plant: 'Gate: cookies planted', clear: 'Gate: cookies expired', count: 'Gate: cookies counted' }[mode];
    return {
      status: 200,
      headers: { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' },
      body: page(title, '<p id="done">…</p>', plantScript(cookiePath, mode)),
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
    const { status, headers, body, planted } = answer(req.method, req.url, req.headers.cookie ?? '');
    if (options.log) {
      appendFileSync(options.log, `${req.method} ${req.url} ${req.headers['sec-fetch-dest'] ?? '-'}${planted ? ` planted=${planted.planted}` : ''}\n`);
    }
    res.writeHead(status, headers);
    res.end(body);
  };
  // WebKit sends a Cookie header of up to about 200 KB (signed-out.test.mjs);
  // Node refuses one over 16 KB by default.
  const serverOptions = { maxHeaderSize: 1024 * 1024 };
  const v4 = createServer(serverOptions, handler);
  await new Promise((resolve, reject) => v4.once('error', reject).listen(Number(options.port ?? 0), '127.0.0.1', resolve));
  const { port } = v4.address();
  // Safari may try ::1 first for a *.localhost name; the same port there.
  const v6 = createServer(serverOptions, handler);
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
