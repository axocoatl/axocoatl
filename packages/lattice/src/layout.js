/**
 * Auto-layout — a layered (Sugiyama-style) DAG layout.
 *
 * Pure functions, no DOM. Given node sizes and edges, returns lattice-space
 * positions. The algorithm:
 *   1. Assign each node a layer = its longest-path depth from a root.
 *   2. Order nodes within each layer to reduce edge crossings (one barycenter
 *      sweep — cheap, good enough for the graph sizes a lattice handles).
 *   3. Place layers along the primary axis, nodes along the secondary axis,
 *      each layer centered.
 *
 * Cycles are tolerated: a back-edge that would deepen a node is ignored for
 * layering so the algorithm always terminates.
 *
 * @module @axocoatl/lattice/layout
 */

/**
 * @typedef {Object} LayoutNode
 * @property {string} id
 * @property {number} [width]
 * @property {number} [height]
 */

/**
 * @typedef {Object} LayoutEdge
 * @property {string} from   source node id (handle suffixes are ignored)
 * @property {string} to     target node id
 */

/**
 * @typedef {Object} LayoutOptions
 * @property {"LR"|"TB"} [direction]   primary flow axis (default "LR")
 * @property {number} [gapMain]        gap between layers (default 120)
 * @property {number} [gapCross]       gap between nodes in a layer (default 40)
 * @property {number} [originX]        x of the layout origin (default 0)
 * @property {number} [originY]        y of the layout origin (default 0)
 */

/** Strip a `nodeId:handleId` reference down to the node id. */
function nodeOf(ref) {
  const i = (ref || '').indexOf(':');
  return i < 0 ? (ref || '') : ref.slice(0, i);
}

/**
 * Compute a layered layout.
 *
 * @param {LayoutNode[]} nodes
 * @param {LayoutEdge[]} edges
 * @param {LayoutOptions} [options]
 * @returns {Map<string, {x:number,y:number}>}  node id → lattice position
 */
export function layeredLayout(nodes, edges, options = {}) {
  const direction = options.direction === 'TB' ? 'TB' : 'LR';
  const gapMain = options.gapMain ?? 120;
  const gapCross = options.gapCross ?? 40;
  const originX = options.originX ?? 0;
  const originY = options.originY ?? 0;

  const ids = nodes.map((n) => n.id);
  const idSet = new Set(ids);
  const sizeOf = new Map(
    nodes.map((n) => [n.id, { w: n.width || 160, h: n.height || 60 }]),
  );

  // Adjacency (only edges between known nodes).
  /** @type {Map<string,string[]>} */
  const out = new Map(ids.map((id) => [id, []]));
  /** @type {Map<string,string[]>} */
  const inc = new Map(ids.map((id) => [id, []]));
  for (const e of edges) {
    const f = nodeOf(e.from), t = nodeOf(e.to);
    if (f === t || !idSet.has(f) || !idSet.has(t)) continue;
    out.get(f).push(t);
    inc.get(t).push(f);
  }

  // Layer assignment by longest path from roots. Process in topological-ish
  // order; a memoized DFS with a visiting-guard tolerates cycles.
  /** @type {Map<string,number>} */
  const layer = new Map();
  const visiting = new Set();
  const depth = (id) => {
    if (layer.has(id)) return layer.get(id);
    if (visiting.has(id)) return 0; // back-edge — break the cycle
    visiting.add(id);
    let d = 0;
    for (const p of inc.get(id)) d = Math.max(d, depth(p) + 1);
    visiting.delete(id);
    layer.set(id, d);
    return d;
  };
  for (const id of ids) depth(id);

  // Bucket nodes into layers, preserving input order initially.
  /** @type {Map<number,string[]>} */
  const layers = new Map();
  for (const id of ids) {
    const d = layer.get(id);
    if (!layers.has(d)) layers.set(d, []);
    layers.get(d).push(id);
  }
  const layerKeys = [...layers.keys()].sort((a, b) => a - b);

  // One barycenter sweep: order each layer by the mean index of its parents
  // in the previous layer. Reduces crossings noticeably for typical DAGs.
  const indexInLayer = new Map();
  const reindex = (key) => {
    layers.get(key).forEach((id, i) => indexInLayer.set(id, i));
  };
  layerKeys.forEach(reindex);
  for (let i = 1; i < layerKeys.length; i++) {
    const key = layerKeys[i];
    const arr = layers.get(key);
    const bary = new Map();
    arr.forEach((id, idx) => {
      const parents = inc.get(id).filter((p) => layer.get(p) === layerKeys[i - 1]);
      if (parents.length === 0) { bary.set(id, idx); return; }
      const mean = parents.reduce((s, p) => s + (indexInLayer.get(p) ?? 0), 0) / parents.length;
      bary.set(id, mean);
    });
    arr.sort((a, b) => bary.get(a) - bary.get(b));
    reindex(key);
  }

  // Place. Main axis = layer index; cross axis = position within layer.
  // Each layer is centered on the cross axis so the graph reads symmetric.
  const pos = new Map();
  // First measure the cross-extent of each layer to center them.
  const crossExtent = (key) =>
    layers.get(key).reduce((s, id) => {
      const sz = sizeOf.get(id);
      return s + (direction === 'LR' ? sz.h : sz.w) + gapCross;
    }, -gapCross);
  const maxCross = Math.max(1, ...layerKeys.map(crossExtent));

  let mainCursor = 0;
  for (const key of layerKeys) {
    const arr = layers.get(key);
    // Widest/tallest node in this layer drives the main-axis step.
    const layerMainSize = Math.max(
      ...arr.map((id) => (direction === 'LR' ? sizeOf.get(id).w : sizeOf.get(id).h)),
    );
    let crossCursor = (maxCross - crossExtent(key)) / 2;
    for (const id of arr) {
      const sz = sizeOf.get(id);
      if (direction === 'LR') {
        pos.set(id, { x: originX + mainCursor, y: originY + crossCursor });
        crossCursor += sz.h + gapCross;
      } else {
        pos.set(id, { x: originX + crossCursor, y: originY + mainCursor });
        crossCursor += sz.w + gapCross;
      }
    }
    mainCursor += layerMainSize + gapMain;
  }
  return pos;
}
