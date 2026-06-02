/**
 * `<ax-minimap>` — a scaled overview of a lattice with a draggable viewport
 * indicator.
 *
 * Link it to a lattice by id:
 *   <ax-minimap for="my-lattice"></ax-minimap>
 *
 * Or assign the element directly:
 *   minimap.target = document.querySelector('ax-lattice');
 *
 * Click anywhere on the minimap to recentre the lattice there; drag to pan.
 *
 * @element ax-minimap
 *
 * @attr {string} for   Id of the `<ax-lattice>` to mirror
 *
 * @cssprop --ax-minimap-bg        Panel background
 * @cssprop --ax-minimap-border    Panel border
 * @cssprop --ax-minimap-node      Node rectangle fill
 * @cssprop --ax-minimap-node-sel  Selected node fill
 * @cssprop --ax-minimap-view      Viewport indicator stroke/fill
 */

const TEMPLATE = `
<style>
  :host {
    display: block;
    width: var(--ax-minimap-width, 220px);
    height: var(--ax-minimap-height, 150px);
    background: var(--ax-minimap-bg, #10131a);
    border: 1px solid var(--ax-minimap-border, #232a37);
    border-radius: 8px;
    overflow: hidden;
    box-shadow: 0 6px 18px rgba(0,0,0,.35);
  }
  svg { width: 100%; height: 100%; display: block; cursor: pointer; }
  .mm-node { fill: var(--ax-minimap-node, #4a536a); }
  .mm-node.sel { fill: var(--ax-minimap-node-sel, var(--ax-accent, #7c5cff)); }
  /* Execution state — mirrors the canvas so a run is visible in the minimap. */
  .mm-node.running { fill: var(--ax-node-running, var(--ax-accent, #7c5cff)); }
  .mm-node.success { fill: var(--ax-node-success, #00d9b1); }
  .mm-node.error   { fill: var(--ax-node-error, #ff6b6b); }
  .mm-node.pending { fill: var(--ax-node-pending, #5a6478); }
  .mm-view {
    fill: var(--ax-minimap-view-fill, rgba(124,92,255,.14));
    stroke: var(--ax-minimap-view, var(--ax-accent, #7c5cff));
    stroke-width: 2;
    vector-effect: non-scaling-stroke;
  }
  .mm-empty { fill: var(--ax-muted, #5a6478); }
</style>
<svg part="svg" preserveAspectRatio="xMidYMid meet">
  <g class="mm-nodes"></g>
  <rect class="mm-view"></rect>
</svg>
`;

export class AxMinimapElement extends HTMLElement {
  static get observedAttributes() { return ['for']; }

