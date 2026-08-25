/**
 * `<ax-editor>` — the code editor, and the one Monaco instance behind it.
 *
 * **Deliberately light DOM.** Every other `ax-*` element here uses shadow DOM,
 * and this one must not: Monaco injects its stylesheet into `document.head`,
 * registers `window.require` and `window.monaco`, and measures against the
 * document. Created inside a shadow root it renders nothing — measured, not
 * assumed: zero view lines, and the editor element falling back to the page's
 * sans-serif because its stylesheet never reaches it.
 *
 * So encapsulating it would break its assumptions while leaving its globals in
 * place anyway. The isolation would be illusory, and paid for in a broken
 * editor. Shadow DOM is for components we own; a component hosting a
 * document-global library keeps the document.
 *
 * One editor instance serves every open file, with a model per file and the
 * view state saved per tab swap — cursor, scroll and folds survive switching
 * away and back, the way an editor is expected to behave.
 *
 * @element ax-editor
 *
 * @attr {string} session  Session whose files are being edited.
 * @attr {boolean} suspended  Pause editor work without dropping open buffers.
 * @attr {boolean} disabled  Alias for `suspended`.
 *
 * @fires files-changed  the set of open files, or which is active, changed
 * @fires state-changed  detail: {path, dirty} — a file's saved-ness moved
 * @fires selection      detail: {path, text, startLine, endLine, at:{x,y}} — or
 *                       null text when the selection collapsed. `at` is in
 *                       viewport coordinates so the shell can anchor to it
 *                       without knowing anything about Monaco.
 * @fires save-result    detail: {path, ok, bytes?, error?}
 */

/** Backend language names that are not Monaco language ids. */
const LANG_ALIASES = {
  rs: 'rust', ts: 'typescript', tsx: 'typescript',
  js: 'javascript', jsx: 'javascript', mjs: 'javascript', cjs: 'javascript',
  py: 'python', md: 'markdown', sh: 'shell', bash: 'shell',
  yml: 'yaml', tf: 'hcl', htm: 'html', cc: 'cpp', cxx: 'cpp', hpp: 'cpp',
};

export const monacoLang = (raw) => {
  const k = (raw || '').toLowerCase();
  return LANG_ALIASES[k] || k || 'plaintext';
};

/** One AMD load for the page, however many editors ask for it. */
let monacoReady = null;

export function loadMonaco() {
  if (monacoReady) return monacoReady;
  monacoReady = new Promise((resolve, reject) => {
    // The AMD loader has to be a real <script>; eval will not do.
    const tag = document.createElement('script');
    tag.src = '/vendor/monaco/vs/loader.js';
    tag.onload = () => {
      window.require.config({ paths: { vs: '/vendor/monaco/vs' } });
      window.require(['vs/editor/editor.main'], () => {
        try {
          const dt = document.documentElement.getAttribute('data-theme');
          window.monaco.editor.setTheme(dt === 'light' ? 'vs' : 'vs-dark');
        } catch { /* theme is cosmetic; a failure here must not block loading */ }
        resolve(window.monaco);
      }, reject);
    };
    tag.onerror = () => reject(new Error('Failed to load /vendor/monaco/vs/loader.js'));
    document.head.append(tag);
  });
  return monacoReady;
}

export class AxEditor extends HTMLElement {
  static get observedAttributes() { return ['session', 'suspended', 'disabled']; }

  #host = null;
  #editor = null;
  /** path → { model, viewState, disposer } */
  #models = new Map();
  /** The open files, in tab order. */
  #files = [];
  #active = null;
  #loading = false;
  /** Invalidates every asynchronous operation when the owning session resets. */
  #generation = 0;
  /** Monotonic identities let a newer request for one file supersede an older one. */
  #requestSequence = 0;
  /** path → the currently owning open/save/reload request. */
  #fileRequests = new Map();
  /** The latest deferred Monaco mount. */
  #mountRequest = null;
  /** True while the owning Session has yielded its runtime to Ways. */
  #suspendedMode = false;

