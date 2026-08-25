/**
 * `<ax-edge>` — a declarative edge between two nodes (or handles).
 *
 * An `<ax-edge>` carries no visuals itself — it is a configuration record.
 * The parent `<ax-lattice>` reads every `<ax-edge>` child and renders the
 * actual bezier `<path>` into a single shared SVG layer (one SVG, many
 * paths — fast, and matches how React Flow draws edges).
 *
 * Endpoints are references:
 *   - `"nodeId"`            connect to the node, auto-picking a side
 *   - `"nodeId:handleId"`   connect to a specific `<ax-handle>`
 *
 * @element ax-edge
 *
 * @attr {string}  from      Source endpoint reference
 * @attr {string}  to        Target endpoint reference
 * @attr {string}  label     Optional text rendered at the curve midpoint
 * @attr {boolean} selected  Selection state
 * @attr {boolean} active    Live state — renders an animated "flowing" curve
 *
 * @cssprop --ax-edge-color         Stroke color (idle)
 * @cssprop --ax-edge-color-sel     Stroke color (selected)
 * @cssprop --ax-edge-color-active  Stroke color (active / flowing)
 * @cssprop --ax-edge-width         Stroke width
 */

export class AxEdgeElement extends HTMLElement {
  static get observedAttributes() {
    return ['from', 'to', 'label', 'selected', 'active'];
  }

  /** @type {HTMLElement|null} parent lattice */
  #lattice = null;

  connectedCallback() {
    // Edges are pure data — never displayed directly.
    this.style.display = 'none';
    if (!this.id) this.id = `ax-edge-${++AxEdgeElement._uid}`;
    this.#lattice = this.closest('ax-lattice');
    if (this.#lattice && typeof this.#lattice._registerEdge === 'function') {
      this.#lattice._registerEdge(this);
    }
  }

  disconnectedCallback() {
    if (this.#lattice && typeof this.#lattice._unregisterEdge === 'function') {
      this.#lattice._unregisterEdge(this);
    }
    this.#lattice = null;
  }

  attributeChangedCallback() {
    // Any change re-renders the edge layer.
    if (this.#lattice && typeof this.#lattice._edgesChanged === 'function') {
      this.#lattice._edgesChanged();
    }
  }

  // ── Public API ─────────────────────────────────────────────────────────

  get from() { return this.getAttribute('from') || ''; }
  set from(v) { this.setAttribute('from', v); }

  get to() { return this.getAttribute('to') || ''; }
  set to(v) { this.setAttribute('to', v); }

  get label() { return this.getAttribute('label') || ''; }
  set label(v) { this.setAttribute('label', v); }

  get selected() { return this.hasAttribute('selected'); }
  set selected(v) {
    if (v) this.setAttribute('selected', '');
    else this.removeAttribute('selected');
  }

  /** Live state — when true the edge renders an animated flowing curve. */
  get active() { return this.hasAttribute('active'); }
  set active(v) {
    if (v) this.setAttribute('active', '');
    else this.removeAttribute('active');
  }

  /** Split an endpoint reference into `{ nodeId, handleId }`. */
  static parseRef(ref) {
    const i = (ref || '').indexOf(':');
    return i < 0
      ? { nodeId: ref || '', handleId: null }
      : { nodeId: ref.slice(0, i), handleId: ref.slice(i + 1) };
  }
}
AxEdgeElement._uid = 0;

if (!customElements.get('ax-edge')) {
  customElements.define('ax-edge', AxEdgeElement);
}
