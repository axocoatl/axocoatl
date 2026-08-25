import { adopt } from './sheets.js';

/**
 * `<ax-browser>` — the running app, viewed from inside the session.
 *
 * Owns the viewport: the frame, the address, history, and the Session proxy
 * that makes a sandbox port reachable. It does not own where URLs come from —
 * the shell discovers those from what the agents run — and it does not own the
 * element picker, which exists to put a reference into the conversation and so
 * belongs to the conversation.
 *
 * Local URLs are rewritten onto a per-Session/per-port Preview origin. That
 * preserves an application's normal module, storage, fetch, form, and
 * WebSocket semantics while keeping it cross-origin from the workbench. The
 * injected picker communicates with the workbench only through postMessage.
 *
 * @element ax-browser
 *
 * @attr {string} session  Session whose sandbox ports are reachable.
 * @attr {string} url      The logical URL being shown.
 *
 * @fires navigate  detail: {url} — the view moved, including via history
 * @fires terminal-request detail: {session} — open this Session's Terminal
 *
 * @csspart frame  The iframe itself
 */

const CSS = `
:host { display: flex; flex-direction: column; min-height: 0; min-width: 0; }
.bar {
  display: flex; align-items: center; gap: var(--sp-1);
  padding: var(--sp-2); border-bottom: 1px solid var(--border); flex-shrink: 0;
}
button, .open-full {
  background: none; border: 1px solid transparent; color: var(--muted);
  border-radius: var(--r-md); padding: 2px var(--sp-2); cursor: pointer;
  font: var(--fs-xs) var(--font-sans); text-decoration: none;
}
button:hover:not(:disabled), .open-full:hover:not([aria-disabled="true"]) {
  color: var(--text); border-color: var(--border);
}
button:disabled, .open-full[aria-disabled="true"] { opacity: .35; cursor: not-allowed; }
button:focus-visible, .open-full:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.open-full { white-space: nowrap; }
input {
  flex: 1; min-width: 0; background: var(--bg-3); color: var(--text);
  border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 3px var(--sp-2); font: var(--fs-xs) var(--font-mono);
}
input:focus-visible { outline: none; box-shadow: var(--focus-ring); }
input:disabled { opacity: .5; cursor: not-allowed; }
.body { flex: 1; min-height: 0; position: relative; display: flex; }
iframe { flex: 1; border: 0; background: #fff; min-height: 0; }
.empty {
  flex: 1; display: flex; align-items: center; justify-content: center;
  color: var(--muted-2); font: var(--fs-sm) var(--font-sans);
  text-align: center; padding: var(--sp-5);
}
.empty-card { display: grid; justify-items: center; gap: var(--sp-2); max-width: 34rem; }
.empty-card strong { color: var(--text); font-size: var(--fs-body); }
.empty-card span { color: var(--muted); line-height: 1.5; }
.empty-card button {
  margin-top: var(--sp-1); border-color: var(--accent); background: var(--accent);
  color: var(--bg); font-weight: var(--fw-medium); padding: 5px var(--sp-3);
}
.empty-card button:hover:not(:disabled) { color: var(--bg); filter: brightness(1.08); }
`;

export class AxBrowser extends HTMLElement {
  static get observedAttributes() { return ['session', 'url']; }

