/**
 * Pure helpers for managing a selection set of nodes (or any unique items).
 * The lattice owns the actual `Set` instance; this module is just operations.
 *
 * @module @axocoatl/lattice/selection
 */

/**
 * Add an item to a selection set.
 * @template T
 * @param {Set<T>} set
 * @param {T} item
 * @returns {Set<T>} the same set (for chaining)
 */
export function add(set, item) {
  set.add(item);
  return set;
}

/**
 * Remove an item from a selection set.
 * @template T
 * @param {Set<T>} set
 * @param {T} item
 * @returns {Set<T>}
 */
export function remove(set, item) {
  set.delete(item);
  return set;
}

/**
 * Toggle an item's membership in the set.
 * @template T
 * @param {Set<T>} set
 * @param {T} item
 * @returns {Set<T>}
 */
export function toggle(set, item) {
  if (set.has(item)) set.delete(item);
  else set.add(item);
  return set;
}

/**
 * Replace the contents of a set with a single item.
 * @template T
 * @param {Set<T>} set
 * @param {T | null} item
 * @returns {Set<T>}
 */
export function replace(set, item) {
  set.clear();
  if (item != null) set.add(item);
  return set;
}

/**
 * Empty the selection.
 * @template T
 * @param {Set<T>} set
 * @returns {Set<T>}
 */
export function clear(set) {
  set.clear();
  return set;
}

/**
 * True iff the two sets contain the same elements.
 * @template T
 * @param {Set<T>} a
 * @param {Set<T>} b
 */
export function equals(a, b) {
  if (a === b) return true;
  if (a.size !== b.size) return false;
  for (const x of a) if (!b.has(x)) return false;
  return true;
}

/**
 * Axis-aligned bounding-box intersection test (screen space rects).
 * @param {{left:number,top:number,right:number,bottom:number}} a
 * @param {{left:number,top:number,right:number,bottom:number}} b
 */
export function rectIntersects(a, b) {
  return !(a.right < b.left || b.right < a.left || a.bottom < b.top || b.bottom < a.top);
}

/**
 * Normalize two points into a rect.
 * @param {{x:number,y:number}} p1
 * @param {{x:number,y:number}} p2
 * @returns {{left:number,top:number,right:number,bottom:number,width:number,height:number}}
 */
export function rectFromPoints(p1, p2) {
  const left = Math.min(p1.x, p2.x);
  const top = Math.min(p1.y, p2.y);
  const right = Math.max(p1.x, p2.x);
  const bottom = Math.max(p1.y, p2.y);
  return { left, top, right, bottom, width: right - left, height: bottom - top };
}
