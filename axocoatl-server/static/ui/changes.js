import { adopt } from './sheets.js';

/**
 * `<ax-changes>` — what the agents changed, as one card in the thread.
 *
 * A run emits a card per tool call, which answers "what happened" and not the
 * question a reviewer actually has: *what did this turn do to my repository*.
 * Twelve edit cards scrolling past is not an answer; "3 files, +142 −8" is, and
 * the per-file counts tell you which one to read first — "3 files" alone cannot
 * distinguish a typo fix from a rewrite.
 *
 * Long lists collapse rather than truncate. Limiting how much renders at once is
 * reasonable; limiting what you can *reach* is the bug, so every file stays one
 * click away.
 *
 * The card opens nothing itself. It reports `file-open` and lets the shell route
 * it, the same as the tree — one gesture, one meaning, wherever a path appears.
 *
 * @element ax-changes
 *
 * @attr {string} session   Session to read the working tree of.
 * @attr {number} preview   Files shown before collapsing (default 3).
 * @attr {boolean} disabled Preserve cached evidence without reading or acting on it.
 * @attr {boolean} suspended Alias for disabled, for temporary runtime suspension.
 *
 * @fires file-open  detail: {path, scope}
 * @fires review     detail: {scope, paths} — review current changes on this turn's attributed paths
 * @fires changes-error detail: {session, message}
 */

const SHOWN = 3;

const STATE_MARK = {
  added: ['A', 'add'],
  modified: ['M', 'mod'],
  deleted: ['D', 'del'],
  renamed: ['R', 'mod'],
  untracked: ['U', 'add'],
};

const CSS = `
:host { display: block; font-family: var(--font-sans); }
:host([empty]) { display: none; }
.card {
  border: 1px solid var(--border); border-radius: var(--r-lg);
  background: var(--panel); overflow: hidden; margin: var(--sp-2) 0;
  max-width: 900px;
}
.head {
  display: flex; align-items: center; gap: var(--sp-3);
  padding: var(--sp-3);
}
.ico {
  width: 26px; height: 26px; border-radius: var(--r-md); flex-shrink: 0;
  display: flex; align-items: center; justify-content: center;
  background: var(--bg-3); color: var(--accent); font-size: var(--fs-sm);
}
.title { flex: 1; min-width: 0; }
.what { font-size: var(--fs-sm); }
.stat { font: var(--fs-xs) var(--font-mono); margin-top: 1px; }
.add { color: var(--ok); }
.del { color: var(--err); }
.mod { color: var(--muted); }
button.act {
  background: none; border: 1px solid var(--border-strong); color: var(--text);
  border-radius: var(--r-md); padding: 3px var(--sp-3); cursor: pointer;
  font: var(--fw-medium) var(--fs-xs) var(--font-sans); flex-shrink: 0;
}
button.act:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
button.act:focus-visible { outline: none; box-shadow: var(--focus-ring); }
button:disabled { opacity: .45; cursor: not-allowed; }
.files { border-top: 1px solid var(--border); }
.f {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: 4px var(--sp-3); cursor: pointer; font: var(--fs-xs) var(--font-mono);
  border: 0; background: none; width: 100%; text-align: left; color: var(--text);
  transition: background var(--dur-fast) var(--ease);
}
.f:hover:not(:disabled) { background: var(--bg-3); }
.f:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.f .mark { width: 12px; flex-shrink: 0; opacity: .8; }
.f .path { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; direction: rtl; text-align: left; }
.f .n { flex-shrink: 0; }
.more {
  width: 100%; background: none; border: 0; border-top: 1px solid var(--border);
  color: var(--muted); padding: var(--sp-2); cursor: pointer;
  font: var(--fs-xs) var(--font-sans);
}
.more:hover:not(:disabled) { color: var(--accent); }
.error {
  border: 1px solid color-mix(in srgb, var(--err) 55%, var(--border));
  border-radius: var(--r-lg); background: var(--panel); color: var(--err);
  display: flex; align-items: center; gap: var(--sp-3); margin: var(--sp-2) 0;
  max-width: 900px; padding: var(--sp-3);
}
.error span { flex: 1; min-width: 0; overflow-wrap: anywhere; }
`;

