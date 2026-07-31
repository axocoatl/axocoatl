import { adopt } from './sheets.js';

/**
 * `<ax-rail>` — the app's navigation: workspaces, and the sessions inside them.
 *
 * Replaces a strip of eight peer destinations. That strip encoded an admin
 * console — chat was one noun among Agents, Skills, MCP, Studio — when the
 * product is one thing: you are in a session, working. So the rail lists one
 * kind of thing, sessions, grouped by the directory they are anchored to, and
 * everything else becomes a module or a setting.
 *
 * Each row carries live attempt state. When a session fans out, its row shows a
 * dot per attempt, so an attempt that stopped and is waiting on you is visible
 * from a session you are not currently looking at. The failure of parallel work
 * is not being unable to see it, it is not noticing one stopped — and no other
 * tool's navigation has to express this, because no other tool runs N attempts
 * against one task.
 *
 * @element ax-rail
 *
 * @attr {string} current   Id of the open session.
 * @attr {boolean} collapsed  Icon-width only.
 *
 * @fires session-open     detail: {id}
 * @fires session-new      detail: {dir}
 * @fires settings-open    detail: {}
 *
 * @cssprop --ax-rail-w   Expanded width (default 248px)
 */

const CSS = `
:host {
  display: flex; flex-direction: column;
  width: var(--ax-rail-w, 248px);
  flex-shrink: 0;
  background: var(--bg-2);
  border-right: 1px solid var(--border);
  font-family: var(--font-sans);
  color: var(--text);
  min-height: 0;
  transition: width var(--dur-base) var(--ease);
}
:host([collapsed]) { width: 52px; }
:host([collapsed]) .label, :host([collapsed]) .group-h,
:host([collapsed]) .ws-path, :host([collapsed]) .dots { display: none; }

.top {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: var(--sp-3) var(--sp-3) var(--sp-2);
  flex-shrink: 0;
}
.mark { width: 22px; height: 22px; flex-shrink: 0; border-radius: var(--r-sm); }
.brand { font-weight: var(--fw-bold); font-size: var(--fs-body); letter-spacing: .01em; }
.brand span { color: var(--muted-2); font-weight: var(--fw-normal); }
.grow { flex: 1; }

.scroll { flex: 1; overflow-y: auto; padding: 0 var(--sp-2) var(--sp-2); min-height: 0; }

.group-h {
  font-size: var(--fs-xs); text-transform: uppercase; letter-spacing: .08em;
  color: var(--muted-2); padding: var(--sp-3) var(--sp-2) var(--sp-1);
  font-weight: var(--fw-medium);
}

.item {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: 6px var(--sp-2); border-radius: var(--r-md);
  cursor: pointer; color: var(--text); font-size: var(--fs-sm);
  border: 0; background: none; width: 100%; text-align: left;
  font-family: inherit;
  transition: background var(--dur-fast) var(--ease);
}
.item:hover { background: var(--bg-3); }
.item[aria-current="true"] { background: var(--panel); }
.item:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.ico { width: 16px; flex-shrink: 0; opacity: .8; text-align: center; }
.label { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* A workspace is a directory. Sessions live under the one they are anchored to,
   which is the model the finder already used: a session at /a/b/c belongs to
   /a/b/c, not to a flat cloud list. */
.ws { margin-top: var(--sp-1); }
.ws-h {
  display: flex; align-items: baseline; gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-2) 2px;
}
.ws-name { font-size: var(--fs-sm); font-weight: var(--fw-medium); }
.ws-path {
  font-size: var(--fs-xs); color: var(--muted-2); overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap; direction: rtl; text-align: left;
}
.sessions { display: flex; flex-direction: column; gap: 1px; }
.sessions .item { padding-left: var(--sp-4); }

/* Live attempt state. A row that needs a human pulses, because that is the one
   thing you must not miss. */
.dots { display: flex; gap: 3px; flex-shrink: 0; }
.dot { width: 6px; height: 6px; border-radius: 50%; background: var(--muted-2); }
.dot.run  { background: var(--warn); animation: pulse 1.4s ease-in-out infinite; }
.dot.pass { background: var(--ok); }
.dot.fail { background: var(--err); }
.dot.need { background: var(--accent-2); animation: pulse 1s ease-in-out infinite; }
@keyframes pulse { 0%,100% { opacity: 1 } 50% { opacity: .25 } }
.count {
  font-size: var(--fs-xs); color: var(--muted-2); font-family: var(--font-mono);
  flex-shrink: 0;
}

.foot { flex-shrink: 0; border-top: 1px solid var(--border); padding: var(--sp-2); }
.empty { color: var(--muted-2); font-size: var(--fs-xs); padding: var(--sp-3) var(--sp-2); }
`;

/** Trailing path segment — what a directory is actually called. */
const baseName = (p) => (p || '').replace(/\/+$/, '').split('/').pop() || p;

