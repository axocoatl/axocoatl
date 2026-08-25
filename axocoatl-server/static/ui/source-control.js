import { adopt } from './sheets.js';

/**
 * `<ax-source-control>` owns the session's Git decision surface.
 *
 * It deliberately stops at the diff boundary. Monaco belongs to the editor,
 * so choosing a path emits `diff-open` and lets the shell route that request.
 *
 * @element ax-source-control
 * @attr {string} session
 * @attr {string} scope    Optional `last-turn` or `all` scope override.
 * @fires diff-open       detail: {path}
 * @fires status-change   detail: {status}
 * @fires files-changed   detail: {paths, status, all?}; all means reconcile every open buffer
 * @fires notify          detail: {title, body, kind}
 * @fires busy-change     detail: {busy, mutating, session}
 */

const SCOPES = {
  all: (file) => Boolean(file),
  lastTurn: (file) => Boolean(file.last_turn),
};

const MARK = {
  modified: 'M', added: 'A', untracked: 'U', deleted: 'D', renamed: 'R',
};

const CSS = `
:host {
  container-type: inline-size;
  display: flex;
  flex: 1;
  min-width: 0;
  min-height: 0;
  color: var(--text);
  font-family: var(--font-sans);
}
* { box-sizing: border-box; }
button, input, select { font: inherit; }
button { cursor: pointer; }
button:disabled, input:disabled, select:disabled { cursor: default; opacity: .5; }
button:focus-visible, input:focus-visible, select:focus-visible {
  outline: none;
  box-shadow: var(--focus-ring);
}
[hidden] { display: none !important; }
.pane {
  position: relative;
  display: flex;
  flex: 1;
  min-width: 0;
  min-height: 0;
  flex-direction: column;
  overflow: hidden;
  background: var(--bg-2);
}

/* Repository context remains quiet and compact. The actual decision hierarchy
   begins with scope, changed paths, and the commit action. */
.repo-head {
  display: flex;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-2);
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.summary {
  display: flex;
  align-items: baseline;
  gap: var(--sp-2);
  min-width: 0;
}
.summary-title {
  color: var(--text);
  font-size: var(--fs-sm);
  font-weight: var(--fw-bold);
}
.summary-count {
  min-width: 0;
  overflow: hidden;
  color: var(--muted);
  font-size: var(--fs-xs);
  text-overflow: ellipsis;
  white-space: nowrap;
}
.branch-row { display: flex; align-items: center; gap: var(--sp-1); min-width: 0; }
.branch-glyph {
  width: 18px;
  flex: 0 0 18px;
  color: var(--accent-2);
  font-size: var(--fs-sm);
  text-align: center;
}
.branch {
  flex: 1;
  min-width: 0;
  height: 28px;
  color: var(--text);
  background: var(--bg-3);
  border: 1px solid var(--border);
  border-radius: var(--r-sm);
  padding: 3px var(--sp-2);
  font: var(--fs-xs) var(--font-mono);
}
.refresh {
  width: 28px;
  height: 28px;
  flex: 0 0 28px;
  padding: 0;
  border: 1px solid transparent;
  border-radius: var(--r-sm);
  background: transparent;
  color: var(--muted);
  font-size: var(--fs-sm);
}
.refresh:hover { border-color: var(--border); color: var(--text); background: var(--bg-3); }

/* Scope is one dimension: provenance. Staging state is represented by the
   two groups below rather than masquerading as two more competing filters. */
.scopes {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--sp-1);
  padding: var(--sp-2);
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.scope {
  display: flex;
  min-width: 0;
  align-items: center;
  justify-content: center;
  gap: var(--sp-1);
  min-height: 28px;
  padding: 4px var(--sp-2);
  border: 1px solid transparent;
  border-radius: var(--r-md);
  background: transparent;
  color: var(--muted);
  font-size: var(--fs-xs);
  white-space: nowrap;
}
.scope:hover { color: var(--text); background: var(--bg-3); }
.scope[aria-pressed="true"] {
  border-color: var(--border-strong);
  background: var(--bg-3);
  color: var(--text);
}
.scope-count {
  min-width: 16px;
  padding: 0 4px;
  border-radius: var(--r-pill);
  background: rgba(var(--axo-jade-rgb), .16);
  color: var(--accent);
  font: var(--fs-xs) var(--font-mono);
  text-align: center;
}

.files {
  flex: 1;
  min-width: 0;
  min-height: 0;
  overflow: auto;
  padding: var(--sp-1) var(--sp-1) var(--sp-3);
  scrollbar-gutter: stable;
}
.section { margin-top: var(--sp-1); }
.section-head {
  display: flex;
  min-width: 0;
  align-items: center;
  gap: var(--sp-1);
  min-height: 30px;
  padding: var(--sp-1) var(--sp-2);
}
.section-title {
  min-width: 0;
  color: var(--muted-2);
  font-size: var(--fs-xs);
  font-weight: var(--fw-bold);
  letter-spacing: .07em;
  overflow: hidden;
  text-overflow: ellipsis;
  text-transform: uppercase;
  white-space: nowrap;
}
.section-count { color: var(--muted); font: var(--fs-xs) var(--font-mono); }
.section-act {
  margin-left: auto;
  flex-shrink: 0;
  padding: 3px var(--sp-1);
  border: 0;
  border-radius: var(--r-sm);
  background: transparent;
  color: var(--muted);
  font-size: var(--fs-xs);
}
.section-act:hover { color: var(--text); background: var(--bg-3); }

/* The path owns the flexible space. Metadata disappears before it and actions
   become denser in narrow containers, so the UI never degrades into glyphs
   with a zero-width filename. */
.file {
  position: relative;
  display: flex;
  width: 100%;
  min-width: 0;
  align-items: center;
  gap: var(--sp-1);
  padding: 2px var(--sp-1);
  border-radius: var(--r-sm);
  color: var(--text);
}
.file:hover, .file:focus-within { background: var(--bg-3); }
.file.selected {
  background: rgba(var(--axo-jade-rgb), .16);
  box-shadow: inset 2px 0 0 var(--accent);
}
.file-main {
  appearance: none;
  display: flex;
  flex: 1;
  min-width: 0;
  align-items: center;
  gap: var(--sp-1);
  height: 26px;
  padding: 0 var(--sp-1);
  border: 0;
  border-radius: var(--r-sm);
  background: transparent;
  color: inherit;
  text-align: left;
  font: var(--fs-xs) var(--font-mono);
}
.mark {
  width: 15px;
  flex: 0 0 15px;
  font-weight: var(--fw-bold);
  text-align: center;
}
.mark.modified { color: var(--warn); }
.mark.added, .mark.untracked { color: var(--ok); }
.mark.deleted { color: var(--err); }
.mark.renamed { color: var(--accent-2); }
.path {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.turn-badge {
  flex-shrink: 0;
  padding: 1px 4px;
  border-radius: var(--r-sm);
  color: var(--accent);
  background: rgba(var(--axo-jade-rgb), .12);
  font: var(--fs-xs) var(--font-sans);
  white-space: nowrap;
}
.numbers { display: inline-flex; margin-left: auto; flex-shrink: 0; }
.add { color: var(--ok); }
.del { margin-left: 4px; color: var(--err); }
.file-actions { display: flex; flex: 0 0 auto; align-items: center; gap: 1px; }
.icon {
  display: inline-flex;
  width: 24px;
  height: 24px;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: 0;
  border-radius: var(--r-sm);
  background: transparent;
  color: var(--muted);
  font-size: var(--fs-sm);
}
.icon:hover { background: var(--panel); color: var(--text); }
.icon.danger:hover { color: var(--err); }
.hunks { margin: 0 var(--sp-1) var(--sp-1) var(--sp-5); }
.hunk {
  display: flex;
  min-width: 0;
  align-items: center;
  gap: var(--sp-1);
  min-height: 26px;
  padding: 2px var(--sp-1);
  color: var(--muted);
  font: var(--fs-xs) var(--font-mono);
}
.hunk-head {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.hunk-state { padding: var(--sp-2); color: var(--muted); font-size: var(--fs-xs); }
.hunk-state.error { color: var(--err); }

.empty, .state {
  display: flex;
  min-height: 120px;
  align-items: center;
  justify-content: center;
  flex-direction: column;
  gap: var(--sp-2);
  padding: var(--sp-4) var(--sp-2);
  color: var(--muted);
  font-size: var(--fs-sm);
  line-height: var(--lh-body);
  text-align: center;
}
.state.error { color: var(--err); }
.empty button, .state button {
  padding: 4px var(--sp-2);
  border: 1px solid var(--border);
  border-radius: var(--r-sm);
  background: var(--bg-3);
  color: var(--text);
  font-size: var(--fs-xs);
}

/* Committing is the terminal action in this surface, so it remains visible at
   the bottom while the change list scrolls. */
.commit-composer {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  padding: var(--sp-2);
  border-top: 1px solid var(--border);
  background: var(--bg-2);
  flex-shrink: 0;
}
.commit-label {
  color: var(--muted-2);
  font-size: var(--fs-xs);
  font-weight: var(--fw-bold);
  letter-spacing: .06em;
  text-transform: uppercase;
}
.message {
  width: 100%;
  min-width: 0;
  height: 32px;
  padding: 5px var(--sp-2);
  color: var(--text);
  background: var(--bg-3);
  border: 1px solid var(--border);
  border-radius: var(--r-md);
  font-size: var(--fs-sm);
}
.message:hover { border-color: var(--border-strong); }
.commit-row { position: relative; display: flex; min-width: 0; gap: var(--sp-1); }
.commit-primary {
  flex: 1;
  min-width: 0;
  min-height: 30px;
  padding: 5px var(--sp-2);
  border: 1px solid var(--accent);
  border-radius: var(--r-md);
  background: rgba(var(--axo-jade-rgb), .2);
  color: var(--text);
  font-size: var(--fs-xs);
  font-weight: var(--fw-bold);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.commit-primary:hover:not(:disabled) { background: rgba(var(--axo-jade-rgb), .3); }
.menu-toggle {
  width: 32px;
  min-height: 30px;
  flex: 0 0 32px;
  padding: 0;
  border: 1px solid var(--border);
  border-radius: var(--r-md);
  background: var(--bg-3);
  color: var(--muted);
  font-size: var(--fs-body);
}
.menu-toggle:hover { border-color: var(--border-strong); color: var(--text); }
.overflow-menu {
  position: absolute;
  right: 0;
  bottom: calc(100% + var(--sp-1));
  z-index: 20;
  display: flex;
  width: min(232px, calc(100cqw - var(--sp-4)));
  min-width: 0;
  flex-direction: column;
  gap: 2px;
  padding: var(--sp-1);
  border: 1px solid var(--border-strong);
  border-radius: var(--r-md);
  background: var(--panel-2);
  box-shadow: var(--shadow-lg);
}
.menu-item {
  width: 100%;
  padding: 6px var(--sp-2);
  border: 0;
  border-radius: var(--r-sm);
  background: transparent;
  color: var(--text);
  font-size: var(--fs-xs);
  line-height: 1.35;
  text-align: left;
}
.menu-item:hover:not(:disabled) { background: var(--bg-3); }
.menu-item.danger { color: var(--err); }
.menu-separator { height: 1px; margin: 2px var(--sp-1); background: var(--border); }

@container (max-width: 260px) {
  .numbers, .turn-badge { display: none; }
  .file { gap: 1px; padding-inline: 2px; }
  .file-main { gap: 3px; padding-inline: 3px; }
  .file-actions { gap: 0; }
  .icon { width: 21px; }
  .hunks { margin-left: var(--sp-3); }
  .section-head { padding-inline: var(--sp-1); }
}
@container (max-width: 190px) {
  .scopes { grid-template-columns: minmax(0, 1fr); }
  .summary { align-items: flex-start; flex-direction: column; gap: 1px; }
  .branch-glyph { display: none; }
  .repo-head, .scopes, .commit-composer { padding-inline: var(--sp-1); }
}
@media (hover: hover) and (pointer: fine) {
  .file-actions { opacity: .45; transition: opacity var(--dur-fast) var(--ease); }
  .file:hover .file-actions, .file:focus-within .file-actions { opacity: 1; }
}
`;