const html = (value) => String(value ?? '')
  .replaceAll('&', '&amp;')
  .replaceAll('<', '&lt;')
  .replaceAll('>', '&gt;')
  .replaceAll('"', '&quot;')
  .replaceAll("'", '&#39;');

export class AxChanges extends HTMLElement {
  static get observedAttributes() { return ['session', 'disabled', 'suspended']; }

  #root; #files = []; #expanded = false; #phase = 'idle'; #error = ''; #requestId = 0;
  #refreshController = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS);
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(v) { v ? this.setAttribute('session', v) : this.removeAttribute('session'); }

  get preview() { return Number(this.getAttribute('preview')) || SHOWN; }

  get disabled() { return this.hasAttribute('disabled') || this.hasAttribute('suspended'); }
  set disabled(v) { this.toggleAttribute('disabled', Boolean(v)); }

  get suspended() { return this.disabled; }
  set suspended(v) { this.toggleAttribute('suspended', Boolean(v)); }

  connectedCallback() { if (this.session && !this.suspended) void this.refresh(); }
  disconnectedCallback() {
    ++this.#requestId;
    this.#abortRefresh();
  }
  attributeChangedCallback(n, p, x) {
    if (n === 'session' && p !== x) {
      this.#expanded = false;
      ++this.#requestId;
      this.#abortRefresh();
      if (this.suspended) {
        // Evidence belongs to one Session. Suspension itself preserves the
        // cache, but rebinding must not display another Session's evidence.
        this.#files = [];
        this.#phase = 'idle';
        this.#error = '';
        this.#render();
      } else if (this.isConnected) void this.refresh();
      return;
    }
    if ((n === 'disabled' || n === 'suspended') && p !== x) {
      // Invalidate a response already in flight before rendering the preserved
      // cache as inert. Resuming performs one fresh, authoritative read.
      ++this.#requestId;
      this.#abortRefresh();
      this.#render();
      if (!this.suspended && this.isConnected && this.session) void this.refresh();
    }
  }

