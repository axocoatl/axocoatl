# @axocoatl/lattice

> Vanilla Web Components graph canvas. Drag-to-wire nodes, infinite zoom-pan-snap, no build step, no framework.

A reactive graph editor in plain Web Components and SVG. Framework-agnostic — drop into React, Vue, Svelte, or plain HTML. The visual layer of [Axocoatl](https://github.com/axocoatl/axocoatl), broken out as its own package because vanilla graph canvases shouldn't require pulling in a 200KB framework dependency.

```html
<script type="module" src="https://cdn.jsdelivr.net/npm/@axocoatl/lattice"></script>

<ax-lattice background="dots" snap="20" fit-view-on-init>
  <!-- nodes & edges go here (phase B+) -->
</ax-lattice>
```

## Why

Most graph editors on the web (React Flow, etc.) ship as React libraries. They're excellent, but they bake your framework choice in and weigh 100–200KB minified gzipped before you write a single component.

`@axocoatl/lattice` is the answer if you want:

- **No build step.** The published source is the dist. Import directly from a CDN or npm.
- **No framework lock-in.** Custom Elements + Shadow DOM. Works the same inside React, Vue, Svelte, Solid, htmx, or plain HTML.
- **Small.** Phase A is < 10 KB of source. No framework runtime to pay for.
- **Themeable via CSS Custom Properties.** Style with `--ax-bg`, `--ax-fg`, `--ax-accent`, no shadow-piercing required for the documented hooks.

## Install

```bash
npm install @axocoatl/lattice
```

Or import directly from a CDN — no install needed:

```html
<script type="module" src="https://cdn.jsdelivr.net/npm/@axocoatl/lattice"></script>
```

## Phase C — Handles, edges & drag-to-connect (current)

```html
<ax-lattice snap="20" background="dots" fit-view-on-init>
  <ax-node id="a" data-x="0" data-y="0">
    research
    <ax-handle type="source" handle-id="out" position="right"></ax-handle>
  </ax-node>
  <ax-node id="b" data-x="280" data-y="0">
    summarize
    <ax-handle type="target" handle-id="in" position="left"></ax-handle>
  </ax-node>

  <ax-edge from="a:out" to="b:in" label="findings"></ax-edge>
</ax-lattice>

<script type="module">
  import '@axocoatl/lattice';
  const lat = document.querySelector('ax-lattice');

  // A user drag from a source handle to a target handle fires this.
  // It's cancellable — preventDefault and create the edge yourself if you
  // want to validate or de-dupe first.
  lat.addEventListener('edge-connect', e => {
    console.log('connect', e.detail.from, '→', e.detail.to);
  });

  lat.addEventListener('selection-change', e => console.log('nodes', e.detail.ids));
  lat.addEventListener('node-moveend', e => console.log(e.target.id, e.detail));

  // Programmatic
  lat.addEdge({ from: 'a:out', to: 'b:in' });
  lat.deleteSelectedEdges();
</script>
```

Drag from a `source` handle to a `target` handle to draw an edge. Click an
edge curve to select it; Delete removes it. Everything from Phase A/B (pan,
zoom, node drag, multi/box-select, group-drag) still applies.

## Phase D — Minimap, controls, undo/redo, copy/paste

```html
<div style="position:relative">
  <ax-lattice id="lat" snap="20" background="dots" fit-view-on-init>
    <!-- nodes & edges -->
  </ax-lattice>
  <ax-controls for="lat" style="position:absolute;top:14px;left:14px"></ax-controls>
  <ax-minimap  for="lat" style="position:absolute;bottom:14px;right:14px"></ax-minimap>
</div>
```

- **Undo / redo** — `Cmd/Ctrl+Z`, `Cmd/Ctrl+Shift+Z`. Or `lat.undo()` / `lat.redo()`.
  Tracks moves, deletes, edge creation, and paste.
- **Copy / paste** — `Cmd/Ctrl+C` / `Cmd/Ctrl+V`. Or `lat.copy()` / `lat.paste()`.
  Copies the node selection plus enclosed edges; paste offsets and re-ids.
- **`<ax-minimap>`** — scaled overview; click or drag it to navigate.
- **`<ax-controls>`** — zoom / fit / undo / redo toolbar; undo/redo reflect state.

Both overlays link to a lattice by `for="latticeId"` or by assigning `.target`.

### Attributes
| Name | Type | Default | Description |
|---|---|---|---|
| `zoom` | number | `1` | Current zoom level |
| `min-zoom` | number | `0.2` | Minimum allowed zoom |
| `max-zoom` | number | `3` | Maximum allowed zoom |
| `pan-x` | number | `0` | Horizontal pan offset |
| `pan-y` | number | `0` | Vertical pan offset |
| `snap` | number | `0` | Grid snap size; `0` disables snapping |
| `background` | `dots` \| `grid` \| `none` | `dots` | Background pattern |
| `fit-view-on-init` | boolean | `false` | Auto fit-view after first render |

### Methods
| Method | Description |
|---|---|
| `setViewport({ x, y, k })` | Set pan + zoom imperatively |
| `getViewport()` | Returns `{ x, y, k }` |
| `fitView({ padding? })` | Frame all content |
| `zoomIn()` / `zoomOut()` | Step zoom |
| `screenToLattice({ x, y })` | Pointer → lattice coordinate |
| `latticeToScreen({ x, y })` | Lattice → screen coordinate |
| `snap(value)` | Snap a lattice coordinate (no-op when snap=0) |
| `nodes` (getter) | Snapshot Set of registered `<ax-node>`s |
| `selection` (getter) | Snapshot Set of currently selected nodes |
| `selectedIds()` | Array of selected node ids in registry order |
| `selectAll()` / `deselectAll()` | Selection ops |
| `deleteSelected()` | Fires `nodes-delete-request` (cancellable); on accept, removes them from DOM |

### Events
| Name | `detail` | Fires when |
|---|---|---|
| `viewport-change` | `{ x, y, k }` | Pan or zoom changes (rAF-coalesced) |
| `selection-change` | `{ ids: string[], count }` | Selection set changes |
| `nodes-delete-request` | `{ ids: string[] }` | Before deletion — preventDefault to keep the nodes |
| `nodes-deleted` | `{ ids: string[] }` | After deletion |
| `node-select` | `{ additive: boolean }` (from a node, bubbles) | A node was clicked / shift-clicked |
| `node-movestart`, `node-moving`, `node-moveend` | `{ x, y, dx?, dy? }` | Node drag lifecycle |

### `<ax-node>` API
| Method / property | Description |
|---|---|
| `x`, `y`, `position` | Lattice-space position (read & write) |
| `selected` (attr & prop) | Current selection state |
| `moveTo({ x, y, snap? })` | Programmatic move (no events) |
| `getBox()` | `{ x, y, width, height }` in lattice space |

### `<ax-node>` CSS Custom Properties
| Var | Default | Effect |
|---|---|---|
| `--ax-node-bg` | `#181d27` | Background |
| `--ax-node-fg` | `#e8ecf3` | Foreground |
| `--ax-node-border` | `#2e3645` | Border |
| `--ax-node-border-hover` | `#4a536a` | Border on hover |
| `--ax-node-border-sel` | `var(--ax-accent)` | Border when selected |
| `--ax-node-shadow` | small drop shadow | Box shadow |
| `--ax-node-shadow-sel` | accent ring + shadow | Box shadow when selected |
| `--ax-node-radius` | `9px` | Border radius |
| `--ax-node-padding` | `11px 14px` | Inner padding |
| `--ax-marquee-fill` (on lattice) | `rgba(124,92,255,.12)` | Box-select overlay fill |

### CSS Custom Properties
| Var | Default | Effect |
|---|---|---|
| `--ax-bg` | `#0a0c11` | Canvas background |
| `--ax-fg` | `#e8ecf3` | Text / foreground |
| `--ax-grid` | `#232a37` | Grid line/dot color |
| `--ax-grid-size` | `20px` | Grid spacing |
| `--ax-accent` | `#7c5cff` | Selection / highlight color |

## Roadmap

| Phase | Version | Status | Surface |
|---|---|---|---|
| **0** | — | ✓ shipped | Scaffold, demo, README |
| **A** | 0.1 | ✓ shipped | `<ax-lattice>` pan/zoom/snap/background |
| **B** | 0.2 | ✓ shipped | `<ax-node>` + click/shift/box select + drag + group drag + delete + keyboard model |
| **C** | 0.3 | ✓ shipped | `<ax-handle>` + `<ax-edge>` + drag-to-connect + edge select/delete + bezier routing |
| **D** | 0.4 | ✓ shipped | `<ax-minimap>`, `<ax-controls>`, undo/redo, copy/paste |
| **E** | 1.0 | ✓ shipped | Virtualization, layered auto-layout |
| **F** | 1.0 | ✓ shipped | API freeze, ARIA, `docs/API.md`, unit + integration test suites |

**v1.1.0** — stable API + native execution state (`<ax-node status>`,
`<ax-edge active>`). See [`docs/API.md`](docs/API.md) for the full reference.

## License

[Apache-2.0](LICENSE) — © Axocoatl Contributors.
