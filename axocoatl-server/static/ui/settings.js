import { adopt } from './sheets.js';

/**
 * `<ax-settings>` — configuration, in one place, out of the way.
 *
 * Agents, Skills, MCP servers, Automations and Schedules used to be five peer
 * destinations in the main navigation, which said they were five of the things
 * this product is. They are not: they are how you set it up, visited rarely and
 * never while working. Navigation should list the work.
 *
 * The panels themselves are not rebuilt — they are the existing, working ones,
 * hosted here through a slot and returned to where they came from on close.
 * Rewriting them to change their framing would risk their behaviour for nothing.
 *
 * @element ax-settings
 *
 * @attr {boolean} open
 * @attr {string} section  Which panel is showing.
 *
 * @fires section-change  detail: {id} — the shell should ready that panel
 * @fires close
 */

export const SETTINGS_SECTIONS = [
  { id: 'agents', title: 'Agents', hint: 'Who works, and with what prompt' },
  { id: 'skills', title: 'Skills', hint: 'What agents may call' },
  { id: 'mcp', title: 'MCP servers', hint: 'Tools from outside' },
  { id: 'automations', title: 'Automations', hint: 'Work that starts itself' },
];

const CSS = `
:host { display: none; }
:host([open]) {
  display: block; position: fixed; inset: 0; z-index: 4500;
  background: rgba(0,0,0,.5);
}
.card {
  position: absolute; inset: 6vh 8vw; display: flex;
  background: var(--panel); border: 1px solid var(--border-strong);
  border-radius: var(--r-xl); box-shadow: var(--shadow-lg);
  overflow: hidden; font-family: var(--font-sans); color: var(--text);
  animation: rise var(--dur-base) var(--ease);
}
@keyframes rise { from { opacity: 0; transform: translateY(6px); } }
nav {
  width: 210px; flex-shrink: 0; background: var(--bg-2);
  border-right: 1px solid var(--border);
  display: flex; flex-direction: column; padding: var(--sp-3) var(--sp-2);
}
.h {
  font-size: var(--fs-xs); text-transform: uppercase; letter-spacing: .08em;
  color: var(--muted-2); padding: var(--sp-2); font-weight: var(--fw-medium);
}
nav button {
  display: block; width: 100%; text-align: left; background: none; border: 0;
  color: var(--text); font: var(--fs-sm) var(--font-sans);
  padding: var(--sp-2); border-radius: var(--r-md); cursor: pointer;
  transition: background var(--dur-fast) var(--ease);
}
nav button:hover { background: var(--bg-3); }
nav button[aria-current="true"] { background: var(--panel); }
nav button:focus-visible { outline: none; box-shadow: var(--focus-ring); }
nav button small { display: block; color: var(--muted-2); font-size: var(--fs-xs); }
.main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.bar {
  display: flex; align-items: center; gap: var(--sp-3);
  padding: var(--sp-3) var(--sp-4); border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.bar h2 { margin: 0; font-size: var(--fs-lg); font-weight: var(--fw-medium); }
.x {
  margin-left: auto; background: none; border: 0; color: var(--muted);
  font-size: var(--fs-xl); line-height: 1; cursor: pointer;
}
.x:hover { color: var(--text); }
.body { flex: 1; min-height: 0; display: flex; overflow: hidden; }
::slotted(*) { flex: 1; min-height: 0; }
`;

export class AxSettings extends HTMLElement {
  static get observedAttributes() { return ['open', 'section']; }

  #root; #nav; #title; #slot;
  /** Where each hosted panel came from, so it can be put back exactly. */
  #home = new Map();

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="card" role="dialog" aria-label="Settings">
        <nav><div class="h">Settings</div></nav>
        <div class="main">
          <div class="bar"><h2></h2><button class="x" title="Close">×</button></div>
          <div class="body"><slot></slot></div>
        </div>
      </div>`;
    this.#nav = this.#root.querySelector('nav');
    this.#title = this.#root.querySelector('h2');
    this.#slot = this.#root.querySelector('slot');

    for (const s of SETTINGS_SECTIONS) {
      const b = document.createElement('button');
      b.dataset.id = s.id;
      b.innerHTML = '<span></span><small></small>';
      b.querySelector('span').textContent = s.title;
      b.querySelector('small').textContent = s.hint;
      b.addEventListener('click', () => { this.section = s.id; });
      this.#nav.append(b);
    }
    this.#root.querySelector('.x').addEventListener('click', () => this.hide());
    this.addEventListener('click', (e) => { if (e.target === this) this.hide(); });
    document.addEventListener('keydown', (e) => {
      if (this.open && e.key === 'Escape') { e.preventDefault(); this.hide(); }
    });
    adopt(this.#root, CSS);
  }

  get open() { return this.hasAttribute('open'); }
  set open(v) { v ? this.setAttribute('open', '') : this.removeAttribute('open'); }

  get section() { return this.getAttribute('section') || SETTINGS_SECTIONS[0].id; }
  set section(v) { this.setAttribute('section', v); }

  attributeChangedCallback(name) {
    if (name === 'open' && this.open) this.#show(this.section);
    if (name === 'section' && this.open) this.#show(this.section);
    this.#markCurrent();
  }

  show(section) {
    if (section) this.setAttribute('section', section);
    this.open = true;
  }

  hide() {
    this.#restoreAll();
    this.open = false;
    this.dispatchEvent(new CustomEvent('close', { bubbles: true, composed: true }));
  }

  /**
   * Host a panel by moving the real element in, remembering where it lived.
   *
   * Moving rather than copying means the panel keeps its listeners, its state
   * and its identity — a copy would be a second, subtly different version of a
   * surface that already works.
   */
  #show(id) {
    this.dispatchEvent(new CustomEvent('section-change', {
      detail: { id }, bubbles: true, composed: true,
    }));
    this.#restoreAll();
    const panel = document.querySelector(`#tab-${id}`);
    if (panel) {
      this.#home.set(id, { parent: panel.parentElement, next: panel.nextSibling });
      panel.classList.remove('hide');
      this.append(panel);
    }
    this.#title.textContent = SETTINGS_SECTIONS.find((s) => s.id === id)?.title || 'Settings';
    this.#markCurrent();
  }

  /** Put every hosted panel back where it was, hidden as it was found. */
  #restoreAll() {
    for (const [id, where] of this.#home) {
      const panel = this.querySelector(`#tab-${id}`);
      if (panel && where.parent) {
        panel.classList.add('hide');
        where.parent.insertBefore(panel, where.next);
      }
    }
    this.#home.clear();
  }

  #markCurrent() {
    for (const b of this.#nav.querySelectorAll('button')) {
      b.setAttribute('aria-current', String(b.dataset.id === this.section));
    }
  }
}

customElements.define('ax-settings', AxSettings);
