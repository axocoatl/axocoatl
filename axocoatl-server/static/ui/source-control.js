import { adopt } from './sheets.js';

/**
 * `<ax-source-control>` owns the session's Git decision surface.
 *
 * It deliberately stops at the diff boundary. Monaco belongs to the editor,
 * so choosing a path emits `diff-open` and lets the shell route that request.
 *
 * @element ax-source-control
 * @attr {string} session
 * @fires diff-open       detail: {path}
 * @fires status-change   detail: {status}
 * @fires files-changed   detail: {paths, status, all?}; all means reconcile every open buffer
 * @fires notify          detail: {title, body, kind}
 */

const VIEWS = [
  ['all', 'All', () => true],
  ['lastTurn', 'Last turn', (file) => file.last_turn],
  ['staged', 'Staged', (file) => file.staged],
  ['unstaged', 'Not staged', (file) => file.unstaged],
];

const MARK = {
  modified: 'M', added: 'A', untracked: 'U', deleted: 'D', renamed: 'R',
};

const CSS = `
:host { display: flex; flex: 1; min-height: 0; color: var(--text); font-family: var(--font-sans); }
* { box-sizing: border-box; }
.pane { display: flex; flex: 1; min-height: 0; flex-direction: column; overflow: hidden; }
.bar { display: flex; gap: var(--sp-1); padding: var(--sp-2); flex-shrink: 0; }
.message {
  flex: 1; min-width: 0; color: var(--text); background: var(--bg-2);
  border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 5px var(--sp-2); font: var(--fs-xs) var(--font-sans);
}
.branch { display: flex; align-items: center; gap: var(--sp-2); padding: 0 var(--sp-2) var(--sp-2); }
.branch-glyph { opacity: .7; color: var(--accent-2); }
select {
  flex: 1; min-width: 0; color: var(--text); background: var(--bg-2);
  border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 4px var(--sp-2); font: var(--fs-xs) var(--font-mono);
}
button {
  border: 1px solid var(--border); border-radius: var(--r-sm); cursor: pointer;
  background: var(--bg-3); color: var(--text); padding: 4px var(--sp-2);
  font: var(--fs-xs) var(--font-sans);
}
button:hover { border-color: var(--accent); color: var(--accent); }
button:focus-visible, input:focus-visible, select:focus-visible, .file:focus-visible { outline: none; box-shadow: var(--focus-ring); }
button:disabled { opacity: .55; cursor: default; }
button.ghost { background: transparent; }
button.danger:hover { color: var(--err); border-color: var(--err); }
.refresh { border: 0; background: transparent; font-size: var(--fs-sm); }
.views { display: flex; gap: var(--sp-1); padding: var(--sp-2) var(--sp-2) 0; flex-wrap: wrap; }
.view { border-color: transparent; background: transparent; color: var(--muted); }
.view.on { background: var(--bg-3); border-color: var(--border); color: var(--text); }
.files { flex: 1; overflow: auto; padding: 0 var(--sp-1) var(--sp-2); }
.section { display: flex; align-items: center; gap: var(--sp-2); padding: var(--sp-2) var(--sp-1) var(--sp-1); }
.section-title { font-size: var(--fs-xs); text-transform: uppercase; letter-spacing: .07em; color: var(--muted-2); }
.section-act { margin-left: auto; border: 0; background: none; color: var(--muted); }
.file, .hunk {
  display: flex; width: 100%; align-items: center; gap: var(--sp-2);
  padding: 4px var(--sp-2); border: 0; background: transparent; text-align: left;
  color: var(--text); border-radius: var(--r-sm); font: var(--fs-xs) var(--font-mono);
}
.file:hover, .hunk:hover { background: var(--bg-3); color: var(--text); }
.mark { width: 15px; text-align: center; flex-shrink: 0; font-weight: var(--fw-bold); }
.mark.modified { color: var(--warn); } .mark.added, .mark.untracked { color: var(--ok); }
.mark.deleted { color: var(--err); } .mark.renamed { color: var(--accent-2); }
.path, .hunk-head { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.turn { color: var(--accent); flex-shrink: 0; }
.numbers { margin-left: auto; flex-shrink: 0; }
.add { color: var(--ok); } .del { color: var(--err); margin-left: 4px; }
.icon { border: 0; padding: 0 var(--sp-1); background: none; color: var(--muted); flex-shrink: 0; }
.icon.danger:hover { color: var(--err); }
.hunks { padding-left: var(--sp-5); }
.empty, .state { padding: var(--sp-4) var(--sp-2); text-align: center; color: var(--muted); font-size: var(--fs-sm); }
.state.error { color: var(--err); }
.state button { margin-top: var(--sp-2); }
@media (max-width: 760px) {
  .bar { flex-wrap: wrap; } .message { flex-basis: 100%; }
  .bar button { flex: 1; }
}
`;

