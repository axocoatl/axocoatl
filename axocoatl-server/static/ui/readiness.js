import { adopt } from './sheets.js';

/**
 * `<ax-readiness>` shows what the first Session still needs, from
 * `GET /api/readiness`: the same checks `axocoatl doctor` runs (Podman, the
 * Ollama server's version and cloud mode, pulled models, writers' sign-ins),
 * each with the exact command or step that fixes it.
 *
 * With `inline` it is a first-run checklist card, shown while something
 * fails; `open()` shows the same checklist in a dialog from anywhere.
 *
 * @element ax-readiness
 * @attr {boolean} inline
 * @fires readiness-change detail: {report, error}
 */

const SECTIONS = [['sandbox', 'Sandbox'], ['models', 'Models'], ['writers', 'Writers'], ['daemon', 'Axocoatl']];
const MARK = { ok: '✓', warn: '!', fail: '✕' };

const CSS = `
:host { display: block; color: var(--text); font: var(--fs-body) / var(--lh-body, 1.5) var(--font-sans); }
:host([hidden]) { display: none; }
* { box-sizing: border-box; }
button { font: inherit; cursor: pointer; }
button:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.card {
  margin: var(--sp-2) var(--sp-2) 0; padding: var(--sp-3) var(--sp-4);
  border: 1px solid color-mix(in srgb, var(--err) 45%, var(--border));
  border-left: 3px solid var(--err); border-radius: var(--r-md); background: var(--panel);
}
.card.warn { border-color: color-mix(in srgb, var(--warn) 45%, var(--border)); border-left-color: var(--warn); }
.card.ready { border-color: var(--border); border-left-color: var(--ok); }
.head { display: flex; align-items: center; gap: var(--sp-2); flex-wrap: wrap; }
.title { margin: 0; font-size: var(--fs-body); font-weight: var(--fw-bold); }
.summary { color: var(--muted); font-size: var(--fs-sm); }
.grow { flex: 1; }
.act {
  padding: 4px 10px; border: 1px solid var(--border); border-radius: var(--r-sm);
  background: var(--bg-3); color: var(--text); font-size: var(--fs-xs);
}
.act:hover { border-color: var(--accent); }
ol { margin: var(--sp-2) 0 0; padding: 0; list-style: none; display: flex; flex-direction: column; gap: var(--sp-2); }
li { display: grid; grid-template-columns: 18px minmax(0, 1fr); gap: 2px var(--sp-2); }
.mark { grid-row: span 3; width: 18px; height: 18px; border-radius: 50%; display: inline-flex; align-items: center; justify-content: center; font-size: 11px; font-weight: var(--fw-bold); }
.mark.fail { background: color-mix(in srgb, var(--err) 18%, transparent); color: var(--err); }
.mark.warn { background: color-mix(in srgb, var(--warn) 18%, transparent); color: var(--warn); }
.mark.ok { background: color-mix(in srgb, var(--ok) 15%, transparent); color: var(--ok); }
.name { font-weight: var(--fw-medium); font-size: var(--fs-sm); }
.detail { color: var(--muted); font-size: var(--fs-sm); overflow-wrap: anywhere; }
.fix-text { margin: 2px 0 0; color: var(--text); font-size: var(--fs-sm); line-height: 1.9; overflow-wrap: anywhere; }
.command { display: inline-flex; align-items: center; gap: 4px; max-width: 100%; vertical-align: baseline; }
.command code {
  min-width: 0; padding: 1px 6px; border: 1px solid var(--border); border-radius: var(--r-sm);
  background: var(--bg-3); color: var(--text); font: var(--fs-xs) / 1.45 var(--font-mono); white-space: pre-wrap; overflow-wrap: anywhere;
}
.copy {
  flex: none; white-space: nowrap; padding: 0 6px; border: 1px solid var(--border); border-radius: var(--r-sm);
  background: transparent; color: var(--muted); font-size: var(--fs-xs); line-height: 1.6;
}
.copy:hover { color: var(--text); border-color: var(--accent); }
.passed { margin-top: var(--sp-2); color: var(--muted); font-size: var(--fs-sm); }
.passed summary { cursor: pointer; }
.error { color: var(--err); font-size: var(--fs-sm); }
dialog {
  width: min(640px, 94vw); max-height: 88vh; padding: 0; border: 1px solid var(--border-strong);
  border-radius: var(--r-xl); background: var(--panel); color: var(--text); box-shadow: var(--shadow-lg);
}
dialog::backdrop { background: rgba(0, 0, 0, .55); }
dialog .card { margin: 0; border: 0; border-radius: 0; }
dialog .foot { display: flex; justify-content: flex-end; padding: 0 var(--sp-4) var(--sp-3); }
@media (max-width: 520px) { .card { padding: var(--sp-3); } }
`;

function node(tag, className = '', text = '') {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== '') element.textContent = text;
  return element;
}

function items(report) {
  return SECTIONS.flatMap(([key, section]) => (report?.sections?.[key] || [])
    .map((item) => ({ ...item, section })));
}

