import { adopt } from './sheets.js';

/**
 * `<ax-session-home>` owns global Session browsing and the two deliberately
 * separate creation flows: opening a Workspace and starting a Session inside
 * the selected Workspace.
 *
 * The component deliberately stops at opening a session. The shell owns the
 * chat/cockpit and listens for `session-open`; this element never creates a
 * second session workspace or keeps cockpit state. A Workspace is durable
 * identity from the daemon; the old browser-local Favorites list is imported
 * once and then retired.
 *
 * @element ax-session-home
 * @fires session-open      detail: {session, source}
 * @fires session-open-new  detail: {session}; cancelable before the fallback window opens
 * @fires session-closed    detail: {session}
 * @fires session-deleted   detail: {session}
 * @fires session-environment-changing detail: {session, source}
 * @fires session-environment-change detail: {session, source}
 * @fires sessions-change   detail: {sessions, count}
 * @fires workspaces-change detail: {workspaces, selectedWorkspaceId}
 * @fires workspace-open    detail: {workspace, source}
 * @fires notify            detail: {title, body, kind}
 */

const LEGACY_FAVORITES_KEY = 'axo.finder.favorites.v1';

const CSS = `
:host {
  display: flex; flex: 1; min-width: 0; min-height: 0;
  color: var(--text); font: var(--fs-body) / var(--lh-body) var(--font-sans);
}
:host([cockpit-active]) .surface {
  display: block; width: 0; height: 0; min-width: 0; min-height: 0; overflow: visible;
}
:host([cockpit-active]) .finder,
:host([cockpit-active]) .notice-host,
:host([cockpit-active]) .menu-host { display: none; }
* { box-sizing: border-box; }
button, input, select { font: inherit; }
button:focus-visible, input:focus-visible, select:focus-visible, [tabindex]:focus-visible {
  outline: none; box-shadow: var(--focus-ring);
}
.surface { position: relative; display: flex; flex: 1; min-width: 0; min-height: 0; flex-direction: column; }
.notice {
  display: flex; align-items: flex-start; gap: var(--sp-2); padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--border); border-bottom: 0; background: var(--panel-2);
  color: var(--text); font-size: var(--fs-sm);
}
.notice.error { border-color: color-mix(in srgb, var(--err) 60%, var(--border)); }
.notice.success { border-color: color-mix(in srgb, var(--ok) 55%, var(--border)); }
.notice strong { display: block; }
.notice-body { color: var(--muted); }
.notice-close { margin-left: auto; border: 0; background: none; color: var(--muted); cursor: pointer; }
.finder {
  display: grid; grid-template-columns: 220px minmax(0, 1fr); flex: 1; min-height: 0;
  overflow: hidden; border: 1px solid var(--border); background: var(--panel);
}
.sidebar {
  display: flex; min-width: 0; min-height: 0; flex-direction: column; overflow: hidden;
  padding: 10px 8px 8px; border-right: 1px solid var(--border); background: var(--bg-2);
}
.side-head {
  padding: 6px 8px 4px; color: var(--muted); font-size: var(--fs-xs);
  letter-spacing: .08em; text-transform: uppercase;
}
.side-head.recent, .side-head.workspaces-head { margin-top: var(--sp-2); padding-top: var(--sp-3); border-top: 1px solid var(--border); }
.side-list { display: flex; min-height: 0; flex-direction: column; gap: 1px; overflow-y: auto; }
.side-empty { padding: 6px 8px; color: var(--muted); font-size: var(--fs-sm); }
.side-row {
  display: flex; align-items: center; gap: 7px; min-width: 0; padding: 5px 8px;
  border-radius: var(--r-md); color: var(--text); cursor: pointer; user-select: none;
  font-size: var(--fs-sm);
}
.side-row:hover { background: var(--bg-3); }
.side-row.active { background: rgba(var(--axo-jade-rgb), .22); }
.side-row.dragging { opacity: .5; }
.side-row.drop-above { box-shadow: inset 0 2px 0 var(--accent); }
.side-row.drop-below { box-shadow: inset 0 -2px 0 var(--accent); }
.side-icon { display: inline-flex; width: 16px; justify-content: center; flex: 0 0 16px; }
.side-label { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.side-remove {
  width: 28px; height: 20px; padding: 0; border: 0; border-radius: var(--r-sm);
  background: transparent; color: var(--muted-2); cursor: pointer; opacity: 0;
}
.side-row:hover .side-remove, .side-remove:focus-visible { opacity: 1; }
.side-remove:hover { color: var(--err); background: var(--bg-3); }
.add-folder {
  margin: var(--sp-2) 4px 0; padding: 5px 10px; border: 1px dashed var(--border-strong);
  border-radius: var(--r-md); background: transparent; color: var(--muted); cursor: pointer;
  font-size: var(--fs-xs);
}
.add-folder:hover { border-color: var(--accent); color: var(--accent); }
.main { display: flex; min-width: 0; min-height: 0; flex-direction: column; background: var(--panel); }
.toolbar {
  display: flex; align-items: center; gap: 7px; padding: var(--sp-2) 11px;
  border-bottom: 1px solid var(--border); background: var(--panel-2); flex-shrink: 0;
}
.nav {
  width: 26px; height: 26px; padding: 0; border: 1px solid var(--border);
  border-radius: var(--r-sm); background: var(--bg-3); color: var(--text); cursor: pointer;
  font-size: var(--fs-body); line-height: 1;
}
.nav:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
.nav:disabled { opacity: .35; cursor: default; }
.path {
  display: flex; align-items: center; flex: 1; min-width: 80px; overflow: hidden;
  gap: var(--sp-2); padding: 4px 10px; border: 1px solid var(--border); border-radius: var(--r-sm);
  background: var(--bg-2); font-size: var(--fs-xs); white-space: nowrap;
}
.path strong { flex-shrink: 0; color: var(--text); }
.path .muted { overflow: hidden; font-family: var(--font-mono); text-overflow: ellipsis; }
.crumb { border: 0; padding: 0; background: none; color: var(--text); cursor: pointer; font: inherit; }
.crumb:hover { color: var(--accent); }
.sep { padding: 0 4px; color: var(--muted-2); }
.muted { color: var(--muted); }
.search {
  width: 180px; min-width: 90px; padding: 4px 9px; border: 1px solid var(--border);
  border-radius: var(--r-sm); background: var(--bg-2); color: var(--text); font-size: var(--fs-xs);
}
.button {
  padding: 5px 11px; border: 1px solid var(--accent); border-radius: var(--r-md);
  background: var(--accent); color: white; cursor: pointer; font-size: var(--fs-sm);
}
.button:hover { filter: brightness(1.08); }
.button:disabled { opacity: .5; cursor: default; filter: none; }
.button.ghost { border-color: var(--border); background: transparent; color: var(--text); }
.button.ghost:hover { border-color: var(--accent); color: var(--accent); filter: none; }
.button.danger { border-color: var(--err); background: var(--err); }
.grid { display: flex; flex: 1; min-height: 0; flex-direction: column; }
.columns, .row {
  display: grid; grid-template-columns: minmax(180px, 1fr) minmax(140px, .7fr) 150px 120px;
  gap: 14px; align-items: center;
}
.columns {
  padding: 6px 14px; border-bottom: 1px solid var(--border); background: var(--panel-2);
  color: var(--muted); font-size: var(--fs-xs); letter-spacing: .06em; text-transform: uppercase;
}
.rows { flex: 1; overflow-y: auto; padding: 4px 0; }
.row {
  width: 100%; padding: 6px 14px; border: 1px solid transparent; color: var(--text);
  cursor: pointer; user-select: none;
}
.row:hover { background: var(--bg-2); }
.row.active { border-color: rgba(var(--axo-jade-rgb), .35); background: rgba(var(--axo-jade-rgb), .22); }
.name { display: flex; align-items: center; gap: var(--sp-2); min-width: 0; }
.name-icon { display: inline-flex; width: 18px; height: 18px; align-items: center; justify-content: center; flex: 0 0 18px; }
.name-text { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.meta { overflow: hidden; color: var(--muted); font: var(--fs-xs) var(--font-mono); text-overflow: ellipsis; white-space: nowrap; }
.status-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--muted-2); box-shadow: none; }
.status-dot.ready { background: var(--ok); box-shadow: 0 0 5px color-mix(in srgb, var(--ok) 65%, transparent); }
.status-dot.awaiting_approval, .status-dot.preparing { background: var(--warn); box-shadow: 0 0 5px color-mix(in srgb, var(--warn) 55%, transparent); }
.status-dot.failed { background: var(--err); box-shadow: 0 0 5px color-mix(in srgb, var(--err) 55%, transparent); }
.status-dot.closed { background: var(--muted-2); box-shadow: none; }
.state { padding: 36px 24px; color: var(--muted); text-align: center; }
.state.error { color: var(--err); }
.state .button { margin-top: var(--sp-3); }
.mark { width: 16px; height: 16px; display: block; object-fit: contain; }
.folder { color: var(--axo-bronze-glow); }
.context {
  position: fixed; z-index: 1200; min-width: 210px; padding: 4px;
  border: 1px solid var(--border-strong); border-radius: var(--r-md);
  background: var(--panel-2); box-shadow: var(--shadow-lg);
}
.menu-item {
  display: flex; width: 100%; padding: 7px 10px; border: 0; border-radius: var(--r-sm);
  background: transparent; color: var(--text); cursor: pointer; text-align: left; font-size: var(--fs-sm);
}
.menu-item:hover { background: rgba(var(--axo-jade-rgb), .22); }
.menu-item.danger { color: var(--err); }
.menu-item:disabled { color: var(--muted-2); cursor: default; background: transparent; }
.menu-sep { height: 1px; margin: 4px 2px; background: var(--border); }
.overlay {
  position: fixed; inset: 0; z-index: 1100; display: flex; align-items: center; justify-content: center;
  padding: var(--sp-3); background: rgba(0,0,0,.55);
}
.modal {
  display: flex; width: min(720px, 94vw); max-height: min(900px, 94vh); flex-direction: column;
  overflow: hidden; border: 1px solid var(--border-strong); border-radius: var(--r-xl);
  background: var(--panel); box-shadow: var(--shadow-lg);
}
.modal.small { width: min(460px, 92vw); }
.modal-head { padding: 14px 18px; border-bottom: 1px solid var(--border); font-weight: var(--fw-bold); }
.picker-path { padding: var(--sp-2) var(--sp-4); border-bottom: 1px solid var(--border); color: var(--muted); font: var(--fs-xs) var(--font-mono); }
.picker-list { min-height: 90px; max-height: 260px; overflow-y: auto; padding: 6px; }
.picker-row {
  display: flex; width: 100%; align-items: center; gap: 7px; padding: 6px 9px;
  border: 0; border-radius: var(--r-sm); background: transparent; color: var(--text); cursor: pointer;
  text-align: left;
}
.picker-row:hover { background: var(--bg-3); }
.config { min-height: 0; overflow-y: auto; }
.config-row { display: flex; align-items: center; gap: 10px; padding: var(--sp-2) var(--sp-4); border-top: 1px solid var(--border); }
.config-row.wrap { align-items: flex-start; flex-wrap: wrap; }
.config-label { color: var(--muted); font-size: var(--fs-sm); white-space: nowrap; }
.config-help { color: var(--muted-2); font-size: var(--fs-xs); }
.grow { flex: 1; }
.input, .select {
  min-width: 0; padding: 6px 9px; border: 1px solid var(--border); border-radius: var(--r-sm);
  background: var(--bg-2); color: var(--text); font-size: var(--fs-sm);
}
.input { flex: 1; font-family: var(--font-mono); }
.select { max-width: 260px; }
.check-list { display: flex; flex-wrap: wrap; gap: 4px 12px; }
.check { display: inline-flex; align-items: center; gap: 5px; color: var(--text); font-size: var(--fs-sm); }
.check small { color: var(--muted-2); }
.probe { color: var(--muted); font-size: var(--fs-sm); }
.probe strong { color: var(--accent); }
.probe-line { margin-top: 2px; font: var(--fs-xs) var(--font-mono); }
.setup-evidence { margin: var(--sp-2) var(--sp-4); border: 1px solid var(--border); border-radius: var(--r-md); background: var(--bg-2); }
.setup-evidence > summary { padding: 8px 10px; color: var(--text); cursor: pointer; font-size: var(--fs-sm); }
.setup-result { padding: 9px 10px; border-top: 1px solid var(--border); }
.setup-result-head { display: flex; align-items: baseline; justify-content: space-between; gap: var(--sp-2); }
.setup-result code { min-width: 0; overflow-wrap: anywhere; color: var(--text); font: var(--fs-xs) var(--font-mono); }
.setup-exit { flex: 0 0 auto; color: var(--muted); font: var(--fs-xs) var(--font-mono); }
.setup-exit.failed { color: var(--err); }
.setup-output { max-height: 180px; margin: 7px 0 0; overflow: auto; white-space: pre-wrap; color: var(--muted); font: var(--fs-xs) / 1.45 var(--font-mono); }
.setup-output.error { color: color-mix(in srgb, var(--err) 78%, var(--text)); }
.runtime-cleanup { margin: var(--sp-2) var(--sp-4); padding: 10px; border: 1px solid color-mix(in srgb, var(--warn) 55%, var(--border)); border-radius: var(--r-md); background: color-mix(in srgb, var(--warn) 7%, var(--bg-2)); }
.runtime-cleanup strong { color: var(--text); }
.runtime-cleanup code { overflow-wrap: anywhere; color: var(--text); font: var(--fs-xs) var(--font-mono); }
.runtime-cleanup .check { align-items: flex-start; overflow-wrap: anywhere; line-height: 1.4; }
.inline-error { padding: var(--sp-2) var(--sp-4); border-top: 1px solid var(--border); color: var(--err); font-size: var(--fs-sm); }
.modal-foot { display: flex; align-items: center; gap: var(--sp-2); padding: 11px var(--sp-4); border-top: 1px solid var(--border); }
.dialog-body { padding: var(--sp-4) 18px; color: var(--muted); }
.dialog-body p { margin: 0; white-space: pre-wrap; }
.dialog-input { width: 100%; margin-top: var(--sp-3); }
@media (max-width: 760px) {
  .finder { grid-template-columns: 1fr; grid-template-rows: auto minmax(0, 1fr); }
  .sidebar { max-height: 190px; border-right: 0; border-bottom: 1px solid var(--border); }
  .side-head.recent, .side-head.recent + .side-list { display: none; }
  .toolbar { flex-wrap: wrap; }
  .path { order: 1; flex-basis: calc(100% - 100px); }
  .search { order: 2; flex: 1; width: auto; }
  .toolbar .button { order: 2; }
  .columns, .row { grid-template-columns: minmax(130px, 1fr) 86px; }
  .columns > :nth-child(3), .columns > :nth-child(4),
  .row > :nth-child(3), .row > :nth-child(4) { display: none; }
  .modal { width: 96vw; max-height: 96vh; }
  .config-row { align-items: stretch; flex-direction: column; gap: 5px; }
  .select { width: 100%; max-width: none; }
}
`;

