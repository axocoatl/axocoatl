#!/usr/bin/env node
// A fake `codex` for the Podman test of `axocoatl connect codex`
// (tests/connect_codex_podman.rs), baked into a test image at
// /usr/local/bin/codex beside the files it reads in /opt/fake: `mode`, the
// `auth` it writes and the `screen` lines it prints. It acts like
// `codex -c cli_auth_credentials_store="file" login [--device-auth]` in the
// login container:
//   browser  listens on 127.0.0.1:1455 in the container, as Codex does, and
//            prints the sign-in address; a request to /auth/callback is the
//            sign-in
//   device   prints a device code and waits for Enter (the code entered
//            elsewhere)
// Then, by `mode`:
//   ok    writes `auth` to $CODEX_HOME/auth.json (0600) and a log, prints
//         that it signed in, and prints an OpenAI API key and a JWT from
//         `screen`, which must be hidden
//   none  writes no auth.json and exits 1
// It prints one `fake-seen` line with what it ran with (never a token; for
// the proxy variables, whether any of them is set), and
// writes nowhere but $CODEX_HOME.
'use strict';
const fs = require('fs');
const http = require('http');
const path = require('path');

const fake = '/opt/fake';
const mode = fs.readFileSync(path.join(fake, 'mode'), 'utf8').trim();
const args = process.argv.slice(2);
const login = ['-c', 'cli_auth_credentials_store="file"', 'login'];
const same = (a, b) => a.length === b.length && a.every((value, i) => value === b[i]);
let device;
if (same(args, login)) {
  device = false;
} else if (same(args, [...login, '--device-auth'])) {
  device = true;
} else {
  console.error('fake codex: unexpected arguments: ' + args.join(' '));
  process.exit(64);
}
const home = process.env.CODEX_HOME || '';
let homeMode = 'none';
try {
  homeMode = (fs.statSync(home).mode & 0o777).toString(8);
} catch (_) {}
let rootWritable = 'no';
try {
  fs.writeFileSync('/fake-root-probe', 'x');
  rootWritable = 'yes';
} catch (_) {}
const unset = (name) => (process.env[name] === undefined ? 'unset' : 'set');
const proxies = ['http_proxy', 'https_proxy', 'ftp_proxy', 'no_proxy'].flatMap((name) => [
  name,
  name.toUpperCase(),
]);
const proxy = proxies.some((name) => process.env[name] !== undefined) ? 'set' : 'unset';
console.log(
  `fake-seen device=${device ? 'yes' : 'no'} codex_home=${home} home_mode=${homeMode} ` +
    `uid=${process.getuid()} root_writable=${rootWritable} ` +
    `OPENAI_API_KEY=${unset('OPENAI_API_KEY')} CODEX_API_KEY=${unset('CODEX_API_KEY')} ` +
    `proxy=${proxy}`
);

function finish() {
  if (mode === 'none') {
    console.log('Error logging in: the sign-in was cancelled');
    process.exit(1);
  }
  fs.writeFileSync(path.join(home, 'auth.json'), fs.readFileSync(path.join(fake, 'auth')), {
    mode: 0o600,
  });
  fs.mkdirSync(path.join(home, 'log'), { recursive: true });
  fs.writeFileSync(path.join(home, 'log', 'codex-login.log'), 'login finished\n');
  const screen = fs.readFileSync(path.join(fake, 'screen'), 'utf8').split('\n');
  console.log('\x1b[32mSuccessfully logged in\x1b[0m');
  console.log('debug: key ' + screen[0]);
  console.log('debug: token ' + screen[1]);
  process.exit(0);
}

if (device) {
  console.log('\nWelcome to Codex [fake]\n');
  console.log('1. Open this link in your browser and sign in to your account');
  console.log('   https://auth.openai.com/codex/device\n');
  console.log('2. Enter this one-time code (expires in 15 minutes)');
  console.log('   \x1b[1mWXYZ-1234\x1b[0m\n');
  process.stdin.once('data', finish);
} else {
  const server = http.createServer((request, response) => {
    const url = new URL(request.url, 'http://localhost');
    if (url.pathname !== '/auth/callback') {
      response.writeHead(404);
      response.end();
      return;
    }
    console.log(
      `fake-callback host=${request.headers.host} code=${url.searchParams.get('code')}`
    );
    response.writeHead(200, { 'content-type': 'text/plain', connection: 'close' });
    response.end('Signed in to fake Codex. You may close this page.', () => {
      server.close();
      finish();
    });
  });
  server.on('error', (error) => {
    console.error('Port 1455 is already in use: ' + error.message);
    process.exit(1);
  });
  server.listen(1455, '127.0.0.1', () => {
    console.log('Starting local login server on http://localhost:1455.');
    console.log('If your browser did not open, navigate to this URL to authenticate:\n');
    console.log(
      'https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_fake' +
        '&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback' +
        '&state=Qsk-pLkJhGfDsAeyJ0x&originator=codex_cli_rs\n'
    );
  });
}