export class AxReadiness extends HTMLElement {
  static observedAttributes = ['inline'];
  #root;
  #report = null;
  #error = '';
  #loading = false;
  #dialog = null;
  #inline = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS);
    this.#render();
  }

  attributeChangedCallback() { if (this.#root) this.#render(); }

  get report() { return this.#report; }
  set report(value) {
    this.#report = value && typeof value === 'object' ? value : null;
    this.#error = '';
    this.#render();
  }

  /** Read the checklist again. Resolves to the report, or null. */
  async refresh() {
    if (this.#loading) return this.#report;
    this.#loading = true;
    this.#render();
    try {
      const response = await fetch('/api/readiness');
      const body = await response.json().catch(() => null);
      if (!response.ok || !body || typeof body !== 'object') throw new Error(body?.error || `HTTP ${response.status}`);
      this.#report = body;
      this.#error = '';
    } catch (error) {
      this.#error = String(error?.message || error);
    } finally {
      this.#loading = false;
    }
    this.#render();
    this.dispatchEvent(new CustomEvent('readiness-change', {
      detail: { report: this.#report, error: this.#error }, bubbles: true, composed: true,
    }));
    return this.#report;
  }

  /** Show the checklist in a dialog. */
  open() {
    if (!this.#dialog) return;
    this.#renderInto(this.#dialog.querySelector('.content'));
    if (!this.#dialog.open) this.#dialog.showModal();
    if (!this.#report && !this.#loading) void this.refresh();
  }

  close() { this.#dialog?.close(); }

  #render() {
    if (!this.#dialog) {
      const dialog = node('dialog');
      dialog.setAttribute('aria-labelledby', 'readiness-dialog-title');
      const content = node('div', 'content');
      const foot = node('div', 'foot');
      const close = node('button', 'act', 'Close');
      close.type = 'button';
      close.addEventListener('click', () => dialog.close());
      foot.append(close);
      dialog.append(content, foot);
      this.#dialog = dialog;
      this.#inline = node('div', 'inline');
      this.#root.append(this.#inline, dialog);
    }
    // The nodes stay in place, so an open dialog is updated, never moved.
    this.#inline.hidden = !this.hasAttribute('inline');
    if (!this.#inline.hidden) this.#renderInto(this.#inline, 'readiness-inline-title');
    if (this.#dialog.open) this.#renderInto(this.#dialog.querySelector('.content'));
  }

  #renderInto(host, titleId = 'readiness-dialog-title') {
    if (!host) return;
    host.replaceChildren();
    const all = items(this.#report);
    const failing = all.filter((item) => item.status === 'fail');
    const warning = all.filter((item) => item.status === 'warn');
    const passed = all.filter((item) => item.status === 'ok');
    const card = node('section', `card${!failing.length ? (warning.length ? ' warn' : ' ready') : ''}`);
    card.setAttribute('aria-labelledby', titleId);
    const head = node('div', 'head');
    const title = node('h2', 'title', !this.#report
      ? (this.#error ? 'Setup could not be checked' : 'Checking setup…')
      : failing.length
        ? `${failing.length} ${failing.length === 1 ? 'step' : 'steps'} left before your first Session`
        : 'Ready for your first Session');
    title.id = titleId;
    const again = node('button', 'act', this.#loading ? 'Checking…' : 'Check again');
    again.type = 'button'; again.dataset.action = 'readiness-refresh'; again.disabled = this.#loading;
    again.addEventListener('click', () => void this.refresh());
    head.append(title, node('span', 'grow'), again);
    card.append(head);
    if (this.#report) {
      card.append(node('div', 'summary', `Axocoatl ${this.#report.version || ''} ran the checks axocoatl doctor runs.`.replace(/\s+/g, ' ')));
    }
    if (this.#error) card.append(node('div', 'error', this.#error));
    const list = node('ol');
    for (const item of [...failing, ...warning]) list.append(this.#item(item));
    if (list.children.length) card.append(list);
    if (passed.length) {
      const details = node('details', 'passed');
      details.append(node('summary', '', `${passed.length} ${passed.length === 1 ? 'check' : 'checks'} passed`));
      const okList = node('ol');
      for (const item of passed) okList.append(this.#item(item));
      details.append(okList);
      card.append(details);
    }
    host.append(card);
  }

  #item(item) {
    const row = node('li');
    row.dataset.id = item.id || '';
    row.dataset.status = item.status;
    const mark = node('span', `mark ${item.status}`, MARK[item.status] || '•');
    mark.setAttribute('aria-label', item.status === 'fail' ? 'Needs a fix' : item.status === 'warn' ? 'Note' : 'Passed');
    row.append(mark, node('div', 'name', `${item.section}: ${item.name}`), node('div', 'detail', item.detail || ''));
    if (item.fix) {
      // The fix is a sentence; `backticked` parts are commands, each with
      // its own Copy.
      const fix = node('div', 'fix');
      const text = node('p', 'fix-text');
      item.fix.split('`').forEach((part, index) => {
        if (index % 2 === 0) { if (part) text.append(document.createTextNode(part)); return; }
        const command = node('span', 'command');
        const code = node('code', '', part);
        const copy = node('button', 'copy', 'Copy');
        copy.type = 'button';
        copy.setAttribute('aria-label', `Copy ${part}`);
        copy.addEventListener('click', async () => {
          try {
            await navigator.clipboard.writeText(part);
            copy.textContent = 'Copied';
          } catch {
            copy.textContent = 'Copy failed';
          }
        });
        command.append(code, copy);
        text.append(command);
      });
      fix.append(text);
      row.append(fix);
    }
    return row;
  }
}

if (!customElements.get('ax-readiness')) customElements.define('ax-readiness', AxReadiness);