function element(tag, className = '', text = '') {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== '') node.textContent = text;
  return node;
}

function deepActiveElement() {
  let active = document.activeElement;
  while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
  return active;
}

const MODAL_FOCUSABLE = 'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function sessionDirectory(session) {
  return typeof session?.working_dir === 'string' ? session.working_dir : '';
}

function workspaceDirectory(workspace) {
  return typeof workspace?.canonical_path === 'string' ? workspace.canonical_path : '';
}

function folderName(path) {
  return String(path || '').split('/').filter(Boolean).pop() || String(path || '');
}

function relativeTime(seconds) {
  if (!seconds) return '—';
  const delta = Math.max(0, Math.floor(Date.now() / 1000) - seconds);
  if (delta < 60) return 'just now';
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  if (delta < 86400 * 7) return `${Math.floor(delta / 86400)}d ago`;
  return new Date(seconds * 1000).toLocaleDateString();
}

function loadLegacyFavorites() {
  try {
    const parsed = JSON.parse(localStorage.getItem(LEGACY_FAVORITES_KEY) || '[]');
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((item) => item && typeof item.path === 'string' && item.path)
      .map((item) => ({ path: item.path, label: String(item.label || folderName(item.path)) }));
  } catch {
    return [];
  }
}

function sessionSelectableAgents(agents) {
  return (Array.isArray(agents) ? agents : []).filter((value) => {
    const agent = typeof value === 'string' ? { id: value } : value;
    return agent?.role !== 'worker';
  });
}

function defaultForm(agents) {
  const first = sessionSelectableAgents(agents)[0];
  return {
    copyFrom: '', enabledSkills: new Set(), exposedPorts: '', imagePreset: '', customImage: '',
    imageTouched: false, modeKind: 'single_agent', agentId: first?.id || first || '',
    customAgents: new Set(), sessionName: 'Untitled Session',
    setupCommand: '', setupTouched: false, setupApproved: false,
    runtimeCleanupId: '', runtimeCreationToken: '', runtimeCleanupConfirmed: false,
    workspaceName: '', workspaceNameTouched: false,
  };
}