  /** @type {ShadowRoot} */
  #root;
  /** @type {SVGSVGElement} */
  #svg;
  /** @type {SVGGElement} */
  #nodesG;
  /** @type {SVGRectElement} */
  #viewRect;
  /** @type {HTMLElement|null} */
  #target = null;
  /** rAF coalescing id. */
  #raf = 0;
  /** Bound listeners (so we can detach). */
  #onLatticeEvent = () => this.#scheduleRender();
  /** Observes node status/active attribute changes so a run shows live. */
  #observer = null;
  /** Drag state for click-to-pan. */
  #dragging = false;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = TEMPLATE;
    this.#svg = /** @type {SVGSVGElement} */ (this.#root.querySelector('svg'));
    this.#nodesG = /** @type {SVGGElement} */ (this.#root.querySelector('.mm-nodes'));
    this.#viewRect = /** @type {SVGRectElement} */ (this.#root.querySelector('.mm-view'));
  }

  connectedCallback() {
    this.#resolveTarget();
    this.#svg.addEventListener('pointerdown', this.#onPointerDown);
    this.#svg.addEventListener('pointermove', this.#onPointerMove);
    this.#svg.addEventListener('pointerup', this.#onPointerUp);
    this.#svg.addEventListener('pointercancel', this.#onPointerUp);
  }

  disconnectedCallback() {
    this.#detachTarget();
    this.#svg.removeEventListener('pointerdown', this.#onPointerDown);
    this.#svg.removeEventListener('pointermove', this.#onPointerMove);
    this.#svg.removeEventListener('pointerup', this.#onPointerUp);
    this.#svg.removeEventListener('pointercancel', this.#onPointerUp);
  }

  attributeChangedCallback(name) {
    if (name === 'for') this.#resolveTarget();
  }

  /** The lattice this minimap mirrors. Assignable directly. */
  get target() { return this.#target; }
  set target(el) {
    this.#detachTarget();
    this.#target = el;
    this.#attachTarget();
    this.#scheduleRender();
  }

  // ── Target wiring ─────────────────────────────────────────────────────

  #resolveTarget() {
    const id = this.getAttribute('for');
    const el = id ? document.getElementById(id) : null;
    if (el !== this.#target) {
      this.#detachTarget();
      this.#target = el;
      this.#attachTarget();
    }
    this.#scheduleRender();
  }

  #attachTarget() {
    const t = this.#target;
    if (!t) return;
    for (const ev of [
      'viewport-change', 'node-moving', 'node-moveend',
      'nodes-deleted', 'edges-deleted', 'selection-change',
    ]) {
      t.addEventListener(ev, this.#onLatticeEvent);
    }
    // Node status changes don't fire lattice events — observe the attribute
    // so a live run is reflected in the minimap.
    this.#observer = new MutationObserver(() => this.#scheduleRender());
    this.#observer.observe(t, {
      subtree: true,
      attributes: true,
      attributeFilter: ['status', 'active', 'selected'],
    });
  }

  #detachTarget() {
    const t = this.#target;
    if (this.#observer) { this.#observer.disconnect(); this.#observer = null; }
    if (!t) return;
    for (const ev of [
      'viewport-change', 'node-moving', 'node-moveend',
      'nodes-deleted', 'edges-deleted', 'selection-change',
    ]) {
      t.removeEventListener(ev, this.#onLatticeEvent);
    }
  }

  // ── Render ────────────────────────────────────────────────────────────

  #scheduleRender() {
    if (this.#raf) return;
    this.#raf = requestAnimationFrame(() => {
      this.#raf = 0;
      this.#render();
    });
  }

  /**
   * Compute the lattice-space rectangle currently visible in the lattice.
   * @returns {{x:number,y:number,width:number,height:number}|null}
   */
  #viewportRect() {
    const t = this.#target;
    if (!t || typeof t.screenToLattice !== 'function') return null;
    const r = t.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) return null;
    const tl = t.screenToLattice({ x: 0, y: 0 });
    const br = t.screenToLattice({ x: r.width, y: r.height });
    return { x: tl.x, y: tl.y, width: br.x - tl.x, height: br.y - tl.y };
  }

  #render() {
    const t = this.#target;
    this.#nodesG.textContent = '';
    if (!t || typeof t.nodes === 'undefined') return;

    const boxes = [];
    for (const n of t.nodes) {
      if (typeof n.getBox === 'function') {
        boxes.push({
          box: n.getBox(),
          selected: !!n.selected,
          status: n.getAttribute ? n.getAttribute('status') : null,
        });
      }
    }
    const view = this.#viewportRect();

    // World bounds = union of all node boxes + the viewport rectangle, so the
    // indicator stays visible even when panned away from the graph.
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    const include = (x, y, w, h) => {
      minX = Math.min(minX, x); minY = Math.min(minY, y);
      maxX = Math.max(maxX, x + w); maxY = Math.max(maxY, y + h);
    };
    for (const { box } of boxes) include(box.x, box.y, box.width, box.height);
    if (view) include(view.x, view.y, view.width, view.height);
    if (!Number.isFinite(minX)) { minX = 0; minY = 0; maxX = 1; maxY = 1; }

    // Pad the world a touch.
    const padX = (maxX - minX) * 0.08 + 20;
    const padY = (maxY - minY) * 0.08 + 20;
    minX -= padX; minY -= padY; maxX += padX; maxY += padY;
    this.#svg.setAttribute('viewBox',
      `${minX} ${minY} ${Math.max(1, maxX - minX)} ${Math.max(1, maxY - minY)}`);

    // Node rectangles (in lattice coords; the viewBox scales them).
    for (const { box, selected, status } of boxes) {
      const r = document.createElementNS('http://www.w3.org/2000/svg', 'rect');
      let cls = 'mm-node';
      if (selected) cls += ' sel';
      if (status && status !== 'idle') cls += ' ' + status;
      r.setAttribute('class', cls);
      r.setAttribute('x', String(box.x));
      r.setAttribute('y', String(box.y));
      r.setAttribute('width', String(Math.max(1, box.width)));
      r.setAttribute('height', String(Math.max(1, box.height)));
      r.setAttribute('rx', '3');
      this.#nodesG.appendChild(r);
    }

    // Viewport indicator
    if (view) {
      this.#viewRect.setAttribute('x', String(view.x));
      this.#viewRect.setAttribute('y', String(view.y));
      this.#viewRect.setAttribute('width', String(Math.max(1, view.width)));
      this.#viewRect.setAttribute('height', String(Math.max(1, view.height)));
      this.#viewRect.style.display = '';
    } else {
      this.#viewRect.style.display = 'none';
    }
  }

  // ── Navigation ────────────────────────────────────────────────────────

  /** Convert a pointer event to a lattice-space point via the SVG viewBox. */
  #toLattice(ev) {
    const pt = this.#svg.createSVGPoint();
    pt.x = ev.clientX;
    pt.y = ev.clientY;
    const ctm = this.#svg.getScreenCTM();
    if (!ctm) return null;
    const p = pt.matrixTransform(ctm.inverse());
    return { x: p.x, y: p.y };
  }

  /** Recentre the lattice viewport on a lattice-space point. */
  #centerOn(latticePt) {
    const t = this.#target;
    if (!t || typeof t.getViewport !== 'function') return;
    const r = t.getBoundingClientRect();
    const vp = t.getViewport();
    t.setViewport({
      x: r.width / 2 - latticePt.x * vp.k,
      y: r.height / 2 - latticePt.y * vp.k,
      k: vp.k,
    });
  }

  #onPointerDown = (ev) => {
    const p = this.#toLattice(ev);
    if (!p) return;
    this.#dragging = true;
    this.#svg.setPointerCapture(ev.pointerId);
    this.#centerOn(p);
  };
  #onPointerMove = (ev) => {
    if (!this.#dragging) return;
    const p = this.#toLattice(ev);
    if (p) this.#centerOn(p);
  };
  #onPointerUp = (ev) => {
    this.#dragging = false;
    if (this.#svg.hasPointerCapture(ev.pointerId)) {
      this.#svg.releasePointerCapture(ev.pointerId);
    }
  };
}

if (!customElements.get('ax-minimap')) {
  customElements.define('ax-minimap', AxMinimapElement);
}
