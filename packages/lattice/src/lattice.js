/**
 * `<ax-lattice>` — infinite zoom-pan canvas Web Component.
 *
 * Phase A surface: pan, zoom, snap, background patterns, programmatic API.
 * Children placed inside `<ax-lattice>` will be transformed in lattice space
 * once Phase B introduces `<ax-node>`. Today, anything inside is rendered
 * as raw HTML/SVG in the unstyled `<slot>`.
 *
 * @element ax-lattice
 *
 * @attr {number}  zoom              Current zoom level (default 1)
 * @attr {number}  min-zoom          Minimum zoom (default 0.2)
 * @attr {number}  max-zoom          Maximum zoom (default 3)
 * @attr {number}  pan-x             Initial pan-x offset
 * @attr {number}  pan-y             Initial pan-y offset
 * @attr {number}  snap              Snap-to-grid spacing in lattice units; 0 disables
 * @attr {"dots"|"grid"|"none"} background  Background pattern
 * @attr {boolean} fit-view-on-init  Auto fit-view once after first render
 *
 * @cssprop --ax-bg          Canvas background color
 * @cssprop --ax-fg          Foreground / text color
 * @cssprop --ax-grid        Grid line/dot color
 * @cssprop --ax-grid-size   Grid spacing in pixels
 * @cssprop --ax-accent      Selection / highlight color
 *
 * @event viewport-change  detail: { x, y, k }  fired (rAF-coalesced) on pan/zoom
 */

import {
  clamp,
  fitView as fitViewMath,
  identity,
  latticeToScreen as ltsMath,
  nodesBounds,
  pan as panMath,
  screenToLattice as stlMath,
  snap as snapMath,
  zoomAt,
  zoomCentered,
  zoomFromWheel,
} from './viewport.js';
import {
  add as selAdd,
  clear as selClear,
  equals as selEquals,
  rectFromPoints,
  rectIntersects,
  replace as selReplace,
  toggle as selToggle,
} from './selection.js';
import {
  autoAnchor,
  bezierPath,
  bezierPoint,
  distance as geoDistance,
} from './geometry.js';
import { History } from './history.js';
import { layeredLayout } from './layout.js';

const TEMPLATE = `
<style>
  :host {
    display: block;
    position: relative;
    overflow: hidden;
    width: 100%;
    height: 100%;
    background: var(--ax-bg, #0a0c11);
    color: var(--ax-fg, #e8ecf3);
    contain: strict;
    /* Disable browser default touch/wheel handling so we can drive zoom/pan. */
    touch-action: none;
    user-select: none;
    -webkit-user-select: none;
  }
  :host([hidden]) { display: none; }

  /* Stacking, bottom to top:
       svg.bg            (background pattern; pointer-events: none)
       .pointer-eater    (catches empty-canvas drags; cursor: grab)
       .viewport         (transformed, pointer-events: none — events fall through to eater)
         <slot>          (slotted nodes set pointer-events: auto themselves)
       .marquee          (box-select overlay; pointer-events: none) */
  svg.bg {
    position: absolute; inset: 0;
    width: 100%; height: 100%;
    pointer-events: none;
  }
  .pointer-eater {
    position: absolute; inset: 0;
    cursor: grab;
    background: transparent;
  }
  .pointer-eater.dragging { cursor: grabbing; }
  .pointer-eater.marquee-active { cursor: crosshair; }
  .viewport {
    position: absolute; inset: 0;
    width: 100%; height: 100%;
    transform-origin: 0 0;
    will-change: transform;
    /* Transparent to pointer events; slotted children opt back in. */
    pointer-events: none;
  }
  /* Slotted nodes opt back into pointer events (their own children take over). */
  ::slotted(*) { pointer-events: auto; user-select: text; }
  ::slotted(ax-node) { pointer-events: auto; user-select: none; }
  ::slotted(ax-edge) { display: none; }
  /* Virtualization: off-screen nodes keep layout (so edges still route) but
     skip painting — the expensive part for styled nodes. */
  ::slotted(ax-node.ax-culled) { visibility: hidden; pointer-events: none; }

  /* Edge layer — one SVG inside the viewport, beneath the nodes. Drawn in
     lattice coordinates so it pans/zooms with the viewport transform. */
  .edge-layer {
    position: absolute; inset: 0;
    width: 100%; height: 100%;
    overflow: visible;
    pointer-events: none;
  }
  .edge-layer .ax-edge-path {
    fill: none;
    stroke: var(--ax-edge-color, #4a536a);
    stroke-width: var(--ax-edge-width, 2);
    pointer-events: none;
    transition: stroke .12s;
  }
  .edge-layer .ax-edge-path.selected {
    stroke: var(--ax-edge-color-sel, var(--ax-accent, #7c5cff));
    stroke-width: calc(var(--ax-edge-width, 2) + 1);
  }
  /* Live "flowing" edge — marching dashes from source toward target. */
  .edge-layer .ax-edge-path.active {
    stroke: var(--ax-edge-color-active, var(--ax-accent-2, #00d9b1));
    stroke-width: calc(var(--ax-edge-width, 2) + 0.5);
    stroke-dasharray: 9 6;
    animation: ax-edge-flow 0.55s linear infinite;
  }
  @keyframes ax-edge-flow {
    to { stroke-dashoffset: -15; }
  }
  .edge-layer .ax-edge-hit {
    fill: none;
    stroke: transparent;
    stroke-width: 16;
    pointer-events: stroke;
    cursor: pointer;
  }
  .edge-layer .ax-edge-preview {
    fill: none;
    stroke: var(--ax-accent-2, #00d9b1);
    stroke-width: 2;
    stroke-dasharray: 6 5;
    pointer-events: none;
  }
  .edge-layer .ax-edge-label {
    fill: var(--ax-fg, #e8ecf3);
    font: 500 11px 'Inter', system-ui, sans-serif;
    paint-order: stroke;
    stroke: var(--ax-bg, #0a0c11);
    stroke-width: 3;
    pointer-events: none;
    text-anchor: middle;
    dominant-baseline: middle;
  }

  /* Box-select marquee */
  .marquee {
    position: absolute;
    background: var(--ax-marquee-fill, rgba(124,92,255,.12));
    border: 1px solid var(--ax-accent, #7c5cff);
    pointer-events: none;
    display: none;
  }
  .marquee.active { display: block; }

  /* Visually-hidden live region for screen-reader announcements. */
  .ax-sr {
    position: absolute;
    width: 1px; height: 1px;
    margin: -1px; padding: 0; border: 0;
    overflow: hidden; clip: rect(0 0 0 0);
    white-space: nowrap;
  }
</style>

<svg class="bg" aria-hidden="true">
  <defs>
    <pattern id="ax-dots"  patternUnits="userSpaceOnUse" width="20" height="20">
      <circle cx="1" cy="1" r="1" fill="var(--ax-grid, #232a37)"></circle>
    </pattern>
    <pattern id="ax-grid" patternUnits="userSpaceOnUse" width="20" height="20">
      <path d="M 20 0 L 0 0 L 0 20" fill="none" stroke="var(--ax-grid, #232a37)" stroke-width="1"></path>
    </pattern>
  </defs>
  <rect class="bg-fill" width="100%" height="100%" fill="url(#ax-dots)"></rect>
</svg>

<div class="pointer-eater" part="pointer-eater"></div>

<div class="viewport" part="viewport">
  <svg class="edge-layer" part="edges" aria-hidden="true">
    <defs>
      <marker id="ax-arrow" viewBox="0 0 10 10" refX="9" refY="5"
              markerWidth="7" markerHeight="7" orient="auto-start-reverse">
        <path d="M0,0 L10,5 L0,10 z" fill="var(--ax-edge-color, #4a536a)"></path>
      </marker>
      <marker id="ax-arrow-sel" viewBox="0 0 10 10" refX="9" refY="5"
              markerWidth="7" markerHeight="7" orient="auto-start-reverse">
        <path d="M0,0 L10,5 L0,10 z" fill="var(--ax-edge-color-sel, #7c5cff)"></path>
      </marker>
    </defs>
    <g class="edge-paths"></g>
    <g class="edge-preview"></g>
  </svg>
  <slot></slot>
</div>

<div class="marquee" part="marquee"></div>

<div class="ax-sr" role="status" aria-live="polite" aria-atomic="true"></div>
`;

