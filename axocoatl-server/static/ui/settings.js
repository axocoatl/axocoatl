import { adopt } from './sheets.js';

/**
 * `<ax-settings>` — configuration, in one place, out of the way.
 *
 * Agents, Skills, MCP servers, Automations and Schedules used to be five peer
 * destinations in the main navigation, which said they were five of the things
 * this product is. They are not: they are how you set it up, visited rarely and
 * never while working. Navigation should list the work.
 *
 * Each section is a permanent child component. The shell never relocates
 * light-DOM panels or owns the section's domain state.
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
::slotted([hidden]) { display: none !important; }
@media (max-width: 680px) {
  .card { inset: 0; flex-direction: column; border: 0; border-radius: 0; }
  nav {
    width: auto; flex-direction: row; gap: var(--sp-1); overflow-x: auto;
    padding: var(--sp-2); border-right: 0; border-bottom: 1px solid var(--border);
  }
  nav .h { display: none; }
  nav button { width: auto; min-width: max-content; padding-inline: var(--sp-3); }
  nav button small { display: none; }
  .bar { padding: var(--sp-2) var(--sp-3); }
}
`;

export class AxSettings extends HTMLElement {
  static get observedAttributes() { return ['open', 'section']; }

  #root; #nav; #title; #returnFocus = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="card" role="dialog" aria-modal="true" aria-labelledby="settings-title">
        <nav><div class="h">Settings</div></nav>
        <div class="main">
          <div class="bar"><h2 id="settings-title"></h2><button class="x" title="Close" aria-label="Close Settings">×</button></div>
          <div class="body"><slot></slot></div>
        </div>
      </div>`;
    this.#nav = this.#root.querySelector('nav');
    this.#title = this.#root.querySelector('h2');

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
    // Dismiss on a backdrop click — but the test has to be "did this land
    // outside the card", not "is the target the host". Inside a shadow root
    // every click retargets to the host on the way out, so `e.target === this`
    // was true for the nav buttons too: choosing any section closed the dialog,
    // and only the section shown on open was ever reachable.
    this.addEventListener('click', (e) => {
      const card = this.#root.querySelector('.card');
      if (!e.composedPath().includes(card)) this.hide();
    });
    document.addEventListener('keydown', (e) => this.#onKeyDown(e));
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
    if (!this.open) this.#returnFocus = document.activeElement;
    this.open = true;
    queueMicrotask(() => this.#nav.querySelector(`[data-id="${globalThis.CSS.escape(this.section)}"]`)?.focus());
  }

  hide() {
    if (!this.open) return;
    this.open = false;
    this.dispatchEvent(new CustomEvent('close', { bubbles: true, composed: true }));
    const target = this.#returnFocus;
    this.#returnFocus = null;
    queueMicrotask(() => target?.isConnected && target.focus?.());
  }

  #show(id) {
    if (!SETTINGS_SECTIONS.some((section) => section.id === id)) id = SETTINGS_SECTIONS[0].id;
    for (const panel of this.querySelectorAll('[data-settings-section]')) {
      panel.hidden = panel.dataset.settingsSection !== id;
    }
    this.dispatchEvent(new CustomEvent('section-change', {
      detail: { id }, bubbles: true, composed: true,
    }));
    this.#title.textContent = SETTINGS_SECTIONS.find((s) => s.id === id)?.title || 'Settings';
    this.#markCurrent();
  }

  #markCurrent() {
    for (const b of this.#nav.querySelectorAll('button')) {
      b.setAttribute('aria-current', String(b.dataset.id === this.section));
    }
  }

  #focusables() {
    const selector = 'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';
    const found = [];
    const seen = new Set();
    const visit = (root) => {
      for (const element of root.querySelectorAll(selector)) {
        if (!seen.has(element) && !element.hidden && element.getClientRects().length > 0) {
          seen.add(element);
          found.push(element);
        }
        if (element.shadowRoot) visit(element.shadowRoot);
      }
      for (const host of root.querySelectorAll('*')) {
        if (host.shadowRoot) visit(host.shadowRoot);
      }
    };
    visit(this.#root);
    visit(this);
    return found;
  }

  #onKeyDown(event) {
    if (!this.open) return;
    if (event.key === 'Escape') {
      event.preventDefault();
      this.hide();
      return;
    }
    if (event.key !== 'Tab') return;
    const focusables = this.#focusables();
    if (!focusables.length) { event.preventDefault(); return; }
    const current = event.composedPath()[0];
    const index = focusables.indexOf(current);
    if (event.shiftKey && index <= 0) {
      event.preventDefault();
      focusables[focusables.length - 1].focus();
    } else if (!event.shiftKey && (index < 0 || index === focusables.length - 1)) {
      event.preventDefault();
      focusables[0].focus();
    }
  }
}

if (!customElements.get('ax-settings')) customElements.define('ax-settings', AxSettings);
