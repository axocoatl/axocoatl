import { adopt } from './sheets.js';

/**
 * `<ax-file-tree>` — the session's working directory, lazily expanded.
 *
 * Folders load their children on first open rather than up front, so a large
 * repository costs one request per folder the user actually looks in. Filtering
 * walks what is loaded and auto-expands any folder holding a match, so a search
 * reveals hits inside collapsed folders instead of appearing to find nothing.
 *
 * The element never opens a file itself. It reports the click as `file-open`
 * and lets whatever owns the layout decide — the same tree then serves an
 * editor pane, a diff view, or a preview without knowing any of them exist.
 *
 * @element ax-file-tree
 *
 * @attr {string} session   Session id whose tree to show. Changing it reloads.
 * @attr {string} filter    Substring filter over loaded rows; empty shows all.
 * @attr {string} selected  Path of the row to mark selected.
 *
 * @fires file-open  detail: {path: string} — a file row was activated
 * @fires dir-toggle detail: {path: string, open: boolean}
 *
 * @cssprop --ax-file-tree-pad       Padding around the rows (default --sp-1)
 * @cssprop --ax-file-tree-row-gap   Gap between icon and name
 * @cssprop --ax-file-tree-indent    Indent per nesting level
 *
 * @csspart tree   The scrolling container
 * @csspart row    A single file or folder row
 */

/**
 * Extension → [codicon glyph, brand tint].
 *
 * The tints are brand colours, not language colours: jade for the things this
 * project is made of, blue for structured/typed sources, bronze for scripting
 * and markup, mute for everything incidental. A file tree that renders in a
 * dozen unrelated hues reads as decoration.
 */
const FILE_ICONS = {
  rs: ['file-code', 'tint-jade'],
  toml: ['settings-gear', 'tint-bronze'], cargo: ['settings-gear', 'tint-bronze'],
  js: ['file-code', 'tint-bronze'], mjs: ['file-code', 'tint-bronze'], cjs: ['file-code', 'tint-bronze'],
  ts: ['file-code', 'tint-blue'], tsx: ['file-code', 'tint-blue'], jsx: ['file-code', 'tint-bronze'],
  html: ['file-code', 'tint-bronze'], htm: ['file-code', 'tint-bronze'],
  css: ['file-code', 'tint-blue'], scss: ['file-code', 'tint-blue'],
  sass: ['file-code', 'tint-blue'], less: ['file-code', 'tint-blue'],
  svg: ['symbol-color', 'tint-jade'],
  json: ['json', 'tint-bronze'],
  yaml: ['file', 'tint-mute'], yml: ['file', 'tint-mute'],
  csv: ['table', 'tint-mute'],
  md: ['markdown', 'tint-jade'], markdown: ['markdown', 'tint-jade'],
  txt: ['file-text', 'tint-mute'],
  py: ['file-code', 'tint-blue'], go: ['file-code', 'tint-blue'],
  java: ['file-code', 'tint-bronze'],
  c: ['file-code', 'tint-mute'], h: ['file-code', 'tint-mute'],
  cpp: ['file-code', 'tint-mute'], cc: ['file-code', 'tint-mute'],
  cxx: ['file-code', 'tint-mute'], hpp: ['file-code', 'tint-mute'],
  sh: ['terminal', 'tint-mute'], bash: ['terminal', 'tint-mute'],
  sql: ['database', 'tint-blue'],
  xml: ['file-code', 'tint-mute'],
  env: ['settings-gear', 'tint-mute'],
  gitignore: ['file', 'tint-mute'], dockerfile: ['file', 'tint-blue'],
  lock: ['lock', 'tint-mute'],
  png: ['file-media', 'tint-jade'], jpg: ['file-media', 'tint-jade'],
  jpeg: ['file-media', 'tint-jade'], gif: ['file-media', 'tint-jade'],
  webp: ['file-media', 'tint-jade'], ico: ['file-media', 'tint-jade'],
  pdf: ['file-pdf', 'tint-bronze'],
  zip: ['file-zip', 'tint-mute'], tar: ['file-zip', 'tint-mute'], gz: ['file-zip', 'tint-mute'],
};

/** Files whose meaning is in the whole name, not an extension. */
const BY_NAME = {
  dockerfile: ['file', 'tint-blue'],
  license: ['law', 'tint-mute'], 'license.md': ['law', 'tint-mute'],
  readme: ['book', 'tint-bronze'], 'readme.md': ['book', 'tint-bronze'],
};

export function iconForFile(name) {
  const lower = name.toLowerCase();
  if (BY_NAME[lower]) return BY_NAME[lower];
  const dot = lower.lastIndexOf('.');
  return FILE_ICONS[dot >= 0 ? lower.slice(dot + 1) : ''] || ['file', 'tint-mute'];
}