  #root; #body; #addr; #back; #fwd; #reload; #openFull;
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
        <button data-act="back" title="Back" aria-label="Go back in Preview">←</button>
        <button data-act="forward" title="Forward" aria-label="Go forward in Preview">→</button>
        <button data-act="reload" title="Reload" aria-label="Reload Preview">⟳</button>
        <input aria-label="Preview address" placeholder="http://localhost:3000" spellcheck="false" autocomplete="off" />
        <a class="open-full" data-act="open-full" target="_blank" rel="noopener noreferrer"
          aria-disabled="true"
          title="Open this isolated Preview in a full tab for app cookies and debugging">Open full preview</a>
      </div>
      <div class="body"></div>`;
    this.#body = this.#root.querySelector('.body');
    this.#addr = this.#root.querySelector('input');
    this.#back = this.#root.querySelector('[data-act="back"]');
    this.#fwd = this.#root.querySelector('[data-act="forward"]');
    this.#reload = this.#root.querySelector('[data-act="reload"]');
    this.#openFull = this.#root.querySelector('[data-act="open-full"]');

    this.#root.addEventListener('click', (e) => {
      const act = e.target.closest('[data-act]')?.dataset.act;
      if (act === 'back') this.back();
      else if (act === 'forward') this.forward();
      else if (act === 'reload') this.reload();
      else if (act === 'terminal' && this.session) {
        this.dispatchEvent(new CustomEvent('terminal-request', {
          detail: { session: this.session }, bubbles: true, composed: true,
        }));
      }
    });
    this.#addr.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); this.go(this.#addr.value); }
    });
    adopt(this.#root, CSS);
    this.#showEmpty();
    this.#syncNav();
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(v) { v ? this.setAttribute('session', v) : this.removeAttribute('session'); }

  get url() { return this.getAttribute('url') || ''; }
  set url(v) { v ? this.setAttribute('url', v) : this.removeAttribute('url'); }

  attributeChangedCallback(name, prev, next) {
    if (prev === next) return;
    if (name === 'session') {
      this.#history = [];
      this.#at = -1;
      this.#showEmpty();
      return;
    }
    // `url` is reflected out as well as accepted in, so a navigation we
    // performed must not come back through as a fresh one: `go()` truncates
    // anything ahead of the cursor, which would delete the forward history the
    // moment you stepped back through it.
    if (name === 'url' && !this.#reflecting) {
      if (next) this.go(next);
      else this.#showEmpty({ reflect: false });
    }
  }

  /** The live frame, for whoever is doing the inspecting. */
  get frame() { return this.#body.querySelector('iframe'); }

  /**
   * Rewrite a sandbox URL through the session's proxy.
   *
   * Every path on `<session>-p<port>.localhost` maps to the same
   * Session port, so root-relative module and API URLs keep working. Anything
   * not local goes through untouched and stays isolated by the frame sandbox.
   */
  proxied(logical) {
    // A Preview URL is meaningful only inside a Session. Returning a raw URL
    // here would let a detached component bypass the Session proxy.
    if (!this.session) return 'about:blank';
    try {
      const u = new URL(logical);
      const local = ['localhost', '127.0.0.1', '0.0.0.0'].includes(u.hostname);
      // Never put a workbench-origin document into a script-capable iframe.
      if (u.origin === location.origin) return 'about:blank';
      if (!local) return logical;
      const port = u.port || (u.protocol === 'https:' ? '443' : '80');
      const listenerPort = location.port ? `:${location.port}` : '';
      const host = `${this.session}-p${port}.localhost${listenerPort}`;
      return `${location.protocol}//${host}${u.pathname}${u.search}${u.hash}`;
    } catch { return logical; }
  }

  /** Navigate, recording history. */
  go(raw) {
    if (!this.session) return;
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

  back() {
    if (!this.session) return;
    if (this.#at > 0) { this.#at -= 1; this.#show(this.#history[this.#at]); }
  }
  forward() {
    if (!this.session) return;
    if (this.#at < this.#history.length - 1) { this.#at += 1; this.#show(this.#history[this.#at]); }
  }
  reload() {
    if (!this.session) return;
    if (this.#history[this.#at]) this.#show(this.#history[this.#at]);
  }

  /** The exact isolated virtual URL currently shown, never the logical localhost URL. */
  fullPreviewUrl() {
    const logical = this.#history[this.#at] || this.url;
    if (!logical || !this.session) return '';
    try {
      const url = new URL(this.proxied(logical), location.href);
      const prefix = `${this.session}-p`;
      const logicalPort = url.hostname.slice(prefix.length, -'.localhost'.length);
      if (url.protocol !== location.protocol
          || url.port !== location.port
          || !url.hostname.startsWith(prefix)
          || !/^\d+$/.test(logicalPort)
          || Number(logicalPort) < 1
          || Number(logicalPort) > 65535
          || !url.hostname.endsWith('.localhost')) return '';
      return url.href;
    } catch { return ''; }
  }

  #show(url) {
    if (!this.session) return;
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
    f.title = 'Session application Preview';
    // `allow-same-origin` is required for modules, storage, fetch, forms, and
    // HMR. It is safe here because Session Preview has a dedicated origin and
    // cannot access the workbench origin or its control routes.
    f.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-forms allow-popups allow-modals');
    f.referrerPolicy = 'origin';
    f.src = this.proxied(url);
    this.#body.append(f);
    this.#syncNav();
    this.dispatchEvent(new CustomEvent('navigate', {
      detail: { url }, bubbles: true, composed: true,
    }));
  }

  #syncNav() {
    const active = Boolean(this.session);
    this.#addr.disabled = !active;
    this.#back.disabled = !active || this.#at <= 0;
    this.#fwd.disabled = !active || this.#at >= this.#history.length - 1;
    const hasUrl = active && Boolean(this.#history[this.#at] || this.url);
    this.#reload.disabled = !hasUrl;
    const fullPreviewUrl = this.fullPreviewUrl();
    if (fullPreviewUrl) {
      this.#openFull.href = fullPreviewUrl;
      this.#openFull.setAttribute('aria-disabled', 'false');
    } else {
      this.#openFull.removeAttribute('href');
      this.#openFull.setAttribute('aria-disabled', 'true');
    }
  }

  #showEmpty({ reflect = true } = {}) {
    this.#body.querySelectorAll('iframe, .empty').forEach((node) => node.remove());
    this.#addr.value = '';
    if (reflect && this.hasAttribute('url')) {
      this.#reflecting = true;
      this.removeAttribute('url');
      this.#reflecting = false;
    }
    const empty = document.createElement('div');
    empty.className = 'empty';
    empty.innerHTML = this.session
      ? '<div class="empty-card"><strong>No application is running yet</strong>'
        + '<span>Start the app in this Session’s Terminal. Its local address will appear here when it is ready.</span>'
        + '<button type="button" data-act="terminal">Open Terminal</button></div>'
      : '<div class="empty-card"><strong>No Session open</strong>'
        + '<span>Open a workspace Session to run and preview its application.</span></div>';
    this.#body.append(empty);
    this.#syncNav();
  }
}

customElements.define('ax-browser', AxBrowser);
