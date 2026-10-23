#!/usr/bin/env node
// safari-workbench.mjs: drive the Axocoatl workbench through the release
// gate's Safari workbench path (newuser-mac.sh --workbench-driver, which
// runs it; docs/RELEASE_GATE.md) and print, for each of its eight
// questions, what the workbench showed:
//
//   1. Did the link open the workbench in Safari, signed in?
//   2. Choose Open workspace… with your home folder. Was it refused?
//   3. Now open <repo> as the workspace. Was it accepted?
//   4. Create a New session (with the Preview port in Exposed ports). Did it
//      start?
//   5. Open Team & budget. Were the limits pre-filled, and did Apply succeed
//      without editing them?
//   6. Send one turn asking to fix the paging bug in src/paginate.js. Did the
//      turn finish and change the file?
//   7. Open Source Control. Does it show 'Last turn N' (N of at least 1: the
//      writer may also add or change a test) listing src/paginate.js?
//   8. Commit the change from Source Control. Did it succeed?
//
// Usage:
//   node safari-workbench.mjs --backend webdriver|playwright --link URL|- \
//     --home DIR --repo DIR --out DIR [--port 4444]
//     [--turn-timeout-min 15] [--env-timeout-min 15] [--prompt TEXT]
//     [--preview-port N] [--no-approve-setup]
//
// webdriver: real Safari through `safaridriver -p PORT` (W3C WebDriver HTTP
//   API). Safari's "Allow Remote Automation" (Settings > Developer) must be on.
// playwright: Playwright's WebKit, for testing this tool. Set
//   SAFARI_WORKBENCH_PLAYWRIGHT to the absolute path of a `playwright`
//   package directory (…/node_modules/playwright).
//
// Each step runs in-page JavaScript that walks the components' shadow roots,
// clicks and types as a person would, and returns JSON of what it saw; waits
// poll it. Every step saves OUT/step-N.png. New session approves the setup
// command the workbench suggests (for node-deps, `npm ci`), as a new user
// must for the Session to become ready, unless --no-approve-setup, and puts
// --preview-port in Exposed ports. The sign-in link, and the token in it,
// is only navigated to: it is never printed or written to a file; with
// `--link -` it is read from standard input, so it is not on a command line
// either. The output starts with `PROMPT: <the turn's prompt>`, and has one
// `STEP N: PASS|FAIL <what the workbench showed>` line per question, which
// the gate records as that question's answer.
// Exit status: 0 when all eight steps pass, 1 when any fails, 2 on bad usage.
// Node 22, no dependencies.