function normalizeScope(value) {
  return value === 'all' ? 'all' : 'lastTurn';
}

function scopeAttribute(scope) {
  return scope === 'all' ? 'all' : 'last-turn';
}

function hunkKey(path, staged) {
  return `${staged ? 'staged' : 'working'}:${path}`;
}

export class AxSourceControl extends HTMLElement {
  static get observedAttributes() { return ['session', 'scope']; }

  #root;
  #els = {};
  #status = null;
  #branches = [];
  #scope = 'lastTurn';
  #scopeTouched = false;
  #phase = 'idle';
  #error = '';
  #generation = 0;
  #bindingGeneration = 0;
  #refreshController = null;
  #hunkControllers = new Set();
  #confirm = null;
  #message = '';
  #mutation = null;
  #selectedPath = '';
  #selectedOperation = '';
  #expandedHunks = new Set();
  #hunkCache = new Map();
  #menuOpen = false;
  #documentPointerDown;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS);
    this.#mount();
    this.#documentPointerDown = (event) => {
      if (this.#menuOpen && !event.composedPath().includes(this)) this.#setMenuOpen(false);
    };
    this.#render();
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(value) { value ? this.setAttribute('session', value) : this.removeAttribute('session'); }
  get status() { return this.#status; }
  get mutating() { return Boolean(this.#mutation); }
  get busy() { return this.mutating; }
  get scope() { return this.#scope; }
  set scope(value) {
    const next = normalizeScope(value);
    const attr = scopeAttribute(next);
    if (this.getAttribute('scope') !== attr) this.setAttribute('scope', attr);
    else this.#setScope(next, true);
  }
  set confirm(callback) { this.#confirm = typeof callback === 'function' ? callback : null; }

  connectedCallback() {
    document.removeEventListener('pointerdown', this.#documentPointerDown, true);
    document.addEventListener('pointerdown', this.#documentPointerDown, true);
    if (this.session) void this.refresh({ branches: true });
  }

  disconnectedCallback() {
    document.removeEventListener('pointerdown', this.#documentPointerDown, true);
    this.#bindingGeneration += 1;
    this.#generation += 1;
    this.#abortReads();
  }

  attributeChangedCallback(name, before, after) {
    if (before === after) return;
    if (name === 'scope') {
      if (after == null) {
        this.#scopeTouched = false;
        this.#chooseInitialScope();
      } else {
        this.#setScope(after === 'all' ? 'all' : 'lastTurn', true);
      }
      return;
    }
    if (name !== 'session') return;
    this.#bindingGeneration += 1;
    this.#generation += 1;
    this.#abortReads();
    this.#setMutation(null);
    this.#status = null;
    this.#branches = [];
    this.#phase = after ? 'loading' : 'idle';
    this.#error = '';
    this.#message = '';
    this.#selectedPath = '';
    this.#selectedOperation = '';
    this.#expandedHunks.clear();
    this.#hunkCache.clear();
    this.#scopeTouched = this.hasAttribute('scope');
    this.#scope = this.getAttribute('scope') === 'all' ? 'all' : 'lastTurn';
    this.#setMenuOpen(false);
    this.#render();
    if (this.isConnected && after) void this.refresh({ branches: true });
  }

  async refresh({ branches = false } = {}) {
    const session = this.session;
    if (!session || !this.isConnected || this.#mutation) return;
    this.#abortReads();
    this.#hunkCache.clear();
    const generation = ++this.#generation;
    const controller = new AbortController();
    this.#refreshController = controller;
    this.#phase = 'loading';
    this.#error = '';
    this.#render();
    try {
      const status = await this.#request('/git/status', { signal: controller.signal }, session);
      // Removing the Session suspends Source Control and increments the
      // generation. Do not let a delayed status response launch a follow-on
      // branches read into a runtime now owned by Ways.
      if (!this.#isCurrent(session, generation)) return;
      let branchInfo = null;
      if (branches) {
        try {
          branchInfo = await this.#request('/git/branches', { signal: controller.signal }, session);
        } catch (error) {
          if (controller.signal.aborted || error?.name === 'AbortError'
              || !this.#isCurrent(session, generation)) return;
          this.#notify('Could not refresh branches', String(error?.message || error), 'warn');
        }
      }
      if (!this.#isCurrent(session, generation)) return;
      this.#status = status;
      if (branchInfo && Array.isArray(branchInfo.branches)) this.#branches = branchInfo.branches;
      this.#phase = 'ready';
      this.#chooseInitialScope();
      this.#pruneSelection();
      this.#render();
      this.#emitStatus();
    } catch (error) {
      if (controller.signal.aborted || error?.name === 'AbortError') return;
      if (!this.#isCurrent(session, generation)) return;
      this.#phase = 'error';
      this.#error = String(error?.message || error);
      this.#render();
    } finally {
      if (this.#refreshController === controller) this.#refreshController = null;
    }
  }

  async #mutate(path, body, { changed = [], success = null, session = this.session } = {}) {
    if (!session || !this.isConnected || this.#mutation) return null;
    const bindingGeneration = this.#bindingGeneration;
    const mutation = { session, bindingGeneration };
    this.#abortReads();
    this.#expandedHunks.clear();
    this.#hunkCache.clear();
    this.#setMutation(mutation);
    const generation = ++this.#generation;
    this.#setMenuOpen(false);
    this.#render();
    try {
      const status = await this.#request(path, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body || {}),
      }, session);
      if (!this.#isCurrent(session, generation)) return null;
      this.#status = status;
      this.#phase = 'ready';
      this.#error = '';
      this.#invalidateHunks(changed);
      this.#chooseInitialScope();
      this.#pruneSelection();
      this.#render();
      this.#emitStatus();
      if (changed.length) this.dispatchEvent(new CustomEvent('files-changed', {
        detail: { paths: changed, status }, bubbles: true, composed: true,
      }));
      if (success) this.#notify(success.title, success.body, 'ok');
      return status;
    } catch (error) {
      if (this.#mutation === mutation && this.#ownsBinding(session, bindingGeneration)) {
        this.#notify('Git operation failed', `${String(error?.message || error)} Refreshing status because the operation may have reached Git.`, 'err');
      }
      return null;
    } finally {
      if (this.#mutation === mutation) {
        this.#setMutation(null);
        // A mutation that was already accepted by the daemon is never aborted.
        // Once it settles, refresh whichever still-connected binding owns this
        // same Session; a different Session was reset above and cannot match.
        if (session === this.session && this.isConnected) await this.refresh({ branches: true });
      }
    }
  }

  async #request(suffix, options, session = this.session) {
    if (!session) throw new Error('No session is selected.');
    const response = await fetch(`/api/sessions/${encodeURIComponent(session)}${suffix}`, options);
    const body = await response.json().catch(() => ({}));
    if (!response.ok || body?.error) throw new Error(body?.error || `HTTP ${response.status}`);
    return body;
  }

  #isCurrent(session, generation) {
    return this.isConnected && session === this.session && generation === this.#generation;
  }

  #ownsBinding(session, generation) {
    return this.isConnected
      && session === this.session
      && generation === this.#bindingGeneration;
  }

  #abortReads() {
    const refresh = this.#refreshController;
    this.#refreshController = null;
    if (refresh) refresh.abort();
    for (const controller of this.#hunkControllers) controller.abort();
    this.#hunkControllers.clear();
  }

  #setMutation(mutation) {
    const previous = this.#mutation;
    const before = Boolean(this.#mutation);
    this.#mutation = mutation;
    const after = Boolean(this.#mutation);
    if (before === after) return;
    this.dispatchEvent(new CustomEvent('busy-change', {
      detail: {
        busy: after,
        mutating: after,
        session: mutation?.session || previous?.session || this.session,
      },
      bubbles: true,
      composed: true,
    }));
  }

  #emitStatus() {
    this.dispatchEvent(new CustomEvent('status-change', {
      detail: { status: this.#status }, bubbles: true, composed: true,
    }));
  }

  #notify(title, body, kind = 'info') {
    this.dispatchEvent(new CustomEvent('notify', {
      detail: { title, body, kind }, bubbles: true, composed: true,
    }));
  }

  async #ask(spec) {
    if (!this.#confirm) return false;
    try { return Boolean(await this.#confirm(spec)); } catch { return false; }
  }

  #mount() {
    this.#root.innerHTML = `<div class="pane">
      <div class="repo-head">
        <div class="summary">
          <span class="summary-title">Working tree</span>
          <span class="summary-count" aria-live="polite"></span>
        </div>
        <div class="branch-row">
          <span class="branch-glyph" aria-hidden="true">⎇</span>
          <select class="branch" aria-label="Current branch" title="Switch branch"></select>
          <button class="refresh" data-action="refresh" title="Refresh changes" aria-label="Refresh changes">⟳</button>
        </div>
      </div>
      <div class="scopes" role="group" aria-label="Changed files scope">
        <button class="scope" data-scope="lastTurn" data-focus-key="scope:last-turn" aria-pressed="true">
          <span>Last turn</span><span class="scope-count" data-count="lastTurn">0</span>
        </button>
        <button class="scope" data-scope="all" data-focus-key="scope:all" aria-pressed="false">
          <span>All changes</span><span class="scope-count" data-count="all">0</span>
        </button>
      </div>
      <div class="files" aria-live="polite"></div>
      <div class="commit-composer">
        <label class="commit-label" for="commit-message">Commit message</label>
        <input id="commit-message" class="message" placeholder="Describe these changes…" spellcheck="false" autocomplete="off" required data-focus-key="commit:message">
        <div class="commit-row">
          <button class="commit-primary" data-action="commit" data-focus-key="commit:primary">Commit 0 staged</button>
          <button class="menu-toggle" data-action="menu" data-focus-key="commit:menu" title="More commit actions" aria-label="More commit actions" aria-haspopup="menu" aria-expanded="false">•••</button>
          <div class="overflow-menu" role="menu" hidden>
            <button class="menu-item" role="menuitem" data-action="commit-all" data-focus-key="menu:commit-all">Stage all and commit</button>
            <div class="menu-separator" role="separator"></div>
            <button class="menu-item danger" role="menuitem" data-action="discard-all" data-focus-key="menu:discard-all">Discard all unstaged…</button>
          </div>
        </div>
      </div>
    </div>`;

    this.#els = {
      pane: this.#root.querySelector('.pane'),
      summary: this.#root.querySelector('.summary-count'),
      branch: this.#root.querySelector('.branch'),
      refresh: this.#root.querySelector('[data-action="refresh"]'),
      scopes: [...this.#root.querySelectorAll('[data-scope]')],
      lastCount: this.#root.querySelector('[data-count="lastTurn"]'),
      allCount: this.#root.querySelector('[data-count="all"]'),
      files: this.#root.querySelector('.files'),
      message: this.#root.querySelector('.message'),
      commit: this.#root.querySelector('[data-action="commit"]'),
      menuToggle: this.#root.querySelector('[data-action="menu"]'),
      menu: this.#root.querySelector('.overflow-menu'),
      commitAll: this.#root.querySelector('[data-action="commit-all"]'),
      discardAll: this.#root.querySelector('[data-action="discard-all"]'),
    };

    this.#els.message.addEventListener('input', () => {
      this.#message = this.#els.message.value;
      this.#syncCommitActions();
    });
    this.#els.message.addEventListener('keydown', (event) => {
      if (event.key === 'Enter') { event.preventDefault(); void this.#commit(false); }
    });
    this.#els.commit.addEventListener('click', () => void this.#commit(false));
    this.#els.refresh.addEventListener('click', () => void this.refresh({ branches: true }));
    this.#els.branch.addEventListener('change', () => void this.#checkout(this.#els.branch.value));
    for (const button of this.#els.scopes) {
      button.addEventListener('click', () => this.#setScope(button.dataset.scope, true));
    }
    this.#els.menuToggle.addEventListener('click', () => this.#setMenuOpen(!this.#menuOpen, true));
    this.#els.commitAll.addEventListener('click', () => { this.#setMenuOpen(false); void this.#commit(true); });
    this.#els.discardAll.addEventListener('click', () => { this.#setMenuOpen(false); void this.#discardAll(); });
    this.#root.addEventListener('keydown', (event) => {
      if (event.key === 'Escape' && this.#menuOpen) {
        event.preventDefault();
        this.#setMenuOpen(false);
        this.#els.menuToggle.focus();
      }
    });
    this.#els.menu.addEventListener('keydown', (event) => {
      if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
      const items = [...this.#els.menu.querySelectorAll('[role="menuitem"]:not(:disabled)')];
      if (!items.length) return;
      event.preventDefault();
      const current = items.indexOf(this.#root.activeElement);
      const index = event.key === 'Home' ? 0
        : event.key === 'End' ? items.length - 1
          : (current + (event.key === 'ArrowUp' ? -1 : 1) + items.length) % items.length;
      items[index].focus();
    });
    this.#root.addEventListener('focusout', () => {
      queueMicrotask(() => {
        const active = this.#root.activeElement;
        if (this.#menuOpen && active !== this.#els.menuToggle && !this.#els.menu.contains(active)) {
          this.#setMenuOpen(false);
        }
      });
    });
  }

  #render() {
    this.#syncChrome();
    this.#renderFiles();
    this.#applyBusy();
  }

  #syncChrome() {
    const files = this.#status?.files || [];
    const staged = files.filter((file) => file.staged).length;
    const unstaged = files.filter((file) => file.unstaged).length;
    const lastTurn = files.filter(SCOPES.lastTurn).length;
    this.#els.summary.textContent = files.length
      ? `${files.length} changed file${files.length === 1 ? '' : 's'}`
      : 'Working tree clean';
    this.#els.lastCount.textContent = String(lastTurn);
    this.#els.allCount.textContent = String(files.length);
    for (const button of this.#els.scopes) {
      const on = button.dataset.scope === this.#scope;
      button.setAttribute('aria-pressed', String(on));
      const count = button.dataset.scope === 'lastTurn' ? lastTurn : files.length;
      button.setAttribute('aria-label', `${button.dataset.scope === 'lastTurn' ? 'Last turn' : 'All changes'}, ${count} file${count === 1 ? '' : 's'}`);
    }

    if (this.#els.message.value !== this.#message) this.#els.message.value = this.#message;
    this.#els.commit.textContent = `Commit ${staged} staged`;
    this.#syncCommitActions();
    this.#els.discardAll.disabled = unstaged === 0 || this.#phase === 'error';
    this.#els.message.disabled = false;
    this.#els.refresh.disabled = false;
    this.#els.menuToggle.disabled = files.length === 0 || this.#phase === 'error';
    for (const button of this.#els.scopes) button.disabled = false;
    if (this.#els.menuToggle.disabled) this.#setMenuOpen(false);
    this.#syncBranches();
    this.#els.pane.setAttribute('aria-busy', String(this.#phase === 'loading' || Boolean(this.#mutation)));
  }

  #syncCommitActions() {
    const files = this.#status?.files || [];
    const staged = files.filter((file) => file.staged).length;
    const hasMessage = Boolean(this.#message.trim());
    const unavailable = this.#phase === 'error';
    this.#els.commit.disabled = staged === 0 || !hasMessage || unavailable;
    this.#els.commitAll.disabled = files.length === 0 || !hasMessage || unavailable;
    this.#els.commit.title = hasMessage ? `Commit ${staged} staged file${staged === 1 ? '' : 's'}` : 'Enter a commit message';
    this.#els.commitAll.title = hasMessage ? 'Stage every change and commit' : 'Enter a commit message';
  }

  #syncBranches() {
    const branch = this.#status?.branch || '';
    const names = [...new Set([...this.#branches, ...(branch ? [branch] : [])])];
    const signature = names.join('\u0000');
    if (this.#els.branch.dataset.signature !== signature) {
      const options = names.map((name) => {
        const option = document.createElement('option');
        option.value = name;
        option.textContent = name;
        return option;
      });
      this.#els.branch.replaceChildren(...options);
      this.#els.branch.dataset.signature = signature;
    }
    if (this.#els.branch.value !== branch) this.#els.branch.value = branch;
    this.#els.branch.disabled = !names.length || this.#phase === 'error';
  }

  #renderFiles() {
    const view = this.#captureView();
    const host = this.#els.files;
    host.replaceChildren();

    if (this.#phase === 'loading' && !this.#status) {
      host.append(this.#state('Reading Git status…'));
      this.#restoreView(view);
      return;
    }
    if (this.#phase === 'error') {
      const retry = document.createElement('button');
      retry.type = 'button';
      retry.textContent = 'Retry';
      retry.dataset.focusKey = 'state:retry';
      retry.onclick = () => void this.refresh({ branches: true });
      const state = this.#state(this.#error || 'Git status failed.', 'error');
      state.append(retry);
      host.append(state);
      this.#restoreView(view);
      return;
    }

    const all = this.#status?.files || [];
    const visible = all.filter(SCOPES[this.#scope] || SCOPES.all);
    if (!visible.length) {
      if (!all.length) {
        host.append(this.#state('No changes — working tree clean.', 'empty'));
      } else if (this.#scope === 'lastTurn') {
        const showAll = document.createElement('button');
        showAll.type = 'button';
        showAll.textContent = 'View all changes';
        showAll.dataset.focusKey = 'state:view-all';
        showAll.onclick = () => this.#setScope('all', true);
        const state = this.#state('No current changes remain on paths attributed to the last turn.', 'empty');
        state.append(showAll);
        host.append(state);
      } else {
        host.append(this.#state('No changed paths reported.', 'empty'));
      }
      this.#restoreView(view);
      return;
    }

    this.#renderSection(host, 'Staged', visible.filter((file) => file.staged), 'unstage');
    this.#renderSection(host, 'Changes', visible.filter((file) => file.unstaged), 'stage');
    this.#restoreView(view);
  }

  #state(message, kind = 'state') {
    const state = document.createElement('div');
    state.className = `${kind === 'empty' ? 'empty' : 'state'}${kind === 'error' ? ' error' : ''}`;
    const text = document.createElement('div');
    text.textContent = message;
    state.append(text);
    return state;
  }

  #renderSection(host, title, files, operation) {
    if (!files.length) return;
    const section = document.createElement('section');
    section.className = 'section';
    section.setAttribute('aria-label', `${title}, ${files.length} file${files.length === 1 ? '' : 's'}`);
    const header = document.createElement('div');
    header.className = 'section-head';
    const label = document.createElement('span');
    label.className = 'section-title';
    label.textContent = title;
    const count = document.createElement('span');
    count.className = 'section-count';
    count.textContent = String(files.length);
    const all = document.createElement('button');
    all.type = 'button';
    all.className = 'section-act';
    all.textContent = operation === 'stage' ? 'Stage all' : 'Unstage all';
    all.setAttribute('aria-label', `${all.textContent} in ${title.toLowerCase()}`);
    all.dataset.focusKey = `section:${operation}`;
    all.onclick = () => void this.#stage(operation, files.map((file) => file.path));
    header.append(label, count, all);
    section.append(header);
    for (const file of files) {
      const staged = operation === 'unstage';
      section.append(this.#fileRow(file, operation));
      const key = hunkKey(file.path, staged);
      if (this.#expandedHunks.has(key)) section.append(this.#hunksHost(file.path, staged));
    }
    host.append(section);
  }

  #fileRow(file, operation) {
    const row = document.createElement('div');
    row.className = 'file';
    row.dataset.path = file.path;
    row.dataset.operation = operation;
    row.classList.toggle('selected', this.#selectedPath === file.path && this.#selectedOperation === operation);

    const main = document.createElement('button');
    main.className = 'file-main';
    main.type = 'button';
    main.dataset.focusKey = `file:${operation}:${file.path}`;
    main.title = file.path;
    const counts = file.added != null || file.removed != null
      ? `, ${file.added ?? 0} additions, ${file.removed ?? 0} deletions` : '';
    main.setAttribute('aria-label', `Open changes for ${file.path}, ${file.state || 'changed'}${counts}${file.last_turn ? ', attributed to last turn' : ''}`);
    main.setAttribute('aria-current', String(this.#selectedPath === file.path && this.#selectedOperation === operation));
    main.onclick = () => {
      this.#selectedPath = file.path;
      this.#selectedOperation = operation;
      this.#syncSelectedRows();
      this.dispatchEvent(new CustomEvent('diff-open', {
        detail: { path: file.path }, bubbles: true, composed: true,
      }));
    };

    const mark = document.createElement('span');
    mark.className = `mark ${file.state}`;
    mark.textContent = MARK[file.state] || '•';
    mark.setAttribute('aria-hidden', 'true');
    const path = document.createElement('span');
    path.className = 'path';
    path.textContent = file.path;
    main.append(mark, path);
    if (file.last_turn && this.#scope !== 'lastTurn') {
      const turn = document.createElement('span');
      turn.className = 'turn-badge';
      turn.textContent = 'Last turn';
      main.append(turn);
    }
    if (file.added != null || file.removed != null) main.append(this.#numbers(file.added, file.removed));

    const actions = document.createElement('div');
    actions.className = 'file-actions';
    if (file.state !== 'untracked') {
      const staged = operation === 'unstage';
      const key = hunkKey(file.path, staged);
      const hunks = this.#iconButton('≡', this.#expandedHunks.has(key) ? 'Hide separate changes' : 'Show separate changes');
      hunks.dataset.focusKey = `hunks:${operation}:${file.path}`;
      hunks.setAttribute('aria-expanded', String(this.#expandedHunks.has(key)));
      hunks.onclick = () => this.#toggleHunks(file.path, staged);
      actions.append(hunks);
    }
    const stageLabel = operation === 'stage' ? 'Stage this file' : 'Unstage this file';
    const stage = this.#iconButton(operation === 'stage' ? '+' : '−', `${stageLabel}: ${file.path}`);
    stage.dataset.focusKey = `stage:${operation}:${file.path}`;
    stage.onclick = () => void this.#stage(operation, [file.path]);
    actions.append(stage);
    if (operation === 'stage') {
      const discard = this.#iconButton('↩', `Discard changes to ${file.path}`, true);
      discard.dataset.focusKey = `discard:${file.path}`;
      discard.onclick = () => void this.#discardFile(file.path);
      actions.append(discard);
    }
    row.append(main, actions);
    return row;
  }

  #numbers(added, removed) {
    const numbers = document.createElement('span');
    numbers.className = 'numbers';
    numbers.setAttribute('aria-hidden', 'true');
    const add = document.createElement('span');
    add.className = 'add';
    add.textContent = `+${added ?? 0}`;
    const del = document.createElement('span');
    del.className = 'del';
    del.textContent = `−${removed ?? 0}`;
    numbers.append(add, del);
    return numbers;
  }

  #iconButton(glyph, label, danger = false) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = `icon${danger ? ' danger' : ''}`;
    button.textContent = glyph;
    button.title = label;
    button.setAttribute('aria-label', label);
    return button;
  }

  #toggleHunks(path, staged) {
    const key = hunkKey(path, staged);
    if (this.#expandedHunks.has(key)) this.#expandedHunks.delete(key);
    else this.#expandedHunks.add(key);
    this.#renderFiles();
  }

  #hunksHost(path, staged) {
    const key = hunkKey(path, staged);
    const host = document.createElement('div');
    host.className = 'hunks';
    host.dataset.hunksKey = key;
    let state = this.#hunkCache.get(key);
    if (!state) {
      state = { phase: 'queued', hunks: [], error: '' };
      this.#hunkCache.set(key, state);
      queueMicrotask(() => void this.#loadHunks(path, staged));
    }
    this.#fillHunksHost(host, path, staged, state);
    return host;
  }

  async #loadHunks(path, staged) {
    const key = hunkKey(path, staged);
    const state = this.#hunkCache.get(key);
    if (!state || state.phase !== 'queued') return;
    const session = this.session;
    const generation = this.#generation;
    if (!session || !this.isConnected) return;
    const controller = new AbortController();
    this.#hunkControllers.add(controller);
    state.phase = 'loading';
    this.#refreshHunksHosts(key, path, staged, state);
    try {
      const hunks = await this.#request(
        `/git/hunks?path=${encodeURIComponent(path)}${staged ? '&staged=true' : ''}`,
        { signal: controller.signal },
        session,
      );
      if (controller.signal.aborted || !this.#isCurrent(session, generation)
          || this.#hunkCache.get(key) !== state) return;
      state.phase = 'ready';
      state.hunks = Array.isArray(hunks) ? hunks : [];
      this.#refreshHunksHosts(key, path, staged, state);
    } catch (error) {
      if (controller.signal.aborted || error?.name === 'AbortError'
          || !this.#isCurrent(session, generation)
          || this.#hunkCache.get(key) !== state) return;
      state.phase = 'error';
      state.error = String(error?.message || error);
      this.#refreshHunksHosts(key, path, staged, state);
      this.#notify('Could not read changes', state.error, 'err');
    } finally {
      this.#hunkControllers.delete(controller);
    }
  }

  #refreshHunksHosts(key, path, staged, state) {
    for (const host of this.#root.querySelectorAll('.hunks')) {
      if (host.dataset.hunksKey === key) this.#fillHunksHost(host, path, staged, state);
    }
  }

  #fillHunksHost(host, path, staged, state) {
    host.replaceChildren();
    if (state.phase === 'queued' || state.phase === 'loading') {
      const loading = document.createElement('div');
      loading.className = 'hunk-state';
      loading.textContent = 'Reading separate changes…';
      host.append(loading);
      return;
    }
    if (state.phase === 'error') {
      const error = document.createElement('div');
      error.className = 'hunk-state error';
      error.textContent = state.error || 'Could not read separate changes.';
      host.append(error);
      return;
    }
    if (!state.hunks.length) {
      const empty = document.createElement('div');
      empty.className = 'hunk-state';
      empty.textContent = 'No separable changes in this file.';
      host.append(empty);
      return;
    }
    for (const hunk of state.hunks) host.append(this.#hunkRow(path, staged, hunk));
  }

  #hunkRow(path, staged, hunk) {
    const row = document.createElement('div');
    row.className = 'hunk';
    const head = document.createElement('span');
    head.className = 'hunk-head';
    head.textContent = hunk.header;
    row.append(head, this.#numbers(hunk.added, hunk.removed));
    const stageLabel = staged ? 'Unstage just this change' : 'Stage just this change';
    const stage = this.#iconButton(staged ? '−' : '+', `${stageLabel}: ${hunk.header}`);
    stage.dataset.focusKey = `hunk:${staged ? 'unstage' : 'stage'}:${path}:${hunk.index}`;
    stage.onclick = () => void this.#hunk(path, hunk.index, !staged);
    row.append(stage);
    if (!staged) {
      const discard = this.#iconButton('↺', `Discard just this change: ${hunk.header}`, true);
      discard.dataset.focusKey = `hunk:discard:${path}:${hunk.index}`;
      discard.onclick = () => void this.#discardHunk(path, hunk.index, hunk.header);
      row.append(discard);
    }
    return row;
  }

  #captureView() {
    const active = this.#root.activeElement;
    return {
      scrollTop: this.#els.files?.scrollTop || 0,
      focusKey: active?.dataset?.focusKey || '',
      selectionStart: active === this.#els.message ? active.selectionStart : null,
      selectionEnd: active === this.#els.message ? active.selectionEnd : null,
    };
  }

  #restoreView(view) {
    this.#els.files.scrollTop = view.scrollTop;
    if (!view.focusKey) return;
    const current = this.#root.activeElement;
    const target = [...this.#root.querySelectorAll('[data-focus-key]')]
      .find((element) => element.dataset.focusKey === view.focusKey);
    if (target && current !== target) target.focus({ preventScroll: true });
    if (target === this.#els.message && view.selectionStart != null) {
      target.setSelectionRange(view.selectionStart, view.selectionEnd ?? view.selectionStart);
    }
  }

  #setScope(scope, touched) {
    const next = normalizeScope(scope);
    if (touched) this.#scopeTouched = true;
    if (this.#scope === next) {
      this.#syncChrome();
      return;
    }
    this.#scope = next;
    this.#syncChrome();
    this.#renderFiles();
  }

  #chooseInitialScope() {
    if (this.#scopeTouched) return;
    const files = this.#status?.files || [];
    this.#scope = files.some(SCOPES.lastTurn) ? 'lastTurn' : 'all';
  }

  #syncSelectedRows() {
    for (const row of this.#root.querySelectorAll('.file')) {
      const selected = row.dataset.path === this.#selectedPath
        && row.dataset.operation === this.#selectedOperation;
      row.classList.toggle('selected', selected);
      row.querySelector('.file-main')?.setAttribute('aria-current', String(selected));
    }
  }

  #pruneSelection() {
    if (!this.#selectedPath) return;
    const file = (this.#status?.files || []).find((candidate) => candidate.path === this.#selectedPath);
    const stillPresent = file && (this.#selectedOperation === 'stage' ? file.unstaged : file.staged);
    if (!stillPresent) {
      this.#selectedPath = '';
      this.#selectedOperation = '';
    }
  }

  #invalidateHunks(paths) {
    const changed = new Set(paths);
    for (const key of this.#hunkCache.keys()) {
      const path = key.slice(key.indexOf(':') + 1);
      if (changed.has(path)) this.#hunkCache.delete(key);
    }
  }

  #setMenuOpen(open, focusFirst = false) {
    this.#menuOpen = Boolean(open);
    this.#els.menu.hidden = !this.#menuOpen;
    this.#els.menuToggle.setAttribute('aria-expanded', String(this.#menuOpen));
    if (this.#menuOpen && focusFirst) {
      queueMicrotask(() => this.#els.menu.querySelector('[role="menuitem"]:not(:disabled)')?.focus());
    }
  }

  #applyBusy() {
    if (!this.#mutation) return;
    this.#root.querySelectorAll('button, input, select').forEach((control) => { control.disabled = true; });
  }

  async #stage(operation, paths) {
    await this.#mutate(`/git/${operation}`, { paths }, { changed: paths });
  }

  async #hunk(path, index, stage) {
    await this.#mutate('/git/hunk', { path, index, stage }, { changed: [path] });
  }

  async #discardHunk(path, index, header) {
    const session = this.session;
    const bindingGeneration = this.#bindingGeneration;
    const generation = this.#generation;
    const approved = await this.#ask({
      title: 'Discard this change?',
      body: `Throw away ${header} in "${path}"? The rest of the file is untouched. This can't be undone.`,
      okLabel: 'Discard', okKind: 'danger',
    });
    if (!approved || !this.#ownsBinding(session, bindingGeneration)
        || !this.#isCurrent(session, generation)) return;
    await this.#mutate('/git/hunk/discard', { path, index }, { changed: [path], session });
  }

  async #discardFile(path) {
    const session = this.session;
    const bindingGeneration = this.#bindingGeneration;
    const generation = this.#generation;
    const approved = await this.#ask({
      title: 'Discard changes?', body: `Discard working changes to "${path}"? This can't be undone.`,
      okLabel: 'Discard', okKind: 'danger',
    });
    if (!approved || !this.#ownsBinding(session, bindingGeneration)
        || !this.#isCurrent(session, generation)) return;
    await this.#mutate('/git/discard', { path }, { changed: [path], session });
  }

  async #discardAll() {
    const session = this.session;
    const bindingGeneration = this.#bindingGeneration;
    const generation = this.#generation;
    const paths = (this.#status?.files || []).filter((file) => file.unstaged).map((file) => file.path);
    const count = paths.length;
    if (!count) { this.#notify('Nothing to discard', 'The working tree is clean.', 'warn'); return; }
    const approved = await this.#ask({
      title: 'Discard unstaged changes?',
      body: `Throw away unstaged changes to ${count} file${count === 1 ? '' : 's'}, including untracked ones? Staged work stays intact. This can't be undone.`,
      okLabel: 'Discard unstaged', okKind: 'danger',
    });
    if (!approved || !this.#ownsBinding(session, bindingGeneration)
        || !this.#isCurrent(session, generation)) return;
    await this.#mutate('/git/discard', {}, {
      changed: paths,
      success: { title: 'Discarded unstaged changes', body: `${count} file${count === 1 ? '' : 's'} restored; staged work was kept.` },
      session,
    });
  }

  async #commit(stageAll) {
    const message = this.#message.trim();
    if (!message) {
      this.#notify('Commit message required', 'Describe these changes before committing.', 'warn');
      this.#els.message.focus();
      return;
    }
    if (!stageAll && !(this.#status?.files || []).some((file) => file.staged)) {
      this.#notify('Nothing staged', 'Stage a file or a hunk first, or use “Stage all and commit”.', 'warn');
      return;
    }
    const session = this.session;
    const bindingGeneration = this.#bindingGeneration;
    const status = await this.#mutate('/git/commit', { message, stage_all: stageAll }, {
      success: { title: 'Committed', body: message },
      session,
    });
    if (status && this.#ownsBinding(session, bindingGeneration)) {
      this.#message = '';
      this.#branches = [];
      await this.refresh({ branches: true });
    }
  }

  async #checkout(reference) {
    const before = this.#status?.branch || '';
    const session = this.session;
    const bindingGeneration = this.#bindingGeneration;
    const status = await this.#mutate('/git/checkout', { ref: reference }, {
      changed: (this.#status?.files || []).map((file) => file.path),
      success: { title: 'Switched branch', body: reference },
      session,
    });
    if (!this.#ownsBinding(session, bindingGeneration)) return;
    if (!status) this.#render();
    else if (status.branch !== reference) this.#notify('Checkout blocked', `Still on ${before || status.branch}.`, 'err');
    else this.dispatchEvent(new CustomEvent('files-changed', {
      detail: { paths: [], all: true, status }, bubbles: true, composed: true,
    }));
  }
}

if (!customElements.get('ax-source-control')) customElements.define('ax-source-control', AxSourceControl);