export class AxSessionHome extends HTMLElement {
  #root;
  #agents = [];
  #workspaces = [];
  #sessions = [];
  #legacyFavorites = loadLegacyFavorites();
  #selectedWorkspaceId = '';
  #scope = 'all';
  #currentPath = '';
  #selectedSession = '';
  #back = [];
  #forward = [];
  #searchTerm = '';
  #contents = { dirs: [], parent: null, path: '' };
  #phase = 'idle';
  #sessionError = '';
  #pathPhase = 'idle';
  #pathError = '';
  #refreshGeneration = 0;
  #pathGeneration = 0;
  #navigationGeneration = 0;
  #picker = null;
  #pickerReturnFocus = null;
  #menu = null;
  #dialog = null;
  #notice = null;
  #started = false;
  #outsidePointer;
  #viewportChange;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS, []);
    this.#root.addEventListener('click', (event) => this.#onClick(event));
    this.#root.addEventListener('contextmenu', (event) => this.#onContextMenu(event));
    this.#root.addEventListener('input', (event) => this.#onInput(event));
    this.#root.addEventListener('change', (event) => this.#onChange(event));
    this.#root.addEventListener('keydown', (event) => this.#onKeyDown(event));
    this.#outsidePointer = (event) => {
      if (this.#menu && !event.composedPath().includes(this)) {
        this.#menu = null;
        this.#renderMenu();
      }
    };
    this.#viewportChange = () => {
      if (!this.#menu) return;
      this.#menu = null;
      this.#renderMenu();
    };
    this.#render();
  }

  connectedCallback() {
    document.addEventListener('pointerdown', this.#outsidePointer, true);
    window.addEventListener('resize', this.#viewportChange);
    window.addEventListener('scroll', this.#viewportChange, true);
    if (!this.#started) {
      this.#started = true;
      void this.refresh();
    }
  }

  disconnectedCallback() {
    document.removeEventListener('pointerdown', this.#outsidePointer, true);
    window.removeEventListener('resize', this.#viewportChange);
    window.removeEventListener('scroll', this.#viewportChange, true);
  }

  get agents() { return this.#agents.slice(); }
  set agents(value) {
    this.#agents = Array.isArray(value) ? value.slice() : [];
    if (this.#picker) {
      const selectable = sessionSelectableAgents(this.#agents);
      const selected = selectable.some((agent) => (agent?.id || agent) === this.#picker.form.agentId);
      if (!selected) {
        const first = selectable[0];
        this.#picker.form.agentId = first?.id || first || '';
      }
      this.#picker.form.customAgents = new Set(
        [...this.#picker.form.customAgents].filter((id) => selectable.some((agent) => (agent?.id || agent) === id)),
      );
    }
    this.#renderRows();
    this.#renderPicker();
  }

  get sessions() { return this.#sessions.slice(); }
  get workspaces() { return this.#workspaces.slice(); }
  get selectedWorkspaceId() { return this.#selectedWorkspaceId; }
  set selectedWorkspaceId(value) { this.selectWorkspace(value, { scope: this.#scope }); }
  get currentPath() { return this.#currentPath; }

  session(id) { return this.#sessions.find((item) => item.id === id) || null; }
  workspace(id) { return this.#workspaces.find((item) => item.id === id) || null; }
  noteExternalNavigation() { this.#navigationGeneration += 1; }

  selectWorkspace(id, { scope = 'workspace' } = {}) {
    const workspace = this.workspace(id);
    this.#selectedWorkspaceId = workspace?.id || '';
    this.#currentPath = workspaceDirectory(workspace);
    this.#scope = scope;
    this.#selectedSession = '';
    this.#renderSidebar();
    this.#renderPath();
    this.#renderRows();
    return workspace || null;
  }

  showAllSessions() {
    this.#scope = 'all';
    this.#selectedSession = '';
    this.#renderSidebar(); this.#renderPath(); this.#renderRows();
  }

  showWorkspaces() {
    this.#scope = 'workspaces';
    this.#selectedSession = '';
    this.#renderSidebar(); this.#renderPath(); this.#renderRows();
  }

  async refresh() {
    // A shell may deliberately prime the element before it connects. Count
    // that as the initial load so connectedCallback does not issue a duplicate
    // sessions read after upgrade.
    this.#started = true;
    const generation = ++this.#refreshGeneration;
    this.#phase = 'loading';
    this.#sessionError = '';
    this.#renderRows();
    try {
      let [workspaces, sessions] = await Promise.all([
        this.#request('/api/workspaces'), this.#request('/api/sessions'),
      ]);
      if (generation !== this.#refreshGeneration) return false;
      if (!Array.isArray(workspaces) || !Array.isArray(sessions)) {
        throw new Error('Workspace navigation returned an invalid list.');
      }
      workspaces = await this.#migrateLegacyFavorites(workspaces);
      if (generation !== this.#refreshGeneration) return false;
      this.#workspaces = workspaces.slice().sort((a, b) => (b.last_active || 0) - (a.last_active || 0));
      this.#sessions = sessions;
      this.#phase = 'ready';
      if (this.#selectedSession && !sessions.some((item) => item.id === this.#selectedSession)) {
        this.#selectedSession = '';
      }
      this.dispatchEvent(new CustomEvent('sessions-change', {
        detail: { sessions: this.sessions, count: sessions.length }, bubbles: true, composed: true,
      }));
      if (this.#selectedWorkspaceId && !this.workspace(this.#selectedWorkspaceId)) {
        this.#selectedWorkspaceId = '';
      }
      this.dispatchEvent(new CustomEvent('workspaces-change', {
        detail: { workspaces: this.workspaces, selectedWorkspaceId: this.#selectedWorkspaceId },
        bubbles: true, composed: true,
      }));
      this.#renderSidebar();
      const selected = this.workspace(this.#selectedWorkspaceId);
      this.#currentPath = workspaceDirectory(selected);
      this.#renderPath();
      this.#renderRows();
      return true;
    } catch (error) {
      if (generation !== this.#refreshGeneration) return false;
      this.#phase = 'error';
      this.#sessionError = String(error?.message || error);
      this.#renderSidebar();
      this.#renderRows();
      return false;
    }
  }

  async #migrateLegacyFavorites(workspaces) {
    if (!this.#legacyFavorites.length) return workspaces;
    const pending = [];
    let imported = false;
    for (const favorite of this.#legacyFavorites) {
      try {
        const existing = workspaces.find((workspace) => workspaceDirectory(workspace) === favorite.path);
        const customName = favorite.label && favorite.label !== folderName(favorite.path);
        if (!existing || customName) {
          const body = { path: favorite.path };
          if (customName) body.name = favorite.label;
          await this.#request('/api/workspaces', {
            method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
          });
          imported = true;
        }
      } catch { pending.push(favorite); }
    }
    // The legacy key is a per-folder retry ledger. Retire every successful
    // import immediately so one unavailable folder cannot replay an older
    // custom label over a later Workspace rename on every refresh.
    this.#legacyFavorites = pending;
    try {
      if (pending.length) localStorage.setItem(LEGACY_FAVORITES_KEY, JSON.stringify(pending));
      else localStorage.removeItem(LEGACY_FAVORITES_KEY);
    } catch {}
    return imported ? this.#request('/api/workspaces') : workspaces;
  }

  async navigate(path, { pushHistory = true } = {}) {
    if (!path) return false;
    const generation = ++this.#pathGeneration;
    if (pushHistory && this.#currentPath && this.#currentPath !== path) {
      this.#back.push(this.#currentPath);
      this.#forward = [];
    }
    this.#currentPath = path;
    this.#selectedSession = '';
    this.#pathPhase = 'loading';
    this.#pathError = '';
    this.#renderSidebar();
    this.#renderPath();
    this.#renderRows();
    try {
      const data = await this.#request(`/api/fs/list?path=${encodeURIComponent(path)}`);
      if (generation !== this.#pathGeneration) return false;
      if (data?.error) throw new Error(data.error);
      this.#contents = {
        dirs: Array.isArray(data?.dirs) ? data.dirs : [],
        parent: data?.parent || null,
        path: data?.path || path,
      };
      this.#currentPath = this.#contents.path;
      this.#pathPhase = 'ready';
      this.#renderSidebar();
      this.#renderPath();
      this.#renderRows();
      return true;
    } catch (error) {
      if (generation !== this.#pathGeneration) return false;
      this.#contents = { dirs: [], parent: null, path };
      this.#pathPhase = 'error';
      this.#pathError = String(error?.message || error);
      this.#renderPath();
      this.#renderRows();
      return false;
    }
  }

  openWorkspace(initialPath = '') { this.#openPicker('workspace', initialPath); }

  newSession(workspaceId = this.#selectedWorkspaceId) {
    const workspace = this.workspace(workspaceId);
    if (!workspace) {
      this.#notify('Open a Workspace first', 'A Session always belongs to one project folder.', 'err');
      return false;
    }
    this.#openPicker('session', workspaceDirectory(workspace), workspace.id);
    return true;
  }

  /** Open the explicit runtime/setup decision for a persisted Session. */
  configureEnvironment(sessionId) {
    const session = this.session(sessionId);
    if (!session) return false;
    this.#openEnvironmentPicker(session);
    return true;
  }

  /** Reproduce the current approved plan, removing its dependency volume. */
  async rebuildEnvironment(sessionId) {
    const session = this.session(sessionId);
    if (!session) return false;
    this.#notify('Rebuilding environment', `${session.name} stays unavailable until preparation succeeds.`, 'info');
    this.#environmentChanging(session, 'rebuild');
    try {
      const updated = await this.#request(`/api/sessions/${encodeURIComponent(session.id)}/environment/rebuild`, {
        method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}',
      });
      await this.refresh();
      this.#environmentNotice(updated, 'Environment rebuilt');
      this.#environmentChanged(updated, 'rebuild');
      return updated.environment?.state === 'ready';
    } catch (error) {
      this.#notify('Environment rebuild failed', String(error?.message || error), 'err');
      await this.refresh();
      this.#environmentChanged(this.session(session.id) || session, 'rebuild-error');
      return false;
    }
  }

  async renameWorkspace(id) {
    const workspace = this.workspace(id);
    if (!workspace) return false;
    const value = await this.#ask({ title: 'Rename Workspace', label: 'Name', value: workspace.name });
    if (!value?.trim() || value.trim() === workspace.name) return false;
    try {
      const updated = await this.#request(`/api/workspaces/${encodeURIComponent(id)}`, {
        method: 'PATCH', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ name: value.trim() }),
      });
      this.#notify('Workspace renamed', updated.name, 'ok');
      await this.refresh();
      return true;
    } catch (error) {
      this.#notify('Rename failed', String(error?.message || error), 'err');
      return false;
    }
  }

  #render() {
    this.#root.innerHTML = `
      <div class="surface">
        <div class="notice-host" aria-live="polite"></div>
        <div class="finder">
          <aside class="sidebar">
            <div class="side-head">Browse</div>
            <div class="side-list scopes"></div>
            <div class="side-head workspaces-head">Workspaces</div>
            <div class="side-list workspaces"></div>
            <button class="add-folder" data-action="open-workspace">＋ Open Workspace…</button>
          </aside>
          <main class="main">
            <div class="toolbar">
              <div class="path"></div>
              <input class="search" type="search" placeholder="Filter Sessions…" aria-label="Filter Sessions" spellcheck="false">
              <button class="button ghost" data-action="open-workspace">Open Workspace…</button>
              <button class="button" data-action="new-session">＋ New Session</button>
            </div>
            <div class="grid">
              <div class="columns"><span>Name</span><span>Workspace</span><span>Mode</span><span>Last active</span></div>
              <div class="rows" role="tree" aria-label="All Sessions"></div>
            </div>
          </main>
        </div>
        <div class="picker-host"></div>
        <div class="menu-host"></div>
        <div class="dialog-host"></div>
      </div>`;
    const search = this.#root.querySelector('.search');
    if (search) search.value = this.#searchTerm;
    this.#renderNotice();
    this.#renderSidebar();
    this.#renderPath();
    this.#renderRows();
    this.#renderPicker();
    this.#renderMenu();
    this.#renderDialog();
  }

  #renderNotice() {
    const host = this.#root.querySelector('.notice-host');
    if (!host) return;
    host.replaceChildren();
    if (!this.#notice) return;
    const box = element('div', `notice ${this.#notice.kind === 'err' ? 'error' : this.#notice.kind === 'ok' ? 'success' : ''}`);
    const copy = element('div');
    copy.append(element('strong', '', this.#notice.title));
    if (this.#notice.body) copy.append(element('div', 'notice-body', this.#notice.body));
    box.append(copy);
    const close = element('button', 'notice-close', '×');
    close.type = 'button'; close.dataset.action = 'dismiss-notice'; close.setAttribute('aria-label', 'Dismiss');
    box.append(close);
    host.append(box);
  }

  #renderSidebar() {
    const scopes = this.#root.querySelector('.scopes');
    const workspaces = this.#root.querySelector('.workspaces');
    if (!scopes || !workspaces) return;
    scopes.replaceChildren();
    const all = element('div', `side-row${this.#scope === 'all' ? ' active' : ''}`);
    all.tabIndex = 0; all.dataset.scope = 'all';
    all.append(element('span', 'side-icon', '▤'), element('span', 'side-label', 'All Sessions'));
    const manage = element('div', `side-row${this.#scope === 'workspaces' ? ' active' : ''}`);
    manage.tabIndex = 0; manage.dataset.scope = 'workspaces';
    manage.append(element('span', 'side-icon', '▱'), element('span', 'side-label', 'Manage Workspaces'));
    scopes.append(all, manage);

    workspaces.replaceChildren();
    if (!this.#workspaces.length) {
      workspaces.append(element('div', 'side-empty', this.#phase === 'loading' ? 'Loading…' : 'No Workspaces yet.'));
    }
    this.#workspaces.forEach((workspace) => {
      const active = this.#scope === 'workspace' && workspace.id === this.#selectedWorkspaceId;
      const row = element('div', `side-row${active ? ' active' : ''}`);
      row.tabIndex = 0; row.title = workspaceDirectory(workspace); row.dataset.workspaceId = workspace.id;
      row.append(element('span', 'side-icon', '▱'), element('span', 'side-label', workspace.name));
      const rename = element('button', 'side-remove', '•••');
      rename.type = 'button'; rename.dataset.action = 'rename-workspace'; rename.dataset.workspaceId = workspace.id;
      rename.title = `Rename ${workspace.name}`; rename.setAttribute('aria-label', rename.title);
      row.append(rename); workspaces.append(row);
    });
  }

  #renderPath() {
    const host = this.#root.querySelector('.path');
    if (!host) return;
    host.replaceChildren();
    if (this.#scope === 'all') {
      host.append(element('strong', '', 'All Sessions'), element('span', 'muted', 'Across every Workspace'));
      return;
    }
    if (this.#scope === 'workspaces') {
      host.append(element('strong', '', 'Manage Workspaces'), element('span', 'muted', 'Names and authorized project folders'));
      return;
    }
    const workspace = this.workspace(this.#selectedWorkspaceId);
    if (!workspace) {
      host.append(element('strong', '', 'Sessions'), element('span', 'muted', 'Choose a Workspace'));
      return;
    }
    host.append(element('strong', '', workspace.name), element('span', 'muted', workspaceDirectory(workspace)));
  }

  #renderRows() {
    const host = this.#root.querySelector('.rows');
    if (!host) return;
    host.replaceChildren();
    const newSession = this.#root.querySelector('[data-action="new-session"]');
    if (newSession) newSession.disabled = !this.workspace(this.#selectedWorkspaceId);
    if (this.#phase === 'loading' && !this.#sessions.length && !this.#workspaces.length) {
      host.append(element('div', 'state', 'Loading Workspaces and Sessions…')); return;
    }
    if (this.#phase === 'error') {
      host.append(this.#errorState('Workspaces and Sessions could not be loaded', this.#sessionError, 'retry-sessions')); return;
    }
    const term = this.#searchTerm.trim().toLowerCase();
    if (this.#scope === 'workspaces') {
      const workspaces = this.#workspaces.filter((workspace) => !term
        || `${workspace.name} ${workspaceDirectory(workspace)}`.toLowerCase().includes(term));
      if (!workspaces.length) {
        host.append(element('div', 'state', term ? 'No Workspaces match this filter.' : 'No Workspaces yet. Open a project folder to begin.'));
        return;
      }
      workspaces.forEach((workspace) => host.append(this.#workspaceRow(workspace)));
      return;
    }
    const selectedWorkspace = this.workspace(this.#selectedWorkspaceId);
    let sessions = this.#scope === 'all'
      ? this.#sessions.slice()
      : this.#sessions.filter((session) => session.workspace_id === selectedWorkspace?.id);
    sessions = sessions
      .filter((session) => {
        if (!term) return true;
        const workspace = this.workspace(session.workspace_id);
        return `${session.name} ${workspace?.name || ''} ${workspaceDirectory(workspace)}`.toLowerCase().includes(term);
      })
      .sort((a, b) => (b.last_active || 0) - (a.last_active || 0));
    if (!sessions.length) {
      let message = 'No Sessions yet.';
      if (term) message = 'No Sessions match this filter.';
      else if (this.#scope === 'workspace' && selectedWorkspace) message = `No Sessions in ${selectedWorkspace.name} yet. Click ＋ New Session to start one.`;
      else if (this.#scope === 'workspace') message = 'Choose a Workspace to see its Sessions.';
      host.append(element('div', 'state', message));
      return;
    }
    sessions.forEach((session) => host.append(this.#sessionRow(session)));
  }

  #workspaceRow(workspace) {
    const row = element('div', 'row workspace-row');
    row.tabIndex = 0; row.dataset.workspaceId = workspace.id;
    row.setAttribute('role', 'treeitem');
    row.setAttribute('aria-label', `Open Workspace ${workspace.name}`);
    const name = element('div', 'name');
    name.append(element('span', 'name-icon folder', '▱'), element('span', 'name-text', workspace.name));
    const count = this.#sessions.filter((session) => session.workspace_id === workspace.id).length;
    row.append(name, element('span', 'meta', workspaceDirectory(workspace)), element('span', 'meta', `${count} ${count === 1 ? 'Session' : 'Sessions'}`), element('span', 'meta', relativeTime(workspace.last_active)));
    return row;
  }

  #sessionRow(session) {
    const row = element('div', `row session-row${session.id === this.#selectedSession ? ' active' : ''}`);
    row.tabIndex = 0; row.dataset.sessionId = session.id;
    row.setAttribute('role', 'treeitem');
    row.setAttribute('aria-selected', String(session.id === this.#selectedSession));
    const workspace = this.workspace(session.workspace_id);
    const sessionName = session.name || 'Untitled Session';
    const openVerb = session.status === 'closed' ? 'Review' : 'Open';
    const closedQualifier = session.status === 'closed' ? ' closed' : '';
    row.setAttribute('aria-label', `${openVerb}${closedQualifier} Session ${sessionName} in Workspace ${workspace?.name || 'Unknown'}`);
    const name = element('div', 'name');
    const icon = element('span', 'name-icon'); icon.append(this.#mark());
    name.append(icon, element('span', 'name-text', sessionName));
    const environment = session.environment || {};
    const environmentState = session.status === 'closed' ? 'closed' : (environment.state || 'unprepared');
    const environmentLabels = {
      ready: `Ready${environment.effective_image ? ` · ${environment.effective_image}` : ''}`,
      awaiting_approval: `Setup approval needed${environment.setup_command ? ` · ${environment.setup_command}` : ''}`,
      preparing: 'Preparing environment…',
      failed: `Environment failed${environment.error ? ` · ${environment.error}` : ''}`,
      unprepared: 'Environment not prepared',
      closed: 'Closed',
    };
    const dot = element('span', `status-dot ${environmentState}`);
    dot.title = environmentLabels[environmentState] || environmentState; name.append(dot);
    const kind = session.mode?.kind;
    const mode = kind === 'single_agent' ? `agent · ${session.mode.agent_id || '—'}`
      : kind === 'custom' ? `custom · ${(session.mode.agents || []).length}` : 'lattice';
    const workspaceCell = element('span', 'meta', workspace?.name || 'Unknown Workspace');
    workspaceCell.title = workspaceDirectory(workspace);
    row.title = workspace
      ? `${sessionName} — ${workspace.name} — ${workspaceDirectory(workspace)} — ${dot.title}`
      : `${sessionName} — ${dot.title}`;
    row.append(name, workspaceCell, element('span', 'meta', mode), element('span', 'meta', relativeTime(session.last_active)));
    return row;
  }

  #errorState(title, body, action) {
    const state = element('div', 'state error');
    state.append(element('strong', '', title));
    if (body) state.append(element('div', '', body));
    const retry = element('button', 'button ghost', 'Retry'); retry.type = 'button'; retry.dataset.action = action;
    state.append(retry); return state;
  }

  #mark() {
    const mark = element('img', 'mark'); mark.src = '/brand/mark.png'; mark.alt = ''; return mark;
  }

  #openSession(session, source) {
    if (!session) return;
    const workspace = this.workspace(session.workspace_id);
    this.#selectedWorkspaceId = workspace?.id || session.workspace_id || '';
    this.#currentPath = workspaceDirectory(workspace);
    this.#navigationGeneration += 1;
    this.dispatchEvent(new CustomEvent('session-open', {
      detail: { session, workspace, source }, bubbles: true, composed: true,
    }));
  }

  #selectSession(id) {
    this.#selectedSession = id || '';
    this.#root.querySelectorAll('[data-session-id]').forEach((row) => {
      row.classList.toggle('active', row.dataset.sessionId === this.#selectedSession);
    });
  }

  #goBack() {
    if (!this.#back.length) return;
    this.#forward.push(this.#currentPath);
    void this.navigate(this.#back.pop(), { pushHistory: false });
  }

  #goForward() {
    if (!this.#forward.length) return;
    this.#back.push(this.#currentPath);
    void this.navigate(this.#forward.pop(), { pushHistory: false });
  }

  #goUp() { if (this.#contents.parent) void this.navigate(this.#contents.parent); }

  async #renameSession(session) {
    const value = await this.#ask({ title: 'Rename session', label: 'Name', value: session.name });
    if (!value?.trim() || value.trim() === session.name) return;
    try {
      await this.#request(`/api/sessions/${encodeURIComponent(session.id)}`, {
        method: 'PATCH', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ name: value.trim() }),
      });
      this.#notify('Session renamed', value.trim(), 'ok'); await this.refresh();
    } catch (error) { this.#notify('Rename failed', String(error?.message || error), 'err'); }
  }

  async #closeSession(session) {
    const remoteE2b = session.environment?.runtime?.backend === 'e2b';
    const ok = await this.#ask({
      kind: 'confirm', title: 'Close this session?',
      body: remoteE2b
        ? `“${session.name}”'s E2B VM will pause with its remote working tree preserved. Reopen resumes that exact runtime, including uncommitted work. Delete session or Change runtime is the explicit destructive boundary.`
        : `“${session.name}”'s container will stop. You can reopen it later — its history stays.`,
      okLabel: 'Close session',
    });
    if (!ok) return;
    this.#environmentChanging(session, 'close');
    try {
      await this.#request(`/api/sessions/${encodeURIComponent(session.id)}`, { method: 'DELETE' });
      this.#notify('Session closed', session.name, 'ok');
      this.dispatchEvent(new CustomEvent('session-closed', { detail: { session }, bubbles: true, composed: true }));
      await this.refresh();
    } catch (error) {
      await this.refresh();
      this.#environmentChanged(this.session(session.id) || session, 'close-error');
      this.#notify('Close failed', String(error?.message || error), 'err');
    }
  }

  async #deleteSession(session) {
    const ok = await this.#ask({
      kind: 'confirm', title: 'Delete this session permanently?',
      body: `“${session.name}”'s config and history will be gone. Files in ${sessionDirectory(session)} are not touched.`,
      okLabel: 'Delete session', danger: true,
    });
    if (!ok) return;
    this.#environmentChanging(session, 'delete');
    try {
      await this.#request(`/api/sessions/${encodeURIComponent(session.id)}?force=true`, { method: 'DELETE' });
      this.#notify('Session deleted', session.name, 'ok');
      this.dispatchEvent(new CustomEvent('session-deleted', { detail: { session }, bubbles: true, composed: true }));
      await this.refresh();
    } catch (error) {
      await this.refresh();
      this.#environmentChanged(this.session(session.id) || session, 'delete-error');
      this.#notify('Delete failed', String(error?.message || error), 'err');
    }
  }

  #showMenu(kind, id, x, y) {
    this.#menu = {
      kind, id,
      x: Math.max(4, Math.min(x, window.innerWidth - 230)),
      y: Math.max(4, Math.min(y, window.innerHeight - 260)),
    };
    this.#renderMenu();
  }

  #renderMenu() {
    const host = this.#root.querySelector('.menu-host');
    if (!host) return;
    host.replaceChildren(); if (!this.#menu) return;
    const menu = element('div', 'context');
    menu.style.left = `${this.#menu.x}px`; menu.style.top = `${this.#menu.y}px`; menu.setAttribute('role', 'menu');
    const items = this.#menuItems();
    items.forEach((item) => {
      if (item.separator) { menu.append(element('div', 'menu-sep')); return; }
      const button = element('button', `menu-item${item.danger ? ' danger' : ''}`, item.label);
      button.type = 'button'; button.dataset.action = 'menu-action'; button.dataset.menuAction = item.action;
      button.disabled = !!item.disabled; button.setAttribute('role', 'menuitem'); menu.append(button);
    });
    host.append(menu);
  }

  #menuItems() {
    if (this.#menu?.kind === 'session') {
      const session = this.session(this.#menu.id);
      if (!session) return [];
      const environmentState = session.environment?.state || 'unprepared';
      const openVerb = session.status === 'closed' ? 'Review' : 'Open';
      return [
        { label: openVerb, action: 'open' },
        { label: `${openVerb} in new window`, action: 'open-window' },
        { separator: true },
        { label: 'Rename…', action: 'rename' },
        { label: 'Copy working directory', action: 'copy-path' },
        { separator: true },
        {
          label: environmentState === 'awaiting_approval' ? 'Review setup…' : 'Change runtime or setup…',
          action: 'configure-environment',
        },
        ...(environmentState === 'ready' || environmentState === 'failed'
          ? [{ label: 'Rebuild environment', action: 'rebuild-environment' }]
          : []),
        { separator: true },
        { label: session.status === 'active' ? 'Close' : 'Close (already closed)', action: 'close', disabled: session.status !== 'active' },
        { label: 'Delete…', action: 'delete', danger: true },
      ];
    }
    if (this.#menu?.kind === 'workspace') {
      return [
        { label: 'Open', action: 'open' }, { label: 'New Session', action: 'new-session' },
        { separator: true }, { label: 'Rename Workspace…', action: 'rename' },
        { label: 'Copy folder path', action: 'copy-path' },
      ];
    }
    return [];
  }

  async #runMenuAction(action) {
    const menu = this.#menu; this.#menu = null; this.#renderMenu();
    if (!menu) return;
    if (menu.kind === 'session') {
      const session = this.session(menu.id); if (!session) return;
      if (action === 'open') this.#openSession(session, 'context-menu');
      else if (action === 'open-window') {
        const event = new CustomEvent('session-open-new', { detail: { session }, bubbles: true, composed: true, cancelable: true });
        if (this.dispatchEvent(event)) window.open('/', '_blank', 'noopener');
      } else if (action === 'rename') await this.#renameSession(session);
      else if (action === 'configure-environment') this.configureEnvironment(session.id);
      else if (action === 'rebuild-environment') await this.rebuildEnvironment(session.id);
      else if (action === 'copy-path') {
        try {
          if (!navigator.clipboard?.writeText) throw new Error('Clipboard access is unavailable.');
          await navigator.clipboard.writeText(sessionDirectory(session));
          this.#notify('Path copied', sessionDirectory(session), 'ok');
        } catch (error) { this.#notify('Copy failed', String(error?.message || error), 'err'); }
      } else if (action === 'close') await this.#closeSession(session);
      else if (action === 'delete') await this.#deleteSession(session);
      return;
    }
    if (menu.kind !== 'workspace') return;
    const workspace = this.workspace(menu.id); if (!workspace) return;
    if (action === 'open') {
      this.selectWorkspace(workspace.id);
      this.dispatchEvent(new CustomEvent('workspace-open', {
        detail: { workspace, source: 'context-menu' }, bubbles: true, composed: true,
      }));
    } else if (action === 'new-session') this.newSession(workspace.id);
    else if (action === 'rename') await this.renameWorkspace(workspace.id);
    else if (action === 'copy-path') {
      try {
        if (!navigator.clipboard?.writeText) throw new Error('Clipboard access is unavailable.');
        await navigator.clipboard.writeText(workspaceDirectory(workspace));
        this.#notify('Path copied', workspaceDirectory(workspace), 'ok');
      } catch (error) { this.#notify('Copy failed', String(error?.message || error), 'err'); }
    }
  }

  #openPicker(kind, initialPath, workspaceId = '') {
    this.#menu = null;
    if (!this.#picker) this.#pickerReturnFocus = deepActiveElement();
    const form = defaultForm(this.#agents);
    this.#picker = {
      kind, workspaceId, path: kind === 'session' ? initialPath : '', requestedPath: initialPath || '',
      dirs: [], parent: null, phase: kind === 'session' ? 'ready' : 'loading',
      error: '', skills: [], skillsError: '', probe: null, probeError: '',
      probePending: kind === 'session', busy: false,
      generation: 0, form,
    };
    this.#renderPicker();
    if (kind === 'session') {
      void this.#loadPickerSkills();
      void this.#probePickerProject(this.#picker, initialPath, 0);
    } else {
      void this.#pickerNavigate(initialPath || '');
    }
  }

  #openEnvironmentPicker(session) {
    this.#menu = null;
    if (!this.#picker) this.#pickerReturnFocus = deepActiveElement();
    const form = defaultForm(this.#agents);
    this.#picker = {
      kind: 'environment', targetSessionId: session.id, workspaceId: session.workspace_id,
      path: sessionDirectory(session), requestedPath: sessionDirectory(session),
      dirs: [], parent: null, phase: 'ready', error: '', skills: [], skillsError: '',
      probe: null, probeError: '', probePending: true, busy: false, generation: 0, form,
    };
    this.#setPickerImage(session.image || '', Boolean(session.image));
    form.setupCommand = session.environment?.setup_command || '';
    form.setupApproved = Boolean(session.environment?.setup_approved && form.setupCommand.trim());
    form.setupTouched = true;
    form.runtimeCleanupId = session.environment?.runtime?.id || '';
    form.runtimeCreationToken = session.environment?.runtime_creation?.token || '';
    this.#renderPicker();
    void this.#probePickerProject(this.#picker, sessionDirectory(session), 0);
  }

  async #loadPickerSkills() {
    const picker = this.#picker; if (!picker) return;
    try {
      const skills = await this.#request('/api/skills');
      if (this.#picker !== picker) return;
      picker.skills = Array.isArray(skills) ? skills : [];
    } catch (error) {
      if (this.#picker !== picker) return;
      picker.skillsError = String(error?.message || error);
    }
    this.#renderPicker();
  }

  async #pickerNavigate(path) {
    const picker = this.#picker; if (!picker) return;
    const generation = ++picker.generation;
    picker.requestedPath = path;
    picker.phase = 'loading'; picker.error = ''; picker.probe = null; picker.probeError = '';
    this.#renderPicker();
    try {
      const suffix = path ? `?path=${encodeURIComponent(path)}` : '';
      const data = await this.#request(`/api/fs/list${suffix}`);
      if (this.#picker !== picker || picker.generation !== generation) return;
      if (data?.error) throw new Error(data.error);
      picker.path = data?.path || path;
      picker.requestedPath = picker.path;
      picker.dirs = Array.isArray(data?.dirs) ? data.dirs : [];
      picker.parent = data?.parent || null;
      if (!picker.form.workspaceNameTouched) {
        const existing = this.#workspaces.find((workspace) => workspaceDirectory(workspace) === picker.path);
        picker.form.workspaceName = existing?.name || folderName(picker.path) || picker.path;
      }
      picker.phase = 'ready'; this.#renderPicker();
    } catch (error) {
      if (this.#picker !== picker || picker.generation !== generation) return;
      picker.phase = 'error'; picker.error = String(error?.message || error); this.#renderPicker();
    }
  }

  async #probePickerProject(picker, path, generation) {
    if (!path) return;
    try {
      const probe = await this.#request(`/api/fs/project?path=${encodeURIComponent(path)}`);
      if (this.#picker !== picker || picker.generation !== generation) return;
      picker.probe = probe || {};
      const image = probe?.devcontainer && !probe.devcontainer.error ? probe.devcontainer.image : '';
      const supportsSessionImage = probe?.runtime?.supports_session_image !== false;
      if (probe?.runtime?.supports_preview === false) picker.form.exposedPorts = '';
      if (image && picker.kind === 'session' && !picker.form.imageTouched) {
        // Keep an explicit repository image visible so E2B incompatibility
        // blocks creation instead of being silently discarded.
        this.#setPickerImage(image, false);
      } else if (image && supportsSessionImage && !picker.form.imageTouched) {
        this.#setPickerImage(image, false);
      }
      const setupCommand = probe?.suggested_setup?.command;
      if (supportsSessionImage && !image
          && probe?.suggested_setup?.source === 'package-lock' && !picker.form.imageTouched) {
        this.#setPickerImage('docker.io/library/node:20-slim', false);
      }
      if (setupCommand && !picker.form.setupTouched) {
        picker.form.setupCommand = setupCommand;
        picker.form.setupApproved = probe?.suggested_setup?.source === 'devcontainer'
          && probe?.runtime?.auto_approve_devcontainer_setup === true;
      }
    } catch (error) {
      if (this.#picker !== picker || picker.generation !== generation) return;
      picker.probeError = String(error?.message || error);
    }
    picker.probePending = false;
    this.#renderPicker();
  }

  #setPickerImage(image, touched = true) {
    const picker = this.#picker; if (!picker) return;
    const known = ['docker.io/library/alpine:3.20', 'docker.io/library/debian:bookworm-slim',
      'docker.io/library/ubuntu:24.04', 'docker.io/library/python:3.12-slim',
      'docker.io/library/node:20-slim', 'docker.io/library/rust:bookworm'];
    const requested = String(image || '').trim();
    const canonical = known.find((candidate) => {
      const short = candidate.replace('docker.io/library/', '');
      return requested === candidate || requested === short || requested === `library/${short}`
        || requested === `docker.io/${short}`;
    }) || requested;
    if (!canonical || known.includes(canonical)) {
      picker.form.imagePreset = canonical; picker.form.customImage = '';
    } else {
      picker.form.imagePreset = '__custom__'; picker.form.customImage = requested;
    }
    picker.form.imageTouched = touched;
  }

  #applyCopiedSession(id) {
    const picker = this.#picker; const session = this.session(id);
    if (!picker || !session) return;
    const form = picker.form;
    form.copyFrom = id;
    form.exposedPorts = picker.probe?.runtime?.supports_preview === false
      ? '' : (session.exposed_ports || []).join(', ');
    this.#setPickerImage(session.image || '', true);
    form.modeKind = session.mode?.kind || 'single_agent';
    form.agentId = session.mode?.agent_id || form.agentId;
    form.customAgents = new Set(session.mode?.agents || []);
    form.enabledSkills = new Set(session.enabled_skills || []);
    form.setupCommand = session.environment?.setup_command || '';
    form.setupApproved = false;
    form.setupTouched = true;
    this.#notify('Copied config', session.name, 'ok'); this.#renderPicker();
  }

  #renderPicker() {
    const host = this.#root.querySelector('.picker-host');
    if (!host) return;
    const active = this.#root.activeElement;
    const focusWasModal = active?.classList?.contains('modal');
    const focusKey = active && host.contains(active) && !focusWasModal
      ? { action: active.dataset.action || '', field: active.dataset.field || '', path: active.dataset.path || '' }
      : null;
    host.replaceChildren(); const picker = this.#picker; if (!picker) return;
    const overlay = element('div', 'overlay'); overlay.dataset.action = 'picker-backdrop';
    const modal = element('div', 'modal'); modal.setAttribute('role', 'dialog'); modal.setAttribute('aria-modal', 'true');
    modal.setAttribute('aria-labelledby', 'session-picker-title');
    modal.tabIndex = -1;
    const workspace = this.workspace(picker.workspaceId);
    const targetSession = picker.kind === 'environment' ? this.session(picker.targetSessionId) : null;
    const title = element('div', 'modal-head', picker.kind === 'workspace'
      ? 'Open workspace'
      : picker.kind === 'environment'
        ? `Environment for ${targetSession?.name || 'Session'}`
        : `New session in ${workspace?.name || 'workspace'}`);
    title.id = 'session-picker-title';
    modal.append(title);
    modal.append(element('div', 'picker-path', picker.kind === 'session'
      ? workspaceDirectory(workspace)
      : picker.kind === 'environment'
        ? sessionDirectory(targetSession)
        : picker.path || picker.requestedPath || 'Choose a project folder'));
    if (picker.kind === 'workspace') {
      const list = element('div', 'picker-list');
      if (picker.phase === 'loading') list.append(element('div', 'state', 'Opening folder…'));
      else if (picker.phase === 'error') {
        list.append(this.#errorState('This folder could not be opened', picker.error, 'retry-picker'));
      } else {
        if (picker.parent) list.append(this.#pickerPathRow(picker.parent, '↑', '.. (parent)'));
        picker.dirs.forEach((directory) => list.append(this.#pickerPathRow(directory.path, '▸', directory.name)));
        if (!picker.parent && !picker.dirs.length) list.append(element('div', 'state', 'No subfolders. You can open this folder.'));
      }
      modal.append(list);
      const nameRow = element('div', 'config-row');
      const nameLabel = element('label', 'config-label', 'Workspace name');
      const name = element('input', 'input');
      name.id = 'workspace-name-input'; nameLabel.htmlFor = name.id;
      name.type = 'text'; name.dataset.field = 'workspace-name'; name.value = picker.form.workspaceName;
      name.placeholder = folderName(picker.path) || 'My workspace';
      nameRow.append(nameLabel, name, element('span', 'config-help', 'The folder path stays unchanged'));
      modal.append(nameRow);
    } else if (picker.kind === 'session') {
      const nameRow = element('div', 'config-row');
      const nameLabel = element('label', 'config-label', 'Session name');
      const name = element('input', 'input');
      name.id = 'session-name-input'; nameLabel.htmlFor = name.id;
      name.type = 'text'; name.dataset.field = 'session-name'; name.value = picker.form.sessionName;
      name.placeholder = 'Untitled Session';
      nameRow.append(nameLabel, name, element('span', 'config-help', `Always belongs to ${workspace?.name || 'this Workspace'}`));
      modal.append(nameRow, this.#pickerConfig(picker));
    } else {
      modal.append(this.#environmentPickerConfig(picker, targetSession));
    }
    if (picker.error && picker.phase !== 'error') modal.append(element('div', 'inline-error', picker.error));
    const foot = element('div', 'modal-foot');
    if (picker.kind === 'session' && picker.form.modeKind === 'single_agent') {
      foot.append(element('span', 'config-label', 'Agent'));
      const agents = this.#agentSelect(picker.form.agentId); agents.dataset.field = 'agent'; foot.append(agents);
    }
    foot.append(element('span', 'grow'));
    const cancel = element('button', 'button ghost', 'Cancel'); cancel.type = 'button'; cancel.dataset.action = 'picker-cancel'; cancel.disabled = picker.busy;
    const use = element('button', 'button', picker.busy
      ? (picker.kind === 'environment' || picker.kind === 'session' ? 'Preparing environment…' : 'Working…')
      : picker.kind === 'workspace' ? 'Open workspace'
        : picker.kind === 'environment' ? 'Apply and rebuild' : 'Create session');
    use.type = 'button'; use.dataset.action = 'picker-use';
    const nameMissing = picker.kind === 'workspace'
      ? !picker.form.workspaceName.trim()
      : picker.kind === 'session' ? !picker.form.sessionName.trim() : false;
    const environmentKind = picker.kind === 'session' || picker.kind === 'environment';
    const malformedDevcontainerBlocksCreation = picker.kind === 'session'
      && picker.probe?.devcontainer?.error;
    const malformedDevcontainerNeedsImageDecision = picker.kind === 'environment'
      && picker.probe?.devcontainer?.error && !picker.form.imageTouched;
    const projectProbeFailureBlocksCreation = picker.kind === 'session' && picker.probeError;
    const requestedImage = (picker.form.imagePreset === '__custom__'
      ? picker.form.customImage : picker.form.imagePreset).trim();
    const remoteImageConflict = picker.probe?.runtime?.supports_session_image === false
      && Boolean(requestedImage);
    use.disabled = picker.busy || picker.phase !== 'ready' || !picker.path || nameMissing
      || (environmentKind && picker.probePending)
      || projectProbeFailureBlocksCreation
      || malformedDevcontainerBlocksCreation
      || malformedDevcontainerNeedsImageDecision
      || remoteImageConflict;
    foot.append(cancel, use); modal.append(foot); overlay.append(modal); host.append(overlay);
    queueMicrotask(() => {
      if (this.#picker !== picker || !modal.isConnected) return;
      let target = null;
      if (focusKey) {
        target = Array.from(modal.querySelectorAll(MODAL_FOCUSABLE)).find((candidate) =>
          (candidate.dataset.action || '') === focusKey.action
          && (candidate.dataset.field || '') === focusKey.field
          && (candidate.dataset.path || '') === focusKey.path);
      }
      (target || modal).focus();
    });
  }

  #closePicker({ restoreFocus = true } = {}) {
    if (!this.#picker) return;
    this.#picker = null;
    this.#renderPicker();
    const target = this.#pickerReturnFocus;
    this.#pickerReturnFocus = null;
    if (restoreFocus) queueMicrotask(() => target?.isConnected && target.focus?.());
  }

  #pickerPathRow(path, icon, label) {
    const row = element('button', 'picker-row'); row.type = 'button'; row.dataset.action = 'picker-navigate'; row.dataset.path = path;
    row.disabled = Boolean(this.#picker?.busy);
    row.append(element('span', 'folder', icon), element('span', '', label)); return row;
  }

  #pickerConfig(picker) {
    const config = element('div', 'config');
    const copyRow = element('div', 'config-row'); copyRow.append(element('label', 'config-label', 'Copy config from'));
    const copy = element('select', 'select'); copy.dataset.field = 'copy-from';
    copy.append(new Option('— start from defaults —', ''));
    this.#sessions.forEach((session) => copy.append(new Option(`${session.name} · ${sessionDirectory(session)}`, session.id)));
    copy.value = picker.form.copyFrom; copyRow.append(copy, element('span', 'config-help', 'Mirrors mode, image, skills, and supported Preview ports')); config.append(copyRow);

    const skills = element('div', 'config-row wrap'); skills.append(element('span', 'config-label', 'Skills this session may fire'));
    const skillList = element('div', 'check-list');
    if (picker.skillsError) skillList.append(element('span', 'config-help', `Unavailable: ${picker.skillsError}`));
    else if (!picker.skills.length) skillList.append(element('span', 'config-help', 'No skills configured.'));
    picker.skills.forEach((skill) => {
      const label = element('label', 'check'); const input = document.createElement('input');
      input.type = 'checkbox'; input.dataset.field = 'skill'; input.value = skill.id; input.checked = picker.form.enabledSkills.has(skill.id);
      label.append(input, document.createTextNode(skill.name || skill.id)); skillList.append(label);
    });
    skills.append(skillList); config.append(skills);

    const ports = element('div', 'config-row');
    const supportsPreview = picker.probe?.runtime?.supports_preview !== false;
    ports.append(element('label', 'config-label', supportsPreview ? 'Exposed ports' : 'Preview ports'));
    if (supportsPreview) {
      const portInput = element('input', 'input'); portInput.type = 'text'; portInput.placeholder = '3000, 5000, 5173, 8000, 8888';
      portInput.dataset.field = 'ports'; portInput.value = picker.form.exposedPorts;
      portInput.disabled = picker.probePending;
      ports.append(portInput, element('span', 'config-help', picker.probePending
        ? 'Checking whether this runtime supports Preview…'
        : 'Leave empty for none. Only listed loopback ports are reachable in Preview'));
    } else {
      ports.append(element('span', 'config-help', 'Unavailable with the configured E2B runtime; no Session ports will be exposed.'));
    }
    config.append(ports);

    this.#appendEnvironmentFields(config, picker, { includeProbe: true });

    const mode = element('div', 'config-row'); mode.append(element('label', 'config-label', 'Mode'));
    const select = element('select', 'select'); select.dataset.field = 'mode';
    [['single_agent', 'Single agent'], ['lattice', 'Full lattice'], ['custom', 'Custom workflow']]
      .forEach(([value, label]) => select.append(new Option(label, value)));
    select.value = picker.form.modeKind; mode.append(select, element('span', 'grow'), element('span', 'config-help', this.#modeHint(picker.form.modeKind))); config.append(mode);
    if (picker.form.modeKind === 'custom') {
      const agents = element('div', 'config-row wrap'); agents.append(element('span', 'config-label', 'Agents'));
      const list = element('div', 'check-list');
      const selectableAgents = sessionSelectableAgents(this.#agents);
      selectableAgents.forEach((agentValue) => {
        const agent = typeof agentValue === 'string' ? { id: agentValue } : agentValue;
        const label = element('label', 'check'); const input = document.createElement('input');
        input.type = 'checkbox'; input.dataset.field = 'custom-agent'; input.value = agent.id;
        input.checked = picker.form.customAgents.has(agent.id); label.append(input, document.createTextNode(agent.name || agent.id));
        if ((agent.depends_on || []).length) label.append(element('small', '', `← ${agent.depends_on.join(', ')}`)); list.append(label);
      });
      if (!selectableAgents.length) list.append(element('span', 'config-help', 'No Session agents configured.'));
      agents.append(list); config.append(agents);
    }
    return config;
  }

  #environmentPickerConfig(picker, session) {
    const config = element('div', 'config');
    const environment = session?.environment || {};
    const state = element('div', 'config-row wrap');
    state.append(element('span', 'config-label', 'Current environment'));
    const detail = environment.state === 'failed'
      ? `Failed · ${environment.error || 'Preparation did not complete'}`
      : environment.state === 'awaiting_approval'
        ? 'Awaiting an explicit setup decision'
        : environment.state === 'ready'
          ? `Ready${environment.effective_image ? ` · ${environment.effective_image}` : ''}`
          : environment.state === 'preparing' ? 'Preparing…' : 'Not prepared';
    state.append(element('span', 'config-help', detail));
    config.append(state);
    const runtimeCleanup = this.#runtimeCleanupConfirmation(picker, session);
    if (runtimeCleanup) config.append(runtimeCleanup);
    this.#appendEnvironmentFields(config, picker, { includeProbe: true });
    const evidence = this.#setupEvidence(environment);
    if (evidence) config.append(evidence);
    return config;
  }

  #runtimeCleanupConfirmation(picker, session) {
    const environment = session?.environment || {};
    const runtime = environment.runtime;
    const creation = environment.runtime_creation;
    const runtimeId = picker.form.runtimeCleanupId;
    const creationToken = picker.form.runtimeCreationToken;
    const retainedRuntime = runtime?.backend === 'e2b'
      && !runtime.cleanup_confirmed && runtimeId && runtime.id === runtimeId;
    const retainedCreation = !runtime && creation?.backend === 'e2b'
      && creationToken && creation.token === creationToken;
    if (!retainedRuntime && !retainedCreation) return null;
    const section = element('section', 'runtime-cleanup');
    if (retainedRuntime) {
      section.append(element('strong', '', 'Manual E2B runtime cleanup confirmation'));
      section.append(element(
        'div',
        'config-help',
        'Use this only after deleting this exact runtime in E2B. Axocoatl will release its retained cleanup record; this action does not contact or delete the runtime.',
      ));
      section.append(element('code', '', runtimeId));
    } else {
      section.append(element('strong', '', 'Manual E2B creation cleanup confirmation'));
      section.append(element(
        'div',
        'config-help',
        'Use this only after deleting every E2B sandbox with this exact metadata token. Axocoatl will release its retained creation record; this action does not contact or delete any sandbox.',
      ));
      section.append(element('code', '', `axocoatl_creation_token=${creationToken}`));
    }
    const affirmation = element('label', 'check');
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.dataset.field = 'runtime-cleanup-confirmed';
    checkbox.checked = picker.form.runtimeCleanupConfirmed;
    checkbox.disabled = picker.busy;
    affirmation.append(
      checkbox,
      document.createTextNode(retainedRuntime
        ? `I deleted E2B runtime ${runtimeId} outside Axocoatl`
        : `I deleted every E2B sandbox with metadata axocoatl_creation_token=${creationToken}`),
    );
    const confirm = element(
      'button',
      'button ghost',
      picker.busy ? 'Confirming…' : 'Confirm manual deletion',
    );
    confirm.type = 'button';
    confirm.dataset.action = 'confirm-runtime-cleanup';
    confirm.disabled = picker.busy || !picker.form.runtimeCleanupConfirmed;
    section.append(affirmation, confirm);
    return section;
  }

  async #confirmRuntimeCleanup(picker) {
    if (this.#picker !== picker || picker.kind !== 'environment'
        || !picker.form.runtimeCleanupConfirmed) return;
    const session = this.session(picker.targetSessionId);
    const environment = session?.environment || {};
    const runtime = environment.runtime;
    const creation = environment.runtime_creation;
    const runtimeId = picker.form.runtimeCleanupId;
    const creationToken = picker.form.runtimeCreationToken;
    const retainedRuntime = runtime?.backend === 'e2b'
      && !runtime.cleanup_confirmed && runtimeId && runtime.id === runtimeId;
    const retainedCreation = !runtime && creation?.backend === 'e2b'
      && creationToken && creation.token === creationToken;
    if (!retainedRuntime && !retainedCreation) {
      picker.error = 'The retained E2B cleanup target changed. Close this review and open it again.';
      this.#renderPicker();
      return;
    }
    picker.busy = true;
    picker.error = '';
    this.#renderPicker();
    try {
      const updated = await this.#request(
        `/api/sessions/${encodeURIComponent(session.id)}/environment/confirm-runtime-cleanup`,
        {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(retainedRuntime
            ? { runtime_id: runtimeId, confirmed: true }
            : {
              creation_token: creationToken,
              confirmed_all_matching_sandboxes_deleted: true,
            }),
        },
      );
      if (this.#picker === picker) this.#closePicker();
      await this.refresh();
      this.#notify(
        'Manual cleanup confirmed',
        retainedRuntime
          ? `E2B runtime ${runtimeId}`
          : `E2B creation token ${creationToken}`,
        'ok',
      );
      this.#environmentChanged(updated, 'confirm-runtime-cleanup');
    } catch (error) {
      if (this.#picker !== picker) return;
      picker.busy = false;
      picker.error = String(error?.message || error);
      this.#renderPicker();
    }
  }

  #setupEvidence(environment) {
    const results = Array.isArray(environment?.setup_results) ? environment.setup_results : [];
    if (!results.length) return null;
    const details = element('details', 'setup-evidence');
    const failed = results.some((result) => Number(result?.exit_code) !== 0);
    const summary = element(
      'summary',
      '',
      `Setup evidence · ${results.length} ${results.length === 1 ? 'command' : 'commands'}${failed ? ' · failed' : ''}`,
    );
    details.append(summary);
    results.forEach((result, index) => {
      const item = element('section', 'setup-result');
      item.setAttribute('aria-label', `Setup command ${index + 1}`);
      const head = element('div', 'setup-result-head');
      head.append(element('code', '', String(result?.command || '(command unavailable)')));
      const exitCode = Number.isFinite(Number(result?.exit_code)) ? Number(result.exit_code) : 'unknown';
      const exit = element('span', `setup-exit${exitCode === 0 ? '' : ' failed'}`, `Exit ${exitCode}`);
      head.append(exit); item.append(head);
      const stdout = String(result?.stdout || '');
      const stderr = String(result?.stderr || '');
      if (stdout) item.append(element('pre', 'setup-output', stdout));
      if (stderr) item.append(element('pre', 'setup-output error', stderr));
      if (!stdout && !stderr) item.append(element('div', 'config-help', 'No output recorded.'));
      details.append(item);
    });
    return details;
  }

  #appendEnvironmentFields(config, picker, { includeProbe }) {
    const image = element('div', 'config-row');
    const runtime = picker.probe?.runtime || {};
    const supportsSessionImage = runtime.supports_session_image !== false;
    if (supportsSessionImage) {
      image.append(element('label', 'config-label', 'Base image'));
      const preset = element('select', 'select'); preset.dataset.field = 'image-preset';
      [['', 'Default (alpine)'], ['docker.io/library/alpine:3.20', 'alpine:3.20 (minimal)'],
        ['docker.io/library/debian:bookworm-slim', 'debian:bookworm-slim'], ['docker.io/library/ubuntu:24.04', 'ubuntu:24.04'],
        ['docker.io/library/python:3.12-slim', 'python:3.12-slim'], ['docker.io/library/node:20-slim', 'node:20-slim'],
        ['docker.io/library/rust:bookworm', 'rust:bookworm'], ['__custom__', 'Custom…']]
        .forEach(([value, label]) => preset.append(new Option(label, value)));
      preset.value = picker.form.imagePreset; image.append(preset);
      if (picker.form.imagePreset === '__custom__') {
        const custom = element('input', 'input'); custom.type = 'text'; custom.placeholder = 'docker.io/your/image:tag';
        custom.dataset.field = 'custom-image'; custom.value = picker.form.customImage; image.append(custom);
      }
      image.append(element('span', 'config-help', 'The exact requested runtime; unsupported images fail visibly'));
    } else {
      image.append(element('label', 'config-label', 'Remote runtime'));
      const requested = (picker.form.imagePreset === '__custom__'
        ? picker.form.customImage : picker.form.imagePreset).trim();
      const preset = element('select', 'select'); preset.dataset.field = 'image-preset';
      const template = runtime.template || 'base';
      preset.append(new Option(`E2B template · ${template}`, ''));
      if (requested) preset.append(new Option(`Incompatible OCI image · ${requested}`, requested));
      preset.value = requested;
      preset.disabled = !requested;
      image.append(preset);
      const malformedNeedsTemplateDecision = picker.kind === 'environment'
        && picker.probe?.devcontainer?.error && !requested;
      if (malformedNeedsTemplateDecision) {
        const chooseTemplate = element(
          'button',
          'button ghost',
          picker.form.imageTouched ? 'E2B template selected' : 'Use E2B template',
        );
        chooseTemplate.type = 'button';
        chooseTemplate.dataset.action = 'confirm-e2b-template';
        chooseTemplate.disabled = picker.form.imageTouched;
        image.append(chooseTemplate);
      }
      image.append(element('span', 'config-help', requested
        ? 'This backend cannot honor the requested OCI image. Choose the E2B template to clear it before continuing.'
        : picker.form.imageTouched
          ? 'You explicitly selected the daemon-configured E2B template; no per-Session OCI image will be substituted.'
          : 'The daemon-configured E2B template owns the runtime; no per-Session OCI image will be substituted.'));
    }
    config.append(image);

    const setup = element('div', 'config-row wrap');
    setup.append(element('label', 'config-label', 'Project setup'));
    const command = element('input', 'input');
    command.type = 'text'; command.dataset.field = 'setup-command'; command.value = picker.form.setupCommand;
    command.placeholder = 'No project setup command'; command.autocomplete = 'off'; command.spellcheck = false;
    setup.append(command);
    const approval = element('label', 'check');
    const approved = document.createElement('input');
    approved.type = 'checkbox'; approved.dataset.field = 'setup-approved';
    approved.checked = picker.form.setupApproved; approved.disabled = !picker.form.setupCommand.trim();
    approval.append(approved, document.createTextNode('Run this exact command before Ready'));
    setup.append(approval);
    const suggestedSource = picker.probe?.suggested_setup?.source;
    const operatorPreapproval = suggestedSource === 'devcontainer'
      && picker.probe?.runtime?.auto_approve_devcontainer_setup === true;
    const help = operatorPreapproval
      ? 'Daemon policy defaults this exact devcontainer setup to approved. This checkbox is the Session decision: unchecked stays awaiting approval; editing clears approval.'
      : suggestedSource
      ? `Suggested from ${suggestedSource}. Unchecked keeps the Session awaiting approval; clearing records an explicit no-setup decision.`
      : 'Unchecked keeps the Session awaiting approval; clearing records an explicit no-setup decision.';
    setup.append(element('span', 'config-help', help));
    config.append(setup);

    if (includeProbe) {
      const probe = this.#projectProbe(picker); if (probe) config.append(probe);
    }
  }

  #projectProbe(picker) {
    const probe = picker.probe;
    const devcontainerError = probe?.devcontainer?.error;
    const devcontainer = probe?.devcontainer && !probe.devcontainer.error ? probe.devcontainer : null;
    const axocoatlFiles = Array.isArray(probe?.axocoatl_md) ? probe.axocoatl_md : [];
    if (!devcontainer && !devcontainerError && !axocoatlFiles.length && !picker.probeError) return null;
    const row = element('div', 'config-row wrap probe');
    if (picker.probeError) {
      const suffix = picker.kind === 'session' ? ' Close and retry before creating the Session.' : '';
      row.append(element('span', 'config-help', `Project config could not be inspected: ${picker.probeError}.${suffix}`));
      return row;
    }
    if (devcontainerError) {
      const block = element('div');
      block.append(element('strong', '', '⚠ devcontainer.json could not be read'));
      block.append(element('div', 'probe-line', devcontainerError));
      const remedy = picker.kind === 'session'
        ? 'Fix or remove this file before creating the Session. Axocoatl will not silently ignore it or substitute its default.'
        : picker.probe?.runtime?.supports_session_image === false
          ? 'Explicitly select the configured E2B template before rebuilding. Axocoatl will not silently ignore this file.'
          : 'Choose a Base image explicitly before rebuilding. Axocoatl will not silently substitute its default.';
      block.append(element('div', 'config-help', remedy));
      row.append(block);
    }
    if (devcontainer) {
      const block = element('div'); block.append(element('strong', '', '📦 devcontainer.json'));
      if (devcontainer.image) block.append(element('span', 'probe-line', ` ${devcontainer.image}`));
      if (devcontainer.post_create_scripts?.length) block.append(element('div', 'probe-line', `↳ post-create: ${devcontainer.post_create_scripts.join(' ; ')}`));
      if (devcontainer.forwarded_ports?.length) block.append(element('div', 'probe-line', `↳ ports: ${devcontainer.forwarded_ports.join(', ')}`));
      if (devcontainer.ignored_fields?.length) block.append(element('div', 'probe-line', `↳ ignored: ${devcontainer.ignored_fields.join(', ')}`));
      row.append(block);
    }
    if (axocoatlFiles.length) {
      const block = element('div'); block.append(element('strong', '', `📜 AXOCOATL.md · ${axocoatlFiles.length} file(s) on path`));
      axocoatlFiles.forEach((path) => block.append(element('div', 'probe-line', `↳ ${path}`))); row.append(block);
    }
    return row;
  }

  #agentSelect(selected) {
    const select = element('select', 'select');
    sessionSelectableAgents(this.#agents).forEach((value) => {
      const agent = typeof value === 'string' ? { id: value } : value;
      select.append(new Option(agent.name || agent.id, agent.id));
    });
    select.value = selected || ''; return select;
  }

  #modeHint(mode) {
    if (mode === 'lattice') return 'The full multi-agent lattice runs in topological order.';
    if (mode === 'custom') return 'Pick the agents; dependencies determine their order.';
    return 'One agent builds in the directory.';
  }

  async #usePicker() {
    const picker = this.#picker; if (!picker?.path || picker.busy) return;
    if (picker.kind === 'environment') {
      const session = this.session(picker.targetSessionId);
      if (!session) { picker.error = 'This Session is no longer available.'; this.#renderPicker(); return; }
      const form = picker.form;
      const image = (form.imagePreset === '__custom__' ? form.customImage : form.imagePreset).trim() || null;
      const setupCommand = form.setupCommand.trim() || null;
      picker.busy = true; picker.error = ''; this.#renderPicker();
      this.#environmentChanging(session, 'configure');
      try {
        const updated = await this.#request(`/api/sessions/${encodeURIComponent(session.id)}/environment`, {
          method: 'PUT', headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            image, setup_command: setupCommand,
            setup_approved: Boolean(setupCommand && form.setupApproved), setup_reviewed: true,
          }),
        });
        if (this.#picker === picker) this.#closePicker();
        await this.refresh();
        this.#environmentNotice(updated, 'Environment updated');
        this.#environmentChanged(updated, 'configure');
      } catch (error) {
        await this.refresh();
        this.#environmentChanged(this.session(session.id) || session, 'configure-error');
        if (this.#picker !== picker) return;
        picker.busy = false; picker.error = String(error?.message || error); this.#renderPicker();
      }
      return;
    }
    if (picker.kind === 'workspace') {
      const name = picker.form.workspaceName.trim();
      if (!name) { picker.error = 'Give this Workspace a name.'; this.#renderPicker(); return; }
      picker.busy = true; picker.error = ''; this.#renderPicker();
      try {
        const existing = this.#workspaces.find((workspace) => workspaceDirectory(workspace) === picker.path);
        const body = { path: picker.path };
        // Reopening a known folder is selection, not an implicit rename. Only
        // send a name for a new Workspace or when the person edited the field.
        if (!existing || picker.form.workspaceNameTouched) body.name = name;
        const workspace = await this.#request('/api/workspaces', {
          method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
        });
        if (this.#picker === picker) this.#closePicker();
        await this.refresh();
        this.selectWorkspace(workspace.id);
        this.#notify(existing ? 'Workspace selected' : 'Workspace opened', workspace.name, 'ok');
        this.dispatchEvent(new CustomEvent('workspace-open', {
          detail: { workspace, source: existing ? 'existing-folder' : 'created' }, bubbles: true, composed: true,
        }));
      } catch (error) {
        if (this.#picker !== picker) return;
        picker.busy = false; picker.error = String(error?.message || error); this.#renderPicker();
      }
      return;
    }
    const workspace = this.workspace(picker.workspaceId);
    if (!workspace) { picker.error = 'This Workspace is no longer available.'; this.#renderPicker(); return; }
    const form = picker.form;
    let mode;
    if (form.modeKind === 'single_agent') {
      if (!form.agentId) { picker.error = 'Configure an agent before creating this session.'; this.#renderPicker(); return; }
      mode = { kind: 'single_agent', agent_id: form.agentId };
    } else if (form.modeKind === 'custom') {
      const agents = [...form.customAgents];
      if (!agents.length) { picker.error = 'Pick at least one agent.'; this.#renderPicker(); return; }
      mode = { kind: 'custom', agents };
    } else mode = { kind: 'lattice' };
    const ports = form.exposedPorts.trim() ? form.exposedPorts.split(/[,\s]+/)
      .map((value) => Number.parseInt(value, 10)).filter((value) => Number.isFinite(value) && value > 0 && value < 65536) : [];
    const image = (form.imagePreset === '__custom__' ? form.customImage : form.imagePreset).trim() || null;
    picker.busy = true; picker.error = ''; this.#renderPicker();
    const navigationGeneration = this.#navigationGeneration;
    try {
      const session = await this.#request(`/api/workspaces/${encodeURIComponent(workspace.id)}/sessions`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          name: form.sessionName.trim() || 'Untitled Session', mode,
          enabled_skills: [...form.enabledSkills], exposed_ports: ports, image,
          setup_command: form.setupCommand.trim() || null,
          setup_approved: Boolean(form.setupCommand.trim() && form.setupApproved),
          setup_reviewed: true,
        }),
      });
      if (this.#picker === picker) this.#closePicker();
      this.#environmentNotice(session, 'Session created');
      await this.refresh();
      this.selectWorkspace(workspace.id);
      if (this.#navigationGeneration === navigationGeneration) this.#openSession(session, 'created');
    } catch (error) {
      if (this.#picker !== picker) return;
      picker.busy = false; picker.error = String(error?.message || error); this.#renderPicker();
    }
  }

  #ask(options) {
    const returnFocus = this.#dialog?.returnFocus || deepActiveElement();
    if (this.#dialog) this.#resolveDialog(this.#dialog.kind === 'confirm' ? false : null);
    return new Promise((resolve) => {
      this.#dialog = {
        kind: 'prompt', label: '', value: '', okLabel: 'Save', danger: false,
        ...options, resolve, returnFocus,
      };
      this.#renderDialog();
      queueMicrotask(() => {
        const input = this.#root.querySelector('.dialog-input'); input?.focus(); input?.select?.();
        if (!input) this.#root.querySelector('[data-action="dialog-ok"]')?.focus();
      });
    });
  }

  #resolveDialog(value) {
    const dialog = this.#dialog; if (!dialog) return;
    this.#dialog = null; this.#renderDialog(); dialog.resolve(value);
    queueMicrotask(() => dialog.returnFocus?.isConnected && dialog.returnFocus.focus?.());
  }

  #renderDialog() {
    const host = this.#root.querySelector('.dialog-host');
    if (!host) return;
    host.replaceChildren(); const dialog = this.#dialog; if (!dialog) return;
    const overlay = element('div', 'overlay'); overlay.dataset.action = 'dialog-backdrop';
    const modal = element('div', 'modal small'); modal.setAttribute('role', 'dialog'); modal.setAttribute('aria-modal', 'true');
    modal.setAttribute('aria-labelledby', 'session-dialog-title');
    const title = element('div', 'modal-head', dialog.title); title.id = 'session-dialog-title'; modal.append(title);
    const body = element('div', 'dialog-body');
    if (dialog.body) body.append(element('p', '', dialog.body));
    if (dialog.kind !== 'confirm') {
      if (dialog.label) body.append(element('label', 'config-label', dialog.label));
      const input = element('input', 'input dialog-input'); input.type = 'text'; input.value = dialog.value || ''; input.dataset.field = 'dialog-value'; body.append(input);
    }
    modal.append(body);
    const foot = element('div', 'modal-foot'); foot.append(element('span', 'grow'));
    const cancel = element('button', 'button ghost', 'Cancel'); cancel.type = 'button'; cancel.dataset.action = 'dialog-cancel';
    const ok = element('button', `button${dialog.danger ? ' danger' : ''}`, dialog.okLabel || 'Continue'); ok.type = 'button'; ok.dataset.action = 'dialog-ok';
    foot.append(cancel, ok); modal.append(foot); overlay.append(modal); host.append(overlay);
  }

  #notify(title, body = '', kind = 'info') {
    this.#notice = { title, body, kind }; this.#renderNotice();
    this.dispatchEvent(new CustomEvent('notify', { detail: { title, body, kind }, bubbles: true, composed: true }));
  }

  #environmentNotice(session, readyTitle = 'Environment ready') {
    const environment = session?.environment || {};
    if (environment.state === 'ready') {
      this.#notify(readyTitle, environment.effective_image || 'The Session environment is ready.', 'ok');
    } else if (environment.state === 'failed') {
      this.#notify('Environment preparation failed', environment.error || 'Change the runtime or setup command, then rebuild.', 'err');
    } else if (environment.state === 'awaiting_approval') {
      const command = environment.setup_command ? `Review the exact command: ${environment.setup_command}` : 'Choose a setup command or explicitly continue without one.';
      this.#notify('Setup decision required', command, 'info');
    } else {
      this.#notify('Environment is not ready', `Current state: ${environment.state || 'unprepared'}.`, 'info');
    }
  }

  #environmentChanged(session, source) {
    this.dispatchEvent(new CustomEvent('session-environment-change', {
      detail: { session, source }, bubbles: true, composed: true,
    }));
  }

  #environmentChanging(session, source) {
    this.dispatchEvent(new CustomEvent('session-environment-changing', {
      detail: { session, source }, bubbles: true, composed: true,
    }));
  }

  async #request(url, options) {
    const response = await fetch(url, options);
    const body = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(body?.error || `HTTP ${response.status}`);
    if (body?.error) throw new Error(body.error);
    return body;
  }

  #onClick(event) {
    if (this.#menu && !event.target.closest('.context')) {
      this.#menu = null;
      this.#renderMenu();
    }
    const actionElement = event.target.closest('[data-action]');
    if (actionElement) {
      const action = actionElement.dataset.action;
      if ((action === 'picker-backdrop' || action === 'dialog-backdrop') && event.target !== actionElement) return;
      if (this.#picker?.busy && ['picker-backdrop', 'picker-cancel', 'picker-navigate', 'retry-picker'].includes(action)) return;
      if (action === 'open-workspace') this.openWorkspace();
      else if (action === 'new-session') this.newSession(this.#selectedWorkspaceId);
      else if (action === 'rename-workspace') void this.renameWorkspace(actionElement.dataset.workspaceId);
      else if (action === 'back') this.#goBack();
      else if (action === 'forward') this.#goForward();
      else if (action === 'up') this.#goUp();
      else if (action === 'navigate') void this.navigate(actionElement.dataset.path);
      else if (action === 'retry-sessions') void this.refresh();
      else if (action === 'retry-path') void this.navigate(this.#currentPath, { pushHistory: false });
      else if (action === 'retry-picker') void this.#pickerNavigate(this.#picker?.requestedPath || this.#picker?.path || '');
      else if (action === 'confirm-e2b-template' && this.#picker) {
        this.#picker.form.imagePreset = '';
        this.#picker.form.customImage = '';
        this.#picker.form.imageTouched = true;
        this.#renderPicker();
      }
      else if (action === 'confirm-runtime-cleanup' && this.#picker) {
        void this.#confirmRuntimeCleanup(this.#picker);
      }
      else if (action === 'picker-navigate') void this.#pickerNavigate(actionElement.dataset.path);
      else if (action === 'picker-use') void this.#usePicker();
      else if (action === 'picker-cancel' || action === 'picker-backdrop') this.#closePicker();
      else if (action === 'menu-action') void this.#runMenuAction(actionElement.dataset.menuAction);
      else if (action === 'dialog-cancel' || action === 'dialog-backdrop') this.#resolveDialog(this.#dialog?.kind === 'confirm' ? false : null);
      else if (action === 'dialog-ok') this.#resolveDialog(this.#dialog?.kind === 'confirm' ? true : this.#dialog?.value?.trim());
      else if (action === 'dismiss-notice') { this.#notice = null; this.#renderNotice(); }
      return;
    }
    const scope = event.target.closest('[data-scope]');
    if (scope) {
      scope.dataset.scope === 'all' ? this.showAllSessions() : this.showWorkspaces();
      return;
    }
    const workspaceRow = event.target.closest('[data-workspace-id]');
    if (workspaceRow) {
      const workspace = this.selectWorkspace(workspaceRow.dataset.workspaceId);
      if (workspace) this.dispatchEvent(new CustomEvent('workspace-open', {
        detail: { workspace, source: 'browser' }, bubbles: true, composed: true,
      }));
      return;
    }
    const session = event.target.closest('[data-session-id]');
    if (session) this.#openSession(this.session(session.dataset.sessionId), 'all-sessions');
  }

  #onContextMenu(event) {
    const session = event.target.closest('[data-session-id]');
    const workspace = event.target.closest('[data-workspace-id]');
    if (!session && !workspace) return;
    event.preventDefault();
    if (session) {
      this.#selectSession(session.dataset.sessionId);
      this.#showMenu('session', session.dataset.sessionId, event.clientX, event.clientY);
    } else this.#showMenu('workspace', workspace.dataset.workspaceId, event.clientX, event.clientY);
  }

  #onInput(event) {
    if (event.target.matches('.search')) {
      this.#searchTerm = event.target.value || ''; this.#renderRows(); return;
    }
    const field = event.target.dataset.field;
    if (field === 'dialog-value' && this.#dialog) this.#dialog.value = event.target.value;
    else if (field === 'workspace-name' && this.#picker) {
      this.#picker.form.workspaceName = event.target.value;
      this.#picker.form.workspaceNameTouched = true;
      const use = this.#root.querySelector('[data-action="picker-use"]');
      if (use) use.disabled = this.#picker.busy || !event.target.value.trim();
    } else if (field === 'session-name' && this.#picker) {
      this.#picker.form.sessionName = event.target.value;
      const use = this.#root.querySelector('[data-action="picker-use"]');
      if (use) use.disabled = this.#picker.busy || !event.target.value.trim();
    }
    else if (field === 'ports' && this.#picker) this.#picker.form.exposedPorts = event.target.value;
    else if (field === 'setup-command' && this.#picker) {
      this.#picker.form.setupCommand = event.target.value;
      this.#picker.form.setupTouched = true;
      this.#picker.form.setupApproved = false;
      const approval = this.#root.querySelector('[data-field="setup-approved"]');
      if (approval) { approval.checked = false; approval.disabled = !event.target.value.trim(); }
    }
    else if (field === 'custom-image' && this.#picker) {
      this.#picker.form.customImage = event.target.value; this.#picker.form.imageTouched = true;
    }
  }

  #onChange(event) {
    const picker = this.#picker; if (!picker) return;
    const field = event.target.dataset.field;
    if (field === 'copy-from') { picker.form.copyFrom = event.target.value; if (event.target.value) this.#applyCopiedSession(event.target.value); }
    else if (field === 'skill') event.target.checked ? picker.form.enabledSkills.add(event.target.value) : picker.form.enabledSkills.delete(event.target.value);
    else if (field === 'custom-agent') event.target.checked ? picker.form.customAgents.add(event.target.value) : picker.form.customAgents.delete(event.target.value);
    else if (field === 'agent') picker.form.agentId = event.target.value;
    else if (field === 'setup-approved') picker.form.setupApproved = Boolean(event.target.checked && picker.form.setupCommand.trim());
    else if (field === 'runtime-cleanup-confirmed') {
      picker.form.runtimeCleanupConfirmed = Boolean(event.target.checked);
      this.#renderPicker();
    }
    else if (field === 'mode') { picker.form.modeKind = event.target.value; picker.error = ''; this.#renderPicker(); }
    else if (field === 'image-preset') {
      picker.form.imagePreset = event.target.value; picker.form.imageTouched = true;
      if (event.target.value !== '__custom__') picker.form.customImage = ''; this.#renderPicker();
    }
  }

  #onKeyDown(event) {
    if (event.key === 'Tab' && (this.#dialog || this.#picker)) {
      const modal = this.#root.querySelector(this.#dialog ? '.dialog-host .modal' : '.picker-host .modal');
      const focusable = modal ? Array.from(modal.querySelectorAll(MODAL_FOCUSABLE)) : [];
      if (!focusable.length) { event.preventDefault(); modal?.focus(); return; }
      const current = event.composedPath()[0];
      const index = focusable.indexOf(current);
      if (event.shiftKey && index <= 0) {
        event.preventDefault(); focusable[focusable.length - 1].focus();
      } else if (!event.shiftKey && (index < 0 || index === focusable.length - 1)) {
        event.preventDefault(); focusable[0].focus();
      }
      return;
    }
    if (event.key === 'Escape') {
      if (this.#dialog || this.#picker || this.#menu) {
        event.preventDefault();
        event.stopPropagation();
      }
      if (this.#dialog) this.#resolveDialog(this.#dialog.kind === 'confirm' ? false : null);
      else if (this.#picker && !this.#picker.busy) this.#closePicker();
      else if (this.#menu) { this.#menu = null; this.#renderMenu(); }
      return;
    }
    if (event.key === 'Enter' && event.target.matches('.dialog-input')) {
      event.preventDefault(); this.#resolveDialog(this.#dialog?.value?.trim()); return;
    }
    if (event.target.closest('button[data-action]')) return;
    if (event.key !== 'Enter' && event.key !== ' ') return;
    const scope = event.target.closest('[data-scope]');
    const workspace = event.target.closest('[data-workspace-id]');
    const session = event.target.closest('[data-session-id]');
    if (!scope && !workspace && !session) return;
    event.preventDefault();
    if (scope) scope.dataset.scope === 'all' ? this.showAllSessions() : this.showWorkspaces();
    else if (workspace) {
      const selected = this.selectWorkspace(workspace.dataset.workspaceId);
      if (selected) this.dispatchEvent(new CustomEvent('workspace-open', {
        detail: { workspace: selected, source: 'keyboard' }, bubbles: true, composed: true,
      }));
    } else this.#openSession(this.session(session.dataset.sessionId), 'all-sessions');
  }
}

if (!customElements.get('ax-session-home')) customElements.define('ax-session-home', AxSessionHome);