  /** Re-read the working tree. */
  async refresh() {
    if (this.suspended || !this.isConnected) return;
    const session = this.session;
    const requestId = ++this.#requestId;
    this.#abortRefresh();
    if (!session) {
      this.#files = [];
      this.#phase = 'idle';
      this.#error = '';
      this.#render();
      return;
    }
    this.#phase = 'loading';
    this.#error = '';
    this.#render();
    const controller = new AbortController();
    this.#refreshController = controller;
    try {
      const response = await fetch(
        `/api/sessions/${encodeURIComponent(session)}/git/status`,
        { signal: controller.signal });
      const st = await response.json().catch(() => ({}));
      if (!response.ok || st?.error) throw new Error(st?.error || `HTTP ${response.status}`);
      if (!Array.isArray(st?.files)) throw new Error('Git status returned an invalid file list.');
      if (requestId !== this.#requestId || session !== this.session
          || this.suspended || !this.isConnected) return;
      // A turn card must never absorb pre-existing workspace dirt. The
      // canonical Git status projection marks only paths attributed to the
      // latest accepted turn; Source Control can still reveal the whole tree.
      this.#files = st.files.filter((file) => file?.last_turn === true);
      this.#phase = 'ready';
    } catch (error) {
      if (requestId !== this.#requestId || session !== this.session
          || this.suspended || !this.isConnected) return;
      if (controller.signal.aborted || error?.name === 'AbortError') return;
      this.#files = [];
      this.#phase = 'error';
      this.#error = String(error?.message || error || 'Git status could not be read.');
      this.dispatchEvent(new CustomEvent('changes-error', {
        detail: { session, message: this.#error }, bubbles: true, composed: true,
      }));
    } finally {
      if (this.#refreshController === controller) this.#refreshController = null;
    }
    if (requestId === this.#requestId && session === this.session
        && !this.suspended && this.isConnected) this.#render();
  }

  #abortRefresh() {
    const controller = this.#refreshController;
    this.#refreshController = null;
    if (controller) controller.abort();
  }

  #render() {
    const failed = this.#phase === 'error';
    const disabled = this.suspended ? ' disabled' : '';
    this.toggleAttribute('empty', !failed && !this.#files.length);
    if (failed) {
      this.#root.innerHTML = `<div class="error" role="alert"><span>Could not read current changes on this turn's paths: ${html(this.#error)}</span>`
        + `<button class="act" data-act="retry" type="button"${disabled}>Try again</button></div>`;
      this.#root.querySelector('[data-act="retry"]').onclick = () => {
        if (!this.suspended) void this.refresh();
      };
      return;
    }
    if (!this.#files.length) { this.#root.innerHTML = ''; return; }

    // Counts are optional — a binary has none, and reporting it as zero would
    // read as "nothing changed" about a file that did.
    const known = this.#files.filter((f) => f.added != null || f.removed != null);
    const adds = known.reduce((n, f) => n + (f.added || 0), 0);
    const dels = known.reduce((n, f) => n + (f.removed || 0), 0);
    const n = this.#files.length;

    const shown = this.#expanded ? this.#files : this.#files.slice(0, this.preview);
    const rest = n - shown.length;

    this.#root.innerHTML = `
      <div class="card">
        <div class="head">
          <span class="ico">⧉</span>
          <span class="title">
            <span class="what">Current changes on ${n} path${n === 1 ? '' : 's'} touched by the last turn</span>
            <span class="stat">${known.length
              ? `<span class="add">+${adds}</span> <span class="del">−${dels}</span>`
              : '<span class="mod">size unknown</span>'}</span>
          </span>
          <button class="act" data-act="review" type="button"${disabled}
            aria-label="Review current changes on paths attributed to the last turn">Review last turn</button>
        </div>
        <div class="files">${shown.map((f) => this.#fileRow(f)).join('')}</div>
        ${rest > 0 ? `<button class="more"${disabled}>Show ${rest} more file${rest === 1 ? '' : 's'}</button>` : ''}
        ${this.#expanded && n > this.preview ? `<button class="more"${disabled}>Show fewer</button>` : ''}
      </div>`;

    this.#root.querySelector('[data-act="review"]').onclick = () => {
      if (this.suspended) return;
      this.dispatchEvent(new CustomEvent('review', {
        detail: { scope: 'last-turn', paths: this.#files.map((file) => file.path) },
        bubbles: true,
        composed: true,
      }));
    };
    const more = this.#root.querySelector('.more');
    if (more) more.onclick = () => {
      if (this.suspended) return;
      this.#expanded = !this.#expanded;
      this.#render();
    };
    for (const b of this.#root.querySelectorAll('.f')) {
      b.onclick = () => {
        if (this.suspended) return;
        this.dispatchEvent(new CustomEvent('file-open', {
          detail: { path: b.dataset.path, scope: 'last-turn' }, bubbles: true, composed: true,
        }));
      };
    }
  }

  #fileRow(f) {
    const [mark, cls] = STATE_MARK[f.state] || ['M', 'mod'];
    const counts = (f.added != null || f.removed != null)
      ? `<span class="n"><span class="add">+${f.added ?? 0}</span> <span class="del">−${f.removed ?? 0}</span></span>`
      : '<span class="n mod">—</span>';
    const disabled = this.suspended ? ' disabled' : '';
    return `<button class="f" type="button"${disabled} data-path="${html(f.path)}" aria-label="Open current changes for ${html(f.path)}, attributed to the last turn">
      <span class="mark ${cls}">${mark}</span>
      <span class="path">${html(f.path)}</span>${counts}</button>`;
  }
}

customElements.define('ax-changes', AxChanges);
