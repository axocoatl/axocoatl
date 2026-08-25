/**
 * Edge geometry — pure functions for routing bezier curves between handles.
 *
 * All coordinates are lattice-space. No DOM.
 *
 * @module @axocoatl/lattice/geometry
 */

/**
 * @typedef {"left"|"right"|"top"|"bottom"} Side
 */

/**
 * @typedef {Object} Anchor
 * @property {number} x
 * @property {number} y
 * @property {Side} side   the node edge this anchor sits on
 */

/**
 * Unit outward vector for a handle side — the direction a curve should
 * leave (source) or arrive along (target).
 * @param {Side} side
 * @returns {{x:number,y:number}}
 */
export function sideVector(side) {
  switch (side) {
    case 'left': return { x: -1, y: 0 };
    case 'right': return { x: 1, y: 0 };
    case 'top': return { x: 0, y: -1 };
    case 'bottom': return { x: 0, y: 1 };
    default: return { x: 1, y: 0 };
  }
}

/**
 * The opposite side — useful for auto-picking a target side.
 * @param {Side} side
 * @returns {Side}
 */
export function oppositeSide(side) {
  return ({ left: 'right', right: 'left', top: 'bottom', bottom: 'top' })[side] || 'left';
}

/**
 * Choose the most natural side of a node box to anchor to, given the
 * direction toward the other endpoint. Used when an edge references a node
 * without a specific handle.
 *
 * @param {{x:number,y:number,width:number,height:number}} box
 * @param {{x:number,y:number}} toward     the other endpoint, lattice space
 * @returns {Anchor}
 */
export function autoAnchor(box, toward) {
  const cx = box.x + box.width / 2;
  const cy = box.y + box.height / 2;
  const dx = toward.x - cx;
  const dy = toward.y - cy;
  // Pick the dominant axis.
  if (Math.abs(dx) >= Math.abs(dy)) {
    return dx >= 0
      ? { x: box.x + box.width, y: cy, side: 'right' }
      : { x: box.x, y: cy, side: 'left' };
  }
  return dy >= 0
    ? { x: cx, y: box.y + box.height, side: 'bottom' }
    : { x: cx, y: box.y, side: 'top' };
}

/**
 * Anchor point for a specific side of a node box, centered on that side.
 * @param {{x:number,y:number,width:number,height:number}} box
 * @param {Side} side
 * @returns {Anchor}
 */
export function sideAnchor(box, side) {
  const cx = box.x + box.width / 2;
  const cy = box.y + box.height / 2;
  switch (side) {
    case 'left': return { x: box.x, y: cy, side };
    case 'right': return { x: box.x + box.width, y: cy, side };
    case 'top': return { x: cx, y: box.y, side };
    case 'bottom': return { x: cx, y: box.y + box.height, side };
    default: return { x: box.x + box.width, y: cy, side: 'right' };
  }
}

/**
 * Control-point offset magnitude for a bezier between two anchors.
 * Scales with the gap so short edges curve gently, long edges sweep.
 * @param {Anchor} src
 * @param {Anchor} tgt
 */
function controlOffset(src, tgt) {
  const dx = Math.abs(tgt.x - src.x);
  const dy = Math.abs(tgt.y - src.y);
  return Math.max(36, Math.max(dx, dy) * 0.4);
}

/**
 * SVG path `d` string for a cubic bezier from `src` to `tgt`. The curve
 * leaves `src` perpendicular to its side and arrives at `tgt` perpendicular
 * to its side — the React-Flow-style smooth connector.
 *
 * @param {Anchor} src
 * @param {Anchor} tgt
 * @returns {string}
 */
export function bezierPath(src, tgt) {
  const off = controlOffset(src, tgt);
  const sv = sideVector(src.side);
  const tv = sideVector(tgt.side);
  const c1x = src.x + sv.x * off;
  const c1y = src.y + sv.y * off;
  const c2x = tgt.x + tv.x * off;
  const c2y = tgt.y + tv.y * off;
  return `M ${src.x},${src.y} C ${c1x},${c1y} ${c2x},${c2y} ${tgt.x},${tgt.y}`;
}

/**
 * Point on the cubic bezier (as built by `bezierPath`) at parameter t.
 * Handy for placing edge labels (t = 0.5).
 *
 * @param {Anchor} src
 * @param {Anchor} tgt
 * @param {number} [t]   0..1, default 0.5
 * @returns {{x:number,y:number}}
 */
export function bezierPoint(src, tgt, t = 0.5) {
  const off = controlOffset(src, tgt);
  const sv = sideVector(src.side);
  const tv = sideVector(tgt.side);
  const p0 = { x: src.x, y: src.y };
  const p1 = { x: src.x + sv.x * off, y: src.y + sv.y * off };
  const p2 = { x: tgt.x + tv.x * off, y: tgt.y + tv.y * off };
  const p3 = { x: tgt.x, y: tgt.y };
  const u = 1 - t;
  const b0 = u * u * u;
  const b1 = 3 * u * u * t;
  const b2 = 3 * u * t * t;
  const b3 = t * t * t;
  return {
    x: b0 * p0.x + b1 * p1.x + b2 * p2.x + b3 * p3.x,
    y: b0 * p0.y + b1 * p1.y + b2 * p2.y + b3 * p3.y,
  };
}

/**
 * Euclidean distance between two points.
 * @param {{x:number,y:number}} a
 * @param {{x:number,y:number}} b
 */
export function distance(a, b) {
  const dx = a.x - b.x, dy = a.y - b.y;
  return Math.sqrt(dx * dx + dy * dy);
}
