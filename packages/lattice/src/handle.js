/**
 * `<ax-handle>` — a connection port on a node.
 *
 * Place handles inside an `<ax-node>`. A handle renders a small dot on one
 * edge of the node. A `source` handle starts connections; a `target` handle
 * receives them. The handle's lattice-space anchor is derived from its parent
 * node's box and its `position` side — it does not need its own coordinates.
 *
 * @element ax-handle
 *
 * @attr {"source"|"target"} type      Connection role (default "source")
 * @attr {"left"|"right"|"top"|"bottom"} position  Which node edge (default
 *                                     "right" for source, "left" for target)
 * @attr {string} handle-id            Optional id, unique within the node.
 *                                     Edges reference it as `nodeId:handleId`.
 *
 * @cssprop --ax-handle-size      Dot diameter (default 11px)
 * @cssprop --ax-handle-bg        Dot fill (default node border / accent)
 * @cssprop --ax-handle-border    Dot border color
 *
 * @event handle-pointerdown   detail: {handle, type, side} — cancellable;
 *                             the lattice starts a connection drag on this.
 */

const TEMPLATE = `
<style>
  :host {
    position: absolute;
    width: var(--ax-handle-size, 11px);
    height: var(--ax-handle-size, 11px);
    box-sizing: border-box;
    border-radius: 50%;
    background: var(--ax-handle-bg, var(--ax-accent, #7c5cff));
    border: 2px solid var(--ax-handle-border, #0a0c11);
    cursor: crosshair;
    pointer-events: auto;
    z-index: 2;
    transition: transform .1s, box-shadow .1s;
  }
  :host(:hover) {
    transform: scale(1.35);
    box-shadow: 0 0 0 3px rgba(124,92,255,.25);
  }
  /* Highlighted as a valid connection target during a connect drag. */
  :host([connect-target]) {
    transform: scale(1.5);
    box-shadow: 0 0 0 4px rgba(0,217,177,.4);
    background: var(--ax-accent-2, #00d9b1);
  }
  /* Side placement — centered on the chosen edge, half-overhanging. */
  :host([position="left"])   { left: 0;   top: 50%;  transform-origin: center; margin-left: calc(var(--ax-handle-size, 11px) / -2); margin-top: calc(var(--ax-handle-size, 11px) / -2); }
  :host([position="right"])  { left: 100%;top: 50%;  margin-left: calc(var(--ax-handle-size, 11px) / -2); margin-top: calc(var(--ax-handle-size, 11px) / -2); }
  :host([position="top"])    { left: 50%; top: 0;    margin-left: calc(var(--ax-handle-size, 11px) / -2); margin-top: calc(var(--ax-handle-size, 11px) / -2); }
  :host([position="bottom"]) { left: 50%; top: 100%; margin-left: calc(var(--ax-handle-size, 11px) / -2); margin-top: calc(var(--ax-handle-size, 11px) / -2); }
</style>
`;

export class AxHandleElement extends HTMLElement {
  static get observedAttributes() {
    return ['type', 'position'];
  }

  /** @type {ShadowRoot} */
  #root;
  /** Parent <ax-node>, resolved on connect. */
  #node = null;
  /** Parent <ax-lattice>, resolved on connect. */
  #lattice = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = TEMPLATE;
  }

  connectedCallback() {
    // Default position depends on role.
    if (!this.hasAttribute('position')) {
      this.setAttribute('position', this.type === 'target' ? 'left' : 'right');
    }
    if (!this.hasAttribute('type')) this.setAttribute('type', 'source');
    this.#node = this.closest('ax-node');
    this.#lattice = this.closest('ax-lattice');
    // Accessibility
    if (!this.hasAttribute('role')) this.setAttribute('role', 'button');
    if (!this.hasAttribute('aria-label')) {
      const nid = this.#node?.id || 'node';
      this.setAttribute('aria-label',
        this.type === 'source'
          ? `Connection source on ${nid}`
          : `Connection target on ${nid}`);
    }
    if (this.#lattice && typeof this.#lattice._registerHandle === 'function') {
      this.#lattice._registerHandle(this);
    }
    this.addEventListener('pointerdown', this.#onPointerDown);
  }

  disconnectedCallback() {
    this.removeEventListener('pointerdown', this.#onPointerDown);
    if (this.#lattice && typeof this.#lattice._unregisterHandle === 'function') {
      this.#lattice._unregisterHandle(this);
    }
    this.#node = null;
    this.#lattice = null;
  }

  // ── Public API ─────────────────────────────────────────────────────────

  /** @returns {"source"|"target"} */
  get type() {
    return this.getAttribute('type') === 'target' ? 'target' : 'source';
  }

  /** @returns {"left"|"right"|"top"|"bottom"} */
  get side() {
    return /** @type {any} */ (this.getAttribute('position')) || 'right';
  }

  /** Handle id (the `handle-id` attribute), or null. */
  get handleId() {
    return this.getAttribute('handle-id');
  }

  /** The parent `<ax-node>` element, if any. */
  get node() {
    return this.#node;
  }

  /**
   * Fully-qualified reference: `nodeId` or `nodeId:handleId`.
   * Edges use this to address the handle.
   */
  get ref() {
    const nid = this.#node?.id || '';
    return this.handleId ? `${nid}:${this.handleId}` : nid;
  }

  /**
   * The handle's anchor in lattice space, derived from the parent node's
   * box and this handle's side.
   * @returns {{x:number,y:number,side:string}|null}
   */
  anchor() {
    if (!this.#node || typeof this.#node.getBox !== 'function') return null;
    const box = this.#node.getBox();
    const cx = box.x + box.width / 2;
    const cy = box.y + box.height / 2;
    switch (this.side) {
      case 'left': return { x: box.x, y: cy, side: 'left' };
      case 'right': return { x: box.x + box.width, y: cy, side: 'right' };
      case 'top': return { x: cx, y: box.y, side: 'top' };
      case 'bottom': return { x: cx, y: box.y + box.height, side: 'bottom' };
      default: return { x: box.x + box.width, y: cy, side: 'right' };
    }
  }

  // ── Internal ───────────────────────────────────────────────────────────

  #onPointerDown = (ev) => {
    if (ev.button !== 0) return;
    // A handle press starts a connection — never a node drag or selection.
    ev.stopPropagation();
    const detail = { handle: this, type: this.type, side: this.side };
    const e = new CustomEvent('handle-pointerdown', {
      detail, bubbles: true, composed: true, cancelable: true,
    });
    // The lattice listens for this and drives the connection drag.
    this.dispatchEvent(e);
    if (e.defaultPrevented) return;
    // Hand the raw pointer event to the lattice via a follow-up so it can
    // capture. We include the pointerId + coords in the detail.
    this.dispatchEvent(new CustomEvent('handle-connect-start', {
      detail: { ...detail, pointerId: ev.pointerId, clientX: ev.clientX, clientY: ev.clientY },
      bubbles: true, composed: true,
    }));
  };

  /** Mark/unmark this handle as a valid connect target (lattice-internal). */
  _setConnectTarget(on) {
    if (on) this.setAttribute('connect-target', '');
    else this.removeAttribute('connect-target');
  }
}

if (!customElements.get('ax-handle')) {
  customElements.define('ax-handle', AxHandleElement);
}
