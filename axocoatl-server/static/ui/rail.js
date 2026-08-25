import { adopt } from './sheets.js';

/**
 * `<ax-rail>` is the stable Workspace -> Session navigation spine.
 *
 * Workspace identity is durable and independent from the open Session. The
 * selector contains every authorized Workspace, while the list beneath it is
 * deliberately scoped to the selected Workspace. Cross-Workspace navigation
 * is explicit through the selector or All Sessions.
 *
 * @attr {string} current    Open Session id.
 * @attr {string} workspace  Selected Workspace id.
 * @attr {boolean} collapsed Icon-width presentation.
 *
 * @fires workspace-open   detail: {workspace}
 * @fires workspace-new
 * @fires workspace-rename detail: {workspace}
 * @fires session-open     detail: {id, workspaceId}
 * @fires session-new      detail: {workspace}
 * @fires sessions-browse
 * @fires collapse-change
 * @fires settings-open
 */

const CSS = `
:host {
  display: flex; position: relative; width: var(--ax-rail-w, 248px);
  min-height: 0; flex-shrink: 0; flex-direction: column;
  border-right: 1px solid var(--border); background: var(--bg-2);
  color: var(--text); font-family: var(--font-sans);
  transition: width var(--dur-base) var(--ease);
}
* { box-sizing: border-box; }
button { font: inherit; }
:host([collapsed]) { width: 52px; }

.top { display: flex; flex-shrink: 0; align-items: center; gap: var(--sp-2); padding: var(--sp-3) var(--sp-3) var(--sp-2); }
.mark { width: 22px; height: 22px; flex-shrink: 0; border-radius: var(--r-sm); }
.brand { font-size: var(--fs-body); font-weight: var(--fw-bold); letter-spacing: .01em; }
.grow { flex: 1; }
.top-collapse {
  display: inline-flex; width: 32px; height: 32px; flex-shrink: 0; align-items: center;
  justify-content: center; padding: 0; border: 0; border-radius: var(--r-sm);
  background: none; color: var(--muted-2); cursor: pointer; font-size: var(--fs-body); line-height: 1;
}
.top-collapse:hover { color: var(--text); }

.switch {
  display: flex; width: calc(100% - var(--sp-4)); min-width: 0; align-items: center; gap: var(--sp-2);
  margin: 0 var(--sp-2) var(--sp-2); padding: 7px var(--sp-2); border: 1px solid var(--border);
  border-radius: var(--r-md); background: var(--bg-3); color: var(--text); cursor: pointer; text-align: left;
}
.switch:hover { border-color: var(--accent); }
.workspace-mark { width: 18px; flex: 0 0 18px; color: var(--axo-bronze-glow, var(--muted)); text-align: center; }
.cur { display: flex; min-width: 0; flex: 1; flex-direction: column; gap: 1px; }
.cur-name { overflow: hidden; font-size: var(--fs-sm); font-weight: var(--fw-medium); text-overflow: ellipsis; white-space: nowrap; }
.cur-path { overflow: hidden; color: var(--muted-2); font: var(--fs-xs) var(--font-mono); text-overflow: ellipsis; white-space: nowrap; }
.caret { flex-shrink: 0; color: var(--muted-2); }

.menu {
  position: absolute; z-index: 60; top: 76px; right: var(--sp-2); left: var(--sp-2);
  max-height: min(66vh, 520px); overflow-y: auto; padding: var(--sp-1);
  border: 1px solid var(--border-strong); border-radius: var(--r-md);
  background: var(--panel-2); box-shadow: var(--shadow-lg);
}
.menu[hidden] { display: none; }
.menu-label { padding: 6px 8px 4px; color: var(--muted-2); font-size: var(--fs-xs); font-weight: var(--fw-medium); letter-spacing: .08em; text-transform: uppercase; }
.menu-workspace { display: grid; grid-template-columns: minmax(0, 1fr) 28px; align-items: center; border-radius: var(--r-sm); }
.menu-workspace:hover { background: var(--bg-3); }
.menu-workspace.selected { background: color-mix(in srgb, var(--accent) 14%, transparent); }
.menu button { border: 0; background: none; color: var(--text); cursor: pointer; text-align: left; }
.workspace-choice { display: grid; min-width: 0; grid-template-columns: 18px minmax(0, 1fr) auto; gap: 7px; align-items: center; padding: 7px 5px 7px 8px; }
.workspace-copy { display: flex; min-width: 0; flex-direction: column; gap: 1px; }
.workspace-name { overflow: hidden; font-size: var(--fs-sm); text-overflow: ellipsis; white-space: nowrap; }
.workspace-path { overflow: hidden; color: var(--muted-2); font: var(--fs-xs) var(--font-mono); text-overflow: ellipsis; white-space: nowrap; }
.workspace-rename { width: 26px; height: 26px; padding: 0; border-radius: var(--r-sm); color: var(--muted-2) !important; text-align: center !important; }
.workspace-rename:hover { color: var(--text) !important; background: var(--panel); }
.menu-rule { height: 1px; margin: var(--sp-1) 3px; background: var(--border); }
.menu-action { display: flex; width: 100%; align-items: center; gap: var(--sp-2); padding: 7px 8px; border-radius: var(--r-sm); font-size: var(--fs-sm); }
.menu-action:hover { background: var(--bg-3); }

.scroll { flex: 1; min-height: 0; overflow-y: auto; padding: 0 var(--sp-2) var(--sp-2); }
.section-h { display: flex; align-items: center; gap: var(--sp-1); padding: var(--sp-2) var(--sp-1) var(--sp-1) var(--sp-2); }
.section-label { flex: 1; color: var(--muted-2); font-size: var(--fs-xs); font-weight: var(--fw-medium); letter-spacing: .08em; text-transform: uppercase; }
.section-new { width: 25px; height: 25px; padding: 0; border: 0; border-radius: var(--r-sm); background: none; color: var(--muted); cursor: pointer; }
.section-new:hover:not(:disabled) { background: var(--bg-3); color: var(--text); }
.section-new:disabled { opacity: .35; cursor: default; }
.sessions { display: flex; flex-direction: column; gap: 1px; }
.item {
  display: flex; width: 100%; align-items: center; gap: var(--sp-2); padding: 6px var(--sp-2);
  border: 0; border-radius: var(--r-md); background: none; color: var(--text);
  cursor: pointer; font-family: inherit; font-size: var(--fs-sm); text-align: left;
  transition: background var(--dur-fast) var(--ease);
}
.item:hover:not(:disabled) { background: var(--bg-3); }
.item[aria-current="true"] { background: var(--panel); }
.item:disabled { opacity: .4; cursor: default; }
.ico { width: 16px; flex-shrink: 0; opacity: .8; text-align: center; }
.label { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.empty { padding: var(--sp-3) var(--sp-2); color: var(--muted-2); font-size: var(--fs-xs); line-height: var(--lh-body); }
.empty button { display: block; margin-top: var(--sp-2); padding: 5px 9px; border: 1px solid var(--border); border-radius: var(--r-sm); background: var(--bg-3); color: var(--text); cursor: pointer; }
.empty button:hover { border-color: var(--accent); color: var(--accent); }

.badge { display: inline-flex; flex-shrink: 0; align-items: center; gap: var(--sp-1); }
.dots { display: flex; flex-shrink: 0; gap: 3px; }
.dot { width: 6px; height: 6px; border-radius: 50%; background: var(--muted-2); }
.dot.run { background: var(--warn); animation: pulse 1.4s ease-in-out infinite; }
.dot.pass { background: var(--ok); }
.dot.fail { background: var(--err); }
.dot.need { background: var(--accent-2); animation: pulse 1s ease-in-out infinite; }
.dot.aggregate { display: none; }
.count { flex-shrink: 0; color: var(--muted-2); font: var(--fs-xs) var(--font-mono); }
.environment-status { display: inline-flex; flex-shrink: 0; align-items: center; }
.environment-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--muted-2); }
.environment-dot.awaiting_approval, .environment-dot.unprepared { background: var(--accent-2); }
.environment-dot.preparing { background: var(--warn); animation: pulse 1.2s ease-in-out infinite; }
.environment-dot.failed { background: var(--err); }
@keyframes pulse { 0%,100% { opacity: 1 } 50% { opacity: .25 } }

.load-error {
  display: grid; grid-template-columns: minmax(0, 1fr) auto; align-items: center; gap: var(--sp-2);
  margin: var(--sp-2) 0; padding: var(--sp-2); border: 1px solid color-mix(in srgb, var(--err) 45%, var(--border));
  border-radius: var(--r-md); background: var(--panel); color: var(--err); font-size: var(--fs-xs);
}
.load-error button { padding: 3px 7px; border: 1px solid var(--border-strong); border-radius: var(--r-sm); background: var(--bg-3); color: var(--text); cursor: pointer; }
.utility { max-height: min(42vh, 300px); min-height: 0; flex-shrink: 0; overflow: auto; padding: var(--sp-2); border-top: 1px solid var(--border); }
slot[name="utility"] { display: block; width: 100%; }
::slotted([slot="utility"]) { width: 100%; min-width: 0; }
.foot { flex-shrink: 0; padding: var(--sp-2); border-top: 1px solid var(--border); }

button:focus-visible { outline: none; box-shadow: var(--focus-ring); }
:host([collapsed]) .top { justify-content: center; gap: 0; padding: var(--sp-2); }
:host([collapsed]) .top .mark, :host([collapsed]) .brand, :host([collapsed]) .grow, :host([collapsed]) .switch { display: none; }
:host([collapsed]) .top-collapse { display: inline-flex; width: 36px; height: 36px; align-items: center; justify-content: center; padding: 0; }
:host([collapsed]) .section-h { justify-content: center; padding: var(--sp-1); }
:host([collapsed]) .section-label, :host([collapsed]) .label, :host([collapsed]) .dots, :host([collapsed]) .count { display: none; }
:host([collapsed]) .item { justify-content: center; }
:host([collapsed]) .badge .aggregate { display: block; }
:host([collapsed]) .empty, :host([collapsed]) .load-error { display: none; }
`;

