/**
 * `<ax-node>` — a draggable, selectable node in lattice space.
 *
 * Lattice position lives on `data-x` / `data-y` attributes (numbers, lattice
 * units). The element renders absolutely positioned via a CSS `translate(...)`
 * transform synced from those attributes. Children placed inside `<ax-node>`
 * render in a slot, so the node is purely a frame for any user content.
 *
 * @element ax-node
 *
 * @attr {number}  data-x          Lattice x position
 * @attr {number}  data-y          Lattice y position
 * @attr {number}  data-w          Optional width hint (default 160) — used by fit-view
 * @attr {number}  data-h          Optional height hint (default 60)
 * @attr {boolean} selected        Selection state
 * @attr {boolean} draggable       (default true) — set to "false" to lock
 * @attr {string}  status          Execution state: idle|pending|running|success|error
 *
 * @csspart frame                  The outer rectangle
 *
 * @cssprop --ax-node-bg           Background color
 * @cssprop --ax-node-fg           Foreground / text color
 * @cssprop --ax-node-border       Border color (idle)
 * @cssprop --ax-node-border-sel   Border color when selected
 * @cssprop --ax-node-shadow       Box shadow (idle)
 * @cssprop --ax-node-shadow-sel   Box shadow when selected
 * @cssprop --ax-node-radius       Border radius
 * @cssprop --ax-node-padding      Padding
 * @cssprop --ax-node-pending      Border color when status=pending
 * @cssprop --ax-node-running      Border color when status=running
 * @cssprop --ax-node-running-glow Pulse glow color when status=running
 * @cssprop --ax-node-success      Border color when status=success
 * @cssprop --ax-node-error        Border color when status=error
 *
 * @event node-pointerdown   detail: {ev, additive} — cancellable; preventDefault to skip selection/drag
 * @event node-select        detail: {additive: boolean, alreadySelected: boolean}
 * @event node-click         detail: {additive: boolean}  — pointer-up with no drag
 * @event node-movestart     detail: {x, y}
 * @event node-moving        detail: {x, y, dx, dy}    (rAF-coalesced)
 * @event node-moveend       detail: {x, y}
 */

const TEMPLATE = `
<style>
  :host {
    /* Positioned in lattice space by the viewport's transform; we just
       translate ourselves relative to that. */
    position: absolute;
    left: 0; top: 0;
    display: inline-block;
    min-width: 80px;
    min-height: 30px;
    cursor: grab;
    user-select: none;
    -webkit-user-select: none;
    touch-action: none;
    will-change: transform;
    background: var(--ax-node-bg, #181d27);
    color: var(--ax-node-fg, #e8ecf3);
    border: 1.5px solid var(--ax-node-border, #2e3645);
    border-radius: var(--ax-node-radius, 9px);
    box-shadow: var(--ax-node-shadow, 0 4px 12px rgba(0,0,0,.35));
    padding: var(--ax-node-padding, 11px 14px);
    font: inherit;
    font-size: 13px;
    box-sizing: border-box;
    transition: border-color .12s, box-shadow .12s;
  }
  :host(:hover) {
    border-color: var(--ax-node-border-hover, #4a536a);
  }
  :host([selected]) {
    border-color: var(--ax-node-border-sel, var(--ax-accent, #7c5cff));
    box-shadow: var(--ax-node-shadow-sel, 0 0 0 1px var(--ax-accent, #7c5cff), 0 8px 24px rgba(124,92,255,.25));
  }
  :host(.dragging) {
    cursor: grabbing;
    z-index: 1;
  }
  :host([draggable="false"]) {
    cursor: not-allowed;
  }

  /* ── Execution state ──────────────────────────────────────────────
     Native "live" dimension: a node's status drives its appearance so
     consumers can visualise a run without hand-rolled styling. */
  :host([status="pending"]) {
    border-style: dashed;
    border-color: var(--ax-node-pending, #5a6478);
    opacity: .8;
  }
  :host([status="running"]) {
    border-color: var(--ax-node-running, var(--ax-accent, #7c5cff));
    animation: ax-node-pulse 1.3s ease-in-out infinite;
    z-index: 1;
  }
  :host([status="success"]) {
    border-color: var(--ax-node-success, #00d9b1);
  }
  :host([status="error"]) {
    border-color: var(--ax-node-error, #ff6b6b);
  }
  @keyframes ax-node-pulse {
    0%, 100% { box-shadow: 0 0 0 0 rgba(124,92,255,0), var(--ax-node-shadow, 0 4px 12px rgba(0,0,0,.35)); }
    50%      { box-shadow: 0 0 0 7px var(--ax-node-running-glow, rgba(124,92,255,.20)), var(--ax-node-shadow, 0 4px 12px rgba(0,0,0,.35)); }
  }
  /* Selected always wins the border color. */
  :host([selected][status]) { border-style: solid; }
</style>
<slot></slot>
`;