const CSS = `
:host {
  display: flex;
  flex-direction: column;
  min-height: 0;
  font-family: var(--font-sans);
  color: var(--text);
}
.tree {
  flex: 1;
  overflow: auto;
  padding: var(--ax-file-tree-pad, var(--sp-1)) var(--sp-1);
  font-size: var(--fs-sm);
}
.empty {
  color: var(--muted);
  font-size: var(--fs-xs);
  text-align: center;
  padding: var(--sp-3);
}
.row {
  display: flex;
  align-items: center;
  gap: var(--ax-file-tree-row-gap, 6px);
  padding: 2px var(--sp-2);
  border-radius: var(--r-sm);
  cursor: pointer;
  white-space: nowrap;
  transition: background var(--dur-fast) var(--ease);
}
.row:hover { background: var(--bg-3); }
.row[aria-selected="true"] { background: rgba(var(--axo-jade-rgb), .15); }
.row:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.row.hide { display: none; }
.name { overflow: hidden; text-overflow: ellipsis; }
.match { color: var(--accent); font-weight: var(--fw-bold); }
/* Chevron sits in a fixed slot so file and folder rows align at the same x. */
.chev {
  width: 12px; text-align: center; opacity: .65;
  font-size: var(--fs-xs); flex-shrink: 0;
  transition: transform var(--dur-fast) var(--ease);
}
.chev.open { transform: rotate(90deg); }
.ico {
  width: 16px; height: 16px; flex-shrink: 0;
  display: inline-flex; align-items: center; justify-content: center;
}
.ico.codicon { font-size: 14px; line-height: 16px; }
.tint-jade   { color: var(--axo-jade); }
.tint-bronze { color: var(--axo-bronze); }
.tint-blue   { color: #6cc1d9; }
.tint-mute   { color: var(--muted); }
.kids { margin-left: var(--ax-file-tree-indent, 18px); }
.kids.hide { display: none; }
`;

export class AxFileTree extends HTMLElement {
  static get observedAttributes() { return ['session', 'filter', 'selected']; }

