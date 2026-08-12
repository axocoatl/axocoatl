import { adopt } from './sheets.js';

/**
 * `<ax-session-home>` owns the folder-anchored session browser and creation flow.
 *
 * The component deliberately stops at opening a session. The shell owns the
 * chat/cockpit and listens for `session-open`; this element never creates a
 * second session workspace or keeps cockpit state.
 *
 * @element ax-session-home
 * @fires session-open      detail: {session, source}
 * @fires session-open-new  detail: {session}; cancelable before the fallback window opens
 * @fires session-closed    detail: {session}
 * @fires session-deleted   detail: {session}
 * @fires sessions-change   detail: {sessions, count}
 * @fires favorites-change  detail: {favorites}
 * @fires notify            detail: {title, body, kind}
 */

const FAVORITES_KEY = 'axo.finder.favorites.v1';

const CSS = `
:host {
  display: flex; flex: 1; min-width: 0; min-height: 0;
  color: var(--text); font: var(--fs-body) / var(--lh-body) var(--font-sans);
}
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
.side-head.recent { margin-top: var(--sp-2); padding-top: var(--sp-3); border-top: 1px solid var(--border); }
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
  width: 20px; height: 20px; padding: 0; border: 0; border-radius: var(--r-sm);
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
  padding: 4px 10px; border: 1px solid var(--border); border-radius: var(--r-sm);
  background: var(--bg-2); font: var(--fs-xs) var(--font-mono); white-space: nowrap;
}
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
  display: grid; grid-template-columns: minmax(220px, 1fr) 110px 90px 130px;
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
.status-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--ok); box-shadow: 0 0 5px color-mix(in srgb, var(--ok) 65%, transparent); }
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

function loadFavorites() {
  try {
    const parsed = JSON.parse(localStorage.getItem(FAVORITES_KEY) || '[]');
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((item) => item && typeof item.path === 'string' && item.path)
      .map((item) => ({ path: item.path, label: String(item.label || folderName(item.path)) }));
  } catch {
    return [];
  }
}

function defaultForm(agents) {
  const first = agents[0];
  return {
    copyFrom: '', enabledSkills: new Set(), exposedPorts: '', imagePreset: '', customImage: '',
    imageTouched: false, modeKind: 'single_agent', agentId: first?.id || first || '',
    customAgents: new Set(),
  };
}

export class AxSessionHome extends HTMLElement {
  #root;
  #agents = [];
  #sessions = [];
  #favorites = loadFavorites();
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
  #dragFavoriteIndex = null;
  #started = false;
  #outsidePointer;
  #viewportChange;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS, []);
    this.#root.addEventListener('click', (event) => this.#onClick(event));
    this.#root.addEventListener('dblclick', (event) => this.#onDoubleClick(event));
    this.#root.addEventListener('contextmenu', (event) => this.#onContextMenu(event));
    this.#root.addEventListener('input', (event) => this.#onInput(event));
    this.#root.addEventListener('change', (event) => this.#onChange(event));
    this.#root.addEventListener('keydown', (event) => this.#onKeyDown(event));
    this.#root.addEventListener('dragstart', (event) => this.#onDragStart(event));
    this.#root.addEventListener('dragover', (event) => this.#onDragOver(event));
    this.#root.addEventListener('dragleave', (event) => this.#onDragLeave(event));
    this.#root.addEventListener('drop', (event) => this.#onDrop(event));
    this.#root.addEventListener('dragend', () => this.#clearDrag());
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
    if (this.#picker && !this.#picker.form.agentId) {
      const first = this.#agents[0];
      this.#picker.form.agentId = first?.id || first || '';
    }
    this.#renderRows();
    this.#renderPicker();
  }

  get sessions() { return this.#sessions.slice(); }
  get favorites() { return this.#favorites.map((item) => ({ ...item })); }
  get currentPath() { return this.#currentPath; }

  session(id) { return this.#sessions.find((item) => item.id === id) || null; }
  noteExternalNavigation() { this.#navigationGeneration += 1; }

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
      const sessions = await this.#request('/api/sessions');
      if (generation !== this.#refreshGeneration) return false;
      if (!Array.isArray(sessions)) throw new Error('The sessions endpoint returned an invalid list.');
      this.#sessions = sessions;
      this.#phase = 'ready';
      if (this.#selectedSession && !sessions.some((item) => item.id === this.#selectedSession)) {
        this.#selectedSession = '';
      }
      this.dispatchEvent(new CustomEvent('sessions-change', {
        detail: { sessions: this.sessions, count: sessions.length }, bubbles: true, composed: true,
      }));
      this.#renderSidebar();
      if (!this.#currentPath) {
        const recent = sessions.slice().sort((a, b) => (b.last_active || 0) - (a.last_active || 0));
        this.#currentPath = recent.length ? sessionDirectory(recent[0]) : (this.#favorites[0]?.path || '');
      }
      if (this.#currentPath) return this.navigate(this.#currentPath, { pushHistory: false });
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

  addFolder(initialPath = '') { this.#openPicker('favorite', initialPath); }

  newSession(initialPath = this.#currentPath) { this.#openPicker('session', initialPath || ''); }

  #render() {
    this.#root.innerHTML = `
      <div class="surface">
        <div class="notice-host" aria-live="polite"></div>
        <div class="finder">
          <aside class="sidebar">
            <div class="side-head">Favorites</div>
            <div class="side-list favorites"></div>
            <button class="add-folder" data-action="add-folder">+ Add folder</button>
            <div class="side-head recent">Recent sessions</div>
            <div class="side-list recents"></div>
          </aside>
          <main class="main">
            <div class="toolbar">
              <button class="nav" data-action="back" title="Back" aria-label="Back">‹</button>
              <button class="nav" data-action="forward" title="Forward" aria-label="Forward">›</button>
              <button class="nav" data-action="up" title="Up one level" aria-label="Up one level">⤴</button>
              <div class="path"></div>
              <input class="search" type="search" placeholder="Filter…" aria-label="Filter folders and sessions" spellcheck="false">
              <button class="button" data-action="new-session">＋ New session</button>
            </div>
            <div class="grid">
              <div class="columns"><span>Name</span><span>Mode</span><span>Agents</span><span>Last active</span></div>
              <div class="rows"></div>
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
    const favorites = this.#root.querySelector('.favorites');
    const recents = this.#root.querySelector('.recents');
    if (!favorites || !recents) return;
    favorites.replaceChildren();
    if (!this.#favorites.length) favorites.append(element('div', 'side-empty', 'No folders yet.'));
    this.#favorites.forEach((favorite, index) => {
      const row = element('div', `side-row${favorite.path === this.#currentPath ? ' active' : ''}`);
      row.tabIndex = 0; row.draggable = true; row.title = favorite.path;
      row.dataset.favoriteIndex = String(index); row.dataset.favoritePath = favorite.path;
      row.append(element('span', 'side-icon', '📁'));
      row.append(element('span', 'side-label', favorite.label || folderName(favorite.path)));
      const remove = element('button', 'side-remove', '×');
      remove.type = 'button'; remove.dataset.action = 'remove-favorite'; remove.dataset.path = favorite.path;
      remove.title = 'Remove from favorites'; remove.setAttribute('aria-label', `Remove ${favorite.label || favorite.path}`);
      row.append(remove); favorites.append(row);
    });

    recents.replaceChildren();
    const recent = this.#sessions.slice().sort((a, b) => (b.last_active || 0) - (a.last_active || 0)).slice(0, 6);
    if (!recent.length) recents.append(element('div', 'side-empty', this.#phase === 'loading' ? 'Loading…' : 'No sessions yet.'));
    recent.forEach((session) => {
      const row = element('div', 'side-row');
      row.tabIndex = 0; row.title = sessionDirectory(session); row.dataset.recentSession = session.id;
      const icon = element('span', 'side-icon'); icon.append(this.#mark());
      row.append(icon, element('span', 'side-label', session.name || folderName(sessionDirectory(session)) || session.id));
      recents.append(row);
    });
  }

  #renderPath() {
    const host = this.#root.querySelector('.path');
    if (!host) return;
    host.replaceChildren();
    const back = this.#root.querySelector('[data-action="back"]');
    const forward = this.#root.querySelector('[data-action="forward"]');
    const up = this.#root.querySelector('[data-action="up"]');
    if (back) back.disabled = !this.#back.length;
    if (forward) forward.disabled = !this.#forward.length;
    if (up) up.disabled = !this.#contents.parent;
    if (!this.#currentPath) {
      host.append(element('span', 'muted', 'No folder selected'));
      return;
    }
    const parts = this.#currentPath.split('/').filter(Boolean);
    if (!parts.length) {
      const root = element('button', 'crumb', '/');
      root.type = 'button'; root.dataset.action = 'navigate'; root.dataset.path = '/';
      host.append(root);
      return;
    }
    let accumulated = '';
    parts.forEach((part, index) => {
      accumulated += `/${part}`;
      if (index) host.append(element('span', 'sep', '›'));
      const crumb = element('button', 'crumb', part);
      crumb.type = 'button'; crumb.dataset.action = 'navigate'; crumb.dataset.path = accumulated;
      host.append(crumb);
    });
  }

  #renderRows() {
    const host = this.#root.querySelector('.rows');
    if (!host) return;
    host.replaceChildren();
    if (this.#phase === 'loading' && !this.#sessions.length) {
      host.append(element('div', 'state', 'Loading sessions…')); return;
    }
    if (this.#phase === 'error') {
      host.append(this.#errorState('Sessions could not be loaded', this.#sessionError, 'retry-sessions')); return;
    }
    if (!this.#currentPath) {
      host.append(element('div', 'state', 'Pick a folder on the left, or click ＋ Add folder.')); return;
    }
    if (this.#pathPhase === 'loading') {
      host.append(element('div', 'state', 'Opening folder…')); return;
    }
    if (this.#pathPhase === 'error') {
      host.append(this.#errorState('This folder could not be opened', this.#pathError, 'retry-path')); return;
    }
    const term = this.#searchTerm.trim().toLowerCase();
    const directories = (this.#contents.dirs || []).filter((item) => !term || String(item.name || '').toLowerCase().includes(term));
    const sessions = this.#sessions
      .filter((item) => sessionDirectory(item) === this.#currentPath)
      .filter((item) => !term || String(item.name || '').toLowerCase().includes(term));
    if (!directories.length && !sessions.length) {
      host.append(element('div', 'state', term
        ? 'No folders or sessions match this filter.'
        : 'This folder has no subfolders and no sessions yet. Click ＋ New session to start one here.'));
      return;
    }
    directories.forEach((directory) => host.append(this.#folderRow(directory)));
    sessions.forEach((session) => host.append(this.#sessionRow(session)));
  }

  #folderRow(directory) {
    const row = element('div', 'row folder-row');
    row.tabIndex = 0; row.dataset.folderPath = directory.path;
    const name = element('div', 'name');
    name.append(element('span', 'name-icon folder', '📁'), element('span', 'name-text', directory.name));
    row.append(name, element('span', 'meta', '—'), element('span', 'meta', '—'), element('span', 'meta', '—'));
    return row;
  }

  #sessionRow(session) {
    const row = element('div', `row session-row${session.id === this.#selectedSession ? ' active' : ''}`);
    row.tabIndex = 0; row.dataset.sessionId = session.id;
    const name = element('div', 'name');
    const icon = element('span', 'name-icon'); icon.append(this.#mark());
    name.append(icon, element('span', 'name-text', session.name || folderName(sessionDirectory(session)) || session.id));
    if (session.status === 'active' || session.status === 'closed') {
      const dot = element('span', `status-dot${session.status === 'closed' ? ' closed' : ''}`);
      dot.title = session.status === 'closed' ? 'Closed' : 'Active'; name.append(dot);
    }
    const kind = session.mode?.kind;
    const mode = kind === 'single_agent' ? `agent · ${session.mode.agent_id || '—'}`
      : kind === 'custom' ? `custom · ${(session.mode.agents || []).length}` : 'lattice';
    const count = kind === 'custom' ? (session.mode.agents || []).length : kind === 'lattice' ? this.#agents.length : 1;
    row.append(name, element('span', 'meta', mode), element('span', 'meta', String(count)), element('span', 'meta', relativeTime(session.last_active)));
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
    this.#navigationGeneration += 1;
    this.dispatchEvent(new CustomEvent('session-open', {
      detail: { session, source }, bubbles: true, composed: true,
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

  #saveFavorites() {
    try { localStorage.setItem(FAVORITES_KEY, JSON.stringify(this.#favorites)); } catch {}
    this.dispatchEvent(new CustomEvent('favorites-change', {
      detail: { favorites: this.favorites }, bubbles: true, composed: true,
    }));
  }

  #addFavorite(path) {
    if (!path) return;
    if (this.#favorites.some((item) => item.path === path)) {
      this.#notify('Already in favorites', path, 'info'); return;
    }
    this.#favorites.push({ path, label: folderName(path) || path });
    this.#saveFavorites(); this.#renderSidebar();
    this.#notify('Added to favorites', folderName(path) || path, 'ok');
  }

  #removeFavorite(path) {
    const index = this.#favorites.findIndex((item) => item.path === path);
    if (index < 0) return;
    this.#favorites.splice(index, 1); this.#saveFavorites(); this.#renderSidebar();
    if (this.#currentPath === path && this.#contents.parent) void this.navigate(this.#contents.parent);
  }

  async #renameFavorite(favorite) {
    const value = await this.#ask({ title: 'Rename favorite', label: 'Name', value: favorite.label || favorite.path });
    if (!value?.trim()) return;
    favorite.label = value.trim(); this.#saveFavorites(); this.#renderSidebar();
  }

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
    const ok = await this.#ask({
      kind: 'confirm', title: 'Close this session?',
      body: `“${session.name}”'s container will stop. You can reopen it later — its history stays.`,
      okLabel: 'Close session',
    });
    if (!ok) return;
    try {
      await this.#request(`/api/sessions/${encodeURIComponent(session.id)}`, { method: 'DELETE' });
      this.#notify('Session closed', session.name, 'ok');
      this.dispatchEvent(new CustomEvent('session-closed', { detail: { session }, bubbles: true, composed: true }));
      await this.refresh();
    } catch (error) { this.#notify('Close failed', String(error?.message || error), 'err'); }
  }

  async #deleteSession(session) {
    const ok = await this.#ask({
      kind: 'confirm', title: 'Delete this session permanently?',
      body: `“${session.name}”'s config and history will be gone. Files in ${sessionDirectory(session)} are not touched.`,
      okLabel: 'Delete session', danger: true,
    });
    if (!ok) return;
    try {
      await this.#request(`/api/sessions/${encodeURIComponent(session.id)}?force=true`, { method: 'DELETE' });
      this.#notify('Session deleted', session.name, 'ok');
      this.dispatchEvent(new CustomEvent('session-deleted', { detail: { session }, bubbles: true, composed: true }));
      await this.refresh();
    } catch (error) { this.#notify('Delete failed', String(error?.message || error), 'err'); }
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
      return [
        { label: 'Open', action: 'open' },
        { label: 'Open in new window', action: 'open-window' },
        { separator: true },
        { label: 'Rename…', action: 'rename' },
        { label: 'Copy working directory', action: 'copy-path' },
        { separator: true },
        { label: session.status === 'active' ? 'Close' : 'Close (already closed)', action: 'close', disabled: session.status !== 'active' },
        { label: 'Delete…', action: 'delete', danger: true },
      ];
    }
    if (this.#menu?.kind === 'folder') {
      const favorite = this.#favorites.some((item) => item.path === this.#menu.id);
      return [
        { label: 'Open', action: 'open' }, { label: 'New session here…', action: 'new-session' },
        { separator: true },
        { label: favorite ? 'Remove from favorites' : 'Add to favorites', action: favorite ? 'remove-favorite' : 'add-favorite' },
      ];
    }
    if (this.#menu?.kind === 'favorite') {
      return [
        { label: 'Open', action: 'open' }, { label: 'Rename…', action: 'rename' },
        { separator: true }, { label: 'Remove from favorites', action: 'remove-favorite', danger: true },
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
    const path = menu.id;
    if (action === 'open') void this.navigate(path);
    else if (action === 'new-session') this.newSession(path);
    else if (action === 'add-favorite') this.#addFavorite(path);
    else if (action === 'remove-favorite') this.#removeFavorite(path);
    else if (action === 'rename') {
      const favorite = this.#favorites.find((item) => item.path === path);
      if (favorite) await this.#renameFavorite(favorite);
    }
  }

  #openPicker(kind, initialPath) {
    this.#menu = null;
    if (!this.#picker) this.#pickerReturnFocus = deepActiveElement();
    this.#picker = {
      kind, path: '', requestedPath: initialPath || '', dirs: [], parent: null,
      phase: 'loading', error: '', skills: [], skillsError: '',
      probe: null, probeError: '', busy: false, generation: 0, form: defaultForm(this.#agents),
    };
    this.#renderPicker();
    void this.#loadPickerSkills();
    void this.#pickerNavigate(initialPath || '');
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
      picker.phase = 'ready'; this.#renderPicker();
      void this.#probePickerProject(picker, picker.path, generation);
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
      if (image && !picker.form.imageTouched) this.#setPickerImage(image, false);
    } catch (error) {
      if (this.#picker !== picker || picker.generation !== generation) return;
      picker.probeError = String(error?.message || error);
    }
    this.#renderPicker();
  }

  #setPickerImage(image, touched = true) {
    const picker = this.#picker; if (!picker) return;
    const known = ['', 'docker.io/library/alpine:3.20', 'docker.io/library/debian:bookworm-slim',
      'docker.io/library/ubuntu:24.04', 'docker.io/library/python:3.12-slim',
      'docker.io/library/node:20-slim', 'docker.io/library/rust:bookworm'];
    if (known.includes(image || '')) {
      picker.form.imagePreset = image || ''; picker.form.customImage = '';
    } else {
      picker.form.imagePreset = '__custom__'; picker.form.customImage = image || '';
    }
    picker.form.imageTouched = touched;
  }

  #applyCopiedSession(id) {
    const picker = this.#picker; const session = this.session(id);
    if (!picker || !session) return;
    const form = picker.form;
    form.copyFrom = id; form.exposedPorts = (session.exposed_ports || []).join(', ');
    this.#setPickerImage(session.image || '', true);
    form.modeKind = session.mode?.kind || 'single_agent';
    form.agentId = session.mode?.agent_id || form.agentId;
    form.customAgents = new Set(session.mode?.agents || []);
    form.enabledSkills = new Set(session.enabled_skills || []);
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
    const title = element('div', 'modal-head', picker.kind === 'favorite'
      ? 'Add a folder Axocoatl can work in'
      : `New session${picker.path ? ` — in ${picker.path}` : ' — choose a project folder'}`);
    title.id = 'session-picker-title';
    modal.append(title);
    modal.append(element('div', 'picker-path', picker.path || picker.requestedPath || 'Choose a folder'));
    const list = element('div', 'picker-list');
    if (picker.phase === 'loading') list.append(element('div', 'state', 'Opening folder…'));
    else if (picker.phase === 'error') {
      list.append(this.#errorState('This folder could not be opened', picker.error, 'retry-picker'));
    } else {
      if (picker.parent) list.append(this.#pickerPathRow(picker.parent, '↑', '.. (parent)'));
      picker.dirs.forEach((directory) => list.append(this.#pickerPathRow(directory.path, '▸', directory.name)));
      if (!picker.parent && !picker.dirs.length) list.append(element('div', 'state', 'No subfolders. You can use this folder.'));
    }
    modal.append(list);
    if (picker.kind === 'session') modal.append(this.#pickerConfig(picker));
    if (picker.error && picker.phase !== 'error') modal.append(element('div', 'inline-error', picker.error));
    const foot = element('div', 'modal-foot');
    if (picker.kind === 'session' && picker.form.modeKind === 'single_agent') {
      foot.append(element('span', 'config-label', 'Agent'));
      const agents = this.#agentSelect(picker.form.agentId); agents.dataset.field = 'agent'; foot.append(agents);
    }
    foot.append(element('span', 'grow'));
    const cancel = element('button', 'button ghost', 'Cancel'); cancel.type = 'button'; cancel.dataset.action = 'picker-cancel'; cancel.disabled = picker.busy;
    const use = element('button', 'button', picker.busy ? 'Creating…' : picker.kind === 'favorite' ? 'Add this folder' : 'Create session');
    use.type = 'button'; use.dataset.action = 'picker-use'; use.disabled = picker.busy || picker.phase !== 'ready' || !picker.path;
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
    copy.value = picker.form.copyFrom; copyRow.append(copy, element('span', 'config-help', 'Mirrors mode, ports, image, and skills')); config.append(copyRow);

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

    const ports = element('div', 'config-row'); ports.append(element('label', 'config-label', 'Exposed ports'));
    const portInput = element('input', 'input'); portInput.type = 'text'; portInput.placeholder = '3000, 5000, 5173, 8000, 8888';
    portInput.dataset.field = 'ports'; portInput.value = picker.form.exposedPorts;
    ports.append(portInput, element('span', 'config-help', 'Browser pane needs these')); config.append(ports);

    const image = element('div', 'config-row'); image.append(element('label', 'config-label', 'Base image'));
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
    image.append(element('span', 'config-help', 'Per-session runtime')); config.append(image);

    const probe = this.#projectProbe(picker); if (probe) config.append(probe);
    const mode = element('div', 'config-row'); mode.append(element('label', 'config-label', 'Mode'));
    const select = element('select', 'select'); select.dataset.field = 'mode';
    [['single_agent', 'Single agent'], ['lattice', 'Full lattice'], ['custom', 'Custom workflow']]
      .forEach(([value, label]) => select.append(new Option(label, value)));
    select.value = picker.form.modeKind; mode.append(select, element('span', 'grow'), element('span', 'config-help', this.#modeHint(picker.form.modeKind))); config.append(mode);
    if (picker.form.modeKind === 'custom') {
      const agents = element('div', 'config-row wrap'); agents.append(element('span', 'config-label', 'Agents'));
      const list = element('div', 'check-list');
      this.#agents.forEach((agentValue) => {
        const agent = typeof agentValue === 'string' ? { id: agentValue } : agentValue;
        const label = element('label', 'check'); const input = document.createElement('input');
        input.type = 'checkbox'; input.dataset.field = 'custom-agent'; input.value = agent.id;
        input.checked = picker.form.customAgents.has(agent.id); label.append(input, document.createTextNode(agent.name || agent.id));
        if ((agent.depends_on || []).length) label.append(element('small', '', `← ${agent.depends_on.join(', ')}`)); list.append(label);
      });
      if (!this.#agents.length) list.append(element('span', 'config-help', 'No agents configured.'));
      agents.append(list); config.append(agents);
    }
    return config;
  }

  #projectProbe(picker) {
    const probe = picker.probe;
    const devcontainer = probe?.devcontainer && !probe.devcontainer.error ? probe.devcontainer : null;
    const axocoatlFiles = Array.isArray(probe?.axocoatl_md) ? probe.axocoatl_md : [];
    if (!devcontainer && !axocoatlFiles.length && !picker.probeError) return null;
    const row = element('div', 'config-row wrap probe');
    if (picker.probeError) { row.append(element('span', 'config-help', `Project config could not be inspected: ${picker.probeError}`)); return row; }
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
    this.#agents.forEach((value) => {
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
    if (picker.kind === 'favorite') {
      const path = picker.path; this.#closePicker(); this.#addFavorite(path); await this.navigate(path); return;
    }
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
      const session = await this.#request('/api/sessions', {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          name: folderName(picker.path) || picker.path, working_dir: picker.path, mode,
          enabled_skills: [...form.enabledSkills], exposed_ports: ports, image,
        }),
      });
      if (this.#picker === picker) this.#closePicker();
      this.#notify('Session created', session.name, 'ok');
      await this.refresh();
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
      if (action === 'add-folder') this.addFolder();
      else if (action === 'new-session') {
        if (this.#currentPath) this.newSession(this.#currentPath); else this.#notify('Pick a folder first', '', 'err');
      } else if (action === 'back') this.#goBack();
      else if (action === 'forward') this.#goForward();
      else if (action === 'up') this.#goUp();
      else if (action === 'navigate') void this.navigate(actionElement.dataset.path);
      else if (action === 'remove-favorite') this.#removeFavorite(actionElement.dataset.path);
      else if (action === 'retry-sessions') void this.refresh();
      else if (action === 'retry-path') void this.navigate(this.#currentPath, { pushHistory: false });
      else if (action === 'retry-picker') void this.#pickerNavigate(this.#picker?.requestedPath || this.#picker?.path || '');
      else if (action === 'picker-navigate') void this.#pickerNavigate(actionElement.dataset.path);
      else if (action === 'picker-use') void this.#usePicker();
      else if (action === 'picker-cancel' || action === 'picker-backdrop') this.#closePicker();
      else if (action === 'menu-action') void this.#runMenuAction(actionElement.dataset.menuAction);
      else if (action === 'dialog-cancel' || action === 'dialog-backdrop') this.#resolveDialog(this.#dialog?.kind === 'confirm' ? false : null);
      else if (action === 'dialog-ok') this.#resolveDialog(this.#dialog?.kind === 'confirm' ? true : this.#dialog?.value?.trim());
      else if (action === 'dismiss-notice') { this.#notice = null; this.#renderNotice(); }
      return;
    }
    const favorite = event.target.closest('[data-favorite-path]');
    if (favorite) { void this.navigate(favorite.dataset.favoritePath); return; }
    const recent = event.target.closest('[data-recent-session]');
    if (recent) { this.#openSession(this.session(recent.dataset.recentSession), 'recent'); return; }
    const folder = event.target.closest('[data-folder-path]');
    if (folder) { void this.navigate(folder.dataset.folderPath); return; }
    const session = event.target.closest('[data-session-id]');
    if (session) this.#selectSession(session.dataset.sessionId);
  }

  #onDoubleClick(event) {
    const row = event.target.closest('[data-session-id]');
    if (row) this.#openSession(this.session(row.dataset.sessionId), 'finder');
  }

  #onContextMenu(event) {
    const session = event.target.closest('[data-session-id]');
    const folder = event.target.closest('[data-folder-path]');
    const favorite = event.target.closest('[data-favorite-path]');
    if (!session && !folder && !favorite) return;
    event.preventDefault();
    if (session) {
      this.#selectSession(session.dataset.sessionId);
      this.#showMenu('session', session.dataset.sessionId, event.clientX, event.clientY);
    } else if (folder) this.#showMenu('folder', folder.dataset.folderPath, event.clientX, event.clientY);
    else this.#showMenu('favorite', favorite.dataset.favoritePath, event.clientX, event.clientY);
  }

  #onInput(event) {
    if (event.target.matches('.search')) {
      this.#searchTerm = event.target.value || ''; this.#renderRows(); return;
    }
    const field = event.target.dataset.field;
    if (field === 'dialog-value' && this.#dialog) this.#dialog.value = event.target.value;
    else if (field === 'ports' && this.#picker) this.#picker.form.exposedPorts = event.target.value;
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
    if (event.key !== 'Enter' && event.key !== ' ') return;
    const favorite = event.target.closest('[data-favorite-path]');
    const recent = event.target.closest('[data-recent-session]');
    const folder = event.target.closest('[data-folder-path]');
    const session = event.target.closest('[data-session-id]');
    if (!favorite && !recent && !folder && !session) return;
    event.preventDefault();
    if (favorite) void this.navigate(favorite.dataset.favoritePath);
    else if (recent) this.#openSession(this.session(recent.dataset.recentSession), 'recent');
    else if (folder) void this.navigate(folder.dataset.folderPath);
    else this.#openSession(this.session(session.dataset.sessionId), 'finder');
  }

  #onDragStart(event) {
    const row = event.target.closest('[data-favorite-index]'); if (!row) return;
    this.#dragFavoriteIndex = Number.parseInt(row.dataset.favoriteIndex, 10); row.classList.add('dragging');
    try { event.dataTransfer.effectAllowed = 'move'; event.dataTransfer.setData('text/plain', row.dataset.favoriteIndex); } catch {}
  }

  #onDragOver(event) {
    const row = event.target.closest('[data-favorite-index]');
    if (!row || this.#dragFavoriteIndex == null) return;
    event.preventDefault();
    const bounds = row.getBoundingClientRect(); const above = event.clientY - bounds.top < bounds.height / 2;
    row.classList.toggle('drop-above', above); row.classList.toggle('drop-below', !above);
  }

  #onDragLeave(event) {
    const row = event.target.closest('[data-favorite-index]');
    row?.classList.remove('drop-above', 'drop-below');
  }

  #onDrop(event) {
    const row = event.target.closest('[data-favorite-index]');
    if (!row || this.#dragFavoriteIndex == null) return;
    event.preventDefault();
    const from = this.#dragFavoriteIndex; const index = Number.parseInt(row.dataset.favoriteIndex, 10);
    const bounds = row.getBoundingClientRect(); const above = event.clientY - bounds.top < bounds.height / 2;
    let to = index + (above ? 0 : 1); if (from < to) to -= 1;
    if (from !== to) {
      const [moved] = this.#favorites.splice(from, 1); this.#favorites.splice(to, 0, moved);
      this.#saveFavorites(); this.#renderSidebar();
    }
    this.#clearDrag();
  }

  #clearDrag() {
    this.#dragFavoriteIndex = null;
    this.#root.querySelectorAll('.side-row').forEach((row) => row.classList.remove('dragging', 'drop-above', 'drop-below'));
  }
}

if (!customElements.get('ax-session-home')) customElements.define('ax-session-home', AxSessionHome);