  get session() { return this.getAttribute('session') || ''; }
  set session(v) { v ? this.setAttribute('session', v) : this.removeAttribute('session'); }
  get suspended() { return this.#suspendedMode; }
  set suspended(v) { this.toggleAttribute('suspended', Boolean(v)); }
  get disabled() { return this.#suspendedMode; }
  set disabled(v) { this.toggleAttribute('disabled', Boolean(v)); }

  /** Open files, as plain data for whoever draws the tabs. */
  get files() {
    return this.#files.map((f) => ({
      path: f.path, lang: f.lang, dirty: !!f.dirty,
      truncated: !!f.truncated, active: f.path === this.#active,
    }));
  }

  get active() { return this.#active; }
  get activeFile() { return this.#files.find((f) => f.path === this.#active) || null; }

  connectedCallback() {
    this.style.display = 'flex';
    this.style.minHeight = '0';
    this.style.minWidth = '0';
    this.#syncSuspension();
  }

  attributeChangedCallback(name, prev, next) {
    // A different session is a different project; nothing carries over.
    if (name === 'session' && prev !== next) this.reset();
    if ((name === 'suspended' || name === 'disabled') && prev !== next) this.#syncSuspension();
  }

  /** Open a file, or activate it if already open. */
  async open(path) {
    if (this.#suspendedMode) return;
    const already = this.#files.find((f) => f.path === path);
    if (already) { this.activate(path); return; }
    const session = this.session;
    if (!session) return;
    const request = this.#beginFileRequest('open', session, path);
    this.#loading = true;
    this.#announce();
    try {
      const response = await fetch(`/api/sessions/${encodeURIComponent(session)}`
        + `/file?path=${encodeURIComponent(path)}`, { signal: request.controller.signal });
      if (!this.#ownsFileRequest(request)) {
        this.#finishFileRequest(request);
        return;
      }
      const j = await response.json();
      if (!this.#ownsFileRequest(request)) {
        this.#finishFileRequest(request);
        return;
      }
      this.#finishFileRequest(request);
      if (j.error) { this.#fail(j.error); return; }
      // Two callers may ask for the same unopened path before either request
      // finishes. Only one file record and model may own that path.
      if (this.#files.some((f) => f.path === path)) return;
      this.#files.push({
        path,
        content: j.content || '',
        lang: j.lang || '',
        truncated: !!j.truncated,
        draft: null,
        dirty: false,
      });
      this.activate(path);
    } catch (e) {
      if (!this.#ownsFileRequest(request)) {
        this.#finishFileRequest(request);
        return;
      }
      this.#finishFileRequest(request);
      this.#fail(String(e));
    }
  }

  /** Make a file the visible one, preserving where you were in the last. */
  activate(path) {
    if (this.#suspendedMode) return;
    if (!this.#files.some((f) => f.path === path)) return;
    this.#active = path;
    this.#announce();
    void this.#mount();
  }

  /**
   * Close a file and release its model.
   *
   * Both the model and its change listener are disposed: leaving the listener
   * would keep firing dirty events for a file nobody has open.
   */
  close(path) {
    const i = this.#files.findIndex((f) => f.path === path);
    if (i === -1) return;
    const entry = this.#models.get(path);
    if (entry) {
      try { entry.disposer?.dispose(); } catch { /* already disposed */ }
      try { entry.model?.dispose(); } catch { /* already disposed */ }
      this.#models.delete(path);
    }
    this.#files.splice(i, 1);
    if (this.#active === path) {
      this.#active = this.#files.length ? this.#files[Math.max(0, i - 1)].path : null;
    }
    this.#announce();
    if (this.#active) void this.#mount(); else this.#showEmpty();
  }

  /** Close everything. Used when the session changes. */
  reset() {
    this.#invalidateAsync();
    for (const path of [...this.#models.keys()]) this.close(path);
    this.#files = [];
    this.#active = null;
    this.#announce();
    this.#showEmpty();
  }

  /** Current text of a file — the model when it exists, else what was read. */
  contentOf(path) {
    const entry = this.#models.get(path);
    const f = this.#files.find((x) => x.path === path);
    if (entry?.model) return entry.model.getValue();
    return f ? (f.draft ?? f.content ?? '') : '';
  }

  /** Write the active file back. Truncated files are never written. */
  async save() {
    if (this.#suspendedMode) return;
    const f = this.activeFile;
    const session = this.session;
    if (!f || !session) return;
    if (f.truncated) {
      this.#emit('save-result', { path: f.path, ok: false, error: 'file was truncated' });
      return;
    }
    const content = this.contentOf(f.path);
    const request = this.#beginFileRequest('save', session, f.path, f);
    try {
      const r = await fetch(`/api/sessions/${encodeURIComponent(session)}`
        + `/file?path=${encodeURIComponent(f.path)}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ content }),
        signal: request.controller.signal,
      });
      if (!this.#ownsFileRequest(request)) {
        this.#finishFileRequest(request);
        return;
      }
      const j = await r.json().catch(() => ({}));
      if (!this.#ownsFileRequest(request)) {
        this.#finishFileRequest(request);
        return;
      }
      this.#finishFileRequest(request);
      if (!r.ok) {
        this.#emit('save-result', { path: f.path, ok: false, error: j.error || `HTTP ${r.status}` });
        return;
      }
      f.content = content;
      f.draft = null;
      f.dirty = false;
      this.#emit('state-changed', { path: f.path, dirty: false });
      this.#emit('save-result', { path: f.path, ok: true, bytes: j.bytes });
      this.#announce();
    } catch (e) {
      if (!this.#ownsFileRequest(request)) {
        this.#finishFileRequest(request);
        return;
      }
      this.#finishFileRequest(request);
      this.#emit('save-result', { path: f.path, ok: false, error: String(e) });
    }
  }

  /**
   * Re-read a file that changed underneath us, keeping unsaved work.
   *
   * An agent editing a file you have open is ordinary here, and silently
   * replacing your buffer would destroy work you had not saved. A dirty file is
   * left alone and reported instead.
   */
  async reload(path) {
    if (this.#suspendedMode) return false;
    const f = this.#files.find((x) => x.path === path);
    const session = this.session;
    if (!f || !session) return false;
    if (f.dirty) return false;
    const request = this.#beginFileRequest('reload', session, path, f);
    try {
      const response = await fetch(`/api/sessions/${encodeURIComponent(session)}`
        + `/file?path=${encodeURIComponent(path)}`, { signal: request.controller.signal });
      if (!this.#ownsFileRequest(request) || f.dirty) {
        this.#finishFileRequest(request);
        return false;
      }
      const j = await response.json();
      if (!this.#ownsFileRequest(request) || f.dirty) {
        this.#finishFileRequest(request);
        return false;
      }
      this.#finishFileRequest(request);
      if (j.error) return false;
      f.content = j.content || '';
      f.truncated = !!j.truncated;
      const entry = this.#models.get(path);
      if (entry?.model && entry.model.getValue() !== f.content) {
        // setValue would jump the cursor to the top; pushEditOperations keeps
        // the position, which matters when the file you are reading is being
        // edited under you.
        const full = entry.model.getFullModelRange();
        entry.model.pushEditOperations([], [{ range: full, text: f.content }], () => null);
      }
      this.#announce();
      return true;
    } catch {
      this.#finishFileRequest(request);
      return false;
    }
  }

  /** Revert the active file to what is on disk. */
  revert(path) {
    if (this.#suspendedMode) return;
    const f = this.#files.find((x) => x.path === (path || this.#active));
    if (!f) return;
    const entry = this.#models.get(f.path);
    if (entry?.model) entry.model.setValue(f.content || '');
    f.draft = null;
    f.dirty = false;
    this.#emit('state-changed', { path: f.path, dirty: false });
    this.#announce();
  }

  focus() {
    if (this.#suspendedMode) return;
    try { this.#editor?.focus(); } catch { /* not created yet */ }
  }

  /**
   * Re-measure.
   *
   * Twice, deliberately. Callers relayout right after moving the element back
   * into a pane — a preview or a diff had it — and at that instant the browser
   * has not laid the element out yet, so Monaco measures zero and draws no
   * lines. `automaticLayout` does not save us: it saw the same zero. The
   * second pass runs after layout has happened and is the one that renders.
   */
  layout() {
    const go = () => { try { this.#editor?.layout(); } catch { /* not created yet */ } };
    go();
    requestAnimationFrame(go);
  }

  // ── internals ────────────────────────────────────────────────────────

  #emit(name, detail) {
    this.dispatchEvent(new CustomEvent(name, { detail, bubbles: true, composed: true }));
  }

  #announce() { this.#emit('files-changed', { files: this.files, active: this.#active, loading: this.#loading }); }

  #showEmpty() { this.textContent = ''; }

  #beginFileRequest(kind, session, path, file = null) {
    try { this.#fileRequests.get(path)?.controller.abort(); } catch { /* already settled */ }
    const request = {
      kind,
      session,
      path,
      file,
      generation: this.#generation,
      id: ++this.#requestSequence,
      controller: new AbortController(),
    };
    this.#fileRequests.set(path, request);
    return request;
  }

  #ownsFileRequest(request) {
    if (this.#suspendedMode) return false;
    if (request.session !== this.session || request.generation !== this.#generation) return false;
    if (this.#fileRequests.get(request.path) !== request) return false;
    return !request.file || this.#files.find((f) => f.path === request.path) === request.file;
  }

  #finishFileRequest(request) {
    if (this.#fileRequests.get(request.path) !== request) return;
    this.#fileRequests.delete(request.path);
    this.#loading = [...this.#fileRequests.values()].some((pending) => pending.kind === 'open');
  }

  #invalidateAsync() {
    this.#generation += 1;
    for (const request of this.#fileRequests.values()) {
      try { request.controller.abort(); } catch { /* already settled */ }
    }
    this.#fileRequests.clear();
    this.#mountRequest = null;
    this.#loading = false;
  }

  #syncSuspension() {
    const suspended = this.hasAttribute('suspended') || this.hasAttribute('disabled');
    if (suspended === this.#suspendedMode) return;
    this.#suspendedMode = suspended;
    this.#invalidateAsync();
    const f = this.activeFile;
    try { this.#editor?.updateOptions({ readOnly: suspended || !!f?.truncated }); } catch { /* not mounted */ }
    this.#announce();
    if (!suspended) {
      const session = this.session;
      const generation = this.#generation;
      const cleanPaths = this.#files.filter((file) => !file.dirty).map((file) => file.path);
      void this.#reloadCleanFilesAfterResume(session, generation, cleanPaths);
      if (f) void this.#mount();
    }
  }

  async #reloadCleanFilesAfterResume(session, generation, paths) {
    await Promise.all(paths.map(async (path) => {
      if (this.#suspendedMode || session !== this.session || generation !== this.#generation) return;
      await this.reload(path);
    }));
  }

  #fail(msg) {
    this.textContent = '';
    const d = document.createElement('div');
    d.className = 'empty small';
    d.textContent = msg;
    this.append(d);
  }

  async #mount() {
    if (this.#suspendedMode) return;
    const f = this.activeFile;
    if (!f) { this.#showEmpty(); return; }
    const request = {
      session: this.session,
      generation: this.#generation,
      path: f.path,
      file: f,
      id: ++this.#requestSequence,
    };
    this.#mountRequest = request;

    // The host survives tab switches so the editor instance is never rebuilt.
    if (!this.#host) {
      this.#host = document.createElement('div');
      this.#host.className = 'monaco-host';
      this.#host.id = 'monaco-host';
    }
    // Clear around the host rather than through it: detaching Monaco's DOM on
    // every tab switch forces a full re-render and drops keyboard focus.
    for (const n of [...this.childNodes]) if (n !== this.#host) n.remove();
    if (f.truncated) {
      const banner = document.createElement('div');
      banner.className = 'file-truncated-banner';
      banner.textContent =
        '⚠ File truncated by backend — saves are disabled. Use the CLI to edit large files.';
      this.prepend(banner);
    }
    if (this.#host.parentNode !== this) this.append(this.#host);

    let monaco;
    try {
      monaco = await loadMonaco();
    } catch (e) {
      if (this.#ownsMount(request)) this.#fail(`Editor failed to load: ${e}`);
      return;
    }
    // The session or active file may have changed while Monaco was loading.
    if (!this.#ownsMount(request)) return;

    if (!this.#editor) {
      this.#editor = monaco.editor.create(this.#host, {
        automaticLayout: true,
        minimap: { enabled: true },
        fontSize: 13,
        scrollBeyondLastLine: false,
        wordWrap: 'off',
        tabSize: 2,
      });
      this.#editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => void this.save());
      // Selection is reported rather than acted on: it exists so the shell can
      // offer to put a reference into the conversation, which is the
      // conversation's business, not the editor's.
      const report = () => {
        const sel = this.#editor.getSelection();
        const model = this.#editor.getModel();
        if (!sel || !model || sel.isEmpty()) {
          this.#emit('selection', { path: this.#active, text: null });
          return;
        }
        // Anchor above the live end of the selection. Monaco draws its own
        // viewport, so this position cannot be recovered from the DOM outside.
        const vis = this.#editor.getScrolledVisiblePosition(sel.getEndPosition());
        const box = this.#editor.getDomNode()?.getBoundingClientRect();
        this.#emit('selection', {
          path: this.#active,
          text: model.getValueInRange(sel),
          startLine: sel.startLineNumber,
          endLine: sel.endLineNumber,
          lang: this.activeFile?.lang || '',
          at: (vis && box) ? { x: box.left + vis.left + 6, y: box.top + vis.top - 30 } : null,
        });
      };
      this.#editor.onDidChangeCursorSelection(report);
      this.#editor.onDidScrollChange(report);
    }

    let entry = this.#models.get(f.path);
    if (!entry) {
      const uri = monaco.Uri.parse(`axo://session/${encodeURIComponent(f.path)}`);
      const model = monaco.editor.getModel(uri)
        || monaco.editor.createModel(f.content || '', monacoLang(f.lang), uri);
      entry = { model, viewState: null, disposer: null };
      this.#models.set(f.path, entry);
      entry.disposer = model.onDidChangeContent(() => {
        f.draft = model.getValue();
        const dirty = f.draft !== (f.content || '');
        if (dirty !== f.dirty) {
          f.dirty = dirty;
          this.#emit('state-changed', { path: f.path, dirty });
          this.#announce();
        }
      });
    }

    // Save where we were in the outgoing file before switching models, so
    // coming back restores cursor, scroll and folds.
    const current = this.#editor.getModel();
    if (current && current !== entry.model) {
      const prev = [...this.#models.entries()].find(([, e]) => e.model === current);
      if (prev) prev[1].viewState = this.#editor.saveViewState();
    }
    this.#editor.setModel(entry.model);
    if (entry.viewState) this.#editor.restoreViewState(entry.viewState);
    this.#editor.updateOptions({ readOnly: !!f.truncated });
    this.#editor.focus();
  }

  #ownsMount(request) {
    return !this.#suspendedMode
      && this.#mountRequest === request
      && request.session === this.session
      && request.generation === this.#generation
      && request.path === this.#active
      && this.activeFile === request.file;
  }
}

customElements.define('ax-editor', AxEditor);