const workspacePath = (workspace) => workspace?.canonical_path || '';

export class AxRail extends HTMLElement {
  static get observedAttributes() { return ['current', 'workspace', 'collapsed']; }

  #root;
  #scroll;
  #switch;
  #menu;
  #workspaces = [];
  #sessions = [];
  #attempts = new Map();
  #refreshGeneration = 0;
  #loadError = '';

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="top">
        <img class="mark" src="/brand/mark.png" alt="">
        <div class="brand">Axocoatl</div><div class="grow"></div>
        <button class="top-collapse" id="collapse" type="button" title="Collapse the rail"
          aria-label="Collapse the rail" aria-expanded="true"><span class="collapse-glyph" aria-hidden="true">⟨</span></button>
      </div>
      <button class="switch" id="switch" type="button" aria-haspopup="menu" aria-controls="menu"
        aria-expanded="false" aria-label="Choose a workspace">
        <span class="workspace-mark" aria-hidden="true">▱</span>
        <span class="cur"><span class="cur-name">Choose a workspace</span><span class="cur-path">No folder selected</span></span>
        <span class="caret" aria-hidden="true">▾</span>
      </button>
      <div class="menu" id="menu" role="menu" aria-label="Workspaces" hidden></div>
      <div class="scroll"></div>
      <div class="utility"><slot name="utility"></slot></div>
      <div class="foot">
        <button class="item" id="open-workspace" type="button" title="Open workspace" aria-label="Open workspace"><span class="ico" aria-hidden="true">＋</span><span class="label">Open workspace…</span></button>
        <button class="item" id="browse" type="button" title="All sessions" aria-label="All sessions"><span class="ico" aria-hidden="true">▤</span><span class="label">All sessions</span></button>
        <button class="item" id="settings" type="button" title="Settings" aria-label="Settings"><span class="ico" aria-hidden="true">◇</span><span class="label">Settings</span></button>
      </div>`;
    this.#scroll = this.#root.querySelector('.scroll');
    this.#switch = this.#root.querySelector('#switch');
    this.#menu = this.#root.querySelector('#menu');

    this.#switch.addEventListener('click', (event) => {
      event.stopPropagation();
      this.#menu.hidden ? this.#openMenu('first') : this.#closeMenu({ restoreFocus: true });
    });
    this.#switch.addEventListener('keydown', (event) => {
      if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
      event.preventDefault();
      this.#openMenu(event.key === 'ArrowUp' ? 'last' : 'first');
    });
    this.#root.addEventListener('keydown', (event) => this.#onMenuKeyDown(event));

    const collapse = this.#root.querySelector('#collapse');
    const toggleCollapsed = () => {
      this.collapsed = !this.collapsed;
      this.dispatchEvent(new CustomEvent('collapse-change', {
        detail: { collapsed: this.collapsed }, bubbles: true, composed: true,
      }));
    };
    collapse.addEventListener('click', toggleCollapsed);
    this.#root.querySelector('#open-workspace').addEventListener('click', () => this.#requestWorkspace());
    this.#root.querySelector('#browse').addEventListener('click', () =>
      this.dispatchEvent(new CustomEvent('sessions-browse', { bubbles: true, composed: true })));
    this.#root.querySelector('#settings').addEventListener('click', () =>
      this.dispatchEvent(new CustomEvent('settings-open', { bubbles: true, composed: true })));
    adopt(this.#root, CSS);
  }

  get current() { return this.getAttribute('current') || ''; }
  set current(value) { value ? this.setAttribute('current', value) : this.removeAttribute('current'); }
  get workspace() { return this.getAttribute('workspace') || ''; }
  set workspace(value) { value ? this.setAttribute('workspace', value) : this.removeAttribute('workspace'); }
  get collapsed() { return this.hasAttribute('collapsed'); }
  set collapsed(value) { value ? this.setAttribute('collapsed', '') : this.removeAttribute('collapsed'); }
  get workspaces() { return this.#workspaces.slice(); }
  get sessions() { return this.#sessions.slice(); }
  workspaceById(id) { return this.#workspaces.find((workspace) => workspace.id === id) || null; }

  connectedCallback() {
    if (!this.hasAttribute('role')) this.setAttribute('role', 'navigation');
    if (!this.hasAttribute('aria-label')) this.setAttribute('aria-label', 'Workspaces and Sessions');
    document.addEventListener('click', this.#handleDocumentClick);
    void this.refresh();
  }

  disconnectedCallback() {
    document.removeEventListener('click', this.#handleDocumentClick);
    this.#refreshGeneration += 1;
    this.#closeMenu();
  }

  attributeChangedCallback(name) {
    if (name === 'collapsed') this.#syncCollapsedControl();
    if (name === 'current') this.#markCurrent();
    if (name === 'workspace') this.render();
  }

  focusFirstControl() { this.#root.querySelector('#collapse')?.focus(); }

  async refresh() {
    const generation = ++this.#refreshGeneration;
    try {
      const [workspaceResponse, sessionResponse] = await Promise.all([
        fetch('/api/workspaces'), fetch('/api/sessions'),
      ]);
      if (!workspaceResponse.ok || !sessionResponse.ok) {
        const failed = !workspaceResponse.ok ? workspaceResponse : sessionResponse;
        const detail = await failed.json().catch(() => ({}));
        throw new Error(detail?.error || `HTTP ${failed.status}`);
      }
      const [workspaces, sessions] = await Promise.all([workspaceResponse.json(), sessionResponse.json()]);
      if (!this.isConnected || generation !== this.#refreshGeneration) return false;
      if (!Array.isArray(workspaces) || !Array.isArray(sessions)) throw new Error('Workspace navigation returned an invalid list.');
      this.#workspaces = workspaces.slice().sort((a, b) => (b.last_active || 0) - (a.last_active || 0));
      this.#sessions = sessions;
      this.#loadError = '';
      this.#ensureSelection();
    } catch (error) {
      if (!this.isConnected || generation !== this.#refreshGeneration) return false;
      this.#loadError = String(error?.message || error);
    }
    this.render();
    return !this.#loadError;
  }

  setAttempts(id, states) {
    if (!states?.length) this.#attempts.delete(id);
    else this.#attempts.set(id, states);
    this.render();
  }

  render() {
    if (!this.#scroll) return;
    this.#scroll.replaceChildren();
    this.#syncSwitch();
    if (this.#loadError) this.#renderLoadError();

    const workspace = this.workspaceById(this.workspace);
    const heading = document.createElement('div');
    heading.className = 'section-h';
    const label = document.createElement('span');
    label.className = 'section-label';
    label.textContent = 'Sessions';
    const add = document.createElement('button');
    add.type = 'button'; add.className = 'section-new'; add.textContent = '＋';
    add.disabled = !workspace;
    add.title = workspace ? `New session in ${workspace.name}` : 'Open a workspace first';
    add.setAttribute('aria-label', add.title);
    add.addEventListener('click', () => workspace && this.#requestSession(workspace));
    heading.append(label, add);
    this.#scroll.append(heading);

    if (!workspace) {
      const empty = document.createElement('div');
      empty.className = 'empty';
      empty.textContent = this.#workspaces.length ? 'Choose a workspace to see its sessions.' : 'Open a project folder to begin.';
      const open = document.createElement('button');
      open.type = 'button'; open.textContent = 'Open workspace…';
      open.addEventListener('click', () => this.#requestWorkspace());
      empty.append(open); this.#scroll.append(empty); return;
    }

    const sessions = this.#sessions
      .filter((session) => session.workspace_id === workspace.id)
      .filter((session) => session.status !== 'closed' || session.id === this.current)
      .sort((a, b) => (b.last_active || 0) - (a.last_active || 0));
    if (!sessions.length) {
      const hasClosed = this.#sessions.some((session) => session.workspace_id === workspace.id && session.status === 'closed');
      const empty = document.createElement('div');
      empty.className = 'empty';
      empty.textContent = hasClosed ? `No open sessions in ${workspace.name}. Closed sessions remain in All sessions.` : `No sessions in ${workspace.name} yet.`;
      const create = document.createElement('button');
      create.type = 'button'; create.textContent = 'New session';
      create.addEventListener('click', () => this.#requestSession(workspace));
      empty.append(create); this.#scroll.append(empty); return;
    }
    const list = document.createElement('div');
    list.className = 'sessions';
    sessions.forEach((session) => list.append(this.#sessionRow(session)));
    this.#scroll.append(list);
    this.#markCurrent();
  }

  #ensureSelection() {
    if (this.workspaceById(this.workspace)) return;
    const current = this.#sessions.find((session) => session.id === this.current);
    if (current?.workspace_id && this.workspaceById(current.workspace_id)) this.workspace = current.workspace_id;
  }

  #selectedWorkspace() { return this.workspaceById(this.workspace); }

  #syncSwitch() {
    const workspace = this.#selectedWorkspace();
    const name = this.#root.querySelector('.cur-name');
    const path = this.#root.querySelector('.cur-path');
    name.textContent = workspace?.name || 'Choose a workspace';
    path.textContent = workspace ? workspacePath(workspace) : 'No folder selected';
    name.title = workspace?.name || '';
    path.title = workspace ? workspacePath(workspace) : '';
    const label = workspace ? `Switch workspace. Current workspace: ${workspace.name}` : 'Choose a workspace';
    this.#switch.title = workspace ? `${workspace.name} — ${workspacePath(workspace)}` : label;
    this.#switch.setAttribute('aria-label', label);
  }

  #sessionRow(session) {
    const row = document.createElement('button');
    row.type = 'button'; row.className = 'item'; row.dataset.id = session.id;
    const ico = document.createElement('span');
    ico.className = 'ico'; ico.textContent = '▣'; ico.setAttribute('aria-hidden', 'true');
    const label = document.createElement('span');
    label.className = 'label'; label.textContent = session.name || 'Untitled Session';
    row.append(ico, label);
    const states = this.#attempts.get(session.id);
    if (states?.length) row.append(this.#stateBadge(states));
    const environmentLabel = this.#environmentLabel(session);
    if (environmentLabel) row.append(this.#environmentBadge(session, environmentLabel));
    const attemptLabel = states?.length ? `. ${this.#attemptLabel(states)}` : '';
    const environmentSuffix = environmentLabel ? `. ${environmentLabel}` : '';
    row.title = `${session.name || 'Untitled Session'}${attemptLabel}${environmentSuffix}`;
    row.setAttribute('aria-label', `Open Session ${session.name || 'Untitled Session'}${attemptLabel}${environmentSuffix}`);
    row.addEventListener('click', () => this.dispatchEvent(new CustomEvent('session-open', {
      detail: { id: session.id, workspaceId: session.workspace_id }, bubbles: true, composed: true,
    })));
    return row;
  }