const ATTR = {
  X: 'data-x',
  Y: 'data-y',
  W: 'data-w',
  H: 'data-h',
  SELECTED: 'selected',
  DRAGGABLE: 'draggable',
  STATUS: 'status',
};

/** Recognised execution states. */
const STATUSES = new Set(['idle', 'pending', 'running', 'success', 'error']);

export class AxNodeElement extends HTMLElement {
  static get observedAttributes() {
    return [ATTR.X, ATTR.Y, ATTR.SELECTED, ATTR.STATUS];
  }

  /** @type {ShadowRoot} */
  #root;

  /** Cached parent lattice (resolved on connect). */
  #lattice = null;

  /** rAF id for node-moving event coalescing. */
  #rafEmit = 0;
  #lastEmittedPos = null;

  /** Drag state — set in pointerdown, cleared in pointerup. */
  #drag = null;

  /** Press tracking — independent of drag, used to detect plain clicks. */
  #press = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = TEMPLATE;
  }

  // ── Lifecycle ──────────────────────────────────────────────────────────

  connectedCallback() {
    if (!this.id) this.id = `ax-node-${++AxNodeElement._uid}`;
    this.#applyTransform();
    this.#bindEvents();
    // Accessibility: each node is a selectable item. It is not in the
    // document tab order — the lattice owns keyboard focus and its Tab
    // handler cycles the selection.
    if (!this.hasAttribute('role')) this.setAttribute('role', 'button');
    if (!this.hasAttribute('tabindex')) this.setAttribute('tabindex', '-1');
    this.setAttribute('aria-selected', this.selected ? 'true' : 'false');
    if (!this.hasAttribute('aria-label')) {
      const text = (this.textContent || '').trim().replace(/\s+/g, ' ');
      this.setAttribute('aria-label', text ? `Node: ${text}` : `Node ${this.id}`);
    }
    this.#lattice = this.closest('ax-lattice');
    if (this.#lattice && typeof this.#lattice._registerNode === 'function') {
      this.#lattice._registerNode(this);
    }
  }

  disconnectedCallback() {
    this.#unbindEvents();
    if (this.#lattice && typeof this.#lattice._unregisterNode === 'function') {
      this.#lattice._unregisterNode(this);
    }
    this.#lattice = null;
  }

  attributeChangedCallback(name, _old, _val) {
    if (name === ATTR.X || name === ATTR.Y) this.#applyTransform();
    if (name === ATTR.SELECTED) {
      this.setAttribute('aria-selected', this.selected ? 'true' : 'false');
    }
    if (name === ATTR.STATUS) {
      // Reflect a running node to assistive tech.
      if (this.status === 'running') this.setAttribute('aria-busy', 'true');
      else this.removeAttribute('aria-busy');
    }
  }

  // ── Public API ─────────────────────────────────────────────────────────

  get x() { return this.#num(ATTR.X, 0); }
  set x(v) { this.setAttribute(ATTR.X, String(v)); }

  get y() { return this.#num(ATTR.Y, 0); }
  set y(v) { this.setAttribute(ATTR.Y, String(v)); }

  /** Lattice-space {x, y}. */
  get position() { return { x: this.x, y: this.y }; }

  /** Returns lattice-space {x, y, width, height} for fit-view & hit-testing. */
  getBox() {
    const rect = this.getBoundingClientRect();
    // The width/height of a node in lattice units is screenWidth / k. We can
    // approximate by reading from data-w/data-h if present, else fall back to
    // dividing the live screen size by the lattice's current zoom.
    let w = this.#num(ATTR.W, NaN);
    let h = this.#num(ATTR.H, NaN);
    const k = this.#lattice?.getViewport?.().k ?? 1;
    if (!Number.isFinite(w)) w = rect.width / k;
    if (!Number.isFinite(h)) h = rect.height / k;
    return { x: this.x, y: this.y, width: w, height: h };
  }

  /** True iff this node is in the selection set. */
  get selected() { return this.hasAttribute(ATTR.SELECTED); }
  set selected(v) {
    if (v) this.setAttribute(ATTR.SELECTED, '');
    else this.removeAttribute(ATTR.SELECTED);
  }

  /**
   * Execution state — the node's "live" dimension.
   * One of: `idle` | `pending` | `running` | `success` | `error`.
   * Setting `idle` (or an unknown value) clears the attribute.
   */
  get status() { return this.getAttribute(ATTR.STATUS) || 'idle'; }
  set status(v) {
    if (v && v !== 'idle' && STATUSES.has(v)) this.setAttribute(ATTR.STATUS, v);
    else this.removeAttribute(ATTR.STATUS);
  }

  /**
   * Move the node to a lattice-space position (with optional snap).
   * Emits no events. Use during programmatic placement.
   *
   * @param {{x: number, y: number, snap?: boolean}} pt
   */
  moveTo({ x, y, snap = false }) {
    let nx = x, ny = y;
    if (snap && this.#lattice && typeof this.#lattice.snap === 'function') {
      nx = this.#lattice.snap(nx);
      ny = this.#lattice.snap(ny);
    }
    this.setAttribute(ATTR.X, String(nx));
    this.setAttribute(ATTR.Y, String(ny));
  }

  // ── Internal ───────────────────────────────────────────────────────────

  #num(attr, dflt) {
    const v = this.getAttribute(attr);
    if (v == null || v === '') return dflt;
    const n = parseFloat(v);
    return Number.isFinite(n) ? n : dflt;
  }

  #applyTransform() {
    const { x, y } = this.position;
    this.style.transform = `translate(${x}px, ${y}px)`;
  }

  // ── Input ─────────────────────────────────────────────────────────────

  #bindEvents() {
    this.addEventListener('pointerdown', this.#onPointerDown);
    this.addEventListener('pointermove', this.#onPointerMove);
    this.addEventListener('pointerup', this.#onPointerEnd);
    this.addEventListener('pointercancel', this.#onPointerEnd);
  }
  #unbindEvents() {
    this.removeEventListener('pointerdown', this.#onPointerDown);
    this.removeEventListener('pointermove', this.#onPointerMove);
    this.removeEventListener('pointerup', this.#onPointerEnd);
    this.removeEventListener('pointercancel', this.#onPointerEnd);
  }

  #isDraggableNow() {
    return this.getAttribute(ATTR.DRAGGABLE) !== 'false';
  }

  #onPointerDown = (ev) => {
    // Right-click / middle-click pass through
    if (ev.button !== 0) return;
    // Don't capture clicks on interactive child elements (links, buttons, inputs)
    const t = /** @type {HTMLElement} */ (ev.target);
    if (t && t !== this && t.matches('input,textarea,select,button,a[href]')) return;

    const additive = ev.shiftKey || ev.metaKey || ev.ctrlKey;
    const alreadySelected = this.selected;

    // Let consumers cancel this entirely
    const pd = new CustomEvent('node-pointerdown', {
      detail: { ev, additive }, bubbles: true, composed: true, cancelable: true,
    });
    if (!this.dispatchEvent(pd)) return;

    // Fire selection event — lattice listens via bubbling and updates the set.
    // The lattice DEFERS collapsing a multi-selection to single until pointer-up
    // (see node-click), so a plain drag of a selected node group-drags.
    this.dispatchEvent(new CustomEvent('node-select', {
      detail: { additive, alreadySelected },
      bubbles: true, composed: true,
    }));

    ev.stopPropagation();
    // Always capture the pointer so we reliably get move/up — even for
    // non-draggable nodes (so plain clicks always produce node-click).
    this.setPointerCapture(ev.pointerId);
    this.#press = {
      pointerId: ev.pointerId,
      screenX: ev.clientX,
      screenY: ev.clientY,
      additive,
      moved: false,
    };

    if (!this.#isDraggableNow()) return;
    this.classList.add('dragging');
    const k = this.#lattice?.getViewport?.().k ?? 1;
    this.#drag = {
      pointerId: ev.pointerId,
      startScreenX: ev.clientX,
      startScreenY: ev.clientY,
      startX: this.x,
      startY: this.y,
      k,
      moved: false,
    };
    this.dispatchEvent(new CustomEvent('node-movestart', {
      detail: { x: this.x, y: this.y },
      bubbles: true, composed: true,
    }));
  };

  #onPointerMove = (ev) => {
    // Track press displacement to distinguish a click from a drag.
    if (this.#press && ev.pointerId === this.#press.pointerId) {
      if (
        Math.abs(ev.clientX - this.#press.screenX) > 3 ||
        Math.abs(ev.clientY - this.#press.screenY) > 3
      ) {
        this.#press.moved = true;
      }
    }
    if (!this.#drag || ev.pointerId !== this.#drag.pointerId) return;
    const dxScreen = ev.clientX - this.#drag.startScreenX;
    const dyScreen = ev.clientY - this.#drag.startScreenY;
    const k = this.#drag.k || 1;
    let nx = this.#drag.startX + dxScreen / k;
    let ny = this.#drag.startY + dyScreen / k;
    if (this.#lattice && typeof this.#lattice.snap === 'function') {
      nx = this.#lattice.snap(nx);
      ny = this.#lattice.snap(ny);
    }
    this.setAttribute(ATTR.X, String(nx));
    this.setAttribute(ATTR.Y, String(ny));
    this.#drag.moved = true;
    this.#scheduleEmit(nx, ny, nx - this.#drag.startX, ny - this.#drag.startY);
  };

  #onPointerEnd = (ev) => {
    const press = this.#press;
    const wasClick =
      press && press.pointerId === ev.pointerId && !press.moved;
    const additive = press ? press.additive : false;
    if (press && press.pointerId === ev.pointerId) this.#press = null;

    if (this.hasPointerCapture(ev.pointerId)) {
      this.releasePointerCapture(ev.pointerId);
    }

    let moved = false;
    if (this.#drag && ev.pointerId === this.#drag.pointerId) {
      this.classList.remove('dragging');
      moved = this.#drag.moved;
      this.#drag = null;
      if (this.#rafEmit) {
        cancelAnimationFrame(this.#rafEmit);
        this.#rafEmit = 0;
      }
    }

    if (moved) {
      this.dispatchEvent(new CustomEvent('node-moveend', {
        detail: { x: this.x, y: this.y },
        bubbles: true, composed: true,
      }));
    } else if (wasClick) {
      // Plain click (no drag) — lets the lattice collapse a deferred
      // multi-selection down to just this node.
      this.dispatchEvent(new CustomEvent('node-click', {
        detail: { additive },
        bubbles: true, composed: true,
      }));
    }
  };

  #scheduleEmit(x, y, dx, dy) {
    if (this.#rafEmit) return;
    this.#lastEmittedPos = { x, y, dx, dy };
    this.#rafEmit = requestAnimationFrame(() => {
      this.#rafEmit = 0;
      const p = this.#lastEmittedPos;
      if (!p) return;
      this.dispatchEvent(new CustomEvent('node-moving', {
        detail: p,
        bubbles: true, composed: true,
      }));
    });
  }
}
AxNodeElement._uid = 0;

if (!customElements.get('ax-node')) {
  customElements.define('ax-node', AxNodeElement);
}
