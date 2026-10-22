import assert from 'node:assert/strict';
import { createServer } from 'node:http';

// A loopback server that records the Cookie header of each request it gets.
// Cookies ignore ports, so what reaches http://<host>:<its port>/ is what the
// browser sends to <host> on the daemon's port too: the workbench at
// localhost, or a Preview origin. It shows what the browser sends, before any
// Preview proxy sees it (and a WebKit route handler is never shown the Cookie
// header). Node reads header bytes as Latin-1, so a byte outside ASCII comes
// back as the character with that code. It answers every path with a small
// page, so `http://<name>.localhost:<its port>/` is also a page on a
// `*.localhost` address that Axocoatl does not serve.
export async function startCookieEcho() {
  const requests = [];
  const handler = (request, response) => {
    requests.push({ path: request.url, cookie: request.headers.cookie || '' });
    response.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
    response.end('<!doctype html><title>cookie echo</title>');
  };
  const listen = (server, port, host) => new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, host, resolve);
  });
  // A flood of cookies is larger than Node's default 16 KB header limit.
  const options = { maxHeaderSize: 1024 * 1024 };
  const servers = [createServer(options, handler)];
  await listen(servers[0], 0, '127.0.0.1');
  const { port } = servers[0].address();
  // localhost and <name>.localhost may resolve to [::1] first.
  const ipv6 = createServer(options, handler);
  try { await listen(ipv6, port, '::1'); servers.push(ipv6); } catch {}
  let next = 0;
  return {
    port,
    /** The Cookie header a top-level navigation to `host` carries. */
    async cookieAt(context, host) {
      next += 1;
      const pathname = `/cookie-echo/${next}`;
      const page = await context.newPage();
      try {
        await page.goto(`http://${host}:${port}${pathname}`);
      } finally {
        await page.close();
      }
      const seen = requests.find((request) => request.path === pathname);
      assert.ok(seen, `the cookie echo at ${host} received no request`);
      return seen.cookie;
    },
    close: () => Promise.all(servers.map((server) => new Promise((resolve) => {
      server.closeAllConnections();
      server.close(resolve);
    }))),
  };
}

/** The names in a Cookie header, for failure messages that must not print values. */
export function cookieNames(header) {
  return header.split(/;\s*/).filter(Boolean).map((pair) => pair.split('=')[0]).join(', ');
}
