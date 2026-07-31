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
 *
 * @fires file-open  detail: {path}
 * @fires review     detail: {} — open the changes for review
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
button.act:hover { border-color: var(--accent); color: var(--accent); }
button.act:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.files { border-top: 1px solid var(--border); }
.f {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: 4px var(--sp-3); cursor: pointer; font: var(--fs-xs) var(--font-mono);
  border: 0; background: none; width: 100%; text-align: left; color: var(--text);
  transition: background var(--dur-fast) var(--ease);
}
.f:hover { background: var(--bg-3); }
.f:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.f .mark { width: 12px; flex-shrink: 0; opacity: .8; }
.f .path { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; direction: rtl; text-align: left; }
.f .n { flex-shrink: 0; }
.more {
  width: 100%; background: none; border: 0; border-top: 1px solid var(--border);
  color: var(--muted); padding: var(--sp-2); cursor: pointer;
  font: var(--fs-xs) var(--font-sans);
}
.more:hover { color: var(--accent); }
`;

export class AxChanges extends HTMLElement {
  static get observedAttributes() { return ['session']; }

  #root; #files = []; #expanded = false;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS);
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(v) { v ? this.setAttribute('session', v) : this.removeAttribute('session'); }

  get preview() { return Number(this.getAttribute('preview')) || SHOWN; }

  connectedCallback() { if (this.session) this.refresh(); }
  attributeChangedCallback(n, p, x) { if (n === 'session' && p !== x) { this.#expanded = false; this.refresh(); } }

  /** Re-read the working tree. */
  async refresh() {
    if (!this.session) { this.#files = []; this.#render(); return; }
    try {
      const st = await fetch(
        `/api/sessions/${encodeURIComponent(this.session)}/git/status`).then((r) => r.json());
      this.#files = st?.files || [];
    } catch { this.#files = []; }
    this.#render();
  }

  #render() {
    this.toggleAttribute('empty', !this.#files.length);
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
            <span class="what">Changed ${n} file${n === 1 ? '' : 's'}</span>
            <span class="stat">${known.length
              ? `<span class="add">+${adds}</span> <span class="del">−${dels}</span>`
              : '<span class="mod">size unknown</span>'}</span>
          </span>
          <button class="act" data-act="review">Review</button>
        </div>
        <div class="files">${shown.map((f) => this.#fileRow(f)).join('')}</div>
        ${rest > 0 ? `<button class="more">Show ${rest} more file${rest === 1 ? '' : 's'}</button>` : ''}
        ${this.#expanded && n > this.preview ? '<button class="more">Show fewer</button>' : ''}
      </div>`;

    this.#root.querySelector('[data-act="review"]').onclick = () =>
      this.dispatchEvent(new CustomEvent('review', { bubbles: true, composed: true }));
    const more = this.#root.querySelector('.more');
    if (more) more.onclick = () => { this.#expanded = !this.#expanded; this.#render(); };
    for (const b of this.#root.querySelectorAll('.f')) {
      b.onclick = () => this.dispatchEvent(new CustomEvent('file-open', {
        detail: { path: b.dataset.path }, bubbles: true, composed: true,
      }));
    }
  }

  #fileRow(f) {
    const [mark, cls] = STATE_MARK[f.state] || ['M', 'mod'];
    const counts = (f.added != null || f.removed != null)
      ? `<span class="n"><span class="add">+${f.added ?? 0}</span> <span class="del">−${f.removed ?? 0}</span></span>`
      : '<span class="n mod">—</span>';
    return `<button class="f" data-path="${f.path}">
      <span class="mark ${cls}">${mark}</span>
      <span class="path">${f.path}</span>${counts}</button>`;
  }
}

customElements.define('ax-changes', AxChanges);