const ATTR = {
  ZOOM: 'zoom',
  MIN_ZOOM: 'min-zoom',
  MAX_ZOOM: 'max-zoom',
  PAN_X: 'pan-x',
  PAN_Y: 'pan-y',
  SNAP: 'snap',
  BACKGROUND: 'background',
  FIT_ON_INIT: 'fit-view-on-init',
  VIRTUALIZE: 'virtualize',
};

export class AxLatticeElement extends HTMLElement {
  static get observedAttributes() {
    return Object.values(ATTR);
  }

  /** @type {ShadowRoot} */
  #root;
  /** @type {SVGElement} */
  #bgSvg;
  /** @type {SVGRectElement} */
  #bgFill;
  /** @type {HTMLElement} */
  #vpEl;
  /** @type {HTMLElement} */
  #eaterEl;
  /** @type {HTMLElement} */
  #marqueeEl;
  /** @type {HTMLElement} */
  #srEl;
  /** @type {SVGGElement} */
  #edgePathsEl;
  /** @type {SVGGElement} */
  #edgePreviewEl;

  // ── Edge / handle registries ──────────────────────────────────────────
  /** @type {Set<HTMLElement>} */
  #edges = new Set();
  /** @type {Set<HTMLElement>} */
  #handles = new Set();
  /** @type {Set<HTMLElement>} */
  #selectedEdges = new Set();
  /** rAF id for edge re-render coalescing. */
  #rafEdges = 0;
  /** Active connection drag, or null. */
  #connect = null;

  /** @type {{x:number,y:number,k:number}} */
  #viewport = identity();

  /** @type {number} */
  #minZoom = 0.2;
  /** @type {number} */
  #maxZoom = 3;
  /** @type {number} */
  #snap = 0;
  /** @type {"dots"|"grid"|"none"} */
  #background = 'dots';
  /** @type {boolean} */
  #fitOnInit = false;
  /** @type {boolean} */
  #initialized = false;
  /** @type {boolean} */
  #virtualize = false;
  /** Count of nodes currently culled by virtualization. */
  #culledCount = 0;

  /** Pending RAF id for viewport-change coalescing. */
  #rafEmit = 0;

  /** Pan-drag tracking */
  #panStart = null;

  /** Pinch tracking — Map<pointerId, {x,y}> */
  #pointers = new Map();
  #pinch = null;

  /** Box-select tracking. */
  #marquee = null;