  #root;
  #tree;
  /** Folders already expanded once, so re-filtering never refetches. */
  #loaded = new Set();
  /**
   * Which reload is current.
   *
   * Loading is asynchronous, so two reloads can be in flight at once — setting
   * `session` before insertion fires `attributeChangedCallback` *and* then
   * `connectedCallback`, which is the ordinary way to use the element. Each
   * clears the tree and appends when its fetch resolves, so without this both
   * results land and every row appears twice. Every append checks that its
   * generation is still the live one and drops itself if not.
   */
  #gen = 0;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#tree = document.createElement('div');
    this.#tree.className = 'tree';
    this.#tree.setAttribute('part', 'tree');
    this.#tree.setAttribute('role', 'tree');
    this.#root.append(this.#tree);
    adopt(this.#root, CSS);
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(v) { v ? this.setAttribute('session', v) : this.removeAttribute('session'); }

  get filter() { return this.getAttribute('filter') || ''; }
  set filter(v) { v ? this.setAttribute('filter', v) : this.removeAttribute('filter'); }

  get selected() { return this.getAttribute('selected') || ''; }
  set selected(v) { v ? this.setAttribute('selected', v) : this.removeAttribute('selected'); }

  connectedCallback() { if (this.session) this.reload(); }

  attributeChangedCallback(name, prev, next) {
    if (prev === next) return;
    if (name === 'session') { this.#loaded.clear(); this.reload(); }
    if (name === 'filter') this.#applyFilter();
    if (name === 'selected') this.#applySelection();
  }

  /** Reload from the root. Safe to call at any time, including concurrently. */
  async reload() {
    const gen = ++this.#gen;
    this.#tree.textContent = '';
    if (!this.session) return;
    await this.#load('', this.#tree, gen);
    if (gen !== this.#gen) return;
    this.#applySelection();
    this.#applyFilter();
  }

  async #fetch(relPath) {
    const url = `/api/sessions/${encodeURIComponent(this.session)}/tree`
      + (relPath ? `?path=${encodeURIComponent(relPath)}` : '');
    try {
      const entries = await fetch(url).then((r) => r.json());
      return Array.isArray(entries) ? entries : [];
    } catch {
      // A tree that cannot be read is reported as empty rather than thrown:
      // the pane is one part of a session and must not take the page with it.
      return [];
    }
  }

  async #load(relPath, container, gen) {
    const entries = await this.#fetch(relPath);
    if (gen !== this.#gen) return; // superseded while fetching
    if (!entries.length && !relPath) {
      const empty = document.createElement('div');
      empty.className = 'empty';
      empty.textContent = 'Empty directory';
      container.append(empty);
      return;
    }
    for (const e of entries) {
      container.append(e.kind === 'dir' ? this.#dirRow(e, gen) : this.#fileRow(e));
    }
  }

  #row(e, glyph, tint, withChevron) {
    const row = document.createElement('div');
    row.className = 'row';
    row.setAttribute('part', 'row');
    row.dataset.path = e.path;
    row.dataset.name = e.name;
    row.tabIndex = 0;

    const chev = document.createElement('i');
    chev.className = withChevron ? 'chev codicon codicon-chevron-right' : 'chev';
    const ico = document.createElement('i');
    ico.className = `ico codicon codicon-${glyph} ${tint}`;
    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = e.name;

    row.append(chev, ico, name);
    return row;
  }

  #fileRow(e) {
    const [glyph, tint] = iconForFile(e.name);
    const row = this.#row(e, glyph, tint, false);
    row.setAttribute('role', 'treeitem');
    const open = () => {
      this.selected = e.path;
      this.dispatchEvent(new CustomEvent('file-open', {
        detail: { path: e.path }, bubbles: true, composed: true,
      }));
    };
    row.addEventListener('click', open);
    row.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); open(); }
    });
    return row;
  }

  #dirRow(e, gen) {
    const row = this.#row(e, 'folder', 'tint-bronze', true);
    row.setAttribute('role', 'treeitem');
    row.setAttribute('aria-expanded', 'false');

    const kids = document.createElement('div');
    kids.className = 'kids hide';
    kids.setAttribute('role', 'group');

    const toggle = async () => {
      const opening = kids.classList.contains('hide');
      kids.classList.toggle('hide', !opening);
      row.setAttribute('aria-expanded', String(opening));
      row.querySelector('.chev').classList.toggle('open', opening);
      row.querySelector('.ico').className =
        `ico codicon codicon-${opening ? 'folder-opened' : 'folder'} tint-bronze`;
      if (opening && !this.#loaded.has(e.path)) {
        this.#loaded.add(e.path);
        await this.#load(e.path, kids, gen);
        if (gen !== this.#gen) return; // the whole tree reloaded under us
        this.#applySelection();
        this.#applyFilter();
      }
      this.dispatchEvent(new CustomEvent('dir-toggle', {
        detail: { path: e.path, open: opening }, bubbles: true, composed: true,
      }));
    };
    row.addEventListener('click', toggle);
    row.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); toggle(); }
    });

    const wrap = document.createElement('div');
    wrap.append(row, kids);
    return wrap;
  }

  #applySelection() {
    for (const row of this.#root.querySelectorAll('.row')) {
      row.setAttribute('aria-selected', String(row.dataset.path === this.selected));
    }
  }

  /**
   * Hide rows that do not match, and open any folder that contains one.
   *
   * Without the auto-expand a search over a collapsed tree appears to find
   * nothing, which reads as a broken filter rather than a closed folder.
   */
  #applyFilter() {
    const q = this.filter.trim().toLowerCase();
    const rows = [...this.#root.querySelectorAll('.row')];

    if (!q) {
      for (const row of rows) {
        row.classList.remove('hide');
        this.#highlight(row, '');
      }
      return;
    }

    for (const row of rows) {
      const isDir = row.getAttribute('aria-expanded') !== null;
      const hit = row.dataset.name.toLowerCase().includes(q);
      row.classList.toggle('hide', !hit && !isDir);
      this.#highlight(row, hit ? q : '');
    }
    // A folder survives only if something under it does; walk deepest-first so
    // a match several levels down keeps its whole chain of ancestors visible.
    for (const kids of [...this.#root.querySelectorAll('.kids')].reverse()) {
      const row = kids.previousElementSibling;
      if (!row) continue;
      const visible = [...kids.querySelectorAll('.row')].some((r) => !r.classList.contains('hide'));
      const self = row.dataset.name.toLowerCase().includes(q);
      row.classList.toggle('hide', !visible && !self);
      if (visible) {
        kids.classList.remove('hide');
        row.querySelector('.chev').classList.add('open');
        row.setAttribute('aria-expanded', 'true');
      }
    }
  }

  /** Wrap the matching run of the name so the hit is visible in place. */
  #highlight(row, q) {
    const name = row.querySelector('.name');
    const text = row.dataset.name;
    if (!q) { name.textContent = text; return; }
    const at = text.toLowerCase().indexOf(q);
    if (at < 0) { name.textContent = text; return; }
    name.textContent = '';
    const mark = document.createElement('span');
    mark.className = 'match';
    mark.textContent = text.slice(at, at + q.length);
    name.append(text.slice(0, at), mark, text.slice(at + q.length));
  }
}

customElements.define('ax-file-tree', AxFileTree);