export class AxRail extends HTMLElement {
  static get observedAttributes() { return ['current', 'collapsed']; }

  #root;
  #scroll;
  /** Sessions as served, newest activity first. */
  #sessions = [];
  /** session id → [{state}] for lanes currently running. */
  #attempts = new Map();

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="top">
        <img class="mark" src="/brand/mark.png" alt="" />
        <div class="brand label">Axocoatl</div>
        <div class="grow"></div>
      </div>
      <div class="scroll"></div>
      <div class="foot">
        <button class="item" id="settings"><span class="ico">◇</span><span class="label">Settings</span></button>
      </div>`;
    this.#scroll = this.#root.querySelector('.scroll');
    this.#root.querySelector('#settings').addEventListener('click', () =>
      this.dispatchEvent(new CustomEvent('settings-open', { bubbles: true, composed: true })));
    adopt(this.#root, CSS);
  }

  get current() { return this.getAttribute('current') || ''; }
  set current(v) { v ? this.setAttribute('current', v) : this.removeAttribute('current'); }

  get collapsed() { return this.hasAttribute('collapsed'); }
  set collapsed(v) { v ? this.setAttribute('collapsed', '') : this.removeAttribute('collapsed'); }

  connectedCallback() { this.refresh(); }

  attributeChangedCallback(name) { if (name === 'current') this.#markCurrent(); }

  /** Re-read the session list. */
  async refresh() {
    try {
      const list = await fetch('/api/sessions').then((r) => r.json());
      // Closing a session is how you say "I am done with this". Listing it
      // afterwards makes the act look like it failed, and the rail is for the
      // work you are doing rather than the work you have finished.
      this.#sessions = (Array.isArray(list) ? list : []).filter((s) => s.status !== 'closed');
    } catch {
      this.#sessions = [];
    }
    this.render();
  }

  /**
   * Report live attempt state for a session.
   * @param {string} id
   * @param {Array<'run'|'pass'|'fail'|'need'>} states one entry per attempt
   */
  setAttempts(id, states) {
    if (!states || !states.length) this.#attempts.delete(id);
    else this.#attempts.set(id, states);
    this.render();
  }

  render() {
    const groups = new Map();
    for (const s of this.#sessions) {
      const dir = s.working_dir || '';
      if (!groups.has(dir)) groups.set(dir, []);
      groups.get(dir).push(s);
    }
    // Most recently touched workspace first — the one you are working in.
    const ordered = [...groups.entries()].sort((a, b) =>
      Math.max(...b[1].map((s) => s.last_active || 0)) -
      Math.max(...a[1].map((s) => s.last_active || 0)));

    this.#scroll.textContent = '';
    if (!ordered.length) {
      const e = document.createElement('div');
      e.className = 'empty';
      e.textContent = 'No sessions yet.';
      this.#scroll.append(e);
      return;
    }

    const head = document.createElement('div');
    head.className = 'group-h';
    head.textContent = 'Workspaces';
    this.#scroll.append(head);

    for (const [dir, sessions] of ordered) {
      const ws = document.createElement('div');
      ws.className = 'ws';

      const h = document.createElement('div');
      h.className = 'ws-h';
      const name = document.createElement('div');
      name.className = 'ws-name label';
      name.textContent = baseName(dir);
      const path = document.createElement('div');
      path.className = 'ws-path';
      path.textContent = dir;
      path.title = dir;
      h.append(name, path);
      ws.append(h);

      const list = document.createElement('div');
      list.className = 'sessions';
      sessions.sort((a, b) => (b.last_active || 0) - (a.last_active || 0));
      for (const s of sessions) list.append(this.#sessionRow(s));
      ws.append(list);
      this.#scroll.append(ws);
    }
    this.#markCurrent();
  }

  #sessionRow(s) {
    const row = document.createElement('button');
    row.className = 'item';
    row.dataset.id = s.id;

    const ico = document.createElement('span');
    ico.className = 'ico';
    ico.textContent = '▣';
    const label = document.createElement('span');
    label.className = 'label';
    label.textContent = s.name || baseName(s.working_dir);
    label.title = s.name || '';
    row.append(ico, label);

    const states = this.#attempts.get(s.id);
    if (states?.length) {
      const count = document.createElement('span');
      count.className = 'count';
      count.textContent = `⑂${states.length}`;
      const dots = document.createElement('span');
      dots.className = 'dots';
      for (const st of states) {
        const d = document.createElement('span');
        d.className = `dot ${st}`;
        dots.append(d);
      }
      row.append(count, dots);
    }

    row.addEventListener('click', () => this.dispatchEvent(new CustomEvent('session-open', {
      detail: { id: s.id }, bubbles: true, composed: true,
    })));
    return row;
  }

  #markCurrent() {
    for (const row of this.#root.querySelectorAll('.item[data-id]')) {
      row.setAttribute('aria-current', String(row.dataset.id === this.current));
    }
  }
}

customElements.define('ax-rail', AxRail);
