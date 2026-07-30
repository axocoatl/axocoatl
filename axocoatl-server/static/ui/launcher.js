import { adopt } from './sheets.js';

/**
 * `<ax-launcher>` — one place to open a module, driven from the keyboard.
 *
 * Panes were reachable only through a menu of toggles, which is a settings
 * gesture: you go somewhere, flip a switch, come back. Opening a pane is not a
 * setting, it is an action you take mid-thought, so it belongs on a shortcut and
 * in one list that shows what those shortcuts are. Every entry states its
 * binding, because a shortcut nobody can discover is a shortcut nobody uses.
 *
 * The launcher owns no panes. It reports `module-open` and lets the shell decide
 * what that means, so a module can be a docked pane today and something else
 * later without touching this.
 *
 * @element ax-launcher
 *
 * @attr {boolean} open  Visible.
 *
 * @fires module-open  detail: {id} — an entry was chosen
 * @fires close        the launcher dismissed itself
 */

/**
 * The modules, and what opens them.
 *
 * Bindings follow the platform conventions people already have in their hands:
 * ⌘P for files, ⌘T for a terminal-ish surface. Inventing our own would cost
 * familiarity and buy nothing.
 */
export const MODULES = [
  { id: 'files',    title: 'Files',      hint: 'Project tree and editor',   key: 'p',  ico: '▤' },
  { id: 'stream',   title: 'Activity',   hint: 'What the agents are doing', key: 'a',  ico: '✦' },
  { id: 'terminal', title: 'Terminal',   hint: 'A shell in the sandbox',    key: 't',  ico: '❯' },
  { id: 'browser',  title: 'Browser',    hint: 'Preview the running app',   key: 'b',  ico: '◈' },
  { id: 'trace',    title: 'Agent graph',hint: 'How the work coordinated',  key: 'g',  ico: '◉' },
];

const CSS = `
:host { display: none; }
:host([open]) {
  display: block; position: fixed; inset: 0; z-index: 5000;
  background: rgba(0,0,0,.45);
}
.card {
  position: absolute; top: 12vh; left: 50%; transform: translateX(-50%);
  width: min(520px, 92vw);
  background: var(--panel-2); border: 1px solid var(--border-strong);
  border-radius: var(--r-lg); box-shadow: var(--shadow-lg);
  overflow: hidden; font-family: var(--font-sans);
  animation: rise var(--dur-base) var(--ease);
}
@keyframes rise { from { opacity: 0; transform: translateX(-50%) translateY(-6px); } }
.q {
  width: 100%; box-sizing: border-box; background: var(--bg-3); border: 0;
  border-bottom: 1px solid var(--border); color: var(--text);
  padding: var(--sp-3) var(--sp-4); font: var(--fs-body) var(--font-sans);
  outline: none;
}
.list { max-height: 46vh; overflow-y: auto; padding: var(--sp-1); }
.row {
  display: flex; align-items: center; gap: var(--sp-3);
  padding: var(--sp-2) var(--sp-3); border-radius: var(--r-md);
  cursor: pointer; color: var(--text); font-size: var(--fs-sm);
}
.row[aria-selected="true"] { background: rgba(var(--axo-jade-rgb), .18); }
.ico { width: 18px; text-align: center; opacity: .8; flex-shrink: 0; }
.txt { flex: 1; min-width: 0; }
.title { display: block; }
.hint { display: block; color: var(--muted-2); font-size: var(--fs-xs); }
.on { color: var(--accent); font-size: var(--fs-xs); flex-shrink: 0; }
kbd {
  font: var(--fs-xs) var(--font-mono); color: var(--muted);
  border: 1px solid var(--border-strong); border-radius: var(--r-sm);
  padding: 1px 6px; flex-shrink: 0;
}
.empty { padding: var(--sp-4); color: var(--muted-2); font-size: var(--fs-sm); text-align: center; }
`;

export class AxLauncher extends HTMLElement {
  static get observedAttributes() { return ['open']; }

  #root; #q; #list; #hits = []; #active = 0;
  /** Which modules are currently on, so the list can say so. */
  #openIds = new Set();

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="card" role="dialog" aria-label="Open module">
        <input class="q" placeholder="Open a module…" spellcheck="false" autocomplete="off" />
        <div class="list" role="listbox"></div>
      </div>`;
    this.#q = this.#root.querySelector('.q');
    this.#list = this.#root.querySelector('.list');

    this.#q.addEventListener('input', () => { this.#active = 0; this.#render(); });
    this.#q.addEventListener('keydown', (e) => this.#onKey(e));
    // A click on the backdrop dismisses; one inside the card must not.
    this.addEventListener('click', (e) => { if (e.target === this) this.hide(); });
    adopt(this.#root, CSS);
  }

  get open() { return this.hasAttribute('open'); }
  set open(v) { v ? this.setAttribute('open', '') : this.removeAttribute('open'); }

  attributeChangedCallback(name) {
    if (name !== 'open') return;
    if (this.open) { this.#q.value = ''; this.#active = 0; this.#render(); this.#q.focus(); }
  }

  /** Tell the launcher which modules are on, so entries can be marked. */
  setOpenModules(ids) { this.#openIds = new Set(ids || []); if (this.open) this.#render(); }

  show() { this.open = true; }
  hide() {
    this.open = false;
    this.dispatchEvent(new CustomEvent('close', { bubbles: true, composed: true }));
  }

  #onKey(e) {
    if (e.key === 'Escape') { e.preventDefault(); this.hide(); return; }
    if (e.key === 'ArrowDown') { e.preventDefault(); this.#move(1); return; }
    if (e.key === 'ArrowUp') { e.preventDefault(); this.#move(-1); return; }
    if (e.key === 'Enter') { e.preventDefault(); this.#choose(this.#hits[this.#active]); }
  }

  #move(d) {
    if (!this.#hits.length) return;
    this.#active = (this.#active + d + this.#hits.length) % this.#hits.length;
    this.#render();
    this.#root.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
  }

  #choose(m) {
    if (!m) return;
    this.hide();
    this.dispatchEvent(new CustomEvent('module-open', {
      detail: { id: m.id }, bubbles: true, composed: true,
    }));
  }

  #render() {
    const q = this.#q.value.trim().toLowerCase();
    this.#hits = MODULES.filter((m) =>
      !q || m.title.toLowerCase().includes(q) || m.hint.toLowerCase().includes(q));
    this.#list.textContent = '';
    if (!this.#hits.length) {
      const e = document.createElement('div');
      e.className = 'empty';
      e.textContent = 'Nothing matches';
      this.#list.append(e);
      return;
    }
    this.#hits.forEach((m, i) => {
      const row = document.createElement('div');
      row.className = 'row';
      row.setAttribute('role', 'option');
      row.setAttribute('aria-selected', String(i === this.#active));
      row.innerHTML = `<span class="ico">${m.ico}</span>
        <span class="txt"><span class="title"></span><span class="hint"></span></span>
        ${this.#openIds.has(m.id) ? '<span class="on">on</span>' : ''}
        <kbd>⌘${m.key.toUpperCase()}</kbd>`;
      row.querySelector('.title').textContent = m.title;
      row.querySelector('.hint').textContent = m.hint;
      row.addEventListener('click', () => this.#choose(m));
      row.addEventListener('mousemove', () => {
        if (this.#active === i) return;
        this.#active = i; this.#render();
      });
      this.#list.append(row);
    });
  }
}

customElements.define('ax-launcher', AxLauncher);