export class AxSourceControl extends HTMLElement {
  static get observedAttributes() { return ['session']; }

  #root;
  #status = null;
  #branches = [];
  #view = 'all';
  #phase = 'idle';
  #error = '';
  #generation = 0;
  #confirm = null;
  #message = '';
  #mutation = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS);
    this.#render();
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(value) { value ? this.setAttribute('session', value) : this.removeAttribute('session'); }
  get status() { return this.#status; }
  set confirm(callback) { this.#confirm = typeof callback === 'function' ? callback : null; }

  connectedCallback() { if (this.session) void this.refresh({ branches: true }); }

  attributeChangedCallback(name, before, after) {
    if (name !== 'session' || before === after) return;
    this.#generation += 1;
    this.#mutation = null;
    this.#status = null;
    this.#branches = [];
    this.#phase = after ? 'loading' : 'idle';
    this.#error = '';
    this.#render();
    if (this.isConnected && after) void this.refresh({ branches: true });
  }

  async refresh({ branches = false } = {}) {
    const session = this.session;
    if (!session) return;
    const generation = ++this.#generation;
    this.#phase = 'loading';
    this.#error = '';
    this.#render();
    try {
      const status = await this.#request('/git/status', undefined, session);
      let branchInfo = null;
      if (branches) {
        try { branchInfo = await this.#request('/git/branches', undefined, session); }
        catch (error) { this.#notify('Could not refresh branches', String(error?.message || error), 'warn'); }
      }
      if (!this.#isCurrent(session, generation)) return;
      this.#status = status;
      if (branchInfo && Array.isArray(branchInfo.branches)) this.#branches = branchInfo.branches;
      this.#phase = 'ready';
      this.#render();
      this.#emitStatus();
    } catch (error) {
      if (!this.#isCurrent(session, generation)) return;
      this.#phase = 'error';
      this.#error = String(error?.message || error);
      this.#render();
    }
  }

  async #mutate(path, body, { changed = [], success = null, session = this.session } = {}) {
    if (!session) return null;
    if (this.#mutation) return null;
    const mutation = {};
    this.#mutation = mutation;
    const generation = ++this.#generation;
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
      this.#render();
      this.#emitStatus();
      if (changed.length) this.dispatchEvent(new CustomEvent('files-changed', {
        detail: { paths: changed, status }, bubbles: true, composed: true,
      }));
      if (success) this.#notify(success.title, success.body, 'ok');
      return status;
    } catch (error) {
      if (session === this.session && this.#mutation === mutation) {
        this.#notify('Git operation failed', `${String(error?.message || error)} Refreshing status because the operation may have reached Git.`, 'err');
      }
      return null;
    } finally {
      if (this.#mutation === mutation) {
        this.#mutation = null;
        if (session === this.session) await this.refresh({ branches: true });
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

  #isCurrent(session, generation) { return session === this.session && generation === this.#generation; }

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

  #render() {
    this.#root.innerHTML = `<div class="pane">
      <div class="bar">
        <input class="message" placeholder="Commit message…" spellcheck="false">
        <button data-action="commit">Commit</button>
        <button class="ghost" data-action="commit-all">Stage all + commit</button>
        <button class="ghost danger" data-action="discard-all">Discard unstaged</button>
      </div>
      <div class="branch"><span class="branch-glyph" aria-hidden="true">⎇</span>
        <select title="Switch branch"></select>
        <button class="refresh" data-action="refresh" title="Refresh" aria-label="Refresh">⟳</button>
      </div>
      <div class="files"></div>
    </div>`;
    this.#wireChrome();
    const files = this.#root.querySelector('.files');
    if (this.#phase === 'loading' && !this.#status) {
      files.innerHTML = '<div class="state">Reading Git status…</div>';
      this.#applyBusy();
      return;
    }
    if (this.#phase === 'error') {
      files.innerHTML = '<div class="state error"><div></div><button data-action="retry">Retry</button></div>';
      files.querySelector('div').textContent = this.#error || 'Git status failed.';
      files.querySelector('[data-action="retry"]').onclick = () => void this.refresh({ branches: true });
      this.#applyBusy();
      return;
    }
    this.#renderFiles(files);
    this.#applyBusy();
  }

  #applyBusy() {
    if (!this.#mutation) return;
    this.#root.querySelectorAll('button, input, select').forEach((control) => { control.disabled = true; });
  }

  #wireChrome() {
    const message = this.#root.querySelector('.message');
    message.value = this.#message;
    message.oninput = () => { this.#message = message.value; };
    message.onkeydown = (event) => {
      if (event.key === 'Enter') { event.preventDefault(); void this.#commit(false); }
    };
    this.#root.querySelector('[data-action="commit"]').onclick = () => void this.#commit(false);
    this.#root.querySelector('[data-action="commit-all"]').onclick = () => void this.#commit(true);
    this.#root.querySelector('[data-action="discard-all"]').onclick = () => void this.#discardAll();
    this.#root.querySelector('[data-action="refresh"]').onclick = () => void this.refresh({ branches: true });
    const select = this.#root.querySelector('select');
    const branch = this.#status?.branch || '';
    const names = [...new Set([...this.#branches, ...(branch ? [branch] : [])])];
    for (const name of names) {
      const option = document.createElement('option');
      option.value = name; option.textContent = name; option.selected = name === branch;
      select.append(option);
    }
    select.disabled = !names.length;
    select.onchange = () => void this.#checkout(select.value);
  }

  #renderFiles(host) {
    host.innerHTML = '';
    const all = this.#status?.files || [];
    const views = document.createElement('div');
    views.className = 'views';
    for (const [id, label, predicate] of VIEWS) {
      const count = all.filter(predicate).length;
      const button = document.createElement('button');
      button.className = `view${this.#view === id ? ' on' : ''}`;
      button.textContent = `${label}${count ? ` ${count}` : ''}`;
      button.onclick = () => { this.#view = id; this.#render(); };
      views.append(button);
    }
    host.append(views);
    const predicate = VIEWS.find(([id]) => id === this.#view)?.[2] || VIEWS[0][2];
    const visible = all.filter(predicate);
    if (!visible.length) {
      const empty = document.createElement('div');
      empty.className = 'empty';
      empty.textContent = all.length ? 'Nothing in this view.'
        : this.#status?.clean ? 'No changes — working tree clean.' : 'No changed paths reported.';
      host.append(empty);
      return;
    }
    this.#renderSection(host, 'Staged', visible.filter((file) => file.staged), 'unstage');
    this.#renderSection(host, 'Not staged', visible.filter((file) => file.unstaged), 'stage');
  }

  #renderSection(host, title, files, operation) {
    if (!files.length) return;
    const header = document.createElement('div');
    header.className = 'section';
    const label = document.createElement('span');
    label.className = 'section-title'; label.textContent = `${title} · ${files.length}`;
    const all = document.createElement('button');
    all.className = 'section-act';
    all.textContent = operation === 'stage' ? 'Stage all' : 'Unstage all';
    all.onclick = () => void this.#stage(operation, files.map((file) => file.path));
    header.append(label, all); host.append(header);
    for (const file of files) host.append(this.#fileRow(file, operation));
  }

  #fileRow(file, operation) {
    const row = document.createElement('div');
    row.className = 'file';
    row.tabIndex = 0;
    row.setAttribute('role', 'button');
    const open = () => this.dispatchEvent(new CustomEvent('diff-open', {
      detail: { path: file.path }, bubbles: true, composed: true,
    }));
    row.onclick = open;
    row.onkeydown = (event) => {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        open();
      }
    };
    const mark = document.createElement('span');
    mark.className = `mark ${file.state}`; mark.textContent = MARK[file.state] || '•';
    const path = document.createElement('span');
    path.className = 'path'; path.textContent = file.path;
    row.append(mark, path);
    if (file.last_turn && this.#view !== 'lastTurn') {
      const turn = document.createElement('span'); turn.className = 'turn'; turn.textContent = '✦'; row.append(turn);
    }
    if (file.added != null || file.removed != null) row.append(this.#numbers(file.added, file.removed));
    if (file.state !== 'untracked') {
      const hunks = document.createElement('button');
      hunks.className = 'icon'; hunks.textContent = '≡'; hunks.title = 'Show separate changes';
      hunks.onclick = (event) => { event.stopPropagation(); void this.#toggleHunks(row, file.path, operation === 'unstage'); };
      row.append(hunks);
    }
    const stage = document.createElement('button');
    stage.className = 'icon'; stage.textContent = operation === 'stage' ? '+' : '−';
    stage.title = operation === 'stage' ? 'Stage this file' : 'Unstage this file';
    stage.onclick = (event) => { event.stopPropagation(); void this.#stage(operation, [file.path]); };
    row.append(stage);
    if (operation === 'stage') {
      const discard = document.createElement('button');
      discard.className = 'icon danger'; discard.textContent = '↩'; discard.title = 'Discard changes to this file';
      discard.onclick = (event) => { event.stopPropagation(); void this.#discardFile(file.path); };
      row.append(discard);
    }
    return row;
  }

  #numbers(added, removed) {
    const numbers = document.createElement('span'); numbers.className = 'numbers';
    const add = document.createElement('span'); add.className = 'add'; add.textContent = `+${added || 0}`;
    const del = document.createElement('span'); del.className = 'del'; del.textContent = `−${removed || 0}`;
    numbers.append(add, del); return numbers;
  }

  async #toggleHunks(row, path, staged) {
    const existing = row.nextElementSibling;
    if (existing?.classList.contains('hunks')) { existing.remove(); return; }
    try {
      const hunks = await this.#request(`/git/hunks?path=${encodeURIComponent(path)}${staged ? '&staged=true' : ''}`);
      if (row.getRootNode() !== this.#root) return;
      const host = document.createElement('div'); host.className = 'hunks';
      if (!Array.isArray(hunks) || !hunks.length) {
        const empty = document.createElement('div'); empty.className = 'empty'; empty.textContent = 'No separable changes in this file.'; host.append(empty);
      } else {
        for (const hunk of hunks) host.append(this.#hunkRow(path, staged, hunk));
      }
      row.after(host);
    } catch (error) { this.#notify('Could not read changes', String(error?.message || error), 'err'); }
  }

  #hunkRow(path, staged, hunk) {
    const row = document.createElement('div'); row.className = 'hunk';
    const head = document.createElement('span'); head.className = 'hunk-head'; head.textContent = hunk.header;
    row.append(head, this.#numbers(hunk.added, hunk.removed));
    const stage = document.createElement('button'); stage.className = 'icon'; stage.textContent = staged ? '−' : '+';
    stage.title = staged ? 'Unstage just this change' : 'Stage just this change';
    stage.onclick = () => void this.#hunk(path, hunk.index, !staged); row.append(stage);
    if (!staged) {
      const discard = document.createElement('button'); discard.className = 'icon danger'; discard.textContent = '↺';
      discard.title = 'Discard just this change';
      discard.onclick = () => void this.#discardHunk(path, hunk.index, hunk.header); row.append(discard);
    }
    return row;
  }

  async #stage(operation, paths) {
    await this.#mutate(`/git/${operation}`, { paths }, { changed: paths });
  }

  async #hunk(path, index, stage) {
    await this.#mutate('/git/hunk', { path, index, stage }, { changed: [path] });
  }

  async #discardHunk(path, index, header) {
    const session = this.session;
    const approved = await this.#ask({
      title: 'Discard this change?',
      body: `Throw away ${header} in "${path}"? The rest of the file is untouched. This can't be undone.`,
      okLabel: 'Discard', okKind: 'danger',
    });
    if (!approved || session !== this.session) return;
    await this.#mutate('/git/hunk/discard', { path, index }, { changed: [path], session });
  }

  async #discardFile(path) {
    const session = this.session;
    const approved = await this.#ask({
      title: 'Discard changes?', body: `Discard working changes to "${path}"? This can't be undone.`,
      okLabel: 'Discard', okKind: 'danger',
    });
    if (!approved || session !== this.session) return;
    await this.#mutate('/git/discard', { path }, { changed: [path], session });
  }

  async #discardAll() {
    const session = this.session;
    const paths = (this.#status?.files || []).filter((file) => file.unstaged).map((file) => file.path);
    const count = paths.length;
    if (!count) { this.#notify('Nothing to discard', 'The working tree is clean.', 'warn'); return; }
    const approved = await this.#ask({
      title: 'Discard unstaged changes?',
      body: `Throw away unstaged changes to ${count} file${count === 1 ? '' : 's'}, including untracked ones? Staged work stays intact. This can't be undone.`,
      okLabel: 'Discard unstaged', okKind: 'danger',
    });
    if (!approved || session !== this.session) return;
    await this.#mutate('/git/discard', {}, {
      changed: paths,
      success: { title: 'Discarded unstaged changes', body: `${count} file${count === 1 ? '' : 's'} restored; staged work was kept.` },
      session,
    });
  }

  async #commit(stageAll) {
    const message = this.#message.trim();
    if (!stageAll && !(this.#status?.files || []).some((file) => file.staged)) {
      this.#notify('Nothing staged', 'Stage a file or a hunk first, or use “Stage all + commit”.', 'warn');
      return;
    }
    const status = await this.#mutate('/git/commit', { message, stage_all: stageAll }, {
      success: { title: 'Committed', body: message || 'snapshot' },
    });
    if (status) {
      this.#message = '';
      this.#branches = [];
      await this.refresh({ branches: true });
    }
  }

  async #checkout(reference) {
    const before = this.#status?.branch || '';
    const status = await this.#mutate('/git/checkout', { ref: reference }, {
      changed: (this.#status?.files || []).map((file) => file.path),
      success: { title: 'Switched branch', body: reference },
    });
    if (!status) this.#render();
    else if (status.branch !== reference) this.#notify('Checkout blocked', `Still on ${before || status.branch}.`, 'err');
    else this.dispatchEvent(new CustomEvent('files-changed', {
      detail: { paths: [], all: true, status }, bubbles: true, composed: true,
    }));
  }
}

if (!customElements.get('ax-source-control')) customElements.define('ax-source-control', AxSourceControl);
