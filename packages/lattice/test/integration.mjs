/**
 * Headless integration test for @axocoatl/lattice.
 *
 * Loads the demo page in a real browser (Playwright/Chromium) and exercises
 * the interaction surface: registration, selection, single drag, group drag.
 *
 * Run:
 *   npm test           (from packages/lattice/)
 *
 * Requires `playwright` available on the system. The test serves the package
 * over a throwaway static server, so no external setup is needed.
 */

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const PKG_ROOT = resolve(fileURLToPath(import.meta.url), '../..');
const PORT = 8199;
const MIME = {
  '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript',
  '.css': 'text/css', '.json': 'application/json', '.svg': 'image/svg+xml',
};

// ── Static server ───────────────────────────────────────────────────────
const server = createServer(async (req, res) => {
  try {
    const urlPath = decodeURIComponent(req.url.split('?')[0]);
    // Browsers auto-probe /favicon.ico; answer it so it isn't logged as a 404.
    if (urlPath === '/favicon.ico') { res.writeHead(204).end(); return; }
    const path = join(PKG_ROOT, urlPath);
    if (!path.startsWith(PKG_ROOT)) { res.writeHead(403).end(); return; }
    let file = path;
    if (req.url.endsWith('/')) file = join(path, 'index.html');
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': MIME[extname(file)] || 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404).end('not found');
  }
});
await new Promise((r) => server.listen(PORT, '127.0.0.1', r));

// ── Browser ─────────────────────────────────────────────────────────────
import { existsSync, readdirSync } from 'node:fs';
import { homedir } from 'node:os';
import { createRequire } from 'node:module';

/** Resolve the `playwright` module from common locations. */
async function loadPlaywright() {
  const candidates = [
    'playwright', // normal: devDependency installed
    join(PKG_ROOT, 'node_modules/playwright/index.mjs'),
    '/tmp/pwtest/node_modules/playwright/index.mjs',
  ];
  for (const c of candidates) {
    try {
      const loaded = await import(c);
      return loaded.chromium ? loaded : (loaded.default || loaded);
    } catch { /* keep trying */ }
  }
  // Last resort: require.resolve from NODE_PATH
  try {
    const req = createRequire(import.meta.url);
    const loaded = await import(req.resolve('playwright'));
    return loaded.chromium ? loaded : (loaded.default || loaded);
  } catch { return null; }
}

/** Find any cached Chromium executable (ms-playwright cache). */
function findChromium() {
  if (process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE) return process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE;
  const cache = process.env.PLAYWRIGHT_BROWSERS_PATH
    || join(homedir(), '.cache/ms-playwright');
  if (!existsSync(cache)) return undefined;
  for (const dir of readdirSync(cache)) {
    if (!dir.startsWith('chromium-')) continue;
    const exe = join(cache, dir, 'chrome-linux64/chrome');
    if (existsSync(exe)) return exe;
    const exe2 = join(cache, dir, 'chrome-linux/chrome');
    if (existsSync(exe2)) return exe2;
  }
  return undefined;
}

const pw = await loadPlaywright();
if (!pw) {
  console.error('playwright not found — run: npm i -D playwright && npx playwright install chromium');
  server.close();
  process.exit(2);
}
const { chromium } = pw;
const exe = findChromium();
const browser = await chromium.launch(exe ? { executablePath: exe } : {});
const page = await browser.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });

