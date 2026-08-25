/**
 * Viewport math for an infinite zoom-pan canvas.
 *
 * A viewport is a pure value: `{ x, y, k }` where `x`/`y` are translate offsets
 * (in screen pixels) and `k` is a uniform scale factor. The transform applied
 * to the canvas content is conceptually `translate(x, y) scale(k)`.
 *
 * All functions here are pure. The DOM-side element (`<ax-lattice>`) is the
 * sole owner of mutable state and consumes these utilities.
 *
 * @module @axocoatl/lattice/viewport
 */

/**
 * @typedef {Object} Viewport
 * @property {number} x  translate x in screen pixels
 * @property {number} y  translate y in screen pixels
 * @property {number} k  uniform scale (zoom level)
 */

/**
 * @typedef {Object} Point
 * @property {number} x
 * @property {number} y
 */

/** Clamp a number to a [min, max] range. */
export function clamp(n, min, max) {
  return n < min ? min : n > max ? max : n;
}

/** Snap a number to the nearest multiple of `step` (no-op when step <= 0). */
export function snap(n, step) {
  if (!step || step <= 0) return n;
  return Math.round(n / step) * step;
}

/**
 * Identity viewport (no pan, no zoom).
 * @returns {Viewport}
 */
export function identity() {
  return { x: 0, y: 0, k: 1 };
}

/**
 * Convert a screen-space point to lattice-space coordinates.
 * If the canvas has a CTM, callers should pre-subtract the canvas origin.
 *
 * @param {Viewport} vp
 * @param {Point} screenPt   point in canvas-local screen pixels
 * @returns {Point}          point in lattice coordinates
 */
export function screenToLattice(vp, screenPt) {
  return {
    x: (screenPt.x - vp.x) / vp.k,
    y: (screenPt.y - vp.y) / vp.k,
  };
}

/**
 * Convert a lattice-space point to canvas-local screen-space.
 *
 * @param {Viewport} vp
 * @param {Point} latticePt
 * @returns {Point}
 */
export function latticeToScreen(vp, latticePt) {
  return {
    x: latticePt.x * vp.k + vp.x,
    y: latticePt.y * vp.k + vp.y,
  };
}

/**
 * Zoom centered on a fixed screen-space point (the point under the user's
 * cursor stays under their cursor across the zoom). This is the canonical
 * "zoom toward pointer" feel.
 *
 * @param {Viewport} vp
 * @param {number} nextK       target zoom level (will be clamped by `bounds`)
 * @param {Point} pivot        screen-space anchor that must remain fixed
 * @param {{minZoom?: number, maxZoom?: number}} [bounds]
 * @returns {Viewport}
 */
export function zoomAt(vp, nextK, pivot, bounds = {}) {
  const min = bounds.minZoom ?? 0.01;
  const max = bounds.maxZoom ?? 100;
  const k2 = clamp(nextK, min, max);
  if (k2 === vp.k) return vp;
  // Solve for new (x, y) so the lattice point under `pivot` is unchanged.
  //   pivot.x = lattice_x * k2 + x2
  //   lattice_x = (pivot.x - vp.x) / vp.k
  //   => x2 = pivot.x - lattice_x * k2
  const lx = (pivot.x - vp.x) / vp.k;
  const ly = (pivot.y - vp.y) / vp.k;
  return {
    x: pivot.x - lx * k2,
    y: pivot.y - ly * k2,
    k: k2,
  };
}

/**
 * Translate (pan) the viewport by a screen-space delta.
 *
 * @param {Viewport} vp
 * @param {number} dx
 * @param {number} dy
 * @returns {Viewport}
 */
export function pan(vp, dx, dy) {
  return { x: vp.x + dx, y: vp.y + dy, k: vp.k };
}

/**
 * Set zoom centered on the geometric center of a viewport rectangle.
 *
 * @param {Viewport} vp
 * @param {number} nextK
 * @param {{width: number, height: number}} viewRect
 * @param {{minZoom?: number, maxZoom?: number}} [bounds]
 * @returns {Viewport}
 */
export function zoomCentered(vp, nextK, viewRect, bounds) {
  return zoomAt(vp, nextK, { x: viewRect.width / 2, y: viewRect.height / 2 }, bounds);
}

/**
 * Given an axis-aligned bounding box in lattice space and a viewport rect in
 * screen space, return the viewport that frames the box with `padding` pixels
 * of margin on all sides.
 *
 * @param {{x: number, y: number, width: number, height: number}} bbox     lattice-space bounds of content
 * @param {{width: number, height: number}} viewRect                       screen-space size of the canvas
 * @param {{padding?: number, minZoom?: number, maxZoom?: number}} [opts]
 * @returns {Viewport}
 */
export function fitView(bbox, viewRect, opts = {}) {
  const padding = opts.padding ?? 40;
  const w = Math.max(1, viewRect.width - 2 * padding);
  const h = Math.max(1, viewRect.height - 2 * padding);
  // Empty content: just center identity
  if (bbox.width <= 0 || bbox.height <= 0) {
    return {
      x: viewRect.width / 2,
      y: viewRect.height / 2,
      k: 1,
    };
  }
  const kx = w / bbox.width;
  const ky = h / bbox.height;
  const k = clamp(Math.min(kx, ky), opts.minZoom ?? 0.01, opts.maxZoom ?? 100);
  // Center the bbox in the view rect at scale k.
  const cx = bbox.x + bbox.width / 2;
  const cy = bbox.y + bbox.height / 2;
  return {
    x: viewRect.width / 2 - cx * k,
    y: viewRect.height / 2 - cy * k,
    k,
  };
}

/**
 * Compute the next zoom level for a wheel delta. Smaller |delta| = finer zoom.
 * Standard exponential: nextK = k * exp(-delta / scale).
 *
 * @param {number} currentK
 * @param {number} wheelDeltaY
 * @param {number} [sensitivity]  larger = finer (default 200)
 */
export function zoomFromWheel(currentK, wheelDeltaY, sensitivity = 200) {
  return currentK * Math.exp(-wheelDeltaY / sensitivity);
}

/**
 * Compute the lattice-space bounding box of a set of nodes.
 * Each node must have `{ x, y, width, height }` in lattice coords.
 *
 * @param {Array<{x: number, y: number, width?: number, height?: number}>} nodes
 * @returns {{x: number, y: number, width: number, height: number}}
 */
export function nodesBounds(nodes) {
  if (!nodes || nodes.length === 0) {
    return { x: 0, y: 0, width: 0, height: 0 };
  }
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const n of nodes) {
    const w = n.width ?? 0;
    const h = n.height ?? 0;
    if (n.x < minX) minX = n.x;
    if (n.y < minY) minY = n.y;
    if (n.x + w > maxX) maxX = n.x + w;
    if (n.y + h > maxY) maxY = n.y + h;
  }
  return { x: minX, y: minY, width: maxX - minX, height: maxY - minY };
}