  // ── Node registry & selection ─────────────────────────────────────────
  /** @type {Set<HTMLElement>} */
  #nodes = new Set();
  /** @type {Set<HTMLElement>} */
  #selection = new Set();
  /** Snapshot of every moving node's start position (dragged + group). */
  #moveSnapshot = null;
  /** The node the user grabbed during a move (moves itself; others follow). */
  #moveDraggedNode = null;
  /** Undo/redo history. */
  #history;
  /** Internal copy/paste clipboard: { nodes: Element[], edges: spec[] }. */
  #clipboard = null;
  /** Counter to keep pasted node ids unique. */
  #pasteCounter = 0;
  /**
   * When a plain (non-additive) pointerdown lands on an already-selected node,
   * we defer collapsing the multi-selection until pointer-up — so a drag
   * group-drags, and only a click collapses. This holds that pending node.
   */
  #pendingCollapse = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = TEMPLATE;
    this.#bgSvg = /** @type {SVGElement} */ (this.#root.querySelector('svg.bg'));
    this.#bgFill = /** @type {SVGRectElement} */ (this.#root.querySelector('.bg-fill'));
    this.#vpEl = /** @type {HTMLElement} */ (this.#root.querySelector('.viewport'));
    this.#eaterEl = /** @type {HTMLElement} */ (this.#root.querySelector('.pointer-eater'));
    this.#marqueeEl = /** @type {HTMLElement} */ (this.#root.querySelector('.marquee'));
    this.#srEl = /** @type {HTMLElement} */ (this.#root.querySelector('.ax-sr'));
    this.#edgePathsEl = /** @type {SVGGElement} */ (this.#root.querySelector('.edge-paths'));
    this.#edgePreviewEl = /** @type {SVGGElement} */ (this.#root.querySelector('.edge-preview'));
    this.#history = new History((state) => {
      this.dispatchEvent(new CustomEvent('history-change', {
        detail: state, bubbles: true, composed: true,
      }));
    });
  }

  // ── Lifecycle ──────────────────────────────────────────────────────────
  connectedCallback() {
    this.#readAttributes();
    this.#bindEvents();
    this.#applyTransform();
    this.#applyBackground();
    // Accessibility: the canvas is an interactive application region.
    if (!this.hasAttribute('role')) this.setAttribute('role', 'application');
    if (!this.hasAttribute('aria-roledescription')) {
      this.setAttribute('aria-roledescription', 'graph editor');
    }
    if (!this.hasAttribute('aria-label')) {
      this.setAttribute('aria-label', 'Lattice graph canvas');
    }
    queueMicrotask(() => {
      if (this.#fitOnInit && !this.#initialized) {
        this.#initialized = true;
        this.fitView();
      }
    });
  }

  disconnectedCallback() {
    this.#unbindEvents();
  }

  attributeChangedCallback(name, _old, val) {
    switch (name) {
      case ATTR.ZOOM:
      case ATTR.PAN_X:
      case ATTR.PAN_Y: {
        const k = this.#num(ATTR.ZOOM, 1);
        const x = this.#num(ATTR.PAN_X, 0);
        const y = this.#num(ATTR.PAN_Y, 0);
        this.#setViewportInternal({ x, y, k }, /* emit */ false);
        break;
      }
      case ATTR.MIN_ZOOM:
        this.#minZoom = this.#num(ATTR.MIN_ZOOM, 0.2);
        break;
      case ATTR.MAX_ZOOM:
        this.#maxZoom = this.#num(ATTR.MAX_ZOOM, 3);
        break;
      case ATTR.SNAP:
        this.#snap = this.#num(ATTR.SNAP, 0);
        break;
      case ATTR.BACKGROUND:
        this.#background = /** @type {any} */ (val || 'dots');
        this.#applyBackground();
        break;
      case ATTR.FIT_ON_INIT:
        this.#fitOnInit = val != null;
        break;
      case ATTR.VIRTUALIZE:
        this.#virtualize = val != null;
        if (this.#virtualize) this.#cullNodes();
        else this.#uncullAll();
        break;
    }
  }

  // ── Public API ─────────────────────────────────────────────────────────

  /** @returns {{x:number,y:number,k:number}} */
  getViewport() {
    return { ...this.#viewport };
  }

  /** @param {{x?:number,y?:number,k?:number}} vp */
  setViewport(vp) {
    const next = {
      x: vp.x ?? this.#viewport.x,
      y: vp.y ?? this.#viewport.y,
      k: clamp(vp.k ?? this.#viewport.k, this.#minZoom, this.#maxZoom),
    };
    this.#setViewportInternal(next, /* emit */ true);
  }

  /** Step zoom in by a factor (default 1.2). Center-anchored. */
  zoomIn(factor = 1.2) {
    const rect = this.getBoundingClientRect();
    const next = zoomCentered(
      this.#viewport,
      this.#viewport.k * factor,
      { width: rect.width, height: rect.height },
      { minZoom: this.#minZoom, maxZoom: this.#maxZoom },
    );
    this.#setViewportInternal(next, true);
  }

  /** Step zoom out by a factor (default 1.2). Center-anchored. */
  zoomOut(factor = 1.2) {
    this.zoomIn(1 / factor);
  }

  /**
   * Frame all "lattice-positioned" descendants. A child counts if it has
   * `data-x` + `data-y` (and optional `data-w`/`data-h`) attributes. Phase B
   * `<ax-node>` will set these automatically.
   *
   * @param {{padding?: number}} [opts]
   */
  fitView(opts = {}) {
    const padding = opts.padding ?? 40;
    const nodes = this.#latticeChildBounds();
    const rect = this.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    const next = fitViewMath(
      nodesBounds(nodes),
      { width: rect.width, height: rect.height },
      { padding, minZoom: this.#minZoom, maxZoom: this.#maxZoom },
    );
    this.#setViewportInternal(next, true);
  }

  /**
   * Convert canvas-local screen coordinates to lattice coordinates.
   * @param {{x:number,y:number}} pt
   */
  screenToLattice(pt) {
    return stlMath(this.#viewport, pt);
  }

  /**
   * Convert lattice coordinates to canvas-local screen coordinates.
   * @param {{x:number,y:number}} pt
   */
  latticeToScreen(pt) {
    return ltsMath(this.#viewport, pt);
  }

  /** Snap a lattice coordinate to the grid (no-op when `snap` attr is 0). */
  snap(value) {
    return snapMath(value, this.#snap);
  }

  // ── Node registry & selection ─────────────────────────────────────────

  /** Set of registered &lt;ax-node&gt; elements (read-only). */
  get nodes() { return new Set(this.#nodes); }

  /** Current selection (read-only). */
  get selection() { return new Set(this.#selection); }

  /** Currently selected node ids, in registry order. */
  selectedIds() {
    const ids = [];
    for (const n of this.#nodes) if (this.#selection.has(n)) ids.push(n.id);
    return ids;
  }

  /** Select all registered nodes. */
  selectAll() {
    selClear(this.#selection);
    for (const n of this.#nodes) selAdd(this.#selection, n);
    this.#syncSelectedAttr();
    this.#emitSelectionChange();
  }

  /** Clear selection. */
  deselectAll() {
    if (this.#selection.size === 0) return;
    selClear(this.#selection);
    this.#syncSelectedAttr();
    this.#emitSelectionChange();
  }

  /**
   * Programmatic delete of all selected nodes. Fires `nodes-delete-request`
   * (cancellable). If not prevented, removes the nodes from the DOM (which
   * fires their disconnectedCallback and unregisters them).
   */
  deleteSelected() {
    if (this.#selection.size === 0) return;
    const ids = this.selectedIds();
    const ev = new CustomEvent('nodes-delete-request', {
      detail: { ids }, bubbles: true, composed: true, cancelable: true,
    });
    if (!this.dispatchEvent(ev)) return;
    // Default behavior: remove the nodes.
    const removed = [...this.#selection];
    for (const n of removed) n.parentNode?.removeChild(n);
    selClear(this.#selection);
    this.#emitSelectionChange();
    this.dispatchEvent(new CustomEvent('nodes-deleted', {
      detail: { ids }, bubbles: true, composed: true,
    }));
    this.#announce(`${ids.length} node${ids.length === 1 ? '' : 's'} deleted`);
    this.#history.push({
      label: 'delete nodes',
      undo: () => { for (const n of removed) this.appendChild(n); },
      redo: () => { for (const n of removed) n.remove(); },
    });
  }

  /** Lattice-internal: called by &lt;ax-node&gt; on connect. */
  _registerNode(node) {
    this.#nodes.add(node);
    this.#scheduleEdgeRender();
  }
  /** Lattice-internal: called by &lt;ax-node&gt; on disconnect. */
  _unregisterNode(node) {
    this.#nodes.delete(node);
    if (this.#selection.delete(node)) this.#emitSelectionChange();
    this.#scheduleEdgeRender();
  }

  #syncSelectedAttr() {
    for (const n of this.#nodes) {
      const want = this.#selection.has(n);
      if (n.selected !== want) n.selected = want;
    }
  }

  #emitSelectionChange() {
    const ids = this.selectedIds();
    this.dispatchEvent(new CustomEvent('selection-change', {
      detail: { ids, count: ids.length },
      bubbles: true, composed: true,
    }));
    if (ids.length === 0) this.#announce('Selection cleared');
    else if (ids.length === 1) this.#announce(`${ids[0]} selected`);
    else this.#announce(`${ids.length} nodes selected`);
  }

  // ── Edge & handle registry ────────────────────────────────────────────

  /** Set of registered `<ax-edge>` elements (read-only). */
  get edges() { return new Set(this.#edges); }

  /** Lattice-internal: called by `<ax-edge>` on connect. */
  _registerEdge(edge) {
    this.#edges.add(edge);
    this.#scheduleEdgeRender();
  }
  /** Lattice-internal: called by `<ax-edge>` on disconnect. */
  _unregisterEdge(edge) {
    this.#edges.delete(edge);
    this.#selectedEdges.delete(edge);
    this.#scheduleEdgeRender();
  }
  /** Lattice-internal: an `<ax-edge>` attribute changed. */
  _edgesChanged() {
    this.#scheduleEdgeRender();
  }
  /** Lattice-internal: called by `<ax-handle>` on connect/disconnect. */
  _registerHandle(handle) {
    this.#handles.add(handle);
    this.#scheduleEdgeRender();
  }
  _unregisterHandle(handle) {
    this.#handles.delete(handle);
  }

  /**
   * Add an edge programmatically. Creates an `<ax-edge>` child.
   * @param {{from: string, to: string, label?: string}} spec
   * @returns {HTMLElement} the created `<ax-edge>`
   */
  addEdge(spec) {
    const e = document.createElement('ax-edge');
    e.setAttribute('from', spec.from);
    e.setAttribute('to', spec.to);
    if (spec.label) e.setAttribute('label', spec.label);
    this.appendChild(e);
    if (spec.recordHistory !== false) {
      this.#history.push({
        label: 'add edge',
        undo: () => e.remove(),
        redo: () => this.appendChild(e),
      });
    }
    return e;
  }

  /** Selected edge ids, in registry order. */
  selectedEdgeIds() {
    const out = [];
    for (const e of this.#edges) if (this.#selectedEdges.has(e)) out.push(e.id);
    return out;
  }

  // ── Edge resolution & rendering ───────────────────────────────────────

  /** Find a registered node element by id. */
  #nodeById(id) {
    for (const n of this.#nodes) if (n.id === id) return n;
    return null;
  }

  /** Find a registered handle by `nodeId` + `handleId`. */
  #handleByRef(nodeId, handleId) {
    for (const h of this.#handles) {
      if (h.node && h.node.id === nodeId && h.handleId === handleId) return h;
    }
    return null;
  }

  /**
   * Resolve an endpoint reference (`"nodeId"` or `"nodeId:handleId"`) to a
   * lattice-space anchor, given the *other* endpoint for auto-side picking.
   * @returns {{x:number,y:number,side:string}|null}
   */
  #resolveAnchor(ref, towardPoint) {
    const i = ref.indexOf(':');
    const nodeId = i < 0 ? ref : ref.slice(0, i);
    const handleId = i < 0 ? null : ref.slice(i + 1);
    if (handleId) {
      const h = this.#handleByRef(nodeId, handleId);
      if (h) return h.anchor();
      // fall through to node-level if the handle isn't found
    }
    const node = this.#nodeById(nodeId);
    if (!node || typeof node.getBox !== 'function') return null;
    const box = node.getBox();
    if (towardPoint) return autoAnchor(box, towardPoint);
    return autoAnchor(box, { x: box.x + box.width + 100, y: box.y });
  }

  #scheduleEdgeRender() {
    if (this.#rafEdges) return;
    this.#rafEdges = requestAnimationFrame(() => {
      this.#rafEdges = 0;
      this.#renderEdges();
    });
  }

  /** Rebuild every edge `<path>` in the shared SVG layer. */
  #renderEdges() {
    const g = this.#edgePathsEl;
    if (!g) return;
    g.textContent = '';
    for (const edge of this.#edges) {
      const from = edge.getAttribute('from') || '';
      const to = edge.getAttribute('to') || '';
      if (!from || !to) continue;
      // Resolve each anchor toward the other endpoint's node center.
      const toNode = this.#nodeById(to.split(':')[0]);
      const fromNode = this.#nodeById(from.split(':')[0]);
      if (!fromNode || !toNode) continue;
      const toBox = toNode.getBox();
      const fromBox = fromNode.getBox();
      const src = this.#resolveAnchor(from, {
        x: toBox.x + toBox.width / 2, y: toBox.y + toBox.height / 2,
      });
      const tgt = this.#resolveAnchor(to, {
        x: fromBox.x + fromBox.width / 2, y: fromBox.y + fromBox.height / 2,
      });
      if (!src || !tgt) continue;
      const d = bezierPath(src, tgt);
      const selected = this.#selectedEdges.has(edge);

      // Fat invisible hit path (for click selection)
      const hit = document.createElementNS('http://www.w3.org/2000/svg', 'path');
      hit.setAttribute('class', 'ax-edge-hit');
      hit.setAttribute('d', d);
      hit.addEventListener('pointerdown', (ev) => {
        ev.stopPropagation();
        // Prevent the browser's default focus-on-mousedown, which (for an SVG
        // element inside shadow DOM) clears focus to <body> and would steal it
        // back from the host. #selectEdge focuses the host explicitly.
        ev.preventDefault();
        this.#selectEdge(edge, ev.shiftKey || ev.metaKey || ev.ctrlKey);
      });
      g.appendChild(hit);

      // Visible curve
      const active = edge.hasAttribute('active');
      const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
      path.setAttribute(
        'class',
        'ax-edge-path' + (selected ? ' selected' : '') + (active ? ' active' : ''),
      );
      path.setAttribute('d', d);
      path.setAttribute('marker-end', `url(#${selected ? 'ax-arrow-sel' : 'ax-arrow'})`);
      g.appendChild(path);

      // Optional label at the midpoint
      const label = edge.getAttribute('label');
      if (label) {
        const mid = bezierPoint(src, tgt, 0.5);
        const text = document.createElementNS('http://www.w3.org/2000/svg', 'text');
        text.setAttribute('class', 'ax-edge-label');
        text.setAttribute('x', String(mid.x));
        text.setAttribute('y', String(mid.y));
        text.textContent = label;
        g.appendChild(text);
      }
    }
  }

  #selectEdge(edge, additive) {
    // Focus the host so keyboard (Delete/Esc) works after an edge click —
    // SVG elements in the shadow DOM don't reliably focus the host on click.
    this.focus({ preventScroll: true });
    if (additive) {
      if (this.#selectedEdges.has(edge)) this.#selectedEdges.delete(edge);
      else this.#selectedEdges.add(edge);
    } else {
      this.#selectedEdges.clear();
      this.#selectedEdges.add(edge);
      // Selecting an edge clears node selection for clarity.
      if (this.#selection.size) { selClear(this.#selection); this.#syncSelectedAttr(); this.#emitSelectionChange(); }
    }
    for (const e of this.#edges) e.selected = this.#selectedEdges.has(e);
    this.#renderEdges();
    this.dispatchEvent(new CustomEvent('edge-selection-change', {
      detail: { ids: this.selectedEdgeIds() }, bubbles: true, composed: true,
    }));
  }

  /** Delete the currently selected edges (fires cancellable request). */
  deleteSelectedEdges() {
    if (this.#selectedEdges.size === 0) return;
    const ids = this.selectedEdgeIds();
    const ev = new CustomEvent('edges-delete-request', {
      detail: { ids }, bubbles: true, composed: true, cancelable: true,
    });
    if (!this.dispatchEvent(ev)) return;
    const removed = [...this.#selectedEdges];
    for (const e of removed) e.parentNode?.removeChild(e);
    this.#selectedEdges.clear();
    this.#renderEdges();
    this.dispatchEvent(new CustomEvent('edges-deleted', {
      detail: { ids }, bubbles: true, composed: true,
    }));
    this.#announce(`${ids.length} edge${ids.length === 1 ? '' : 's'} deleted`);
    this.#history.push({
      label: 'delete edges',
      undo: () => { for (const e of removed) this.appendChild(e); },
      redo: () => { for (const e of removed) e.remove(); },
    });
  }

  // ── Undo / redo ───────────────────────────────────────────────────────

  /** True iff there is a command to undo. */
  canUndo() { return this.#history.canUndo(); }
  /** True iff there is a command to redo. */
  canRedo() { return this.#history.canRedo(); }

  /** Undo the most recent operation (move / add / delete / paste). */
  undo() {
    if (this.#history.undo()) {
      this.#scheduleEdgeRender();
      this.#announce('Undo');
    }
  }
  /** Redo the most recently undone operation. */
  redo() {
    if (this.#history.redo()) {
      this.#scheduleEdgeRender();
      this.#announce('Redo');
    }
  }
  /** Drop all undo/redo history. */
  clearHistory() { this.#history.clear(); }

  // ── Accessibility ─────────────────────────────────────────────────────

  /** Announce a message to assistive technology via the live region. */
  announce(message) { this.#announce(String(message)); }

  #announce(message) {
    if (!this.#srEl) return;
    // Clear first so an identical consecutive message still re-announces.
    this.#srEl.textContent = '';
    requestAnimationFrame(() => { this.#srEl.textContent = message; });
  }

  // ── Copy / paste ──────────────────────────────────────────────────────

  /**
   * Copy the current node selection (and any edges fully enclosed by it) to
   * the lattice's internal clipboard. Returns the number of nodes copied.
   */
  copy() {
    if (this.#selection.size === 0) return 0;
    const ids = new Set();
    const nodes = [];
    for (const n of this.#selection) {
      nodes.push(n.cloneNode(true));
      ids.add(n.id);
    }
    const edges = [];
    for (const e of this.#edges) {
      const fromNode = (e.getAttribute('from') || '').split(':')[0];
      const toNode = (e.getAttribute('to') || '').split(':')[0];
      if (ids.has(fromNode) && ids.has(toNode)) {
        edges.push({
          from: e.getAttribute('from'),
          to: e.getAttribute('to'),
          label: e.getAttribute('label') || '',
        });
      }
    }
    this.#clipboard = { nodes, edges };
    return nodes.length;
  }

  /**
   * Paste the clipboard contents — fresh ids, offset by `offset` lattice
   * units, edges remapped to the new ids. The pasted nodes become the new
   * selection. Undoable. Returns the created `<ax-node>` elements.
   *
   * @param {number} [offset]
   * @returns {HTMLElement[]}
   */
  paste(offset = 28) {
    if (!this.#clipboard) return [];
    const idMap = new Map();
    const createdNodes = [];
    const createdEdges = [];

    for (const clone of this.#clipboard.nodes) {
      const fresh = /** @type {HTMLElement} */ (clone.cloneNode(true));
      const oldId = fresh.id || 'node';
      const newId = `${oldId}-copy-${++this.#pasteCounter}`;
      idMap.set(oldId, newId);
      fresh.id = newId;
      fresh.removeAttribute('selected');
      fresh.setAttribute('data-x',
        String((parseFloat(fresh.getAttribute('data-x')) || 0) + offset));
      fresh.setAttribute('data-y',
        String((parseFloat(fresh.getAttribute('data-y')) || 0) + offset));
      this.appendChild(fresh);
      createdNodes.push(fresh);
    }

    const remap = (ref) => {
      const i = ref.indexOf(':');
      const node = i < 0 ? ref : ref.slice(0, i);
      const rest = i < 0 ? '' : ref.slice(i);
      return (idMap.get(node) || node) + rest;
    };
    for (const spec of this.#clipboard.edges) {
      const e = document.createElement('ax-edge');
      e.setAttribute('from', remap(spec.from));
      e.setAttribute('to', remap(spec.to));
      if (spec.label) e.setAttribute('label', spec.label);
      this.appendChild(e);
      createdEdges.push(e);
    }

    // Select the pasted nodes.
    selClear(this.#selection);
    for (const n of createdNodes) selAdd(this.#selection, n);
    this.#syncSelectedAttr();
    this.#emitSelectionChange();

    this.#history.push({
      label: 'paste',
      undo: () => {
        for (const el of [...createdNodes, ...createdEdges]) el.remove();
      },
      redo: () => {
        for (const el of createdNodes) this.appendChild(el);
        for (const el of createdEdges) this.appendChild(el);
      },
    });
    return createdNodes;
  }

  // ── Connection drag ───────────────────────────────────────────────────

  #onHandleConnectStart = (ev) => {
    const { handle, pointerId } = ev.detail || {};
    if (!handle) return;
    const srcAnchor = handle.anchor();
    if (!srcAnchor) return;
    this.#connect = {
      sourceHandle: handle,
      sourceNodeId: handle.node?.id || '',
      pointerId,
      targetHandle: null,
    };
    // Listen for the rest of the gesture at the document level so the drag
    // works even if the pointer leaves the handle.
    window.addEventListener('pointermove', this.#onConnectMove, true);
    window.addEventListener('pointerup', this.#onConnectEnd, true);
    window.addEventListener('pointercancel', this.#onConnectEnd, true);
  };

  #onConnectMove = (ev) => {
    if (!this.#connect) return;
    const hostRect = this.getBoundingClientRect();
    const screenPt = { x: ev.clientX - hostRect.left, y: ev.clientY - hostRect.top };
    const latticePt = stlMath(this.#viewport, screenPt);

    // Find the nearest valid target handle within a snap radius.
    const src = this.#connect.sourceHandle;
    let best = null;
    let bestDist = Infinity;
    for (const h of this.#handles) {
      if (h.type !== 'target') continue;
      if (h.node && h.node.id === this.#connect.sourceNodeId) continue;
      const hr = h.getBoundingClientRect();
      const hc = {
        x: hr.left + hr.width / 2 - hostRect.left,
        y: hr.top + hr.height / 2 - hostRect.top,
      };
      const d = geoDistance(screenPt, hc);
      if (d < 28 && d < bestDist) { best = h; bestDist = d; }
    }
    // Update highlight
    if (this.#connect.targetHandle && this.#connect.targetHandle !== best) {
      this.#connect.targetHandle._setConnectTarget(false);
    }
    this.#connect.targetHandle = best;
    if (best) best._setConnectTarget(true);

    // Draw the preview path: from source anchor to either the snapped target
    // or the free pointer position.
    const srcAnchor = src.anchor();
    let endAnchor;
    if (best) {
      endAnchor = best.anchor();
    } else {
      endAnchor = { x: latticePt.x, y: latticePt.y, side: 'left' };
    }
    if (srcAnchor && endAnchor) {
      this.#edgePreviewEl.textContent = '';
      const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
      p.setAttribute('class', 'ax-edge-preview');
      p.setAttribute('d', bezierPath(srcAnchor, endAnchor));
      this.#edgePreviewEl.appendChild(p);
    }
  };

  #onConnectEnd = () => {
    if (!this.#connect) return;
    window.removeEventListener('pointermove', this.#onConnectMove, true);
    window.removeEventListener('pointerup', this.#onConnectEnd, true);
    window.removeEventListener('pointercancel', this.#onConnectEnd, true);
    this.#edgePreviewEl.textContent = '';

    const c = this.#connect;
    this.#connect = null;
    if (c.targetHandle) c.targetHandle._setConnectTarget(false);

    if (c.targetHandle) {
      const fromRef = c.sourceHandle.ref;
      const toRef = c.targetHandle.ref;
      const ev = new CustomEvent('edge-connect', {
        detail: { from: fromRef, to: toRef },
        bubbles: true, composed: true, cancelable: true,
      });
      // If the consumer doesn't cancel, create the edge ourselves.
      if (this.dispatchEvent(ev)) {
        this.addEdge({ from: fromRef, to: toRef });
        this.#announce(`Connected ${fromRef} to ${toRef}`);
      }
    }
  };

  // ── Internal ───────────────────────────────────────────────────────────

  #readAttributes() {
    this.#minZoom = this.#num(ATTR.MIN_ZOOM, 0.2);
    this.#maxZoom = this.#num(ATTR.MAX_ZOOM, 3);
    this.#snap = this.#num(ATTR.SNAP, 0);
    this.#background = /** @type {any} */ (this.getAttribute(ATTR.BACKGROUND) || 'dots');
    this.#fitOnInit = this.hasAttribute(ATTR.FIT_ON_INIT);
    this.#virtualize = this.hasAttribute(ATTR.VIRTUALIZE);
    this.#viewport = {
      x: this.#num(ATTR.PAN_X, 0),
      y: this.#num(ATTR.PAN_Y, 0),
      k: clamp(this.#num(ATTR.ZOOM, 1), this.#minZoom, this.#maxZoom),
    };
  }

  #num(attr, dflt) {
    const v = this.getAttribute(attr);
    if (v == null || v === '') return dflt;
    const n = parseFloat(v);
    return Number.isFinite(n) ? n : dflt;
  }

  #applyBackground() {
    if (this.#background === 'none') {
      this.#bgFill.setAttribute('fill', 'transparent');
    } else {
      const id = this.#background === 'grid' ? 'ax-grid' : 'ax-dots';
      this.#bgFill.setAttribute('fill', `url(#${id})`);
    }
  }

  #applyTransform() {
    const { x, y, k } = this.#viewport;
    this.#vpEl.style.transform = `translate(${x}px, ${y}px) scale(${k})`;
    // Background pattern is rendered in screen space but should appear to pan/zoom
    // with the canvas. We achieve that by shifting/scaling the pattern coordinates.
    const defs = this.#bgSvg.querySelectorAll('pattern');
    const size = 20 * k;
    const ox = ((x % size) + size) % size;
    const oy = ((y % size) + size) % size;
    defs.forEach((p) => {
      p.setAttribute('width', String(size));
      p.setAttribute('height', String(size));
      p.setAttribute('x', String(ox));
      p.setAttribute('y', String(oy));
      // also scale inner content if needed
      const c = p.querySelector('circle');
      if (c) {
        c.setAttribute('r', String(Math.max(0.5, k)));
        c.setAttribute('cx', String(k));
        c.setAttribute('cy', String(k));
      }
      const path = p.querySelector('path');
      if (path) {
        path.setAttribute('d', `M ${size} 0 L 0 0 L 0 ${size}`);
        path.setAttribute('stroke-width', String(Math.max(0.5, k * 0.5)));
      }
    });
  }

  #setViewportInternal(next, emit) {
    const prev = this.#viewport;
    if (next.x === prev.x && next.y === prev.y && next.k === prev.k) return;
    this.#viewport = next;
    this.#applyTransform();
    // Mirror to attributes (non-emit; observed callbacks won't fight us because
    // we only re-set if values differ).
    this.#syncAttr(ATTR.ZOOM, next.k);
    this.#syncAttr(ATTR.PAN_X, next.x);
    this.#syncAttr(ATTR.PAN_Y, next.y);
    if (emit) this.#scheduleEmit();
  }

  #syncAttr(name, value) {
    const cur = this.getAttribute(name);
    const want = String(value);
    if (cur !== want) this.setAttribute(name, want);
  }

  #scheduleEmit() {
    if (this.#rafEmit) return;
    this.#rafEmit = requestAnimationFrame(() => {
      this.#rafEmit = 0;
      if (this.#virtualize) this.#cullNodes();
      this.dispatchEvent(
        new CustomEvent('viewport-change', {
          detail: { ...this.#viewport },
          bubbles: true,
          composed: true,
        }),
      );
    });
  }

  // ── Virtualization ────────────────────────────────────────────────────

  /** Stats: total / visible / culled node counts. */
  get virtualization() {
    return {
      enabled: this.#virtualize,
      total: this.#nodes.size,
      culled: this.#culledCount,
      visible: this.#nodes.size - this.#culledCount,
    };
  }

  #uncullAll() {
    for (const n of this.#nodes) n.classList.remove('ax-culled');
    this.#culledCount = 0;
  }

  /**
   * Hide nodes whose box lies outside the visible viewport (expanded by a
   * half-viewport margin so nodes don't pop in at the edges). Culled nodes
   * keep layout — only painting is skipped — so edges still route correctly.
   */
  #cullNodes() {
    if (!this.#virtualize) return;
    const rect = this.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    const tl = this.screenToLattice({ x: 0, y: 0 });
    const br = this.screenToLattice({ x: rect.width, y: rect.height });
    const mx = (br.x - tl.x) * 0.5;
    const my = (br.y - tl.y) * 0.5;
    const view = {
      left: tl.x - mx, top: tl.y - my,
      right: br.x + mx, bottom: br.y + my,
    };
    let culled = 0;
    for (const n of this.#nodes) {
      const b = typeof n.getBox === 'function' ? n.getBox() : null;
      if (!b) continue;
      const outside =
        b.x > view.right || b.x + b.width < view.left ||
        b.y > view.bottom || b.y + b.height < view.top;
      if (outside) { n.classList.add('ax-culled'); culled++; }
      else n.classList.remove('ax-culled');
    }
    this.#culledCount = culled;
  }

  // ── Auto-layout ───────────────────────────────────────────────────────

  /**
   * Arrange all nodes with a layered DAG layout, fit the view, and record
   * the move as a single undoable command.
   *
   * @param {{direction?: "LR"|"TB", gapMain?: number, gapCross?: number}} [options]
   */
  autoLayout(options = {}) {
    if (this.#nodes.size === 0) return;
    const layoutNodes = [];
    for (const n of this.#nodes) {
      const b = n.getBox();
      layoutNodes.push({ id: n.id, width: b.width, height: b.height });
    }
    const layoutEdges = [];
    for (const e of this.#edges) {
      layoutEdges.push({
        from: e.getAttribute('from') || '',
        to: e.getAttribute('to') || '',
      });
    }
    const pos = layeredLayout(layoutNodes, layoutEdges, options);

    const before = new Map();
    const after = new Map();
    for (const n of this.#nodes) {
      before.set(n, { x: n.x, y: n.y });
      const p = pos.get(n.id);
      after.set(n, p
        ? { x: this.snap(p.x), y: this.snap(p.y) }
        : { x: n.x, y: n.y });
    }
    const apply = (positions) => {
      for (const [n, p] of positions) {
        n.setAttribute('data-x', String(p.x));
        n.setAttribute('data-y', String(p.y));
      }
      this.#scheduleEdgeRender();
    };
    apply(after);
    this.#history.push({
      label: 'auto-layout',
      undo: () => apply(before),
      redo: () => apply(after),
    });
    this.fitView();
    this.#announce(`Auto-layout applied to ${this.#nodes.size} nodes`);
  }

  // ── Execution state ───────────────────────────────────────────────────

  /**
   * Set a node's execution status by id.
   * @param {string} id
   * @param {"idle"|"pending"|"running"|"success"|"error"} status
   */
  setNodeStatus(id, status) {
    const n = this.#nodeById(id);
    if (n) n.status = status;
  }

  /**
   * Mark an edge active/inactive (animated flowing curve) by edge id, or by
   * a `{from,to}` node-id pair.
   * @param {string|{from:string,to:string}} ref
   * @param {boolean} active
   */
  setEdgeActive(ref, active) {
    for (const e of this.#edges) {
      let match;
      if (typeof ref === 'string') {
        match = e.id === ref;
      } else {
        const f = (e.getAttribute('from') || '').split(':')[0];
        const t = (e.getAttribute('to') || '').split(':')[0];
        match = f === ref.from && t === ref.to;
      }
      if (match) {
        if (active) e.setAttribute('active', '');
        else e.removeAttribute('active');
      }
    }
  }

  /** Reset every node to `idle` and clear all active edges. */
  resetStatuses() {
    for (const n of this.#nodes) n.status = 'idle';
    for (const e of this.#edges) e.removeAttribute('active');
  }

  // ── Input handling ─────────────────────────────────────────────────────

  #bindEvents() {
    this.#eaterEl.addEventListener('pointerdown', this.#onPointerDown);
    this.#eaterEl.addEventListener('pointermove', this.#onPointerMove);
    this.#eaterEl.addEventListener('pointerup', this.#onPointerEnd);
    this.#eaterEl.addEventListener('pointercancel', this.#onPointerEnd);
    this.#eaterEl.addEventListener('wheel', this.#onWheel, { passive: false });
    this.#eaterEl.addEventListener('dblclick', this.#onDblClick);
    this.addEventListener('keydown', this.#onKeyDown);
    // Bubbling node events from descendant <ax-node>s
    this.addEventListener('node-select', this.#onNodeSelect);
    this.addEventListener('node-click', this.#onNodeClick);
    this.addEventListener('node-movestart', this.#onNodeMoveStart);
    this.addEventListener('node-moving', this.#onNodeMoving);
    this.addEventListener('node-moveend', this.#onNodeMoveEnd);
    // Connection drag from a handle
    this.addEventListener('handle-connect-start', this.#onHandleConnectStart);
    // Make the host focusable so it can receive keys without click.
    if (!this.hasAttribute('tabindex')) this.setAttribute('tabindex', '0');
  }

  #unbindEvents() {
    this.#eaterEl.removeEventListener('pointerdown', this.#onPointerDown);
    this.#eaterEl.removeEventListener('pointermove', this.#onPointerMove);
    this.#eaterEl.removeEventListener('pointerup', this.#onPointerEnd);
    this.#eaterEl.removeEventListener('pointercancel', this.#onPointerEnd);
    this.#eaterEl.removeEventListener('wheel', this.#onWheel);
    this.#eaterEl.removeEventListener('dblclick', this.#onDblClick);
    this.removeEventListener('keydown', this.#onKeyDown);
    this.removeEventListener('node-select', this.#onNodeSelect);
    this.removeEventListener('node-click', this.#onNodeClick);
    this.removeEventListener('node-movestart', this.#onNodeMoveStart);
    this.removeEventListener('node-moving', this.#onNodeMoving);
    this.removeEventListener('node-moveend', this.#onNodeMoveEnd);
    this.removeEventListener('handle-connect-start', this.#onHandleConnectStart);
  }

  // ── Node event handlers ───────────────────────────────────────────────

  #onNodeSelect = (ev) => {
    const node = /** @type {HTMLElement} */ (ev.target);
    const { additive } = ev.detail || {};
    const before = new Set(this.#selection);
    this.#pendingCollapse = null;

    if (additive) {
      // Shift / Cmd-click — toggle this node immediately.
      selToggle(this.#selection, node);
    } else if (this.#selection.has(node)) {
      // Plain click on an already-selected node. DON'T collapse the
      // selection yet — the user may be starting a group drag. We collapse
      // to just this node on pointer-up *only if* no drag happened
      // (see #onNodeClick). Selection is unchanged for now.
      this.#pendingCollapse = node;
    } else {
      // Plain click on an unselected node — select only it.
      selReplace(this.#selection, node);
    }
    this.#syncSelectedAttr();
    if (!selEquals(before, this.#selection)) this.#emitSelectionChange();
  };

  #onNodeClick = (ev) => {
    // Fired on pointer-up when the node did NOT drag.
    const node = /** @type {HTMLElement} */ (ev.target);
    if (this.#pendingCollapse === node) {
      // The deferred collapse: a plain click on a node that was part of a
      // multi-selection now narrows the selection to just that node.
      const before = new Set(this.#selection);
      selReplace(this.#selection, node);
      this.#syncSelectedAttr();
      if (!selEquals(before, this.#selection)) this.#emitSelectionChange();
    }
    this.#pendingCollapse = null;
  };

  #onNodeMoveStart = (ev) => {
    const node = /** @type {HTMLElement} */ (ev.target);
    // Make sure the dragged node is in the selection (selection happens just
    // before this in #onPointerDown via node-select, so it usually is).
    if (!this.#selection.has(node)) selReplace(this.#selection, node);
    this.#syncSelectedAttr();

    // Snapshot start positions of every moving node — the dragged one plus
    // the rest of the selection. The dragged node moves itself; the others
    // follow in lockstep on node-moving. The whole snapshot also feeds the
    // undoable move command on node-moveend.
    this.#moveDraggedNode = node;
    this.#moveSnapshot = new Map();
    this.#moveSnapshot.set(node, {
      x: ev.detail?.x ?? node.x, y: ev.detail?.y ?? node.y,
    });
    for (const n of this.#selection) {
      if (n === node) continue;
      this.#moveSnapshot.set(n, { x: n.x, y: n.y });
    }
  };

  #onNodeMoving = (ev) => {
    if (this.#moveSnapshot) {
      const { dx, dy } = ev.detail || {};
      if (typeof dx === 'number' && typeof dy === 'number') {
        for (const [n, start] of this.#moveSnapshot) {
          if (n === this.#moveDraggedNode) continue; // moves itself
          n.setAttribute('data-x', String(this.snap(start.x + dx)));
          n.setAttribute('data-y', String(this.snap(start.y + dy)));
        }
      }
    }
    // Edges connected to moving nodes must follow.
    this.#scheduleEdgeRender();
  };

  #onNodeMoveEnd = () => {
    // A drag happened — keep the group selection intact (cancel any pending
    // collapse-to-single from the pointerdown).
    this.#pendingCollapse = null;
    this.#scheduleEdgeRender();

    // Record an undoable move command if anything actually moved.
    if (this.#moveSnapshot) {
      const before = this.#moveSnapshot;
      const after = new Map();
      let moved = false;
      for (const [n, start] of before) {
        after.set(n, { x: n.x, y: n.y });
        if (n.x !== start.x || n.y !== start.y) moved = true;
      }
      if (moved) {
        const apply = (positions) => {
          for (const [n, p] of positions) {
            n.setAttribute('data-x', String(p.x));
            n.setAttribute('data-y', String(p.y));
          }
          this.#scheduleEdgeRender();
        };
        this.#history.push({
          label: 'move',
          undo: () => apply(before),
          redo: () => apply(after),
        });
      }
    }
    this.#moveSnapshot = null;
    this.#moveDraggedNode = null;
  };

  /** Local screen-coords from a pointer event. */
  #local(ev) {
    const rect = this.getBoundingClientRect();
    return { x: ev.clientX - rect.left, y: ev.clientY - rect.top };
  }

  #onPointerDown = (ev) => {
    this.#eaterEl.setPointerCapture(ev.pointerId);
    this.#pointers.set(ev.pointerId, this.#local(ev));
    this.focus({ preventScroll: true });

    if (this.#pointers.size === 1) {
      const additive = ev.shiftKey || ev.metaKey || ev.ctrlKey;
      const start = this.#local(ev);
      if (additive) {
        // Shift/Cmd+drag on empty canvas → box-select
        this.#marquee = {
          start,
          end: start,
          additive: ev.metaKey || ev.ctrlKey, // shift = replace; cmd = add
          before: new Set(this.#selection),
          movedEnough: false,
        };
        this.#eaterEl.classList.add('marquee-active');
        this.#marqueeEl.classList.add('active');
        this.#applyMarquee();
      } else {
        // Plain drag on empty canvas → pan
        this.#panStart = { ...start, vp: { ...this.#viewport }, moved: false };
        this.#eaterEl.classList.add('dragging');
      }
    } else if (this.#pointers.size === 2) {
      this.#panStart = null;
      this.#marquee = null;
      this.#marqueeEl.classList.remove('active');
      const pts = Array.from(this.#pointers.values());
      this.#pinch = {
        startDist: dist(pts[0], pts[1]),
        startMid: mid(pts[0], pts[1]),
        startVp: { ...this.#viewport },
      };
    }
  };

  #onPointerMove = (ev) => {
    if (!this.#pointers.has(ev.pointerId)) return;
    this.#pointers.set(ev.pointerId, this.#local(ev));

    if (this.#pinch && this.#pointers.size === 2) {
      const pts = Array.from(this.#pointers.values());
      const d = dist(pts[0], pts[1]) || 1;
      const m = mid(pts[0], pts[1]);
      const targetK = this.#pinch.startVp.k * (d / this.#pinch.startDist);
      const next = zoomAt(this.#pinch.startVp, targetK, this.#pinch.startMid, {
        minZoom: this.#minZoom,
        maxZoom: this.#maxZoom,
      });
      const dx = m.x - this.#pinch.startMid.x;
      const dy = m.y - this.#pinch.startMid.y;
      this.#setViewportInternal(panMath(next, dx, dy), true);
    } else if (this.#marquee) {
      const p = this.#local(ev);
      this.#marquee.end = p;
      const r = rectFromPoints(this.#marquee.start, this.#marquee.end);
      if (r.width > 3 || r.height > 3) this.#marquee.movedEnough = true;
      this.#applyMarquee();
      // Live hit-test as the user drags — gives instant feedback.
      this.#commitMarqueeSelection(/* live */ true);
    } else if (this.#panStart) {
      const p = this.#local(ev);
      this.#panStart.moved =
        this.#panStart.moved ||
        Math.abs(p.x - this.#panStart.x) > 2 ||
        Math.abs(p.y - this.#panStart.y) > 2;
      this.#setViewportInternal(
        {
          x: this.#panStart.vp.x + (p.x - this.#panStart.x),
          y: this.#panStart.vp.y + (p.y - this.#panStart.y),
          k: this.#panStart.vp.k,
        },
        true,
      );
    }
  };

  #onPointerEnd = (ev) => {
    if (this.#eaterEl.hasPointerCapture(ev.pointerId)) {
      this.#eaterEl.releasePointerCapture(ev.pointerId);
    }
    this.#pointers.delete(ev.pointerId);
    if (this.#pointers.size < 2) this.#pinch = null;

    if (this.#marquee) {
      // Finalize box-select; if the user didn't actually drag, treat as a
      // click on empty canvas (deselect all).
      if (!this.#marquee.movedEnough) {
        this.deselectAll();
      } else {
        this.#commitMarqueeSelection(/* live */ false);
      }
      this.#marquee = null;
      this.#marqueeEl.classList.remove('active');
      this.#marqueeEl.style.width = '0px';
      this.#marqueeEl.style.height = '0px';
      this.#eaterEl.classList.remove('marquee-active');
    } else if (this.#panStart) {
      const wasClick = !this.#panStart.moved && this.#pointers.size === 0;
      if (wasClick) {
        // Click on empty canvas deselects.
        this.deselectAll();
      }
      this.#panStart = null;
    }
    if (this.#pointers.size === 0) {
      this.#eaterEl.classList.remove('dragging');
    }
  };

  #applyMarquee() {
    if (!this.#marquee) return;
    const r = rectFromPoints(this.#marquee.start, this.#marquee.end);
    const m = this.#marqueeEl;
    m.style.left = `${r.left}px`;
    m.style.top = `${r.top}px`;
    m.style.width = `${r.width}px`;
    m.style.height = `${r.height}px`;
  }

  /**
   * Hit-test the marquee against every registered node's getBoundingClientRect.
   * If `live` is true, the selection is recomputed against the snapshot of
   * what was selected before the marquee started (so dragging in & back out
   * leaves the prior selection intact). When false (release), commit final.
   */
  #commitMarqueeSelection(live) {
    if (!this.#marquee) return;
    const hostRect = this.getBoundingClientRect();
    const r = rectFromPoints(this.#marquee.start, this.#marquee.end);
    const screenRect = {
      left: hostRect.left + r.left,
      top: hostRect.top + r.top,
      right: hostRect.left + r.right,
      bottom: hostRect.top + r.bottom,
    };
    const hits = new Set();
    for (const n of this.#nodes) {
      const nb = n.getBoundingClientRect();
      if (rectIntersects(nb, screenRect)) hits.add(n);
    }
    const before = new Set(this.#selection);
    if (this.#marquee.additive) {
      // Cmd-drag: add hits to the prior set.
      const next = new Set(this.#marquee.before);
      for (const n of hits) next.add(n);
      this.#selection = next;
    } else {
      // Shift-drag (or plain when we route it here): replace with hits.
      this.#selection = hits;
    }
    this.#syncSelectedAttr();
    if (!selEquals(before, this.#selection)) this.#emitSelectionChange();
    if (live) {
      // During live drag, keep the same Set identity so subsequent updates work.
    }
  }

  #onWheel = (ev) => {
    // Pinch-zoom on macOS trackpad arrives as ctrlKey + wheel; normalize.
    const isZoom = ev.ctrlKey || ev.metaKey || Math.abs(ev.deltaY) > 0;
    if (!isZoom) return;
    ev.preventDefault();
    const pivot = this.#local(ev);
    // Trackpad ctrl-wheel has very small deltas; touch/wheel mouse has large.
    const sensitivity = ev.ctrlKey ? 100 : 250;
    const nextK = zoomFromWheel(this.#viewport.k, ev.deltaY, sensitivity);
    this.#setViewportInternal(
      zoomAt(this.#viewport, nextK, pivot, {
        minZoom: this.#minZoom,
        maxZoom: this.#maxZoom,
      }),
      true,
    );
  };

  #onDblClick = (ev) => {
    // dblclick on empty canvas = fit view
    if (ev.target === this.#eaterEl) {
      this.fitView();
    }
  };

  #onKeyDown = (ev) => {
    // Ignore keys when typing in an input/textarea inside slotted content.
    const t = /** @type {HTMLElement} */ (ev.composedPath()[0]);
    if (t && t.matches && t.matches('input,textarea,[contenteditable=""],[contenteditable="true"]')) {
      return;
    }

    const hasSel = this.#selection.size > 0;
    const hasEdgeSel = this.#selectedEdges.size > 0;
    const mod = ev.metaKey || ev.ctrlKey;

    // ── Select all ──
    if (mod && ev.key.toLowerCase() === 'a') {
      this.selectAll();
      ev.preventDefault();
      return;
    }

    // ── Undo / redo ──
    if (mod && ev.key.toLowerCase() === 'z') {
      if (ev.shiftKey) this.redo();
      else this.undo();
      ev.preventDefault();
      return;
    }
    if (mod && ev.key.toLowerCase() === 'y') {
      this.redo();
      ev.preventDefault();
      return;
    }

    // ── Copy / paste ──
    if (mod && ev.key.toLowerCase() === 'c' && hasSel) {
      this.copy();
      ev.preventDefault();
      return;
    }
    if (mod && ev.key.toLowerCase() === 'v') {
      this.paste();
      ev.preventDefault();
      return;
    }

    // ── Esc deselects (nodes and edges) ──
    if (ev.key === 'Escape' && (hasSel || hasEdgeSel)) {
      this.deselectAll();
      if (hasEdgeSel) {
        this.#selectedEdges.clear();
        for (const e of this.#edges) e.selected = false;
        this.#renderEdges();
      }
      ev.preventDefault();
      return;
    }

    // ── Delete selected (nodes and/or edges) ──
    if ((ev.key === 'Delete' || ev.key === 'Backspace') && (hasSel || hasEdgeSel)) {
      if (hasEdgeSel) this.deleteSelectedEdges();
      if (hasSel) this.deleteSelected();
      ev.preventDefault();
      return;
    }

    // ── Arrow keys ──
    // With selection → nudge selected nodes in lattice space.
    // Without → pan the viewport.
    if (ev.key.startsWith('Arrow')) {
      if (hasSel) {
        const step = ev.shiftKey ? 10 : 1;
        let dx = 0, dy = 0;
        if (ev.key === 'ArrowLeft') dx = -step;
        else if (ev.key === 'ArrowRight') dx = step;
        else if (ev.key === 'ArrowUp') dy = -step;
        else if (ev.key === 'ArrowDown') dy = step;
        for (const n of this.#selection) {
          n.setAttribute('data-x', String(this.snap(n.x + dx)));
          n.setAttribute('data-y', String(this.snap(n.y + dy)));
        }
        ev.preventDefault();
        return;
      }
      const step = ev.shiftKey ? 100 : 40;
      let dx = 0, dy = 0;
      if (ev.key === 'ArrowLeft') dx = step;
      else if (ev.key === 'ArrowRight') dx = -step;
      else if (ev.key === 'ArrowUp') dy = step;
      else if (ev.key === 'ArrowDown') dy = -step;
      this.#setViewportInternal(panMath(this.#viewport, dx, dy), true);
      ev.preventDefault();
      return;
    }

    // ── Tab / Shift+Tab cycle selection ──
    if (ev.key === 'Tab' && this.#nodes.size > 0) {
      const arr = [...this.#nodes];
      const current = [...this.#selection][0];
      let i = current ? arr.indexOf(current) : -1;
      i = (i + (ev.shiftKey ? -1 : 1) + arr.length) % arr.length;
      selReplace(this.#selection, arr[i]);
      this.#syncSelectedAttr();
      this.#emitSelectionChange();
      ev.preventDefault();
      return;
    }

    // ── Zoom keys (no modifiers) ──
    switch (ev.key) {
      case '+':
      case '=':
        this.zoomIn();
        ev.preventDefault();
        break;
      case '-':
      case '_':
        this.zoomOut();
        ev.preventDefault();
        break;
      case '0':
        this.fitView();
        ev.preventDefault();
        break;
    }
  };

  // ── Children helpers ──────────────────────────────────────────────────

  /**
   * Find slotted descendants that declare lattice coordinates via data-* attrs.
   * Phase A: any element with `data-x` and `data-y` is considered.
   * @returns {Array<{x:number,y:number,width:number,height:number}>}
   */
  #latticeChildBounds() {
    const slot = this.#root.querySelector('slot');
    if (!slot) return [];
    /** @type {Element[]} */
    const assigned = slot.assignedElements({ flatten: true });
    const out = [];
    const walk = (n) => {
      if (n instanceof HTMLElement || n instanceof SVGElement) {
        const xAttr = n.getAttribute('data-x');
        const yAttr = n.getAttribute('data-y');
        if (xAttr != null && yAttr != null) {
          const x = parseFloat(xAttr);
          const y = parseFloat(yAttr);
          const w = parseFloat(n.getAttribute('data-w') || '160');
          const h = parseFloat(n.getAttribute('data-h') || '60');
          if (Number.isFinite(x) && Number.isFinite(y)) {
            out.push({ x, y, width: w, height: h });
          }
        }
        n.childNodes.forEach((c) => walk(c));
      }
    };
    assigned.forEach((n) => walk(n));
    return out;
  }
}

// ── small geometry helpers ──
function dist(a, b) {
  const dx = a.x - b.x, dy = a.y - b.y;
  return Math.sqrt(dx * dx + dy * dy);
}
function mid(a, b) {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

// Register the element exactly once.
if (!customElements.get('ax-lattice')) {
  customElements.define('ax-lattice', AxLatticeElement);
}
