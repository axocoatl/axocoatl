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
 * @prop {Array<{path: string, label?: string}>} favourites  Pinned directories.
 *
 * @fires session-open     detail: {id}
 * @fires session-new      start a session somewhere new
 * @fires sessions-browse  open the browse-everything view
 * @fires workspace-open   detail: {dir} — go to a directory
 * @fires collapse-change  detail: {collapsed} — so the shell can remember it
 * @fires settings-open
 *
 * @slot utility  Shell-owned Docs, status, and theme controls.
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
.badge { display: inline-flex; align-items: center; gap: var(--sp-1); flex-shrink: 0; }
:host([collapsed]) .badge .count { display: none; }
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

/* The workspace switcher. The rail lists sessions grouped by directory, and
   with several directories in play the list gets long — this jumps to one
   without scrolling for it, and names which one you are in. */
.switch {
  display: flex; align-items: center; gap: var(--sp-1);
  width: calc(100% - var(--sp-4)); margin: 0 var(--sp-2) var(--sp-1);
  padding: var(--sp-1) var(--sp-2); border-radius: var(--r-md);
  background: var(--bg-3); border: 1px solid var(--border);
  color: var(--text); font: var(--fs-xs) var(--font-mono); cursor: pointer;
  text-align: left;
}
.switch:hover { border-color: var(--accent); }
.switch:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.switch .cur {
  flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  direction: rtl; text-align: left;
}
.switch .caret { color: var(--muted-2); flex-shrink: 0; }
:host([collapsed]) .switch { display: none; }

.menu {
  position: absolute; z-index: 60; left: var(--sp-2); right: var(--sp-2);
  background: var(--panel-2); border: 1px solid var(--border-strong);
  border-radius: var(--r-md); box-shadow: var(--shadow-lg);
  padding: var(--sp-1); max-height: 50vh; overflow-y: auto;
}
.menu[hidden] { display: none; }
.menu button {
  display: flex; align-items: center; gap: var(--sp-2); width: 100%;
  background: none; border: 0; color: var(--text); cursor: pointer;
  padding: var(--sp-1) var(--sp-2); border-radius: var(--r-sm);
  font: var(--fs-xs) var(--font-mono); text-align: left;
}
.menu button:hover { background: var(--bg-3); }
.menu button:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.menu .m-name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* Pinned directories. They were only reachable from the browse view, which is
   the one place you go to stop browsing — a pin you have to navigate to is not
   a pin. */
.fav .item .ico { color: var(--accent-2); }

.top-collapse {
  background: none; border: 0; color: var(--muted-2); cursor: pointer;
  padding: 0 var(--sp-1); font-size: var(--fs-body); line-height: 1; flex-shrink: 0;
  border-radius: var(--r-sm);
}
.top-collapse:hover { color: var(--text); }
.top-collapse:focus-visible { outline: none; box-shadow: var(--focus-ring); }

