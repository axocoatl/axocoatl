/**
 * @axocoatl/lattice
 *
 * Vanilla Web Components graph canvas.
 *
 * Importing this module registers all the Custom Elements as a side effect:
 *   import '@axocoatl/lattice';
 *
 * Named exports are also available for advanced use (subclassing, direct
 * instantiation, pure math):
 *   import {
 *     AxLatticeElement, AxNodeElement, AxHandleElement, AxEdgeElement,
 *     AxMinimapElement, AxControlsElement,
 *     viewport, selection, geometry, History,
 *   } from '@axocoatl/lattice';
 */

export { AxLatticeElement } from './lattice.js';
export { AxNodeElement } from './node.js';
export { AxHandleElement } from './handle.js';
export { AxEdgeElement } from './edge.js';
export { AxMinimapElement } from './minimap.js';
export { AxControlsElement } from './controls.js';
export { History } from './history.js';

import './lattice.js';
import './node.js';
import './handle.js';
import './edge.js';
import './minimap.js';
import './controls.js';

export * as viewport from './viewport.js';
export * as selection from './selection.js';
export * as geometry from './geometry.js';
export * as layout from './layout.js';