let failures = 0;
const check = (label, cond, extra) => {
  const ok = cond === true;
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${label}${extra !== undefined ? `  → ${JSON.stringify(extra)}` : ''}`);
};

try {
  await page.goto(`http://127.0.0.1:${PORT}/demo/`, { waitUntil: 'networkidle' });
  await page.waitForTimeout(400);

  // Registration
  const reg = await page.evaluate(() => ({
    lattice: !!customElements.get('ax-lattice'),
    node: !!customElements.get('ax-node'),
    count: document.querySelector('ax-lattice')?.nodes?.size ?? -1,
  }));
  check('ax-lattice registered', reg.lattice);
  check('ax-node registered', reg.node);
  check('6 nodes registered', reg.count === 6, reg.count);

  // Single select
  await page.locator('#architect').click();
  await page.waitForTimeout(80);
  let sel = await page.evaluate(() => document.querySelector('ax-lattice').selectedIds());
  check('click selects one', JSON.stringify(sel) === '["architect"]', sel);

  // Multi select
  await page.locator('#planner').click({ modifiers: ['Shift'] });
  await page.waitForTimeout(80);
  sel = await page.evaluate(() => document.querySelector('ax-lattice').selectedIds().sort());
  check('shift-click multi-selects', JSON.stringify(sel) === '["architect","planner"]', sel);

  // Single drag
  await page.evaluate(() => document.querySelector('ax-lattice').deselectAll());
  const before1 = await page.evaluate(() => ({ x: document.querySelector('#coder').x, y: document.querySelector('#coder').y }));
  const cb = await page.locator('#coder').boundingBox();
  await page.mouse.move(cb.x + cb.width / 2, cb.y + cb.height / 2);
  await page.mouse.down();
  await page.mouse.move(cb.x + cb.width / 2 + 120, cb.y + cb.height / 2 + 60, { steps: 8 });
  await page.mouse.up();
  await page.waitForTimeout(80);
  const after1 = await page.evaluate(() => ({ x: document.querySelector('#coder').x, y: document.querySelector('#coder').y }));
  check('single drag moves node', after1.x !== before1.x || after1.y !== before1.y, { before1, after1 });

  // Group drag
  await page.evaluate(() => document.querySelector('ax-lattice').deselectAll());
  await page.locator('#architect').click();
  await page.locator('#planner').click({ modifiers: ['Shift'] });
  await page.locator('#reviewer').click({ modifiers: ['Shift'] });
  await page.waitForTimeout(80);
  const ids = ['architect', 'planner', 'reviewer'];
  const before3 = await page.evaluate((ids) => Object.fromEntries(ids.map((id) => {
    const n = document.querySelector('#' + id); return [id, { x: n.x, y: n.y }];
  })), ids);
  const ab = await page.locator('#architect').boundingBox();
  await page.mouse.move(ab.x + ab.width / 2, ab.y + ab.height / 2);
  await page.mouse.down();
  await page.mouse.move(ab.x + ab.width / 2 + 100, ab.y + ab.height / 2 + 100, { steps: 10 });
  await page.mouse.up();
  await page.waitForTimeout(120);
  const after3 = await page.evaluate((ids) => Object.fromEntries(ids.map((id) => {
    const n = document.querySelector('#' + id); return [id, { x: n.x, y: n.y }];
  })), ids);
  const deltas = ids.map((id) => ({ dx: after3[id].x - before3[id].x, dy: after3[id].y - before3[id].y }));
  check('group drag — all 3 moved', deltas.every((d) => d.dx !== 0 || d.dy !== 0), deltas);
  check('group drag — uniform delta', deltas.every((d) => d.dx === deltas[0].dx && d.dy === deltas[0].dy), deltas);

  // ── Phase C: handles & edges ──────────────────────────────────────────
  await page.evaluate(() => document.querySelector('ax-lattice').deselectAll());

  const counts = await page.evaluate(() => ({
    edges: document.querySelector('ax-lattice').edges.size,
    handles: document.querySelectorAll('ax-handle').length,
  }));
  check('5 edges registered', counts.edges === 5, counts.edges);
  check('10 handles present', counts.handles === 10, counts.handles);

  const edgePaths = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    const g = lat.shadowRoot.querySelector('.edge-paths');
    return g ? g.querySelectorAll('path.ax-edge-path').length : -1;
  });
  check('5 edge curves rendered in SVG layer', edgePaths === 5, edgePaths);

  // Drag-to-connect: researcher:out  →  coder:in  (creates a new edge)
  const srcHandle = await page.locator('#researcher ax-handle[type="source"]').boundingBox();
  const tgtHandle = await page.locator('#coder ax-handle[type="target"]').boundingBox();
  check('source handle has a screen box', !!srcHandle);
  check('target handle has a screen box', !!tgtHandle);
  if (srcHandle && tgtHandle) {
    await page.mouse.move(srcHandle.x + srcHandle.width / 2, srcHandle.y + srcHandle.height / 2);
    await page.mouse.down();
    await page.mouse.move(tgtHandle.x + tgtHandle.width / 2, tgtHandle.y + tgtHandle.height / 2, { steps: 12 });
    await page.mouse.up();
    await page.waitForTimeout(120);
  }
  const afterConnect = await page.evaluate(() => document.querySelector('ax-lattice').edges.size);
  check('drag-to-connect created an edge (5 → 6)', afterConnect === 6, afterConnect);

  // Select an edge by clicking its curve, then delete it.
  const edgeHit = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    const hit = lat.shadowRoot.querySelector('path.ax-edge-hit');
    if (!hit) return null;
    const r = hit.getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  });
  if (edgeHit) {
    await page.mouse.click(edgeHit.x, edgeHit.y);
    await page.waitForTimeout(80);
  }
  const edgeSel = await page.evaluate(() => ({
    count: document.querySelector('ax-lattice').selectedEdgeIds().length,
    active: document.activeElement?.tagName,
  }));
  check('clicking an edge selects it', edgeSel.count >= 1, edgeSel);
  check('edge click focuses the lattice host (keyboard works)', edgeSel.active === 'AX-LATTICE', edgeSel.active);

  // Keyboard Delete removes the selected edge.
  await page.keyboard.press('Delete');
  await page.waitForTimeout(100);
  const afterEdgeDelete = await page.evaluate(() => document.querySelector('ax-lattice').edges.size);
  check('Delete key removes the selected edge (6 → 5)', afterEdgeDelete === 5, afterEdgeDelete);

  // ── Phase D: undo / redo ──────────────────────────────────────────────
  // The last history command was the edge delete (6 → 5). Undo brings it back.
  await page.evaluate(() => document.querySelector('ax-lattice').undo());
  await page.waitForTimeout(80);
  const afterUndo = await page.evaluate(() => document.querySelector('ax-lattice').edges.size);
  check('undo restores the deleted edge (5 → 6)', afterUndo === 6, afterUndo);

  await page.evaluate(() => document.querySelector('ax-lattice').redo());
  await page.waitForTimeout(80);
  const afterRedo = await page.evaluate(() => document.querySelector('ax-lattice').edges.size);
  check('redo re-deletes the edge (6 → 5)', afterRedo === 5, afterRedo);

  // Undo a node move and confirm the position is restored.
  await page.evaluate(() => document.querySelector('ax-lattice').deselectAll());
  const moveBox = await page.locator('#summarizer').boundingBox();
  const moveBefore = await page.evaluate(() => {
    const n = document.querySelector('#summarizer'); return { x: n.x, y: n.y };
  });
  await page.mouse.move(moveBox.x + moveBox.width / 2, moveBox.y + moveBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(moveBox.x + moveBox.width / 2 + 140, moveBox.y + moveBox.height / 2 - 60, { steps: 8 });
  await page.mouse.up();
  await page.waitForTimeout(80);
  await page.evaluate(() => document.querySelector('ax-lattice').undo());
  await page.waitForTimeout(80);
  const moveAfterUndo = await page.evaluate(() => {
    const n = document.querySelector('#summarizer'); return { x: n.x, y: n.y };
  });
  check('undo restores a moved node position',
    moveAfterUndo.x === moveBefore.x && moveAfterUndo.y === moveBefore.y,
    { moveBefore, moveAfterUndo });

  // ── Phase D: copy / paste ─────────────────────────────────────────────
  await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    lat.deselectAll();
  });
  await page.locator('#researcher').click();
  await page.waitForTimeout(60);
  const pasteResult = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    const before = lat.nodes.size;
    lat.copy();
    const pasted = lat.paste();
    return { before, after: lat.nodes.size, pastedCount: pasted.length };
  });
  check('copy + paste creates a new node', pasteResult.after === pasteResult.before + 1, pasteResult);

  // Undo the paste so the node count returns to 6 for the delete test below.
  await page.evaluate(() => document.querySelector('ax-lattice').undo());
  await page.waitForTimeout(80);
  const afterPasteUndo = await page.evaluate(() => document.querySelector('ax-lattice').nodes.size);
  check('undo removes the pasted node (back to 6)', afterPasteUndo === 6, afterPasteUndo);

  // ── Phase D: minimap & controls ───────────────────────────────────────
  const phaseD = await page.evaluate(() => ({
    minimap: !!customElements.get('ax-minimap'),
    controls: !!customElements.get('ax-controls'),
    minimapHasViewBox: !!document.querySelector('ax-minimap')
      ?.shadowRoot?.querySelector('svg')?.getAttribute('viewBox'),
    minimapNodeRects: document.querySelector('ax-minimap')
      ?.shadowRoot?.querySelectorAll('.mm-node').length ?? -1,
  }));
  check('ax-minimap registered', phaseD.minimap);
  check('ax-controls registered', phaseD.controls);
  check('minimap rendered a viewBox', phaseD.minimapHasViewBox);
  check('minimap drew node rects (6)', phaseD.minimapNodeRects === 6, phaseD.minimapNodeRects);

  // ── Phase E: auto-layout ──────────────────────────────────────────────
  await page.evaluate(() => document.querySelector('ax-lattice').deselectAll());
  // Asserts against edges known to still exist at this point:
  //   architect → coder → reviewer  (architect→planner was deleted earlier).
  const layoutResult = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    lat.autoLayout({ direction: 'LR' });
    const x = (id) => document.querySelector('#' + id).x;
    return { architect: x('architect'), coder: x('coder'), reviewer: x('reviewer') };
  });
  check('auto-layout layers the DAG left-to-right',
    layoutResult.architect < layoutResult.coder &&
    layoutResult.coder < layoutResult.reviewer,
    layoutResult);

  // Undo the auto-layout (one undoable command).
  await page.evaluate(() => document.querySelector('ax-lattice').undo());
  await page.waitForTimeout(60);

  // ── Phase E: virtualization ───────────────────────────────────────────
  const virtResult = await page.evaluate(async () => {
    const lat = document.querySelector('ax-lattice');
    lat.setAttribute('virtualize', '');
    lat.setViewport({ x: -80000, y: -80000, k: 1 });
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    const culledAway = lat.virtualization.culled;
    lat.fitView();
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    const culledHome = lat.virtualization.culled;
    lat.removeAttribute('virtualize');
    lat.fitView();
    return { culledAway, culledHome, total: lat.nodes.size };
  });
  check('virtualization culls nodes panned off-screen',
    virtResult.culledAway === virtResult.total, virtResult);
  check('virtualization un-culls nodes back in view',
    virtResult.culledHome === 0, virtResult);

  // ── Phase F: accessibility ────────────────────────────────────────────
  const a11y = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    const node = document.querySelector('#researcher');
    return {
      latticeRole: lat.getAttribute('role'),
      latticeLabel: !!lat.getAttribute('aria-label'),
      nodeRole: node.getAttribute('role'),
      nodeAriaSelected: node.getAttribute('aria-selected'),
      nodeAriaLabel: !!node.getAttribute('aria-label'),
      liveRegion: !!lat.shadowRoot.querySelector('[aria-live="polite"]'),
    };
  });
  check('lattice has role=application', a11y.latticeRole === 'application', a11y);
  check('lattice has an aria-label', a11y.latticeLabel);
  check('node has role=button', a11y.nodeRole === 'button', a11y);
  check('node exposes aria-selected', a11y.nodeAriaSelected === 'false', a11y);
  check('node has an aria-label', a11y.nodeAriaLabel);
  check('lattice has an aria-live region', a11y.liveRegion);

  // ── v1.1: execution state (node status + active edges) ────────────────
  const exec = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    lat.setNodeStatus('architect', 'running');
    lat.setNodeStatus('coder', 'success');
    lat.setNodeStatus('reviewer', 'error');
    lat.setNodeStatus('planner', 'blocked');
    lat.setNodeStatus('researcher', 'cancelled');
    const arch = document.querySelector('#architect');
    const out = {
      runningAttr: arch.getAttribute('status'),
      runningAriaBusy: arch.getAttribute('aria-busy'),
      coderStatus: document.querySelector('#coder').status,
      reviewerStatus: document.querySelector('#reviewer').status,
      plannerStatus: document.querySelector('#planner').status,
      researcherStatus: document.querySelector('#researcher').status,
    };
    // Active edge
    lat.setEdgeActive({ from: 'architect', to: 'coder' }, true);
    const activeEdges = [...lat.edges].filter((e) => e.hasAttribute('active')).length;
    out.activeEdges = activeEdges;
    return out;
  });
  check('node.status drives the status attribute', exec.runningAttr === 'running', exec);
  check('running node sets aria-busy', exec.runningAriaBusy === 'true', exec);
  check('setNodeStatus applies success/error', exec.coderStatus === 'success' && exec.reviewerStatus === 'error', exec);
  check('setNodeStatus preserves blocked/cancelled',
    exec.plannerStatus === 'blocked' && exec.researcherStatus === 'cancelled', exec);
  check('setEdgeActive marks an edge active', exec.activeEdges >= 1, exec);

  // Active edge renders with the .active class on its path.
  await page.waitForTimeout(60);
  const activePathRendered = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    return !!lat.shadowRoot.querySelector('path.ax-edge-path.active');
  });
  check('active edge renders a flowing (.active) path', activePathRendered);

  // resetStatuses clears everything.
  const afterReset = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    lat.resetStatuses();
    const anyStatus = [...lat.nodes].some((n) => n.status !== 'idle');
    const anyActive = [...lat.edges].some((e) => e.hasAttribute('active'));
    return { anyStatus, anyActive };
  });
  check('resetStatuses clears node + edge state',
    !afterReset.anyStatus && !afterReset.anyActive, afterReset);

  // View blocks every editing entry point while the host can render evidence.
  const viewBefore = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    lat.setSelection(['architect', 'planner']);
    lat.copy(); // Existing clipboard/history must not remain executable in View.
    lat.mode = 'view';
    window.viewMutationEvents = [];
    for (const kind of ['nodes-delete-request', 'edges-delete-request', 'edge-connect', 'node-moving']) {
      lat.addEventListener(kind, () => window.viewMutationEvents.push(kind));
    }
    window.viewSnapshot = () => JSON.stringify({
      nodes: [...lat.nodes].map(n => [n.id, n.x, n.y]),
      edges: [...lat.edges].map(e => [e.id, e.getAttribute('from'), e.getAttribute('to')]),
    });
    const snapshot = window.viewSnapshot();
    const result = { copy: lat.copy(), pasted: lat.paste().length, edge: lat.addEdge({ from: 'architect', to: 'planner' }) };
    lat.deleteSelected(); lat.deleteSelectedEdges(); lat.undo(); lat.redo(); lat.autoLayout();
    return { snapshot, after: window.viewSnapshot(), result, canUndo: lat.canUndo(), canRedo: lat.canRedo(),
      role: lat.getAttribute('aria-roledescription'),
      handlesHidden: [...lat.querySelectorAll('ax-handle')].every(h => getComputedStyle(h).display === 'none'),
      historyHidden: [...document.querySelector('ax-controls').shadowRoot.querySelectorAll('.undo,.redo')].every(b => b.hidden),
    };
  });
  check('View public mutation methods leave graph unchanged', viewBefore.snapshot === viewBefore.after, viewBefore.result);
  check('View reports no executable history', !viewBefore.canUndo && !viewBefore.canRedo);
  check('View hides handles and history buttons', viewBefore.handlesHidden && viewBefore.historyHidden, viewBefore);
  check('View describes an execution graph', viewBefore.role === 'execution graph', viewBefore.role);
  const viewNode = await page.locator('#architect').boundingBox();
  await page.mouse.move(viewNode.x + 35, viewNode.y + 20);
  await page.mouse.down(); await page.mouse.move(viewNode.x + 90, viewNode.y + 60, { steps: 4 }); await page.mouse.up();
  await page.evaluate(() => document.querySelector('ax-lattice').focus());
  for (const key of ['Delete', 'Backspace', 'Control+z', 'Control+Shift+z', 'Control+y', 'Control+c', 'Control+v', 'ArrowRight', 'Shift+ArrowDown']) {
    await page.keyboard.press(key);
  }
  const afterViewKeys = await page.evaluate(() => ({ snapshot: window.viewSnapshot(), events: window.viewMutationEvents,
    ids: document.querySelector('ax-lattice').selectedIds() }));
  check('View drag and keyboard never mutate graph', afterViewKeys.snapshot === viewBefore.snapshot, afterViewKeys);
  check('View never emits editing requests', afterViewKeys.events.length === 0, afterViewKeys.events);
  check('View arrows keep keyboard inspection available', afterViewKeys.ids.length === 1, afterViewKeys.ids);
  const viewCamera = await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice'); const before = lat.getViewport(); lat.zoomIn();
    const zoomed = lat.getViewport(); lat.fitView(); lat.setSelection(['architect']);
    return { zoomed: zoomed.k !== before.k, selection: lat.selectedIds() };
  });
  check('View camera and selection APIs remain available', viewCamera.zoomed && viewCamera.selection[0] === 'architect');
  await page.evaluate(() => document.querySelector('ax-lattice').mode = 'edit');
  const switchBox = await page.locator('#architect').boundingBox();
  const beforeSwitch = await page.evaluate(() => window.viewSnapshot());
  await page.mouse.move(switchBox.x + 35, switchBox.y + 20); await page.mouse.down();
  await page.mouse.move(switchBox.x + 60, switchBox.y + 35, { steps: 3 });
  await page.evaluate(() => document.querySelector('ax-lattice').mode = 'view');
  await page.mouse.move(switchBox.x + 120, switchBox.y + 65, { steps: 3 }); await page.mouse.up();
  check('Switching to View cancels and restores an unfinished drag', await page.evaluate(() => window.viewSnapshot()) === beforeSwitch);
  await page.evaluate(() => document.querySelector('ax-lattice').mode = 'edit');

  // ── Node delete (do last; removes nodes) ──────────────────────────────
  await page.evaluate(() => {
    const lat = document.querySelector('ax-lattice');
    lat.deselectAll();
  });
  await page.locator('#architect').click();
  await page.locator('#planner').click({ modifiers: ['Shift'] });
  await page.locator('#reviewer').click({ modifiers: ['Shift'] });
  await page.waitForTimeout(80);
  await page.keyboard.press('Delete');
  await page.waitForTimeout(80);
  const remaining = await page.evaluate(() => document.querySelector('ax-lattice').nodes.size);
  check('Delete key removes selected nodes (6 - 3 = 3)', remaining === 3, remaining);

  // Chromium auto-probes /favicon.ico; the demo uses a data-URI icon so a
  // 404 for that path is expected noise, not a real error.
  const realErrors = errors.filter((e) => !/favicon\.ico/i.test(e));
  check('no page/console errors', realErrors.length === 0, realErrors);
} finally {
  await browser.close();
  server.close();
}

process.exit(failures > 0 ? 1 : 0);