.utility {
  flex-shrink: 0; min-height: 0; max-height: min(42vh, 300px); overflow: auto;
  border-top: 1px solid var(--border); padding: var(--sp-2);
}
slot[name="utility"] { display: block; width: 100%; }
::slotted([slot="utility"]) { width: 100%; min-width: 0; }
.foot { flex-shrink: 0; border-top: 1px solid var(--border); padding: var(--sp-2); }
.empty { color: var(--muted-2); font-size: var(--fs-xs); padding: var(--sp-3) var(--sp-2); }
.load-error {
  display: grid; grid-template-columns: minmax(0, 1fr) auto; align-items: center;
  gap: var(--sp-2); margin: var(--sp-2) 0; padding: var(--sp-2);
  border: 1px solid color-mix(in srgb, var(--err) 45%, var(--border));
  border-radius: var(--r-md); color: var(--err); background: var(--panel);
  font-size: var(--fs-xs); line-height: var(--lh-body);
}
.load-error button {
  padding: 3px 7px; border: 1px solid var(--border-strong); border-radius: var(--r-sm);
  background: var(--bg-3); color: var(--text); cursor: pointer; font: inherit;
}
.load-error button:hover { border-color: var(--accent); color: var(--accent); }
.load-error button:focus-visible { outline: none; box-shadow: var(--focus-ring); }
`;

/** Trailing path segment — what a directory is actually called. */
const baseName = (p) => (p || '').replace(/\/+$/, '').split('/').pop() || p;

export class AxRail extends HTMLElement {
  static get observedAttributes() { return ['current', 'collapsed']; }

  #root;
  #scroll;
  #switch;
  #menu;
  /** Pinned directories, as the shell knows them. */
  #favourites = [];
  /** Sessions as served, newest activity first. */
  #sessions = [];
  /** session id → [{state}] for lanes currently running. */
  #attempts = new Map();
  #refreshGeneration = 0;
  #loadError = '';

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="top">
        <img class="mark" src="/brand/mark.png" alt="" />
        <div class="brand label">Axocoatl</div>
        <div class="grow"></div>
        <button class="top-collapse" id="collapse" title="Collapse the rail" aria-label="Collapse the rail">⟨</button>
      </div>
      <button class="switch" id="switch" aria-haspopup="true" aria-expanded="false">
        <span class="cur">no workspace</span><span class="caret">▾</span>
      </button>
      <div class="menu" id="menu" role="menu" hidden></div>
      <div class="scroll"></div>
      <div class="utility"><slot name="utility"></slot></div>
      <div class="foot">
        <button class="item" id="new"><span class="ico">＋</span><span class="label">New session</span></button>
        <button class="item" id="browse"><span class="ico">▤</span><span class="label">All sessions</span></button>
        <button class="item" id="settings"><span class="ico">◇</span><span class="label">Settings</span></button>
      </div>`;
    this.#scroll = this.#root.querySelector('.scroll');
    this.#switch = this.#root.querySelector('#switch');
    this.#menu = this.#root.querySelector('#menu');
    this.#switch.addEventListener('click', (e) => { e.stopPropagation(); this.#toggleMenu(); });
    // A menu that outlives the click that opened it is a menu you have to
    // dismiss on purpose; this closes on any click elsewhere and on Escape.
    document.addEventListener('click', () => this.#closeMenu());
    this.#root.addEventListener('keydown', (e) => {
      if (e.key === 'Escape' && !this.#menu.hidden) { e.stopPropagation(); this.#closeMenu(); }
    });
    // Collapsing is the shell's to remember, so it is announced rather than
    // stored here — the component does not own where preferences live.
    this.#root.querySelector('#collapse').addEventListener('click', () => {
      this.collapsed = !this.collapsed;
      this.dispatchEvent(new CustomEvent('collapse-change', {
        detail: { collapsed: this.collapsed }, bubbles: true, composed: true,
      }));
    });
    // Starting work is a navigation act, so it belongs in the navigation. It
    // used to require going back to the browse view first, which made "new
    // session" something you found rather than something you did.
    this.#root.querySelector('#new').addEventListener('click', () =>
      this.dispatchEvent(new CustomEvent('session-new', { bubbles: true, composed: true })));
    this.#root.querySelector('#browse').addEventListener('click', () =>
      this.dispatchEvent(new CustomEvent('sessions-browse', { bubbles: true, composed: true })));
    this.#root.querySelector('#settings').addEventListener('click', () =>
      this.dispatchEvent(new CustomEvent('settings-open', { bubbles: true, composed: true })));
    adopt(this.#root, CSS);
  }

  get current() { return this.getAttribute('current') || ''; }
  set current(v) { v ? this.setAttribute('current', v) : this.removeAttribute('current'); }

  get collapsed() { return this.hasAttribute('collapsed'); }
  set collapsed(v) {
    v ? this.setAttribute('collapsed', '') : this.removeAttribute('collapsed');
    this.#root.querySelector('#collapse').textContent = v ? '⟩' : '⟨';
    this.#root.querySelector('#collapse').title = v ? 'Expand the rail' : 'Collapse the rail';
  }

  get favourites() { return this.#favourites.slice(); }
  set favourites(v) { this.#favourites = Array.isArray(v) ? v.slice() : []; this.render(); }

  connectedCallback() { void this.refresh(); }
  disconnectedCallback() { this.#refreshGeneration += 1; }

  attributeChangedCallback(name) {
    if (name === 'current') { this.#markCurrent(); this.#syncSwitch(); }
  }

  /** The directory of the open session — what the switcher is showing. */
  get currentDir() {
    return this.#sessions.find((s) => s.id === this.current)?.working_dir || '';
  }

  #syncSwitch() {
    const dir = this.currentDir;
    const cur = this.#root.querySelector('.cur');
    cur.textContent = dir || 'no workspace';
    cur.title = dir || '';
  }

  #toggleMenu() { this.#menu.hidden ? this.#openMenu() : this.#closeMenu(); }

  #closeMenu() {
    this.#menu.hidden = true;
    this.#switch.setAttribute('aria-expanded', 'false');
  }

  /**
   * Every directory you could go to: the ones you have sessions in, plus the
   * ones you pinned but may have no session in yet.
   */
  #openMenu() {
    const dirs = [...new Set(this.#sessions.map((s) => s.working_dir).filter(Boolean))];
    for (const f of this.#favourites) if (f.path && !dirs.includes(f.path)) dirs.push(f.path);
    this.#menu.textContent = '';
    if (!dirs.length) {
      const e = document.createElement('div');
      e.className = 'empty';
      e.textContent = 'No workspaces yet.';
      this.#menu.append(e);
    }
    for (const d of dirs) {
      const b = document.createElement('button');
      b.setAttribute('role', 'menuitem');
      const pinned = this.#favourites.some((f) => f.path === d);
      const ico = document.createElement('span');
      ico.textContent = pinned ? '★' : '▸';
      const name = document.createElement('span');
      name.className = 'm-name';
      name.textContent = d;
      name.title = d;
      b.append(ico, name);
      b.addEventListener('click', (e) => {
        e.stopPropagation();
        this.#closeMenu();
        this.dispatchEvent(new CustomEvent('workspace-open', {
          detail: { dir: d }, bubbles: true, composed: true,
        }));
      });
      this.#menu.append(b);
    }
    this.#menu.hidden = false;
    this.#switch.setAttribute('aria-expanded', 'true');
  }

  /** Re-read the session list. */
  async refresh() {
    const generation = ++this.#refreshGeneration;
    try {
      const response = await fetch('/api/sessions');
      if (!response.ok) {
        const detail = await response.json().catch(() => ({}));
        throw new Error(detail?.error || `HTTP ${response.status}`);
      }
      const list = await response.json();
      if (!this.isConnected || generation !== this.#refreshGeneration) return false;
      if (!Array.isArray(list)) throw new Error('The sessions endpoint returned an invalid list.');
      // Closing a session is how you say "I am done with this". Listing it
      // afterwards makes the act look like it failed, and the rail is for the
      // work you are doing rather than the work you have finished.
      this.#sessions = list.filter((s) => s.status !== 'closed');
      this.#loadError = '';
    } catch (error) {
      if (!this.isConnected || generation !== this.#refreshGeneration) return false;
      // Navigation is durable state. A transient daemon or network failure must
      // not replace the last-known list with a false empty state.
      this.#loadError = String(error?.message || error);
    }
    this.render();
    return !this.#loadError;
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
    this.#renderFavourites();
    if (this.#loadError) {
      const error = document.createElement('div');
      error.className = 'load-error';
      error.setAttribute('role', 'status');
      const message = document.createElement('span');
      message.textContent = this.#sessions.length
        ? 'Sessions are temporarily unavailable. Showing the last known list.'
        : 'Sessions are temporarily unavailable.';
      message.title = this.#loadError;
      const retry = document.createElement('button');
      retry.type = 'button';
      retry.textContent = 'Retry';
      retry.addEventListener('click', () => void this.refresh());
      error.append(message, retry);
      this.#scroll.append(error);
    }
    if (!ordered.length) {
      if (!this.#loadError) {
        const e = document.createElement('div');
        e.className = 'empty';
        e.textContent = 'No sessions yet.';
        this.#scroll.append(e);
      }
      this.#syncSwitch();
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
      // The workspace's own live state, so a collapsed rail — or one scrolled
      // past this group — still shows that something in here is running or
      // waiting on you. Per-session dots answer "which session"; this answers
      // "is there anything at all in this directory".
      const wsStates = sessions.flatMap((s) => this.#attempts.get(s.id) || []);
      if (wsStates.length) h.append(this.#stateBadge(wsStates));
      ws.append(h);

      const list = document.createElement('div');
      list.className = 'sessions';
      sessions.sort((a, b) => (b.last_active || 0) - (a.last_active || 0));
      for (const s of sessions) list.append(this.#sessionRow(s));
      ws.append(list);
      this.#scroll.append(ws);
    }
    this.#markCurrent();
    this.#syncSwitch();
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
    if (states?.length) row.append(this.#stateBadge(states));

    row.addEventListener('click', () => this.dispatchEvent(new CustomEvent('session-open', {
      detail: { id: s.id }, bubbles: true, composed: true,
    })));
    return row;
  }

  /**
   * `⑂N` and one dot per attempt, wrapped so it can be appended anywhere.
   *
   * The same shape serves a session row and a workspace header, because it is
   * the same question at two scales — and answering it differently at each
   * would make the rail two vocabularies instead of one.
   */
  #stateBadge(states) {
    const wrap = document.createElement('span');
    wrap.className = 'badge';
    const count = document.createElement('span');
    count.className = 'count';
    count.textContent = `⑂${states.length}`;
    const dots = document.createElement('span');
    dots.className = 'dots';
    // A workspace can run more attempts than there is room for; the count
    // stays exact while the dots stop at a readable number.
    for (const st of states.slice(0, 6)) {
      const d = document.createElement('span');
      d.className = `dot ${st}`;
      dots.append(d);
    }
    wrap.append(count, dots);
    return wrap;
  }

  /** Pinned directories, above the workspaces you happen to have open. */
  #renderFavourites() {
    if (!this.#favourites.length) return;
    const head = document.createElement('div');
    head.className = 'group-h';
    head.textContent = 'Pinned';
    const list = document.createElement('div');
    list.className = 'sessions fav';
    for (const f of this.#favourites) {
      const b = document.createElement('button');
      b.className = 'item';
      const ico = document.createElement('span');
      ico.className = 'ico';
      ico.textContent = '★';
      const label = document.createElement('span');
      label.className = 'label';
      label.textContent = f.label || baseName(f.path);
      label.title = f.path;
      b.append(ico, label);
      // A pinned directory may have live work in it even with no session open.
      const states = this.#sessions
        .filter((s) => s.working_dir === f.path)
        .flatMap((s) => this.#attempts.get(s.id) || []);
      if (states.length) b.append(this.#stateBadge(states));
      b.addEventListener('click', () => this.dispatchEvent(new CustomEvent('workspace-open', {
        detail: { dir: f.path }, bubbles: true, composed: true,
      })));
      list.append(b);
    }
    this.#scroll.append(head, list);
  }

  #markCurrent() {
    for (const row of this.#root.querySelectorAll('.item[data-id]')) {
      row.setAttribute('aria-current', String(row.dataset.id === this.current));
    }
  }
}

customElements.define('ax-rail', AxRail);
