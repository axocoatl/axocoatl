import { adopt, CODICONS } from './sheets.js';

/**
 * `<ax-terminal>` — one live shell in the session's sandbox.
 *
 * Owns exactly one terminal: the xterm instance, the PTY socket, the theme, and
 * the resize protocol. It does not own *which* terminals exist or where they are
 * shown — the shell keeps that, so the same element serves the drawer, a docked
 * pane, or anywhere else without knowing about any of them.
 *
 * Everything about a terminal is stateful and expensive to recreate: scrollback,
 * the running process, the socket. So the element is created once per task and
 * moved, never rebuilt.
 *
 * @element ax-terminal
 *
 * @attr {string} session  Session whose sandbox this shell runs in.
 * @attr {string} task     Terminal id within that session.
 *
 * @fires terminal-closed  the socket dropped
 *
 * @cssprop --ax-terminal-pad  Padding around the screen (default --sp-2)
 */

/** The vendored xterm stylesheet. Its rules are selector-based, so unlike the
 *  brand tokens they do not reach into a shadow root on their own. */
const XTERM_CSS = '/vendor/xterm.css';

/**
 * Axocoatl's terminal palette.
 *
 * ANSI green and brightGreen are the brand jade glow, so shell prompts,
 * `ls --color` directories and test-pass markers all land in the brand rather
 * than in terminal-default colours that read as a different application.
 */
const THEME = {
  background: '#000000',
  foreground: '#C8CBD1',
  cursor: '#4FCB8E',
  cursorAccent: '#000000',
  selectionBackground: 'rgba(79,203,142,0.30)',
  black: '#0A0A0A',
  red: '#E88A8A',
  green: '#4FCB8E',
  yellow: '#E8C275',
  blue: '#7FB8D6',
  magenta: '#C7A6E0',
  cyan: '#3FA9C8',
  white: '#C8CBD1',
  brightBlack: '#636366',
  brightRed: '#FFB0B0',
  brightGreen: '#7DEBB0',
  brightYellow: '#FFD89A',
  brightBlue: '#A6D2E8',
  brightMagenta: '#E0C2F0',
  brightCyan: '#7FCEE0',
  brightWhite: '#ECECEC',
};

const CSS = `
:host { display: flex; flex-direction: column; min-height: 0; min-width: 0; }
.screen { flex: 1; min-height: 0; padding: var(--ax-terminal-pad, var(--sp-2)); }
.gone {
  color: var(--muted-2); font: var(--fs-sm) var(--font-sans);
  padding: var(--sp-4); text-align: center;
}
`;

// Retry for as long as this exact element still owns the same Session/task.
// The cap prevents an unavailable daemon from turning into a tight loop.
const TERMINAL_RECONNECT_DELAYS_MS = [100, 250, 500, 1000, 2000];

export class AxTerminal extends HTMLElement {
  static get observedAttributes() { return ['session', 'task']; }

  #root; #screen;
  #term = null;
  #fit = null;
  #ws = null;
  #dataSubscription = null;
  #retryTimer = null;
  #retryAttempt = 0;
  #lifecycle = 0;
  #destroyed = false;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#screen = document.createElement('div');
    this.#screen.className = 'screen';
    this.#root.append(this.#screen);
    // xterm's own stylesheet has to be adopted: its rules are selector-based
    // and stop at the shadow boundary, unlike the brand tokens which inherit.
    //
    // Kept as a promise because xterm *measures the DOM* to work out character
    // dimensions when it opens. Open it before its stylesheet has landed and the
    // measurement comes back as nothing: the socket connects, frames arrive,
    // writes are accepted, and not one glyph is drawn.
    this.#styled = adopt(this.#root, CSS, [CODICONS, XTERM_CSS]);
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(v) { v ? this.setAttribute('session', v) : this.removeAttribute('session'); }

  get task() { return this.getAttribute('task') || ''; }
  set task(v) { v ? this.setAttribute('task', v) : this.removeAttribute('task'); }

  connectedCallback() {
    if (!this.#destroyed) void this.#start();
    // Re-fitting on our own resize means the shell does not have to remember to
    // tell us — a terminal that is the wrong size after a pane drag is the most
    // common way this goes wrong.
    this.#ro = new ResizeObserver(() => this.fit());
    this.#ro.observe(this);
  }

  disconnectedCallback() {
    this.#ro?.disconnect();
    // Deliberately *not* tearing down here: panes move elements between
    // containers, and disposing on every move would kill a live shell and its
    // scrollback. `destroy()` is explicit.
  }

  attributeChangedCallback(name, prev, next) {
    if (prev === next) return;
    // A different task is a different shell; the old one cannot be reused.
    // Dispose it even while detached: panes may move a terminal unchanged, but
    // changing its identity while it is out of the DOM must not preserve a
    // WebSocket to the previous Session/task until the element is inserted again.
    const destroyed = this.#destroyed;
    this.#destroyed = true;
    this.#disposeCurrent();
    this.#destroyed = destroyed;
    if (!destroyed && this.isConnected) void this.#start();
  }

  async #start() {
    if (this.#destroyed || this.#term || !this.session || !this.task) return;
    const lifecycle = this.#lifecycle;
    await this.#styled;
    // Another start may have won while we waited, or the element may have been
    // torn down; either way this one is stale.
    if (this.#destroyed || lifecycle !== this.#lifecycle || this.#term
        || !this.isConnected || !this.session || !this.task) return;
    const Term = window.Terminal;
    if (typeof Term === 'undefined') {
      this.#screen.innerHTML = '<div class="gone">xterm.js failed to load.</div>';
      return;
    }
    const term = new Term({
      fontFamily: '"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace',
      fontSize: 12,
      lineHeight: 1.35,
      convertEol: true,
      cursorBlink: true,
      cursorStyle: 'block',
      theme: THEME,
    });
    const fit = window.FitAddon?.FitAddon ? new window.FitAddon.FitAddon() : null;
    if (fit) term.loadAddon(fit);
    term.open(this.#screen);
    this.#term = term;
    this.#fit = fit;
    // Fitting needs real dimensions, which the element does not have until it
    // has been laid out once.
    requestAnimationFrame(() => this.fit());

    this.#dataSubscription = term.onData((data) => {
      const socket = this.#ws;
      if (socket?.readyState === WebSocket.OPEN) socket.send(data);
    });
    this.#retryAttempt = 0;
    this.#connect(lifecycle);
  }

