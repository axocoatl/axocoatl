import { adopt } from './sheets.js';

const APPROVAL_ID = (frameOrId) => {
  const value = typeof frameOrId === 'object' && frameOrId !== null
    ? (frameOrId.approval_id ?? frameOrId.approvalId)
    : frameOrId;
  return value === undefined || value === null || value === '' ? '' : String(value);
};

/**
 * Small, DOM-free FIFO used by `<ax-mcp-approval>`.
 *
 * Resolved ids are retained in a bounded tombstone set so a delayed duplicate
 * `required` frame cannot reopen a completed decision. A complete authoritative
 * snapshot may deliberately revive a tombstoned id: that means a locally sent
 * decision never reached the runtime gate and the person must be able to retry.
 */
export class ApprovalQueue {
  #waiting = [];
  #current = null;
  #activeIds = new Set();
  #settledIds = new Set();
  #settledOrder = [];
  #tombstoneLimit;

  constructor({ tombstoneLimit = 512 } = {}) {
    this.#tombstoneLimit = Math.max(1, Number(tombstoneLimit) || 512);
  }

  get current() { return this.#current; }
  get waiting() { return [...this.#waiting]; }
  get active() { return this.#current ? [this.#current, ...this.#waiting] : []; }
  get length() { return this.#waiting.length + (this.#current ? 1 : 0); }

  has(frameOrId) {
    const id = APPROVAL_ID(frameOrId);
    return Boolean(id && (this.#activeIds.has(id) || this.#settledIds.has(id)));
  }

  /** Add one unique frame and promote it immediately when the queue is idle. */
  enqueue(frame) {
    const id = APPROVAL_ID(frame);
    if (!id || this.#activeIds.has(id) || this.#settledIds.has(id)) return false;
    const item = { ...frame, approval_id: id };
    this.#activeIds.add(id);
    if (this.#current) this.#waiting.push(item);
    else this.#current = item;
    return true;
  }

  /**
   * Remove an approval wherever it is and remember that it was resolved.
   * Unknown ids are tombstoned too, which handles resolved-before-snapshot
   * ordering without special cases in the component.
   */
  resolve(frameOrId) {
    const id = APPROVAL_ID(frameOrId);
    if (!id) return { removed: false, wasCurrent: false, current: this.#current };

    let removed = false;
    let wasCurrent = false;
    if (APPROVAL_ID(this.#current) === id) {
      this.#current = this.#waiting.shift() || null;
      removed = true;
      wasCurrent = true;
    } else {
      const index = this.#waiting.findIndex((item) => APPROVAL_ID(item) === id);
      if (index >= 0) {
        this.#waiting.splice(index, 1);
        removed = true;
      }
    }
    this.#activeIds.delete(id);
    this.#rememberSettled(id);
    return { removed, wasCurrent, current: this.#current };
  }

  /**
   * Reconcile against a complete server snapshot. Missing ids are settled and
   * newly authoritative frames are queued. Existing items keep their position
   * so a reconnect cannot replace the decision currently under the cursor.
   */
  reconcile(frames) {
    const authoritative = [];
    const ids = new Set();
    for (const frame of Array.isArray(frames) ? frames : []) {
      const id = APPROVAL_ID(frame);
      if (!id || ids.has(id)) continue;
      ids.add(id);
      authoritative.push(frame);
    }

    let changed = false;
    for (const active of this.active) {
      if (!ids.has(APPROVAL_ID(active))) {
        changed = this.resolve(active).removed || changed;
      }
    }
    for (const frame of authoritative) {
      const id = APPROVAL_ID(frame);
      if (!this.#activeIds.has(id)) this.#forgetSettled(id);
      changed = this.enqueue(frame) || changed;
    }
    return { changed, current: this.#current, active: this.active };
  }

  #rememberSettled(id) {
    if (this.#settledIds.has(id)) return;
    this.#settledIds.add(id);
    this.#settledOrder.push(id);
    while (this.#settledOrder.length > this.#tombstoneLimit) {
      this.#settledIds.delete(this.#settledOrder.shift());
    }
  }

  #forgetSettled(id) {
    if (!this.#settledIds.delete(id)) return;
    this.#settledOrder = this.#settledOrder.filter((settledId) => settledId !== id);
  }
}

const CSS = `
:host { display: none; }
:host([open]) {
  position: fixed; inset: 0; z-index: 6000;
  display: flex; align-items: center; justify-content: center;
  box-sizing: border-box; padding: var(--sp-3, 12px);
  background: rgba(0, 0, 0, .55);
  color: var(--text, #eee); font-family: var(--font-sans, system-ui, sans-serif);
  animation: fade var(--dur-fast, .14s) var(--ease, ease-out);
}
@keyframes fade { from { opacity: 0; } }
.modal {
  width: min(640px, 92vw); max-height: min(88vh, 760px);
  display: flex; flex-direction: column; overflow: hidden;
  background: var(--panel, #181818); border: 1px solid var(--border-strong, #444);
  border-radius: var(--r-xl, 12px); box-shadow: var(--shadow-lg, 0 24px 60px rgba(0,0,0,.4));
  animation: rise var(--dur-fast, .18s) var(--ease, ease-out);
}
@keyframes rise { from { transform: translateY(8px); opacity: 0; } }
.head {
  padding: 14px 18px; border-bottom: 1px solid var(--border, #333);
  font-size: var(--fs-body, 14px); font-weight: var(--fw-semibold, 600);
}
.body {
  display: flex; flex-direction: column; gap: 12px; min-height: 0;
  padding: 16px 18px; overflow: auto;
}
.subject { color: var(--text, #eee); font-size: var(--fs-body, 14px); line-height: 1.6; }
.agent, .tool, .server { font-family: var(--font-mono, ui-monospace, monospace); font-weight: 600; }
.agent { color: var(--axo-jade-glow, #56d7a1); }
.tool { color: var(--axo-bronze-glow, #d6a66b); }
.server { color: var(--axo-blue-glow, #69aeea); }
.label {
  color: var(--muted-2, #999); font-size: var(--fs-xs, 11px);
  text-transform: uppercase; letter-spacing: .06em;
}
.arguments {
  box-sizing: border-box; max-height: 220px; overflow: auto; margin: 0;
  padding: 10px 12px; border: 1px solid var(--border, #333);
  border-radius: var(--r-md, 7px); background: var(--bg-3, #111);
  color: var(--muted, #bbb); font: var(--fs-sm, 12px)/1.5 var(--font-mono, ui-monospace, monospace);
  white-space: pre-wrap; word-break: break-word;
}
.help, .status { color: var(--muted-2, #999); font-size: var(--fs-xs, 11px); line-height: 1.5; }
.status.error { color: var(--err, #ef6b73); }
[hidden] { display: none !important; }
.foot {
  display: flex; flex-wrap: wrap; align-items: center; gap: 8px;
  padding: 11px 16px; border-top: 1px solid var(--border, #333);
}
.spacer { flex: 1; }
button {
  box-sizing: border-box; padding: 9px 16px; white-space: nowrap;
  border: 1px solid var(--axo-jade, #238060); border-radius: var(--r-lg, 9px);
  background: linear-gradient(180deg, var(--axo-jade-glow, #56d7a1), var(--axo-jade, #238060));
  color: #fff; cursor: pointer; box-shadow: 0 1px 0 rgba(255,255,255,.16) inset, var(--shadow-sm, none);
  font: 600 var(--fs-body, 14px) var(--font-sans, system-ui, sans-serif);
  transition: filter var(--dur-fast, .15s) var(--ease, ease),
              box-shadow var(--dur-fast, .15s) var(--ease, ease),
              transform .06s ease;
}
button:hover:not(:disabled) { filter: brightness(1.06); }
button:active:not(:disabled) { transform: translateY(1px); }
button:focus-visible { outline: none; box-shadow: var(--focus-ring, 0 0 0 2px #56d7a1); }
button:disabled { cursor: not-allowed; opacity: .5; filter: none; }
button.ghost { background: transparent; border-color: var(--border-strong, #444); color: var(--text, #eee); box-shadow: none; }
button.ghost:hover:not(:disabled) { background: var(--bg-3, #111); filter: none; }
button.danger { color: var(--err, #ef6b73); border-color: transparent; }
button.danger:hover:not(:disabled) { border-color: var(--err, #ef6b73); }
@media (max-width: 620px) {
  :host([open]) { align-items: flex-end; padding: 0; }
  .modal { width: 100%; max-width: none; max-height: 94vh; border-radius: var(--r-xl, 12px) var(--r-xl, 12px) 0 0; }
  .foot { align-items: stretch; }
  .spacer { display: none; }
  button { flex: 1 1 calc(50% - 8px); }
}
`;

/**
 * A serialized MCP permission prompt.
 *
 * The shell owns transport. On `decision`, it sends the command and calls
 * `detail.complete(true)` only after the send was accepted. A false result
 * leaves the same approval visible and actionable; no later queue item can
 * leapfrog it.
 *
 * @element ax-mcp-approval
 * @fires decision detail: {approvalId, decision, persist, complete(success)}
 */
export class AxMcpApproval extends HTMLElement {
  #root;
  #queue = new ApprovalQueue();
  #pending = false;
  #decisionToken = null;
  #status = '';
  #statusError = false;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <section class="modal" role="dialog" aria-modal="true" aria-labelledby="approval-title">
        <header class="head" id="approval-title">Allow MCP tool call?</header>
        <div class="body">
          <div class="subject">
            <span class="agent"></span><span> wants to call </span><span class="tool"></span><span> on </span><span class="server"></span>
          </div>
          <div class="arguments-block">
            <div class="label">Arguments</div>
            <pre class="arguments"></pre>
          </div>
          <div class="help">"Allow once" trusts this single call. The "always" options persist your decision so you won't be prompted again — Deny "always" works the same way for blocklisting.</div>
          <div class="status" aria-live="polite"></div>
        </div>
        <footer class="foot">
          <button class="ghost danger" data-decision="deny" data-persist="once">Deny</button>
          <button class="ghost danger" data-decision="deny" data-persist="agent_server">Deny always</button>
          <span class="spacer" aria-hidden="true"></span>
          <button class="ghost allow-once" data-decision="allow" data-persist="once">Allow once</button>
          <button class="ghost" data-decision="allow" data-persist="agent_server">Allow this agent</button>
          <button data-decision="allow" data-persist="any_agent_server">Allow always</button>
        </footer>
      </section>`;

    this.#root.querySelector('.foot').addEventListener('click', (event) => {
      const button = event.target.closest('button[data-decision]');
      if (!button) return;
      this.#requestDecision(button.dataset.decision, button.dataset.persist);
    });
    adopt(this.#root, CSS);
  }

  connectedCallback() {
    window.addEventListener('keydown', this.#onKeyDown, true);
    this.#render();
  }

  disconnectedCallback() {
    window.removeEventListener('keydown', this.#onKeyDown, true);
  }

  get current() { return this.#queue.current; }
  get pending() { return this.#pending; }
  get size() { return this.#queue.length; }

  /** Queue a required-approval WebSocket frame. Returns false for a replay. */
  enqueue(frame) {
    const previousId = APPROVAL_ID(this.#queue.current);
    const added = this.#queue.enqueue(frame);
    if (added && APPROVAL_ID(this.#queue.current) !== previousId) this.#render();
    return added;
  }

  /** Drop a decision made in another tab or restored from authoritative state. */
  resolveExternally(frameOrId) {
    const id = APPROVAL_ID(frameOrId);
    const result = this.#queue.resolve(id);
    if (result.wasCurrent) {
      this.#decisionToken = null;
      this.#pending = false;
      this.#status = '';
      this.#statusError = false;
      this.#render();
    }
    return result.removed;
  }

  /** Replace stale local queue entries with the authoritative reconnect list. */
  reconcile(frames) {
    const previousId = APPROVAL_ID(this.#queue.current);
    const result = this.#queue.reconcile(frames);
    if (APPROVAL_ID(this.#queue.current) !== previousId) {
      this.#decisionToken = null;
      this.#pending = false;
      this.#status = '';
      this.#statusError = false;
      this.#render();
    }
    return result;
  }

  #onKeyDown = (event) => {
    if (!this.#queue.current || this.#pending || event.key !== 'Escape' || event.repeat) return;
    event.preventDefault();
    event.stopPropagation();
    this.#requestDecision('deny', 'once');
  };

  #requestDecision(decision, persist) {
    const approval = this.#queue.current;
    if (!approval || this.#pending) return;
    const approvalId = APPROVAL_ID(approval);
    const token = Symbol(approvalId);
    this.#decisionToken = token;
    this.#pending = true;
    this.#status = 'Sending decision…';
    this.#statusError = false;
    this.#syncPending();

    let completed = false;
    const complete = (success) => {
      if (completed || this.#decisionToken !== token) return false;
      completed = true;
      this.#decisionToken = null;
      this.#pending = false;
      if (success) {
        this.#queue.resolve(approvalId);
        this.#status = '';
        this.#statusError = false;
        this.#render();
      } else {
        this.#status = 'Decision was not sent. Check the live connection and try again.';
        this.#statusError = true;
        this.#syncPending();
        this.#focusAllowOnce();
      }
      return true;
    };

    this.dispatchEvent(new CustomEvent('decision', {
      bubbles: true,
      composed: true,
      detail: { approvalId, decision, persist, complete },
    }));
  }

  #render() {
    const approval = this.#queue.current;
    this.toggleAttribute('open', Boolean(approval));
    if (!approval) return;

    this.#root.querySelector('.agent').textContent = approval.agent_id || approval.agentId || 'An agent';
    this.#root.querySelector('.tool').textContent = approval.tool_display || approval.toolDisplay || approval.tool || 'a tool';
    this.#root.querySelector('.server').textContent = approval.server || 'an MCP server';

    const preview = approval.arguments_preview ?? approval.argumentsPreview;
    const argumentsBlock = this.#root.querySelector('.arguments-block');
    argumentsBlock.hidden = !preview;
    this.#root.querySelector('.arguments').textContent = preview || '';
    this.#syncPending();
    this.#focusAllowOnce();
  }

  #syncPending() {
    this.#root.querySelector('.modal').setAttribute('aria-busy', String(this.#pending));
    for (const button of this.#root.querySelectorAll('button')) button.disabled = this.#pending;
    const status = this.#root.querySelector('.status');
    status.textContent = this.#status;
    status.classList.toggle('error', this.#statusError);
    status.hidden = !this.#status;
  }

  #focusAllowOnce() {
    const approvalId = APPROVAL_ID(this.#queue.current);
    if (!approvalId || this.#pending || !this.isConnected) return;
    const focus = () => {
      if (APPROVAL_ID(this.#queue.current) !== approvalId || this.#pending) return;
      this.#root.querySelector('.allow-once')?.focus({ preventScroll: true });
    };
    queueMicrotask(focus);
    requestAnimationFrame(focus);
  }
}

if (!customElements.get('ax-mcp-approval')) {
  customElements.define('ax-mcp-approval', AxMcpApproval);
}