  #environmentLabel(session) {
    const state = session?.environment?.state || 'unprepared';
    return {
      unprepared: 'Project environment not prepared',
      awaiting_approval: 'Project setup approval needed',
      preparing: 'Project environment preparing',
      failed: 'Project environment failed',
    }[state] || '';
  }

  #environmentBadge(session, label) {
    const state = session?.environment?.state || 'unprepared';
    const badge = document.createElement('span');
    badge.className = 'environment-status';
    badge.setAttribute('role', 'img');
    badge.setAttribute('aria-label', label);
    badge.title = label;
    const dot = document.createElement('span');
    dot.className = `environment-dot ${state}`;
    dot.setAttribute('aria-hidden', 'true');
    badge.append(dot);
    return badge;
  }

  #stateBadge(states) {
    const wrap = document.createElement('span');
    wrap.className = 'badge';
    const label = this.#attemptLabel(states);
    wrap.setAttribute('role', 'img'); wrap.setAttribute('aria-label', label); wrap.title = label;
    const count = document.createElement('span');
    count.className = 'count'; count.textContent = `⑂${states.length}`; count.setAttribute('aria-hidden', 'true');
    const dots = document.createElement('span');
    dots.className = 'dots'; dots.setAttribute('aria-hidden', 'true');
    states.slice(0, 6).forEach((state) => {
      const dot = document.createElement('span'); dot.className = `dot ${state}`; dots.append(dot);
    });
    const aggregate = document.createElement('span');
    const aggregateState = ['need', 'fail', 'run', 'pass'].find((state) => states.includes(state)) || '';
    aggregate.className = `dot aggregate ${aggregateState}`; aggregate.setAttribute('aria-hidden', 'true');
    wrap.append(count, dots, aggregate); return wrap;
  }

  #attemptLabel(states) {
    const names = { need: 'waiting for you', fail: 'failed', run: 'running', pass: 'passed' };
    const counts = new Map();
    states.forEach((state) => counts.set(state, (counts.get(state) || 0) + 1));
    const parts = ['need', 'fail', 'run', 'pass'].filter((state) => counts.has(state))
      .map((state) => `${counts.get(state)} ${names[state]}`);
    return `${states.length} ${states.length === 1 ? 'attempt' : 'attempts'}${parts.length ? `: ${parts.join(', ')}` : ''}`;
  }

  #workspaceStates(workspaceId) {
    return this.#sessions.filter((session) => session.workspace_id === workspaceId)
      .flatMap((session) => this.#attempts.get(session.id) || []);
  }

  #openMenu(focus = 'first') {
    this.#menu.replaceChildren();
    const heading = document.createElement('div');
    heading.className = 'menu-label'; heading.textContent = 'Workspaces'; this.#menu.append(heading);
    if (!this.#workspaces.length) {
      const empty = document.createElement('div'); empty.className = 'empty'; empty.textContent = 'No Workspaces yet.'; this.#menu.append(empty);
    }
    this.#workspaces.forEach((workspace) => {
      const row = document.createElement('div');
      row.className = `menu-workspace${workspace.id === this.workspace ? ' selected' : ''}`;
      const choice = document.createElement('button');
      choice.type = 'button'; choice.className = 'workspace-choice'; choice.setAttribute('role', 'menuitem'); choice.tabIndex = -1;
      const check = document.createElement('span');
      check.textContent = workspace.id === this.workspace ? '✓' : '▱'; check.setAttribute('aria-hidden', 'true');
      const copy = document.createElement('span'); copy.className = 'workspace-copy';
      const name = document.createElement('span'); name.className = 'workspace-name'; name.textContent = workspace.name;
      const path = document.createElement('span'); path.className = 'workspace-path'; path.textContent = workspacePath(workspace); path.title = workspacePath(workspace);
      copy.append(name, path); choice.append(check, copy);
      const states = this.#workspaceStates(workspace.id); if (states.length) choice.append(this.#stateBadge(states));
      choice.setAttribute('aria-label', `Open Workspace ${workspace.name}`);
      choice.addEventListener('click', (event) => {
        event.stopPropagation(); this.workspace = workspace.id; this.#closeMenu();
        this.dispatchEvent(new CustomEvent('workspace-open', { detail: { workspace }, bubbles: true, composed: true }));
      });
      const rename = document.createElement('button');
      rename.type = 'button'; rename.className = 'workspace-rename'; rename.textContent = '•••'; rename.tabIndex = -1;
      rename.title = `Rename ${workspace.name}`; rename.setAttribute('aria-label', rename.title);
      rename.addEventListener('click', (event) => {
        event.stopPropagation(); this.#closeMenu();
        this.dispatchEvent(new CustomEvent('workspace-rename', { detail: { workspace }, bubbles: true, composed: true }));
      });
      row.append(choice, rename); this.#menu.append(row);
    });
    const rule = document.createElement('div'); rule.className = 'menu-rule'; this.#menu.append(rule);
    const open = document.createElement('button');
    open.type = 'button'; open.className = 'menu-action'; open.setAttribute('role', 'menuitem'); open.tabIndex = -1;
    open.innerHTML = '<span aria-hidden="true">＋</span><span>Open workspace…</span>';
    open.addEventListener('click', (event) => { event.stopPropagation(); this.#closeMenu(); this.#requestWorkspace(); });
    const manage = document.createElement('button');
    manage.type = 'button'; manage.className = 'menu-action'; manage.setAttribute('role', 'menuitem'); manage.tabIndex = -1;
    manage.innerHTML = '<span aria-hidden="true">⌘</span><span>Manage workspaces…</span>';
    manage.addEventListener('click', (event) => {
      event.stopPropagation(); this.#closeMenu();
      this.dispatchEvent(new CustomEvent('sessions-browse', { detail: { view: 'workspaces' }, bubbles: true, composed: true }));
    });
    this.#menu.append(open, manage);
    this.#menu.hidden = false; this.#switch.setAttribute('aria-expanded', 'true');
    const items = this.#menuItems();
    if (items.length) items[focus === 'last' ? items.length - 1 : 0].focus();
  }

  #menuItems() { return [...this.#menu.querySelectorAll('button:not([disabled])')]; }
  #closeMenu({ restoreFocus = false } = {}) {
    this.#menu.hidden = true; this.#switch.setAttribute('aria-expanded', 'false');
    if (restoreFocus && this.isConnected && !this.collapsed) this.#switch.focus();
  }
  #handleDocumentClick = () => this.#closeMenu();

  #onMenuKeyDown(event) {
    if (this.#menu.hidden) return;
    if (event.key === 'Escape') {
      event.preventDefault(); event.stopPropagation(); this.#closeMenu({ restoreFocus: true }); return;
    }
    if (event.key === 'Tab') { this.#closeMenu(); return; }
    if (!this.#menu.contains(event.target)) return;
    const items = this.#menuItems(); if (!items.length) return;
    const current = items.indexOf(event.target);
    let next = null;
    if (event.key === 'ArrowDown') next = current < 0 ? 0 : (current + 1) % items.length;
    if (event.key === 'ArrowUp') next = current < 0 ? items.length - 1 : (current - 1 + items.length) % items.length;
    if (event.key === 'Home') next = 0;
    if (event.key === 'End') next = items.length - 1;
    if (next === null) return;
    event.preventDefault(); event.stopPropagation(); items[next].focus();
  }

  #requestWorkspace() {
    this.dispatchEvent(new CustomEvent('workspace-new', { bubbles: true, composed: true }));
  }
  #requestSession(workspace) {
    this.dispatchEvent(new CustomEvent('session-new', { detail: { workspace }, bubbles: true, composed: true }));
  }

  #renderLoadError() {
    const box = document.createElement('div'); box.className = 'load-error'; box.setAttribute('role', 'status');
    const message = document.createElement('span');
    message.textContent = this.#workspaces.length ? 'Workspace navigation is temporarily unavailable. Showing the last known list.' : 'Workspaces are temporarily unavailable.';
    message.title = this.#loadError;
    const retry = document.createElement('button'); retry.type = 'button'; retry.textContent = 'Retry'; retry.addEventListener('click', () => void this.refresh());
    box.append(message, retry); this.#scroll.append(box);
  }

  #markCurrent() {
    this.#root.querySelectorAll('.item[data-id]').forEach((row) =>
      row.setAttribute('aria-current', String(row.dataset.id === this.current)));
  }

  #syncCollapsedControl() {
    const button = this.#root.querySelector('#collapse');
    const collapsed = this.collapsed;
    const label = collapsed ? 'Expand the rail' : 'Collapse the rail';
    const active = this.#root.activeElement;
    const focusWillHide = active === this.#switch || this.#menu.contains(active);
    button.querySelector('.collapse-glyph').textContent = collapsed ? '⟩' : '⟨';
    button.title = label; button.setAttribute('aria-label', label); button.setAttribute('aria-expanded', String(!collapsed));
    if (collapsed) {
      this.#closeMenu();
      if (focusWillHide && this.isConnected) button.focus();
    }
  }
}

customElements.define('ax-rail', AxRail);