  #connect(lifecycle) {
    if (this.#destroyed || lifecycle !== this.#lifecycle || !this.#term
      || !this.session || !this.task) return;
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let ws;
    try {
      ws = new WebSocket(`${proto}//${location.host}/api/sessions/`
        + `${encodeURIComponent(this.session)}/terminals/${encodeURIComponent(this.task)}/ws`);
    } catch {
      this.#scheduleReconnect(lifecycle);
      return;
    }
    ws.binaryType = 'arraybuffer';
    this.#ws = ws;

    ws.onopen = () => {
      if (this.#ws !== ws || lifecycle !== this.#lifecycle || this.#destroyed) {
        try { ws.close(); } catch { /* stale constructor */ }
        return;
      }
      this.#retryAttempt = 0;
      this.fit();
    };
    ws.onmessage = (ev) => {
      if (this.#ws !== ws || lifecycle !== this.#lifecycle || this.#destroyed) return;
      const term = this.#term;
      if (!term) return;
      if (ev.data instanceof ArrayBuffer) { term.write(new Uint8Array(ev.data)); return; }
      if (typeof ev.data !== 'string') return;
      // Could be an error envelope; try JSON first, else write it as output.
      try {
        const j = JSON.parse(ev.data);
        if (j && j.kind === 'error') term.write(`\r\n\x1b[31m[${j.message}]\x1b[0m\r\n`);
        else term.write(ev.data);
      } catch { term.write(ev.data); }
    };
    ws.onclose = (event) => {
      if (this.#ws !== ws || lifecycle !== this.#lifecycle || this.#destroyed) return;
      this.#ws = null;
      this.dispatchEvent(new CustomEvent('terminal-closed', {
        bubbles: true,
        composed: true,
        detail: {
          code: event.code,
          reason: event.reason,
          reconnecting: true,
        },
      }));
      this.#scheduleReconnect(lifecycle);
    };
  }

  #scheduleReconnect(lifecycle) {
    if (this.#destroyed || lifecycle !== this.#lifecycle || this.#retryTimer) return;
    // A new attach always begins from the server's retained PTY snapshot. The
    // prior xterm buffer may contain bytes beyond that snapshot cut, so keeping
    // any of it would create an arbitrary duplicate or gap after reconnect.
    try { this.#term?.reset(); } catch { /* already disposed */ }
    try { this.#term?.clear(); } catch { /* already disposed */ }
    const index = Math.min(this.#retryAttempt, TERMINAL_RECONNECT_DELAYS_MS.length - 1);
    const delay = TERMINAL_RECONNECT_DELAYS_MS[index];
    this.#retryAttempt += 1;
    this.#retryTimer = setTimeout(() => {
      this.#retryTimer = null;
      this.#connect(lifecycle);
    }, delay);
  }

  /** Re-measure and tell the far end, so the process wraps at the right width. */
  fit() {
    try { this.#fit?.fit(); } catch { /* not laid out yet */ }
    try {
      if (this.#ws?.readyState === WebSocket.OPEN && this.#term) {
        this.#ws.send(JSON.stringify({
          kind: 'resize', rows: this.#term.rows, cols: this.#term.cols,
        }));
      }
    } catch { /* socket closed under us */ }
  }

  focus() { try { this.#term?.focus(); } catch { /* not ready */ } }

  /** Close the socket and dispose the terminal. Not reversible. */
  destroy() {
    this.#destroyed = true;
    this.#disposeCurrent();
  }

  #disposeCurrent() {
    this.#lifecycle += 1;
    if (this.#retryTimer) clearTimeout(this.#retryTimer);
    this.#retryTimer = null;
    this.#retryAttempt = 0;
    const ws = this.#ws;
    this.#ws = null;
    if (ws) {
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      try { ws.close(); } catch { /* already closed */ }
    }
    try { this.#dataSubscription?.dispose?.(); } catch { /* already disposed */ }
    this.#dataSubscription = null;
    try { this.#term?.dispose(); } catch { /* already disposed */ }
    this.#term = null;
    this.#fit = null;
    this.#screen.replaceChildren();
  }

  #ro = null;
  #styled = null;
}

customElements.define('ax-terminal', AxTerminal);
