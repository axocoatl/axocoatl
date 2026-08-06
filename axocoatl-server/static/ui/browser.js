import { adopt } from './sheets.js';

/**
 * `<ax-browser>` — the running app, viewed from inside the session.
 *
 * Owns the viewport: the frame, the address, history, and the same-origin proxy
 * that makes a sandbox port reachable. It does not own where URLs come from —
 * the shell discovers those from what the agents run — and it does not own the
 * element picker, which exists to put a reference into the conversation and so
 * belongs to the conversation.
 *
 * Local URLs are rewritten through the session's proxy rather than loaded
 * directly. That is not a convenience: same-origin is what lets anything inside
 * the frame be inspected at all, and a direct localhost load is opaque.
 *
 * @element ax-browser
 *
 * @attr {string} session  Session whose sandbox ports are reachable.
 * @attr {string} url      The logical URL being shown.
 *
 * @fires navigate  detail: {url} — the view moved, including via history
 *
 * @csspart frame  The iframe itself
 */

const CSS = `
:host { display: flex; flex-direction: column; min-height: 0; min-width: 0; }
.bar {
  display: flex; align-items: center; gap: var(--sp-1);
  padding: var(--sp-2); border-bottom: 1px solid var(--border); flex-shrink: 0;
}
button {
  background: none; border: 1px solid transparent; color: var(--muted);
  border-radius: var(--r-md); padding: 2px var(--sp-2); cursor: pointer;
  font: var(--fs-xs) var(--font-sans);
}
button:hover:not(:disabled) { color: var(--text); border-color: var(--border); }
button:disabled { opacity: .35; cursor: not-allowed; }
button:focus-visible { outline: none; box-shadow: var(--focus-ring); }
input {
  flex: 1; min-width: 0; background: var(--bg-3); color: var(--text);
  border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 3px var(--sp-2); font: var(--fs-xs) var(--font-mono);
}
input:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.body { flex: 1; min-height: 0; position: relative; display: flex; }
iframe { flex: 1; border: 0; background: #fff; min-height: 0; }
.empty {
  flex: 1; display: flex; align-items: center; justify-content: center;
  color: var(--muted-2); font: var(--fs-sm) var(--font-sans);
  text-align: center; padding: var(--sp-5);
}
`;

export class AxBrowser extends HTMLElement {
  static get observedAttributes() { return ['session', 'url']; }

  #root; #body; #addr; #back; #fwd;
  /** Visited URLs and where we are in them. Ours, not the frame's — a
   *  cross-origin frame will not share its history. */
  #history = [];
  #at = -1;
  /** True while we are writing our own `url` attribute. */
  #reflecting = false;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="bar">
        <button data-act="back" title="Back">←</button>
        <button data-act="forward" title="Forward">→</button>
        <button data-act="reload" title="Reload">⟳</button>
        <input placeholder="http://localhost:3000" spellcheck="false" autocomplete="off" />
        <button data-act="open" title="Open in a real browser">↗</button>
      </div>
      <div class="body"><div class="empty">Run a dev server in the terminal — its address appears here.</div></div>`;
    this.#body = this.#root.querySelector('.body');
    this.#addr = this.#root.querySelector('input');
    this.#back = this.#root.querySelector('[data-act="back"]');
    this.#fwd = this.#root.querySelector('[data-act="forward"]');

    this.#root.querySelector('.bar').addEventListener('click', (e) => {
      const act = e.target.closest('[data-act]')?.dataset.act;
      if (act === 'back') this.back();
      else if (act === 'forward') this.forward();
      else if (act === 'reload') this.reload();
      else if (act === 'open' && this.url) window.open(this.url, '_blank', 'noopener');
    });
    this.#addr.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); this.go(this.#addr.value); }
    });
    adopt(this.#root, CSS);
    this.#syncNav();
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(v) { v ? this.setAttribute('session', v) : this.removeAttribute('session'); }

  get url() { return this.getAttribute('url') || ''; }
  set url(v) { v ? this.setAttribute('url', v) : this.removeAttribute('url'); }

  attributeChangedCallback(name, prev, next) {
    if (prev === next) return;
    // `url` is reflected out as well as accepted in, so a navigation we
    // performed must not come back through as a fresh one: `go()` truncates
    // anything ahead of the cursor, which would delete the forward history the
    // moment you stepped back through it.
    if (name === 'url' && next && !this.#reflecting) this.go(next);
  }

  /** The live frame, for whoever is doing the inspecting. */
  get frame() { return this.#body.querySelector('iframe'); }

  /**
   * Rewrite a sandbox URL through the session's proxy.
   *
   * Same-origin is the whole point: it is what allows the page to be inspected
   * from outside. Anything not local goes through untouched — it will be opaque,
   * but it should still be viewable.
   */
  proxied(logical) {
    try {
      const u = new URL(logical);
      const local = ['localhost', '127.0.0.1', '0.0.0.0'].includes(u.hostname);
      if (!local || !this.session) return logical;
      const port = u.port || (u.protocol === 'https:' ? '443' : '80');
      const tail = u.pathname.replace(/^\//, '') + (u.search || '');
      return `/api/sessions/${encodeURIComponent(this.session)}/proxy/${port}${tail ? '/' + tail : ''}`;
    } catch { return logical; }
  }

  /** Navigate, recording history. */
  go(raw) {
    let url = String(raw || '').trim();
    if (!url) return;
    if (!/^https?:\/\//i.test(url)) url = 'http://' + url;
    // A new navigation truncates anything forward of here, as a history does.
    if (this.#at < this.#history.length - 1) this.#history = this.#history.slice(0, this.#at + 1);
    if (this.#history[this.#history.length - 1] !== url) {
      this.#history.push(url);
      this.#at = this.#history.length - 1;
    }
    this.#show(url);
  }

  back() { if (this.#at > 0) { this.#at -= 1; this.#show(this.#history[this.#at]); } }
  forward() {
    if (this.#at < this.#history.length - 1) { this.#at += 1; this.#show(this.#history[this.#at]); }
  }
  reload() { if (this.#history[this.#at]) this.#show(this.#history[this.#at]); }

  #show(url) {
    if (this.getAttribute('url') !== url) {
      this.#reflecting = true;
      this.setAttribute('url', url);
      this.#reflecting = false;
    }
    this.#addr.value = url;
    // Replace only the frame. An earlier version cleared the whole body, which
    // silently destroyed sibling UI living in it.
    this.#body.querySelectorAll('iframe, .empty').forEach((n) => n.remove());
    const f = document.createElement('iframe');
    f.setAttribute('part', 'frame');
    f.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-forms allow-popups allow-modals');
    f.src = this.proxied(url);
    this.#body.append(f);
    this.#syncNav();
    this.dispatchEvent(new CustomEvent('navigate', {
      detail: { url }, bubbles: true, composed: true,
    }));
  }

  #syncNav() {
    this.#back.disabled = this.#at <= 0;
    this.#fwd.disabled = this.#at >= this.#history.length - 1;
  }
}

customElements.define('ax-browser', AxBrowser);
