/**
 * Unit tests for the pure (DOM-free) modules of @axocoatl/lattice.
 *
 * Run:  node --test test/unit.mjs   (or  npm run test:unit)
 *
 * These exercise the math: viewport transforms, edge geometry, selection set
 * ops, the history stack, and the auto-layout algorithm — none of which need
 * a browser.
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import * as vp from '../src/viewport.js';
import * as geo from '../src/geometry.js';
import * as sel from '../src/selection.js';
import { History } from '../src/history.js';
import { layeredLayout } from '../src/layout.js';

// ── viewport ────────────────────────────────────────────────────────────

test('clamp bounds a value', () => {
  assert.equal(vp.clamp(5, 0, 10), 5);
  assert.equal(vp.clamp(-3, 0, 10), 0);
  assert.equal(vp.clamp(99, 0, 10), 10);
});

test('snap rounds to the nearest step; step<=0 is identity', () => {
  assert.equal(vp.snap(23, 20), 20);
  assert.equal(vp.snap(31, 20), 40);
  assert.equal(vp.snap(7, 0), 7);
});

test('screenToLattice and latticeToScreen are inverses', () => {
  const v = { x: 120, y: -40, k: 1.5 };
  const p = { x: 300, y: 210 };
  const lat = vp.screenToLattice(v, p);
  const back = vp.latticeToScreen(v, lat);
  assert.ok(Math.abs(back.x - p.x) < 1e-9);
  assert.ok(Math.abs(back.y - p.y) < 1e-9);
});

test('zoomAt keeps the pivot point fixed on screen', () => {
  const v = { x: 0, y: 0, k: 1 };
  const pivot = { x: 400, y: 250 };
  const before = vp.screenToLattice(v, pivot);
  const z = vp.zoomAt(v, 2.5, pivot);
  const after = vp.screenToLattice(z, pivot);
  assert.ok(Math.abs(before.x - after.x) < 1e-6);
  assert.ok(Math.abs(before.y - after.y) < 1e-6);
});

test('zoomAt respects min/max bounds', () => {
  const v = { x: 0, y: 0, k: 1 };
  assert.equal(vp.zoomAt(v, 99, { x: 0, y: 0 }, { maxZoom: 3 }).k, 3);
  assert.equal(vp.zoomAt(v, 0.01, { x: 0, y: 0 }, { minZoom: 0.5 }).k, 0.5);
});

test('fitView frames a bounding box centered', () => {
  const bbox = { x: 0, y: 0, width: 200, height: 100 };
  const v = vp.fitView(bbox, { width: 800, height: 600 }, { padding: 40 });
  // The bbox center should map to the view center.
  const center = vp.latticeToScreen(v, { x: 100, y: 50 });
  assert.ok(Math.abs(center.x - 400) < 1e-6);
  assert.ok(Math.abs(center.y - 300) < 1e-6);
});

test('nodesBounds unions node boxes', () => {
  const b = vp.nodesBounds([
    { x: 0, y: 0, width: 50, height: 50 },
    { x: 100, y: 80, width: 60, height: 40 },
  ]);
  assert.deepEqual(b, { x: 0, y: 0, width: 160, height: 120 });
});

// ── geometry ────────────────────────────────────────────────────────────

test('sideVector points outward from each side', () => {
  assert.deepEqual(geo.sideVector('right'), { x: 1, y: 0 });
  assert.deepEqual(geo.sideVector('left'), { x: -1, y: 0 });
  assert.deepEqual(geo.sideVector('top'), { x: 0, y: -1 });
  assert.deepEqual(geo.sideVector('bottom'), { x: 0, y: 1 });
});

test('bezierPath starts at src and ends at tgt', () => {
  const d = geo.bezierPath(
    { x: 0, y: 0, side: 'right' },
    { x: 200, y: 80, side: 'left' },
  );
  assert.ok(d.startsWith('M 0,0 C'));
  assert.ok(d.trim().endsWith('200,80'));
});

test('bezierPoint at t=0 and t=1 hits the endpoints', () => {
  const s = { x: 10, y: 20, side: 'right' };
  const t = { x: 300, y: 90, side: 'left' };
  const p0 = geo.bezierPoint(s, t, 0);
  const p1 = geo.bezierPoint(s, t, 1);
  assert.ok(Math.abs(p0.x - 10) < 1e-6 && Math.abs(p0.y - 20) < 1e-6);
  assert.ok(Math.abs(p1.x - 300) < 1e-6 && Math.abs(p1.y - 90) < 1e-6);
});

test('autoAnchor picks the side facing the other endpoint', () => {
  const box = { x: 0, y: 0, width: 100, height: 100 };
  assert.equal(geo.autoAnchor(box, { x: 500, y: 50 }).side, 'right');
  assert.equal(geo.autoAnchor(box, { x: -500, y: 50 }).side, 'left');
  assert.equal(geo.autoAnchor(box, { x: 50, y: 500 }).side, 'bottom');
  assert.equal(geo.autoAnchor(box, { x: 50, y: -500 }).side, 'top');
});

// ── selection ───────────────────────────────────────────────────────────

test('selection toggle / replace / equals', () => {
  const s = new Set();
  sel.toggle(s, 'a');
  assert.ok(s.has('a'));
  sel.toggle(s, 'a');
  assert.ok(!s.has('a'));
  sel.replace(s, 'b');
  assert.deepEqual([...s], ['b']);
  assert.ok(sel.equals(new Set(['x', 'y']), new Set(['y', 'x'])));
  assert.ok(!sel.equals(new Set(['x']), new Set(['x', 'y'])));
});

test('rectIntersects and rectFromPoints', () => {
  const a = { left: 0, top: 0, right: 10, bottom: 10 };
  assert.ok(sel.rectIntersects(a, { left: 5, top: 5, right: 15, bottom: 15 }));
  assert.ok(!sel.rectIntersects(a, { left: 20, top: 20, right: 30, bottom: 30 }));
  const r = sel.rectFromPoints({ x: 30, y: 5 }, { x: 10, y: 25 });
  assert.deepEqual(r, { left: 10, top: 5, right: 30, bottom: 25, width: 20, height: 20 });
});

// ── history ─────────────────────────────────────────────────────────────

test('history applies undo / redo in order', () => {
  let value = 0;
  const h = new History();
  const cmd = (delta) => ({
    label: `add ${delta}`,
    undo: () => { value -= delta; },
    redo: () => { value += delta; },
  });
  value += 5; h.push(cmd(5));
  value += 3; h.push(cmd(3));
  assert.equal(value, 8);
  h.undo(); assert.equal(value, 5);
  h.undo(); assert.equal(value, 0);
  assert.equal(h.canUndo(), false);
  h.redo(); assert.equal(value, 5);
  h.redo(); assert.equal(value, 8);
  assert.equal(h.canRedo(), false);
});

test('history push clears the redo stack', () => {
  const h = new History();
  const noop = { label: 'x', undo() {}, redo() {} };
  h.push(noop); h.push(noop);
  h.undo();
  assert.equal(h.canRedo(), true);
  h.push(noop); // a fresh action invalidates redo
  assert.equal(h.canRedo(), false);
});

test('history onChange reports state', () => {
  const states = [];
  const h = new History((s) => states.push(s));
  h.push({ label: 'a', undo() {}, redo() {} });
  assert.deepEqual(states.at(-1), { canUndo: true, canRedo: false });
  h.undo();
  assert.deepEqual(states.at(-1), { canUndo: false, canRedo: true });
});

// ── layout ──────────────────────────────────────────────────────────────

test('layeredLayout places a chain in increasing layers (LR)', () => {
  const nodes = [
    { id: 'a', width: 100, height: 50 },
    { id: 'b', width: 100, height: 50 },
    { id: 'c', width: 100, height: 50 },
  ];
  const edges = [{ from: 'a', to: 'b' }, { from: 'b', to: 'c' }];
  const pos = layeredLayout(nodes, edges, { direction: 'LR' });
  assert.ok(pos.get('a').x < pos.get('b').x);
  assert.ok(pos.get('b').x < pos.get('c').x);
});

test('layeredLayout puts siblings in the same layer', () => {
  const nodes = ['root', 'l', 'r'].map((id) => ({ id, width: 100, height: 50 }));
  const edges = [{ from: 'root', to: 'l' }, { from: 'root', to: 'r' }];
  const pos = layeredLayout(nodes, edges, { direction: 'LR' });
  assert.equal(pos.get('l').x, pos.get('r').x); // same layer ⇒ same x
  assert.notEqual(pos.get('l').y, pos.get('r').y); // different rows
});

test('layeredLayout handles cycles without infinite recursion', () => {
  const nodes = ['a', 'b'].map((id) => ({ id, width: 100, height: 50 }));
  const edges = [{ from: 'a', to: 'b' }, { from: 'b', to: 'a' }];
  const pos = layeredLayout(nodes, edges);
  assert.equal(pos.size, 2); // terminates, positions both
});

test('layeredLayout ignores edges to unknown nodes', () => {
  const nodes = [{ id: 'a', width: 100, height: 50 }];
  const pos = layeredLayout(nodes, [{ from: 'a', to: 'ghost' }]);
  assert.equal(pos.size, 1);
});

test('layeredLayout strips handle suffixes from edge refs', () => {
  const nodes = ['a', 'b'].map((id) => ({ id, width: 100, height: 50 }));
  const pos = layeredLayout(nodes, [{ from: 'a:out', to: 'b:in' }], { direction: 'LR' });
  assert.ok(pos.get('a').x < pos.get('b').x);
});

test('layeredLayout deeper chains gain monotonic layer offsets', () => {
  const nodes = ['a', 'b', 'c', 'd'].map((id) => ({ id, width: 80, height: 40 }));
  const edges = [
    { from: 'a', to: 'b' }, { from: 'b', to: 'c' }, { from: 'c', to: 'd' },
  ];
  const pos = layeredLayout(nodes, edges, { direction: 'TB' });
  // TB ⇒ deeper nodes have larger y.
  assert.ok(pos.get('a').y < pos.get('b').y);
  assert.ok(pos.get('b').y < pos.get('c').y);
  assert.ok(pos.get('c').y < pos.get('d').y);
});