import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { mkdir, readdir, readFile, realpath, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import path from 'node:path';

const USAGE = 'usage: node safari-workbench.mjs --backend webdriver|playwright --link URL|- --home DIR --repo DIR --out DIR [--port 4444] [--turn-timeout-min 15] [--env-timeout-min 15] [--prompt TEXT] [--preview-port N] [--no-approve-setup]';
const DEFAULT_PROMPT = 'Fix the paging bug in src/paginate.js.';
const TARGET_FILE = 'src/paginate.js';
const COMMIT_MESSAGE = 'Fix the paging bug in src/paginate.js';
const FIXTURE_AUTHOR = 'New User <new-user@example.invalid>';
const VIEWPORT = { width: 1440, height: 900 };

// ── Output: the sign-in link and its token are never shown ──────────────

const secrets = [];
function redact(value) {
  let text = String(value);
  for (const secret of secrets) if (secret) text = text.split(secret).join('<redacted>');
  return text.replace(/([?&](?:token|access_token)=)[^&\s"'<>]+/gi, '$1<redacted>');
}
const say = (line = '') => process.stdout.write(`${redact(line)}\n`);
const sayErr = (line) => process.stderr.write(`${redact(line)}\n`);
const short = (value, max = 220) => {
  const text = String(value ?? '').replace(/\s+/g, ' ').trim();
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
};
// The start and the end of a long text (a dialog's question is at its end).
const clip = (value, head = 150, tail = 130) => {
  const text = String(value ?? '').replace(/\s+/g, ' ').trim();
  return text.length > head + tail + 3 ? `${text.slice(0, head)} … ${text.slice(-tail)}` : text;
};
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const elapsed = (start) => {
  const seconds = Math.round((Date.now() - start) / 1000);
  return seconds >= 60 ? `${Math.floor(seconds / 60)}m${String(seconds % 60).padStart(2, '0')}s` : `${seconds}s`;
};

function parseArgs(argv) {
  const options = { port: 4444, turnTimeoutMin: 15, envTimeoutMin: 15, prompt: DEFAULT_PROMPT, approveSetup: true, previewPort: '' };
  const takes = new Map([
    ['--backend', 'backend'], ['--link', 'link'], ['--home', 'home'], ['--repo', 'repo'], ['--out', 'out'],
    ['--port', 'port'], ['--turn-timeout-min', 'turnTimeoutMin'], ['--env-timeout-min', 'envTimeoutMin'],
    ['--prompt', 'prompt'], ['--preview-port', 'previewPort'],
  ]);
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--help' || arg === '-h') { say(USAGE); process.exit(0); }
    if (arg === '--no-approve-setup') { options.approveSetup = false; continue; }
    const equals = arg.indexOf('=');
    const flag = arg.startsWith('--') && equals > 0 ? arg.slice(0, equals) : arg;
    const inline = arg.startsWith('--') && equals > 0 ? arg.slice(equals + 1) : undefined;
    const key = takes.get(flag);
    if (!key) throw new Error(`unknown argument ${flag.startsWith('-') ? flag : '(a value without a flag)'}`);
    const value = inline ?? argv[++index];
    if (value === undefined) throw new Error(`${flag} needs a value`);
    options[key] = value;
  }
  for (const key of ['backend', 'link', 'home', 'repo', 'out']) {
    if (!options[key]) throw new Error(`--${key} is required`);
  }
  if (!['webdriver', 'playwright'].includes(options.backend)) throw new Error('--backend must be webdriver or playwright');
  for (const key of ['port', 'turnTimeoutMin', 'envTimeoutMin']) {
    options[key] = Number(options[key]);
    if (!Number.isFinite(options[key]) || options[key] <= 0) throw new Error(`invalid number for ${key}`);
  }
  if (options.previewPort !== '' && !/^[1-9][0-9]{0,4}$/.test(String(options.previewPort))) throw new Error('--preview-port must be a port number');
  if (options.link === '-') options.link = readFileSync(0, 'utf8').trim();
  let link;
  try { link = new URL(options.link); } catch { throw new Error('--link is not a URL (its value is not shown)'); }
  if (!/^https?:$/.test(link.protocol)) throw new Error('--link must be an http(s) URL (its value is not shown)');
  secrets.push(options.link);
  for (const [, value] of link.searchParams) if (value.length >= 8) secrets.push(value, encodeURIComponent(value));
  options.origin = link.origin;
  return options;
}

function run(command, args, options = {}) {
  return new Promise((resolve) => {
    execFile(command, args, { timeout: 60_000, maxBuffer: 8 * 1024 * 1024, ...options }, (error, stdout, stderr) => {
      resolve({ code: error ? (typeof error.code === 'number' ? error.code : 1) : 0, stdout: String(stdout || ''), stderr: String(stderr || error?.message || '') });
    });
  });
}
const git = (repo, ...args) => run('git', ['-C', repo, ...args]);
const sha = (text) => createHash('sha256').update(text ?? '').digest('hex').slice(0, 12);
async function readText(file) { try { return await readFile(file, 'utf8'); } catch { return null; } }

// ── In-page helpers, prepended to every snippet ─────────────────────────
// Plain ES2017 so any current Safari runs them; `args` holds the arguments.

const LIB = String.raw`
var __sw = window.__sw || (window.__sw = { notices: [], toasts: [] });
function roots() {
  var out = [document];
  for (var i = 0; i < out.length; i += 1) {
    var nodes = out[i].querySelectorAll('*');
    for (var j = 0; j < nodes.length; j += 1) if (nodes[j].shadowRoot) out.push(nodes[j].shadowRoot);
  }
  return out;
}
function all(selector) {
  var found = [];
  roots().forEach(function (root) { Array.prototype.push.apply(found, root.querySelectorAll(selector)); });
  return found;
}
function shadow(host) {
  var element = typeof host === 'string' ? document.querySelector(host) : host;
  return element && element.shadowRoot ? element.shadowRoot : null;
}
function inHost(host, selector) { var root = shadow(host); return root ? root.querySelector(selector) : null; }
function vis(element) {
  if (!element || !element.isConnected) return false;
  var rect = element.getBoundingClientRect();
  if (rect.width === 0 && rect.height === 0) return false;
  var style = getComputedStyle(element);
  return style.visibility !== 'hidden' && style.display !== 'none';
}
function text(element, max) {
  if (!element) return '';
  var value = element.innerText;
  if (typeof value !== 'string' || (!value && element.textContent)) value = element.textContent || '';
  value = value.replace(/\s+/g, ' ').trim();
  return value.length > (max || 300) ? value.slice(0, (max || 300) - 1) + '…' : value;
}
function clickEl(element) {
  if (element.scrollIntoView) element.scrollIntoView({ block: 'center', inline: 'nearest' });
  element.click();
}
function setVal(element, value) {
  element.focus();
  var proto = element instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, 'value').set.call(element, value);
  element.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
  element.dispatchEvent(new Event('change', { bubbles: true, composed: true }));
}
function since(list, at) { return list.filter(function (item) { return item.at >= at; }); }
async function apiGet(pathname) {
  var response = await fetch(pathname, { credentials: 'same-origin', headers: { accept: 'application/json' } });
  var body = await response.text(), json = null;
  try { json = JSON.parse(body); } catch (error) {}
  return { status: response.status, json: json, text: json === null ? body.slice(0, 300) : '' };
}
// ax-session-home's folder / New session picker, as the person sees it.
function pickerState() {
  var root = shadow('ax-session-home');
  var modal = root && root.querySelector('.picker-host .modal');
  if (!modal || !vis(modal)) return { open: false };
  var pathEl = modal.querySelector('.picker-path');
  var first = pathEl && pathEl.firstChild;
  var shownPath = first && first.nodeType === 3 ? first.textContent : text(pathEl);
  var state = modal.querySelector('.picker-list .state');
  var scope = modal.querySelector('.picker-scope');
  var use = modal.querySelector('[data-action="picker-use"]');
  var field = function (name) {
    var input = modal.querySelector('[data-field="' + name + '"]');
    return input ? { value: input.type === 'checkbox' ? input.checked : input.value, disabled: input.disabled } : null;
  };
  var image = modal.querySelector('[data-field="image-preset"]');
  return {
    open: true, title: text(modal.querySelector('.modal-head')), path: String(shownPath || '').trim(),
    git: Boolean(pathEl && pathEl.querySelector('.git-badge')),
    rows: Array.prototype.map.call(modal.querySelectorAll('.picker-row'), function (row) {
      return { path: row.getAttribute('data-path') || '', label: text(row, 120), disabled: row.disabled };
    }),
    loading: Boolean(state && /Opening folder/.test(text(state))), state: text(state, 300),
    scope: text(scope, 400), scopeProtected: Boolean(scope && scope.classList.contains('protected')),
    use: use ? { label: text(use), disabled: use.disabled } : null,
    error: text(modal.querySelector('.inline-error:not(.picker-blocked)'), 400),
    blocked: text(modal.querySelector('.picker-blocked'), 400),
    workspaceName: field('workspace-name'), sessionName: field('session-name'),
    setupCommand: field('setup-command'), setupApproved: field('setup-approved'), agent: field('agent'),
    image: image ? (image.value === '__custom__' ? (field('custom-image') || {}).value : image.value) : '',
  };
}
// ax-session-home's confirmation dialog.
function dialogState() {
  var root = shadow('ax-session-home');
  var modal = root && root.querySelector('.dialog-host .modal');
  if (!modal || !vis(modal)) return { open: false };
  var title = text(modal.querySelector('.modal-head'));
  var body = Array.prototype.filter.call(modal.children, function (child) {
    return !child.classList.contains('modal-head') && !child.classList.contains('modal-foot');
  }).map(function (child) { return text(child, 700); }).join(' ');
  return { open: true, title: title, text: body, ok: text(modal.querySelector('[data-action="dialog-ok"]')) };
}
`;

// ── Back ends: one small interface ──────────────────────────────────────
// start() -> description, goto(url), exec(fn, args) runs a synchronous
// snippet, execAsync(fn, args) one that returns a promise, screenshot(file),
// stop(). Snippets return JSON text, so both back ends hand back plain data.

class WebDriverBackend {
  constructor(port) { this.base = `http://127.0.0.1:${port}`; this.id = null; }

  async command(method, pathname, body) {
    let response;
    try {
      response = await fetch(`${this.base}${pathname}`, {
        method,
        headers: body === undefined ? {} : { 'content-type': 'application/json; charset=utf-8' },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch (error) {
      throw new Error(`safaridriver did not answer at ${this.base} (${error.cause?.code || error.message}); start it with \`safaridriver -p ${new URL(this.base).port}\``);
    }
    const raw = await response.text();
    let json;
    try { json = JSON.parse(raw); } catch { json = { value: { error: `HTTP ${response.status}`, message: raw.slice(0, 300) } }; }
    const value = json?.value;
    if (!response.ok || (value && typeof value === 'object' && typeof value.error === 'string')) {
      const where = pathname.replace(/^\/session\/[^/]+/, '/session/…');
      throw new Error(`WebDriver ${method} ${where}: ${value?.error || response.status}: ${short(value?.message, 400)}`);
    }
    return value;
  }

  async start() {
    const created = await this.command('POST', '/session', { capabilities: { alwaysMatch: { browserName: 'safari' } } });
    this.id = created.sessionId;
    const caps = created.capabilities || {};
    await this.command('POST', `/session/${this.id}/timeouts`, { script: 120_000, pageLoad: 120_000, implicit: 0 });
    try { await this.command('POST', `/session/${this.id}/window/rect`, { x: 0, y: 0, ...VIEWPORT }); } catch {}
    return `Safari ${caps.browserVersion || ''} through safaridriver at ${this.base}`.replace('  ', ' ');
  }

  async goto(url) { await this.command('POST', `/session/${this.id}/url`, { url }); }

  async exec(fn, args = []) {
    const script = `var args = Array.prototype.slice.call(arguments);\n${LIB}\nreturn JSON.stringify((${fn}).apply(null, args));`;
    const value = await this.command('POST', `/session/${this.id}/execute/sync`, { script, args });
    return value == null ? null : JSON.parse(value);
  }

  async execAsync(fn, args = []) {
    const script = `var done = arguments[arguments.length - 1];
var args = Array.prototype.slice.call(arguments, 0, -1);
${LIB}
Promise.resolve().then(function () { return (${fn}).apply(null, args); }).then(
  function (value) { done(JSON.stringify({ ok: true, value: value === undefined ? null : value })); },
  function (error) { done(JSON.stringify({ ok: false, error: String((error && error.message) || error) })); });`;
    const result = JSON.parse(await this.command('POST', `/session/${this.id}/execute/async`, { script, args }));
    if (!result.ok) throw new Error(result.error);
    return result.value;
  }

  async screenshot(file) {
    const base64 = await this.command('GET', `/session/${this.id}/screenshot`);
    await writeFile(file, Buffer.from(base64, 'base64'));
  }

  async stop() {
    if (!this.id) return;
    const id = this.id;
    this.id = null;
    await this.command('DELETE', `/session/${id}`).catch((error) => sayErr(`note: ${error.message}`));
  }
}

class PlaywrightBackend {
  async start() {
    const directory = process.env.SAFARI_WORKBENCH_PLAYWRIGHT;
    if (!directory || !path.isAbsolute(directory)) {
      throw new Error('--backend playwright needs SAFARI_WORKBENCH_PLAYWRIGHT=<absolute path to node_modules/playwright>');
    }
    const { webkit } = createRequire(import.meta.url)(directory);
    this.browser = await webkit.launch({ headless: process.env.SAFARI_WORKBENCH_HEADED !== '1' });
    this.context = await this.browser.newContext({ viewport: VIEWPORT });
    this.page = await this.context.newPage();
    return `Playwright WebKit ${this.browser.version()} (a stand-in for Safari)`;
  }

  async goto(url) { await this.page.goto(url, { waitUntil: 'load', timeout: 120_000 }); }

  // A string is evaluated as an expression, the way WebDriver runs a script.
  async exec(fn, args = []) {
    const value = await this.page.evaluate(`(function () { var args = ${JSON.stringify(args)};\n${LIB}\nreturn JSON.stringify((${fn}).apply(null, args)); })()`);
    return value == null ? null : JSON.parse(value);
  }

  async execAsync(fn, args = []) {
    const value = await this.page.evaluate(`(function () { var args = ${JSON.stringify(args)};\n${LIB}
return Promise.resolve().then(function () { return (${fn}).apply(null, args); }).then(
  function (value) { return JSON.stringify({ ok: true, value: value === undefined ? null : value }); },
  function (error) { return JSON.stringify({ ok: false, error: String((error && error.message) || error) }); }); })()`);
    const result = JSON.parse(value);
    if (!result.ok) throw new Error(result.error);
    return result.value;
  }

  async screenshot(file) { await this.page.screenshot({ path: file }); }

  async stop() { await this.browser?.close().catch(() => {}); }
}

// ── In-page snippets (serialized with toString; they use only LIB) ─────

const IN_PAGE = {
  // The shell is up and connected, or a signed-out message shows.
  app: function () {
    var rail = document.querySelector('ax-rail');
    var signedOut = all('[role="alertdialog"]').filter(vis).map(function (e) { return text(e, 200); })
      .filter(function (t) { return /signed out/i.test(t); })[0] || '';
    var live = null;
    try { live = typeof S !== 'undefined' && S ? S.liveConnection : null; } catch (error) {}
    var ready = Boolean(rail && rail.shadowRoot && customElements.get('ax-session-home') && customElements.get('ax-rail'));
    return {
      done: (ready && live === 'connected') || Boolean(signedOut),
      ready: ready, live: live, signedOut: signedOut, title: document.title,
      origin: location.origin, pathname: location.pathname,
      tokenInAddress: /[?&]token=/i.test(location.search),
      page: ready ? '' : text(document.body, 200),
    };
  },
  // Record the workbench's notices (the components' `notify` events) and
  // toasts, so a step can say which ones it caused.
  installWatch: function () {
    if (window.__swWatching) return true;
    window.__swWatching = true;
    document.addEventListener('notify', function (event) {
      var d = event.detail || {};
      __sw.notices.push({ title: String(d.title || ''), body: String(d.body || '').slice(0, 300), kind: String(d.kind || ''), at: Date.now() });
    }, true);
    var host = document.getElementById('toasts');
    if (host) {
      new MutationObserver(function (records) {
        records.forEach(function (record) {
          record.addedNodes.forEach(function (node) {
            if (node.nodeType !== 1) return;
            __sw.toasts.push({ title: text(node.querySelector('.t-title')), body: text(node.querySelector('.t-body'), 300), kind: String(node.className || ''), at: Date.now() });
          });
        });
      }).observe(host, { childList: true });
    }
    return true;
  },
  now: function () { return Date.now(); },
  events: function (at) { return { notices: since(__sw.notices, at), toasts: since(__sw.toasts, at) }; },
  workspaces: async function () {
    var result = await apiGet('/api/workspaces');
    return {
      status: result.status, text: result.text,
      list: Array.isArray(result.json) ? result.json.map(function (w) { return { id: w.id, name: w.name, path: w.canonical_path }; }) : null,
    };
  },
  rail: function () {
    var rail = document.querySelector('ax-rail');
    var add = inHost('ax-rail', '.section-new');
    var sw = inHost('ax-rail', '#switch');
    return {
      workspace: rail ? rail.getAttribute('workspace') || '' : '',
      newSession: add ? { label: add.getAttribute('aria-label') || '', disabled: add.disabled } : null,
      switchLabel: sw ? sw.getAttribute('aria-label') || '' : '',
    };
  },
  railClick: function (selector) {
    var button = inHost('ax-rail', selector);
    if (!button) return { clicked: false, why: 'not found: ax-rail ' + selector };
    if (button.disabled) return { clicked: false, why: 'disabled: ' + (button.getAttribute('aria-label') || text(button)) };
    clickEl(button);
    return { clicked: true, label: button.getAttribute('aria-label') || text(button) };
  },
  picker: function () { return pickerState(); },
  home: function () { return { picker: pickerState(), dialog: dialogState() }; },
  pickerClick: function (selector) {
    var root = shadow('ax-session-home');
    var target = root && root.querySelector('.picker-host .modal ' + selector);
    if (!target) return { clicked: false, why: 'not found: ' + selector };
    if (target.disabled) return { clicked: false, why: 'disabled: ' + text(target) };
    clickEl(target);
    return { clicked: true, label: text(target) };
  },
  pickerRow: function (rowPath) {
    var root = shadow('ax-session-home');
    var rows = root ? root.querySelectorAll('.picker-host .modal .picker-row') : [];
    for (var i = 0; i < rows.length; i += 1) {
      if (rows[i].getAttribute('data-path') !== rowPath) continue;
      if (rows[i].disabled) return { clicked: false, why: 'row disabled' };
      clickEl(rows[i]);
      return { clicked: true, label: text(rows[i]) };
    }
    return { clicked: false, why: 'no row for that folder' };
  },
  pickerField: function (name, value) {
    var input = inHost('ax-session-home', '.picker-host .modal [data-field="' + name + '"]');
    if (!input) return { set: false, why: 'no ' + name + ' field' };
    if (input.disabled) return { set: false, why: name + ' field disabled' };
    setVal(input, value);
    return { set: true, value: input.value };
  },
  pickerApprove: function () {
    var box = inHost('ax-session-home', '.picker-host .modal [data-field="setup-approved"]');
    if (!box) return { checked: null, why: 'no setup approval checkbox' };
    if (box.disabled) return { checked: box.checked, why: 'checkbox disabled' };
    if (!box.checked) box.click();
    return { checked: box.checked };
  },
  dialogClick: function (action) {
    var button = inHost('ax-session-home', '.dialog-host .modal [data-action="' + action + '"]');
    if (!button) return { clicked: false };
    clickEl(button);
    return { clicked: true };
  },
  cockpit: function () {
    var cockpit = document.getElementById('session-cockpit');
    var env = document.getElementById('session-environment');
    var pill = document.getElementById('cockpit-live');
    var send = document.getElementById('session-send');
    var session = null;
    try { session = typeof S !== 'undefined' && S ? S.session : null; } catch (error) {}
    var messages = Array.prototype.slice.call(document.querySelectorAll('#session-msgs .smsg'));
    var last = messages.length ? messages[messages.length - 1] : null;
    return {
      open: Boolean(cockpit && vis(cockpit)),
      sessionId: (session && session.id) || new URL(location.href).searchParams.get('session') || '',
      title: text(document.getElementById('cockpit-title')),
      envState: env ? env.dataset.state || '' : '', envShown: Boolean(env && !env.classList.contains('hide')),
      envTitle: text(document.getElementById('session-environment-title')),
      envDetail: text(document.getElementById('session-environment-detail'), 300),
      pill: pill ? pill.dataset.state || '' : '', pillLabel: text(pill), status: text(document.getElementById('cockpit-status'), 200),
      send: send ? { label: text(send), disabled: send.disabled } : null,
      running: Boolean(session && session.activeTurnId), pending: Boolean(session && session.pendingTurn),
      messages: messages.length, lastRole: last ? text(last.querySelector('.smsg-role'), 60) : '',
      lastText: last ? text(last.querySelector('.smsg-body') || last, 300) : '',
      errors: messages.filter(function (m) { return /^error$/i.test(text(m.querySelector('.smsg-role'))); })
        .map(function (m) { return text(m.querySelector('.smsg-body') || m, 300); }),
    };
  },
  team: function () {
    var root = shadow('ax-session-team');
    if (!root) return { found: false };
    var dialog = root.querySelector('dialog');
    var status = root.querySelector('.status');
    var fields = {};
    Array.prototype.forEach.call(root.querySelectorAll('.panel label'), function (label) {
      var input = label.querySelector('input,select,textarea');
      if (!input || input.type === 'checkbox') return;
      var name = label.firstChild && label.firstChild.nodeType === 3 ? label.firstChild.textContent.trim() : '';
      if (name) fields[name] = input.value;
    });
    var apply = root.querySelector('.apply'), preview = root.querySelector('.preview');
    var mode = root.querySelector('[data-mode][aria-pressed="true"]');
    return {
      found: true, open: Boolean(dialog && dialog.open), status: text(status, 900),
      error: Boolean(status && status.classList.contains('error')), fields: fields,
      mode: mode ? mode.getAttribute('data-mode') : '',
      applyDisabled: apply ? apply.disabled : null, previewDisabled: preview ? preview.disabled : null,
      review: Array.prototype.map.call(root.querySelectorAll('.review li'), function (li) { return text(li, 200); }),
    };
  },
  teamClick: function (selector) {
    var button = inHost('ax-session-team', selector);
    if (!button) return { clicked: false, why: 'not found: ' + selector };
    if (button.disabled) return { clicked: false, why: 'disabled: ' + text(button) };
    clickEl(button);
    return { clicked: true, label: text(button) };
  },
  send: function (prompt) {
    var box = document.getElementById('session-text'), send = document.getElementById('session-send');
    if (!box || !send) return { sent: false, why: 'no composer' };
    setVal(box, prompt);
    var label = text(send);
    if (send.disabled) return { sent: false, why: 'Send is disabled (' + label + ')' };
    clickEl(send);
    return { sent: true, label: label };
  },
  click: function (selector) {
    var target = document.querySelector(selector);
    if (!target) return { clicked: false, why: 'not found: ' + selector };
    if (target.disabled) return { clicked: false, why: 'disabled' };
    clickEl(target);
    return { clicked: true, label: text(target) };
  },
  sourceControl: function () {
    var host = document.getElementById('source-control');
    var root = shadow(host);
    if (!root) return { found: false };
    var scope = root.querySelector('[data-scope][aria-pressed="true"]');
    var commit = root.querySelector('[data-action="commit"]');
    var pane = root.querySelector('.pane');
    return {
      found: true, shown: Boolean(!host.classList.contains('hide') && vis(host)),
      lastTurn: text(root.querySelector('[data-count="lastTurn"]')), all: text(root.querySelector('[data-count="all"]')),
      scope: scope ? scope.getAttribute('aria-label') || text(scope) : '', summary: text(root.querySelector('.summary-count')),
      files: Array.prototype.map.call(root.querySelectorAll('.files .file'), function (file) {
        return { path: file.getAttribute('data-path') || '', text: text(file, 160) };
      }),
      empty: text(root.querySelector('.files .empty') || root.querySelector('.files .state'), 300),
      busy: Boolean(pane && pane.getAttribute('aria-busy') === 'true'),
      identity: text(root.querySelector('.commit-identity'), 300),
      commit: commit ? { label: text(commit), disabled: commit.disabled, title: commit.title || '' } : null,
    };
  },
  scClick: function (selector) {
    var target = inHost('#source-control', selector);
    if (!target) return { clicked: false, why: 'not found: ' + selector };
    if (target.disabled) return { clicked: false, why: 'disabled: ' + (target.title || text(target)) };
    clickEl(target);
    return { clicked: true, label: text(target) };
  },
  scMessage: function (message) {
    var input = inHost('#source-control', '#commit-message');
    if (!input) return { typed: false };
    setVal(input, message);
    return { typed: true };
  },
};
const snippet = (name) => IN_PAGE[name].toString();

// ── The eight steps ─────────────────────────────────────────────────────

class Runner {
  constructor(options, backend) {
    this.options = options;
    this.backend = backend;
    this.results = new Map();
    this.facts = {};
    this.shotTaken = false;
  }

  exec(name, ...args) { return this.backend.exec(snippet(name), args); }
  execAsync(name, ...args) { return this.backend.execAsync(snippet(name), args); }

  // Run snippet `name` until `until(value)` holds or the time is up.
  // `progress(value)` runs after each try; if it throws, the wait stops.
  async poll(name, args, until, { timeoutMs = 30_000, intervalMs = 400, progress } = {}) {
    const deadline = Date.now() + timeoutMs;
    let value = null, lastError = null;
    for (;;) {
      try {
        value = await this.backend.exec(snippet(name), args);
        lastError = null;
        if (until(value)) return { value, ok: true };
      } catch (error) {
        lastError = error;
      }
      if (progress) {
        try { await progress(value); } catch (error) { return { value, ok: false, error }; }
      }
      if (Date.now() >= deadline) return { value, ok: false, error: lastError };
      await delay(intervalMs);
    }
  }

  // Save OUT/step-N.png now (a step's evidence moment), or at the end.
  async shot(step) {
    const file = path.join(this.options.out, `step-${step}.png`);
    try { await this.backend.screenshot(file); this.shotTaken = true; }
    catch (error) { sayErr(`note: no screenshot for step ${step}: ${error.message}`); }
  }

  async events(at) {
    try { return await this.exec('events', at); } catch { return { notices: [], toasts: [] }; }
  }

  async step(number, needs, body) {
    this.shotTaken = false;
    const missing = needs.filter((n) => this.results.get(n) !== 'PASS');
    let pass = false, seen = '';
    if (missing.length) {
      seen = `not attempted: step ${missing.join(', ')} did not pass`;
    } else {
      try { ({ pass, seen } = await body()); }
      catch (error) { pass = false; seen = `${seen}error: ${error.message}`; }
    }
    if (!this.shotTaken) await this.shot(number);
    this.results.set(number, pass ? 'PASS' : 'FAIL');
    say(`STEP ${number}: ${pass ? 'PASS' : 'FAIL'} ${seen}`);
  }

  findWorkspace(list, folder) { return (list || []).find((w) => w.path === folder) || null; }

  // The rail's "Open workspace…", which opens the folder picker.
  async openPicker() {
    const current = await this.exec('picker');
    if (current?.open && /Open workspace/.test(current.title)) return current;
    const click = await this.exec('railClick', '#open-workspace');
    if (!click.clicked) throw new Error(`could not click Open workspace… (${click.why})`);
    const opened = await this.poll('picker', [], (p) => p?.open && !p.loading && p.path, { timeoutMs: 30_000 });
    if (!opened.ok) throw new Error(`the Open workspace picker showed no folder (${short(JSON.stringify(opened.value), 300)})`);
    return opened.value;
  }

  // Walk the picker to `target` through its parent and folder rows.
  async navigatePicker(target) {
    for (let hops = 0; hops < 64; hops += 1) {
      const state = (await this.poll('picker', [], (p) => p?.open && !p.loading, { timeoutMs: 30_000 })).value;
      if (!state?.open) throw new Error('the picker closed');
      if (/could not be opened/i.test(state.state || '')) throw new Error(`the picker could not open a folder: ${state.state}`);
      if (state.path === target) return state;
      const prefix = state.path.endsWith('/') ? state.path : `${state.path}/`;
      let next;
      if (target.startsWith(prefix)) {
        const segment = target.slice(prefix.length).split('/')[0];
        next = state.rows.find((row) => row.path === `${prefix}${segment}`);
        if (!next) throw new Error(`the picker at ${state.path} lists no folder ${segment}`);
      } else {
        next = state.rows.find((row) => /\(parent\)/.test(row.label));
        if (!next) throw new Error(`the picker at ${state.path} has no parent row on the way to ${target}`);
      }
      const click = await this.exec('pickerRow', next.path);
      if (!click.clicked) throw new Error(`could not open ${next.path} in the picker (${click.why})`);
      await this.poll('picker', [], (p) => p?.open && !p.loading && p.path === next.path, { timeoutMs: 30_000 });
    }
    throw new Error(`the picker did not reach ${target}`);
  }

  // 1. The sign-in link opens the workbench, signed in.
  async step1() {
    await this.backend.goto(this.options.link);
    const app = await this.poll('app', [], (a) => a?.done, { timeoutMs: 90_000, intervalMs: 500 });
    const a = app.value || {};
    if (!app.ok || !a.ready) {
      return { pass: false, seen: `the workbench did not load (shell ${a.ready ? 'up' : 'not up'}, live connection ${a.live}, page: "${short(a.page || a.title, 160)}")${app.error ? `; ${app.error.message}` : ''}` };
    }
    await this.exec('installWatch');
    const api = await this.execAsync('workspaces');
    const parts = [`the workbench loaded at ${a.origin}${a.pathname} ("${a.title}")`, `live connection ${a.live}`,
      `GET /api/workspaces from the page: HTTP ${api.status}${Array.isArray(api.list) ? `, ${api.list.length} workspace(s)` : ''}`];
    parts.push(a.tokenInAddress ? 'the address bar still carries the token' : 'no token left in the address');
    if (a.signedOut) parts.push(`signed-out message: "${short(a.signedOut, 120)}"`);
    return { pass: a.live === 'connected' && api.status === 200 && !a.signedOut, seen: parts.join('; ') };
  }

  // 2. Open workspace… on the home folder is refused.
  async step2() {
    const home = this.options.home;
    const before = await this.exec('now');
    const opened = await this.openPicker();
    const parts = [`Open workspace… opened the picker at ${opened.path}${opened.path === home ? ' (the home folder)' : ''}`];
    const atHome = await this.navigatePicker(home);
    if (atHome.scope) parts.push(`it says: "${short(atHome.scope, 200)}"`);
    if (atHome.use?.disabled) {
      const list = await this.execAsync('workspaces');
      parts.push(`"${atHome.use.label}" is disabled for ${home}`);
      return { pass: atHome.scopeProtected && !this.findWorkspace(list.list, home), seen: parts.join('; ') };
    }
    const click = await this.exec('pickerClick', '[data-action="picker-use"]');
    if (!click.clicked) throw new Error(`could not click ${atHome.use?.label || 'Open workspace'} (${click.why})`);
    parts.push(`clicked "${click.label}"`);
    const outcome = await this.poll('home', [], (h) => h?.dialog?.open || !h?.picker?.open || Boolean(h?.picker?.error), { timeoutMs: 20_000 });
    const dialog = outcome.value?.dialog || {};
    let refused = false;
    if (dialog.open) {
      refused = /home folder|type its name/i.test(dialog.text);
      parts.push(`the daemon refused it; dialog "${dialog.title}": "${clip(dialog.text)}"`);
      await this.shot(2);
      const cancel = await this.exec('dialogClick', 'dialog-cancel');
      parts.push(cancel.clicked ? 'chose Cancel' : 'could not click Cancel');
      await this.poll('home', [], (h) => !h?.dialog?.open, { timeoutMs: 10_000 });
    } else {
      const p = outcome.value?.picker || {};
      parts.push(p.open ? `no confirmation; the picker says: "${short(p.error || p.state, 200)}"` : 'no confirmation: the picker closed');
    }
    const list = await this.execAsync('workspaces');
    const homeWorkspace = this.findWorkspace(list.list, home);
    const notices = (await this.events(before)).notices.filter((n) => /Workspace/.test(n.title)).map((n) => n.title);
    parts.push(homeWorkspace ? `the home folder IS now a workspace ("${homeWorkspace.name}")` : 'the home folder is not a workspace (GET /api/workspaces)');
    if (notices.length) parts.push(`notices: ${notices.join(', ')}`);
    return { pass: refused && !homeWorkspace && !notices.includes('Workspace opened'), seen: parts.join('; ') };
  }

  // 3. The repository opens as the workspace, with no confirmation.
  async step3() {
    const repo = this.options.repo;
    const before = await this.exec('now');
    await this.openPicker();
    const there = await this.navigatePicker(repo);
    const parts = [`walked the picker to ${there.path}${there.git ? ' (marked git)' : ''}`, `workspace name "${there.workspaceName?.value ?? ''}"`];
    if (there.scope) parts.push(`it says: "${short(there.scope, 160)}"`);
    const click = await this.exec('pickerClick', '[data-action="picker-use"]');
    if (!click.clicked) return { pass: false, seen: `${parts.join('; ')}; could not click Open workspace (${click.why})` };
    parts.push(`clicked "${click.label}"`);
    const outcome = await this.poll('home', [], (h) => h?.dialog?.open || !h?.picker?.open || Boolean(h?.picker?.error), { timeoutMs: 30_000 });
    const h = outcome.value || {};
    if (h.dialog?.open) {
      parts.push(`asked to confirm: "${clip(h.dialog.text)}"`);
      await this.shot(3);
      await this.exec('dialogClick', 'dialog-cancel');
      return { pass: false, seen: parts.join('; ') };
    }
    if (h.picker?.open) {
      return { pass: false, seen: `${parts.join('; ')}; the picker stayed open: "${short(h.picker.error || h.picker.state || h.picker.use?.label, 240)}"` };
    }
    const list = await this.execAsync('workspaces');
    const workspace = this.findWorkspace(list.list, repo);
    const notice = (await this.events(before)).notices.find((n) => /^Workspace/.test(n.title));
    if (notice) parts.push(`notice "${notice.title}: ${short(notice.body, 80)}"`);
    parts.push(workspace ? `workspace "${workspace.name}" at ${workspace.path} (GET /api/workspaces)` : 'no workspace for the repository in GET /api/workspaces');
    if (workspace) {
      const rail = await this.poll('rail', [], (r) => r?.workspace === workspace.id, { timeoutMs: 10_000 });
      parts.push(rail.ok ? `the rail switched to "${short(rail.value.switchLabel, 80)}"` : 'the rail did not switch to it');
    }
    this.facts.workspace = workspace;
    return { pass: Boolean(workspace), seen: parts.join('; ') };
  }

  // 4. New session starts: its environment becomes ready.
  async step4() {
    const workspace = this.facts.workspace;
    const before = await this.exec('now');
    const rail = await this.poll('rail', [], (r) => r?.newSession && !r.newSession.disabled && r.workspace === workspace.id, { timeoutMs: 20_000 });
    if (!rail.ok) throw new Error(`the rail offers no New session for ${workspace.name} (${short(JSON.stringify(rail.value), 200)})`);
    const click = await this.exec('railClick', '.section-new');
    if (!click.clicked) throw new Error(`could not click New session (${click.why})`);
    const parts = [`clicked "${click.label}"`];
    const ready = await this.poll('picker', [], (p) => p?.open && /New session/.test(p.title) && p.use && !p.use.disabled, { timeoutMs: 90_000 });
    const p = ready.value || {};
    if (!ready.ok) {
      return { pass: false, seen: `${parts.join('; ')}; the New session dialog cannot create (${short(p.blocked || p.error || JSON.stringify(p.use), 300)})` };
    }
    parts.push(`dialog "${p.title}": name "${p.sessionName?.value}", agent ${p.agent?.value || '?'}, image ${p.image || '(default)'}, setup "${p.setupCommand?.value || ''}"`);
    if (this.options.previewPort) {
      const ports = await this.exec('pickerField', 'ports', String(this.options.previewPort));
      if (!ports.set) return { pass: false, seen: `${parts.join('; ')}; could not put ${this.options.previewPort} in Exposed ports (${ports.why})` };
      parts.push(`Exposed ports "${ports.value}"`);
    }
    if (p.setupCommand?.value && !p.setupApproved?.value) {
      if (this.options.approveSetup) {
        const approved = await this.exec('pickerApprove');
        parts.push(approved.checked ? 'ticked "Run this exact command before Ready"' : `could not approve the setup (${approved.why})`);
      } else {
        parts.push('left the setup unapproved (--no-approve-setup)');
      }
    }
    const create = await this.exec('pickerClick', '[data-action="picker-use"]');
    if (!create.clicked) return { pass: false, seen: `${parts.join('; ')}; could not click Create session (${create.why})` };
    parts.push(`clicked "${create.label}"`);
    const started = Date.now();
    const timeoutMs = this.options.envTimeoutMin * 60_000;
    let lastReport = Date.now(), lastLine = '';
    // The dialog stays open ("Preparing environment…") until the daemon has
    // created the Session; then the cockpit opens on it.
    const closed = await this.poll('picker', [], (q) => !q?.open || Boolean(q.error), {
      timeoutMs, intervalMs: 1000,
      progress: (q) => {
        if (Date.now() - lastReport < 30_000) return;
        lastReport = Date.now();
        say(`  ... step 4, ${elapsed(started)}: ${q?.use?.label || 'waiting'}`);
      },
    });
    if (closed.value?.open) {
      return { pass: false, seen: `${parts.join('; ')}; the dialog stayed open after ${elapsed(started)}: "${short(closed.value.error || closed.value.blocked || closed.value.use?.label, 300)}"` };
    }
    // Ready or failed settles it; a state that needs the person (awaiting
    // approval, unprepared) settles it once it has held for 20 s.
    let heldState = '', heldSince = 0;
    const settled = await this.poll('cockpit', [], (c) => {
      if (!c?.open || !c.envState) return false;
      if (c.envState !== heldState) { heldState = c.envState; heldSince = Date.now(); }
      if (c.envState === 'ready') return !c.envShown;
      if (c.envState === 'failed') return true;
      return ['awaiting_approval', 'unprepared'].includes(c.envState) && Date.now() - heldSince > 20_000;
    }, {
      timeoutMs: Math.max(60_000, timeoutMs - (Date.now() - started)), intervalMs: 1000,
      progress: (c) => {
        const line = `environment ${c?.envState || '?'}${c?.envTitle && c?.envShown ? ` ("${short(c.envTitle, 80)}")` : ''}`;
        if (line === lastLine && Date.now() - lastReport < 30_000) return;
        lastLine = line; lastReport = Date.now();
        say(`  ... step 4, ${elapsed(started)}: ${line}`);
      },
    });
    const c = settled.value || {};
    this.facts.sessionId = c.sessionId;
    const notice = (await this.events(before)).notices.find((n) => /Session created|Environment|Setup decision/.test(n.title));
    parts.push(`the cockpit shows "${c.title}" (Session ${c.sessionId || '?'}), environment ${c.envState || '?'} after ${elapsed(started)}`);
    if (c.envState !== 'ready' && c.envTitle) parts.push(`banner "${short(c.envTitle, 80)}: ${short(c.envDetail, 200)}"`);
    if (notice) parts.push(`notice "${notice.title}: ${short(notice.body, 100)}"`);
    return { pass: Boolean(c.open && c.envState === 'ready' && c.sessionId), seen: parts.join('; ') };
  }

  // 5. Team & budget: the suggested limits are filled in; Apply works as is.
  async step5() {
    const opened = await this.exec('teamClick', '.open');
    if (!opened.clicked) throw new Error(`could not click Team and budget (${opened.why})`);
    const parts = [`clicked "${opened.label}"`];
    const loaded = await this.poll('team', [], (t) => t?.open && t.status && !/^Loading/.test(t.status), { timeoutMs: 30_000 });
    const t = loaded.value || {};
    if (!loaded.ok) return { pass: false, seen: `${parts.join('; ')}; Team & budget did not load ("${short(t.status, 200)}")` };
    const keys = ['Activation limit', 'Provider and tool invocation limit', 'Total token limit', 'Cost limit (USD)', 'Budget expires (your local time)'];
    const values = keys.map((key) => [key, t.fields?.[key] ?? '']);
    const prefilled = values.every(([, value]) => String(value).trim() !== '');
    const suggested = /Suggested limits are filled in/.test(t.status);
    parts.push(`it says: "${short(t.status, 260)}"`);
    parts.push(`${t.fields?.Name || 'Agent'} (${t.fields?.Model || '?'}): ${values.map(([k, v]) => `${k}=${v === '' ? '(empty)' : v}`).join(', ')}`);
    const edit = await this.exec('teamClick', '[data-mode="edit"]');
    if (!edit.clicked) return { pass: false, seen: `${parts.join('; ')}; could not choose Edit (${edit.why})` };
    const previewReady = await this.poll('team', [], (x) => x?.previewDisabled === false, { timeoutMs: 10_000 });
    if (!previewReady.ok) return { pass: false, seen: `${parts.join('; ')}; Preview changes stayed disabled ("${short(previewReady.value?.status, 200)}")` };
    await this.exec('teamClick', '.preview');
    const reviewed = await this.poll('team', [], (x) => x && !/^Validating/.test(x.status) && (x.applyDisabled === false || x.error), { timeoutMs: 60_000 });
    const r = reviewed.value || {};
    if (!reviewed.ok || r.error || r.applyDisabled) {
      return { pass: false, seen: `${parts.join('; ')}; chose Edit and Preview changes, which did not allow Apply: "${short(r.status, 400)}"` };
    }
    parts.push(`chose Edit (no field changed) and Preview changes: "${short(r.status, 100)}"${r.review?.length ? ` (${short(r.review.join(' | '), 200)})` : ''}`);
    await this.exec('teamClick', '.apply');
    const applied = await this.poll('team', [], (x) => x && (x.error || /Saved Session configuration \d+/.test(x.status)), { timeoutMs: 60_000 });
    const a = applied.value || {};
    const saved = applied.ok && !a.error && /Saved Session configuration \d+/.test(a.status);
    parts.push(`after Apply to this Session: "${short(a.status, 200)}"`);
    if (saved) {
      await this.shot(5);
      await this.exec('teamClick', '.close');
      const send = await this.poll('cockpit', [], (c) => c?.send?.label === 'Send', { timeoutMs: 10_000 });
      parts.push(`closed it; the composer button reads "${send.value?.send?.label}"`);
    }
    return { pass: prefilled && suggested && saved, seen: parts.join('; ') };
  }

  // 6. One turn fixes src/paginate.js.
  async step6() {
    const file = path.join(this.options.repo, TARGET_FILE);
    const beforeText = await readText(file);
    const before = await this.exec('now');
    const initial = await this.exec('cockpit');
    const sent = await this.exec('send', this.options.prompt);
    if (!sent.sent) return { pass: false, seen: `could not send (${sent.why})` };
    const parts = [`typed "${short(this.options.prompt, 80)}" and clicked "${sent.label}"`];
    const started = Date.now();
    let lastReport = 0, lastLine = '', sawRun = false;
    const finished = (c) => ['done', 'error', 'Needs attention', 'stopped'].includes(c?.pill) || c?.pillLabel === 'stopped';
    const result = await this.poll('cockpit', [], (c) => {
      if (!c) return false;
      if (c.running || c.pending || ['running', 'starting', 'stopping', 'reconnecting'].includes(c.pill)) sawRun = true;
      return finished(c) && !c.running && !c.pending && (sawRun || c.pill !== initial.pill || c.messages > initial.messages);
    }, {
      timeoutMs: this.options.turnTimeoutMin * 60_000, intervalMs: 1000,
      progress: async (c) => {
        const line = `${c?.pillLabel || '?'}${c?.status ? ` · ${short(c.status, 100)}` : ''}`;
        if (line !== lastLine || Date.now() - lastReport >= 30_000) {
          lastLine = line; lastReport = Date.now();
          say(`  ... step 6, ${elapsed(started)}: ${line}`);
        }
        if (!sawRun && Date.now() - started > 15_000) {
          const team = await this.exec('team').catch(() => null);
          if (team?.open) throw new Error('Send opened Team & budget instead of starting a turn');
        }
      },
    });
    const c = result.value || (await this.exec('cockpit'));
    const afterText = await readText(file);
    const changed = beforeText !== null && afterText !== null && afterText !== beforeText;
    parts.push(`the turn ended "${c.pillLabel}" after ${elapsed(started)}${c.status ? ` (${short(c.status, 120)})` : ''}`);
    if (!result.ok) parts.push(result.error ? `stopped waiting: ${result.error.message}` : `not finished after ${this.options.turnTimeoutMin} min`);
    if (c.errors?.length) parts.push(`error in the conversation: "${short(c.errors.at(-1), 200)}"`);
    if (c.lastText) parts.push(`last message (${c.lastRole || '?'}): "${short(c.lastText, 160)}"`);
    const warnings = (await this.events(before)).toasts.filter((x) => /warn|err/.test(x.kind)).map((x) => `${x.title}: ${short(x.body, 100)}`);
    if (warnings.length) parts.push(`toasts: ${short(warnings.join(' | '), 240)}`);
    if (changed) {
      const diff = await git(this.options.repo, 'diff', '--no-color', '-U0', '--', TARGET_FILE);
      const lines = diff.stdout.split('\n').filter((l) => /^[-+](?![-+]{2})/.test(l)).slice(0, 4).map((l) => short(l, 90));
      parts.push(`${TARGET_FILE} changed on disk (sha256 ${sha(beforeText)} -> ${sha(afterText)})${lines.length ? `: ${lines.join(' / ')}` : ''}`);
    } else {
      parts.push(`${TARGET_FILE} is unchanged on disk`);
    }
    return { pass: result.ok && c.pill === 'done' && changed, seen: parts.join('; ') };
  }

  // 7. Source Control shows Last turn N (at least 1: a writer that also adds
  // or changes a test changed two files), src/paginate.js among them.
  async step7() {
    const parts = [];
    const files = await this.exec('click', '[data-session-view="files"]');
    if (!files.clicked) parts.push(`could not open Files (${files.why})`);
    const tab = await this.exec('click', '#explorer-tabs .etab[data-etab="git"]');
    if (!tab.clicked) throw new Error(`could not click the Source Control tab (${tab.why})`);
    parts.push(`opened ${files.clicked ? 'Files, then ' : ''}"${short(tab.label, 40)}"`);
    let refreshed = false;
    const started = Date.now();
    const shown = await this.poll('sourceControl', [], (s) => s?.shown && !s.busy && Number(s.lastTurn) > 0 && s.files.length > 0, {
      timeoutMs: 60_000, intervalMs: 700,
      progress: async (s) => {
        if (refreshed || !s?.shown || Date.now() - started < 15_000) return;
        refreshed = true;
        await this.exec('scClick', '[data-action="refresh"]');
      },
    });
    const s = shown.value || {};
    const listed = (s.files || []).map((f) => f.path);
    parts.push(`scope "${s.scope}"`, `Last turn ${s.lastTurn}, All changes ${s.all}`, `"${s.summary}"`);
    parts.push(listed.length ? `listed: ${(s.files || []).map((f) => short(f.text, 80)).join(' | ')}` : `nothing listed${s.empty ? `: "${short(s.empty, 200)}"` : ''}`);
    if (refreshed) parts.push('(after one Refresh)');
    return { pass: shown.ok && Number(s.lastTurn) >= 1 && /^Last turn/.test(s.scope) && listed.includes(TARGET_FILE), seen: parts.join('; ') };
  }

  // 8. Commit from Source Control; then what Git recorded.
  async step8() {
    const repo = this.options.repo;
    const headBefore = (await git(repo, 'rev-parse', 'HEAD')).stdout.trim();
    const before = await this.exec('now');
    const ready = await this.poll('sourceControl', [], (s) => s?.shown && s.identity, { timeoutMs: 20_000 });
    const s = ready.value || {};
    const parts = [`it says "${short(s.identity, 160)}"`];
    const typed = await this.exec('scMessage', COMMIT_MESSAGE);
    if (!typed.typed) throw new Error('no commit message field');
    const staged = Number((s.commit?.label || '').match(/Commit (\d+) staged/)?.[1] || 0);
    let click;
    if (staged > 0) {
      click = await this.exec('scClick', '[data-action="commit"]');
    } else {
      const menu = await this.exec('scClick', '[data-action="menu"]');
      if (!menu.clicked) return { pass: false, seen: `${parts.join('; ')}; could not open the commit menu (${menu.why})` };
      click = await this.exec('scClick', '[data-action="commit-all"]');
    }
    if (!click.clicked) return { pass: false, seen: `${parts.join('; ')}; could not commit (${click.why})` };
    parts.push(`typed "${COMMIT_MESSAGE}" and chose "${click.label}"`);
    const isResult = (n) => /^(Not )?[Cc]ommitted$/.test(n.title);
    const result = await this.poll('events', [before], (e) => e?.notices?.some(isResult), { timeoutMs: 60_000, intervalMs: 500 });
    const notice = (result.value?.notices || []).find(isResult);
    parts.push(notice ? `notice "${notice.title}${notice.body ? `: ${short(notice.body, 200)}` : ''}"` : 'no Committed / Not committed notice within 60 s');
    const after = await this.poll('sourceControl', [], (x) => x?.shown && !x.busy, { timeoutMs: 10_000 });
    if (after.value?.summary) parts.push(`then "${after.value.summary}"`);
    const head = (await git(repo, 'rev-parse', 'HEAD')).stdout.trim();
    const author = (await git(repo, 'log', '-1', '--format=%an <%ae>')).stdout.trim();
    const subject = (await git(repo, 'log', '-1', '--format=%s')).stdout.trim();
    const changedFiles = (await git(repo, 'show', '--name-only', '--format=', 'HEAD')).stdout.split('\n').filter(Boolean);
    const dirty = (await git(repo, 'status', '--porcelain', '--', TARGET_FILE)).stdout.trim();
    const globalName = (await run('git', ['config', '--global', 'user.name'])).stdout.trim();
    const newCommit = head !== headBefore;
    const authorOk = Boolean(author) && author !== FIXTURE_AUTHOR && (!globalName || author.startsWith(`${globalName} `));
    this.facts.git = { author, subject, changedFiles, newCommit, dirty, globalName, authorOk };
    const pass = notice?.title === 'Committed' && newCommit && changedFiles.includes(TARGET_FILE) && !dirty && authorOk;
    return { pass, seen: parts.join('; ') };
  }

  async all() {
    const steps = [
      [1, [], () => this.step1()],
      [2, [1], () => this.step2()],
      [3, [1], () => this.step3()],
      [4, [3], () => this.step4()],
      [5, [4], () => this.step5()],
      [6, [5], () => this.step6()],
      [7, [4], () => this.step7()],
      [8, [7], () => this.step8()],
    ];
    for (const [number, needs, body] of steps) await this.step(number, needs, body);
  }
}

async function main() {
  let options;
  try {
    options = parseArgs(process.argv.slice(2));
    options.home = await realpath(options.home);
    options.repo = await realpath(options.repo);
    await mkdir(options.out, { recursive: true });
    options.out = await realpath(options.out);
  } catch (error) {
    sayErr(`safari-workbench: ${error.message}\n${USAGE}`);
    process.exit(2);
  }

  const backend = options.backend === 'webdriver' ? new WebDriverBackend(options.port) : new PlaywrightBackend();
  const runner = new Runner(options, backend);
  let started = false;
  const stop = async () => { if (started) { started = false; await backend.stop(); } };
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => { void stop().finally(() => process.exit(130)); });
  try {
    const description = await backend.start();
    started = true;
    say(`PROMPT: ${options.prompt}`);
    say(`Browser: ${description}`);
    say(`Workbench: ${options.origin}/ (the sign-in link is not shown)`);
    say(`Home: ${options.home}`);
    say(`Repository: ${options.repo}`);
    await runner.all();
  } catch (error) {
    sayErr(`safari-workbench: ${error.message}`);
    for (let n = 1; n <= 8; n += 1) {
      if (!runner.results.has(n)) { runner.results.set(n, 'FAIL'); say(`STEP ${n}: FAIL not attempted: ${error.message}`); }
    }
  } finally {
    await stop();
  }

  const g = runner.facts.git;
  if (g) {
    say(`GIT: git -C repo log -1 --format='%an <%ae>': ${g.author || '(no commit)'}${g.globalName ? ` (git config --global user.name: ${g.globalName})` : ' (no global user.name set)'}${g.authorOk ? '' : ' -- not the expected identity'}`);
    say(`GIT: last commit "${g.subject}"${g.newCommit ? ' (made in step 8)' : ' (no new commit)'} changed ${g.changedFiles.join(', ') || 'nothing'}; ${TARGET_FILE} ${g.changedFiles.includes(TARGET_FILE) ? 'changed in it' : 'NOT in it'}${g.dirty ? `; still uncommitted: ${g.dirty}` : ', nothing left uncommitted'}`);
  }
  const shots = (await readdir(options.out).catch(() => [])).filter((name) => /^step-\d\.png$/.test(name)).sort();
  say(`Screenshots (${options.out}): ${shots.join(', ') || 'none'}`);
  const failed = [...runner.results].filter(([, result]) => result !== 'PASS').map(([n]) => n);
  say(failed.length ? `RESULT: FAIL (step ${failed.join(', ')})` : 'RESULT: PASS (all 8 steps)');
  process.exit(failed.length ? 1 : 0);
}

await main();
