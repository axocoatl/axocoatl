# Changelog

All notable changes to `@axocoatl/lattice` are documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0] — 2026-05

Live execution state — a native "run" dimension.

### Added
- **Node execution status** — `<ax-node status>` accepts `idle` / `pending` /
  `running` / `success` / `error`. Each state has built-in styling: `running`
  pulses with an accent glow, `pending` dashes, `success`/`error` recolor the
  border. `node.status` property; `running` also sets `aria-busy`.
- **Active (flowing) edges** — `<ax-edge active>` renders the curve with an
  animated marching-dash flow from source toward target. `edge.active` property.
- `<ax-lattice>` execution helpers: `setNodeStatus(id, status)`,
  `setEdgeActive(idOrFromTo, bool)`, `resetStatuses()`.
- The minimap tints node rectangles by status and re-renders live during a run
  (via a `MutationObserver` on `status` / `active` attributes).
- New CSS Custom Properties: `--ax-node-pending`, `--ax-node-running`,
  `--ax-node-running-glow`, `--ax-node-success`, `--ax-node-error`,
  `--ax-edge-color-active`.

### Notes
- This is the feature that lets a consumer visualise a workflow run natively —
  no hand-rolled status overlays. The Axocoatl dashboard's Workflows tab drives
  it from the live event stream.

## [1.0.0] — 2026-05

Phases E + F — Virtualization, auto-layout, accessibility, API freeze.

### Added
- **Auto-layout** (`layout.js`) — layered (Sugiyama-style) DAG layout: longest-path
  layering, one barycenter sweep to reduce crossings, `LR`/`TB` direction. Cycle-safe.
  `lattice.autoLayout({direction, gapMain, gapCross})` applies it as one undoable
  command and fits the view.
- **Virtualization** — the `virtualize` attribute on `<ax-lattice>` hides nodes
  outside the viewport (plus a half-viewport margin). Culled nodes keep layout so
  edges still route correctly; only painting is skipped. `lattice.virtualization`
  reports `{enabled, total, visible, culled}`.
- **Accessibility** — `<ax-lattice>` is `role="application"` with
  `aria-roledescription` and a default `aria-label`; nodes are `role="button"`
  with `aria-selected` and a content-derived `aria-label`; handles carry
  `aria-label`s. A visually-hidden `aria-live` region announces selection
  changes, deletions, connections, undo/redo, and auto-layout. New public
  `lattice.announce(message)`.
- `layout` module exported for consumers.
- `docs/API.md` — complete, frozen API reference.
- Unit test suite (`test/unit.mjs`, `node:test`) — 21 tests covering viewport,
  geometry, selection, history, and layout math.

### Changed
- **API freeze** — version 1.0.0. The element attributes, methods, events, and
  CSS Custom Properties documented in `docs/API.md` are now stable under SemVer.
- `npm test` runs the unit suite then the integration suite.

### Notes
- Subflow/group containers were evaluated for this milestone and deliberately
  deferred — multi-select group-drag already covers the common case, and a true
  nested-container model is better designed against real usage. Tracked for a
  future minor.

## [0.4.0-alpha.0] — 2026-05

Phase D — Minimap, controls, undo/redo, copy/paste.

### Added
- **Undo / redo** — command-pattern history (`history.js`). Tracks node moves
  (incl. group moves), node deletes, edge deletes, edge creates, and paste.
  `Cmd/Ctrl+Z` undoes, `Cmd/Ctrl+Shift+Z` (or `Cmd/Ctrl+Y`) redoes. Public
  API: `undo()`, `redo()`, `canUndo()`, `canRedo()`, `clearHistory()`.
- **Copy / paste** — `Cmd/Ctrl+C` copies the node selection plus any fully
  enclosed edges to an internal clipboard; `Cmd/Ctrl+V` pastes with fresh ids,
  a position offset, remapped edge references, and selects the result. Public
  API: `copy()`, `paste(offset?)`. Paste is undoable.
- `<ax-minimap>` Custom Element — a scaled overview linked to a lattice via
  `for="latticeId"` (or `.target`). Draws node rectangles and a viewport
  indicator; click or drag the minimap to navigate the lattice.
- `<ax-controls>` Custom Element — a zoom-in / zoom-out / fit / undo / redo
  toolbar linked via `for`. Undo/redo buttons reflect history state.
- `history-change` event — `{ canUndo, canRedo }`, fired on every history
  mutation.
- New CSS Custom Properties: `--ax-minimap-*`, `--ax-controls-*`.
- `History` class exported for advanced consumers.

### Testing
- Integration test extended to 27 checks — undo/redo of edge-delete and
  node-move, copy/paste, undo-of-paste, minimap registration + render. All
  passing.

## [0.3.0-alpha.0] — 2026-05

Phase C — Handles, edges & drag-to-connect.

### Added
- `<ax-handle>` Custom Element — connection ports placed inside an `<ax-node>`.
  `type` (source/target), `position` (left/right/top/bottom), optional `handle-id`.
  Renders a dot on the node edge; its lattice anchor is derived from the node box.
- `<ax-edge>` declarative Custom Element — `from` / `to` endpoint references
  (`"nodeId"` or `"nodeId:handleId"`), optional `label`. Carries no visuals; it
  is a configuration record.
- Shared SVG edge layer inside the viewport — one `<svg>`, many `<path>`s, drawn
  in lattice coordinates so edges pan/zoom with the canvas. Arrowhead markers.
- Bezier edge routing (`geometry.js`) — curves leave/enter perpendicular to each
  handle side; control-point offset scales with edge length.
- **Drag-to-connect**: press a `source` handle, drag — a dashed preview edge
  follows the pointer, valid `target` handles highlight (green glow) within a
  snap radius, release on one to create the edge.
- Edge selection by clicking the curve (a fat invisible hit-path makes it easy);
  Delete/Backspace removes selected edges; Esc deselects.
- Edges connected to a moving node re-route live (rAF-coalesced).
- `<ax-lattice>` API: `edges`, `addEdge({from,to,label})`, `selectedEdgeIds()`,
  `deleteSelectedEdges()`.
- Events: `edge-connect` (cancellable — preventDefault to create the edge
  yourself), `edge-selection-change`, `edges-delete-request` (cancellable),
  `edges-deleted`; `handle-pointerdown` / `handle-connect-start` from handles.
- New CSS Custom Properties: `--ax-edge-color`, `--ax-edge-color-sel`,
  `--ax-edge-width`, `--ax-handle-size`, `--ax-handle-bg`, `--ax-handle-border`.
- `geometry` module exported for consumers (bezier path / point / anchors).

### Fixed
- Clicking an edge now focuses the lattice host (`preventDefault` on the SVG
  hit-path's pointerdown stops the browser clearing focus to `<body>`), so
  keyboard Delete works after selecting an edge.

### Testing
- Integration test extended to 18 checks — handles, edge rendering,
  drag-to-connect, edge selection, edge delete. All passing.

## [0.2.0-alpha.0] — 2026-05

Phase B — Nodes & selection.

### Added
- `<ax-node>` Custom Element with Shadow DOM, slot for arbitrary content
- Lattice-space position via `data-x` / `data-y` attributes
- Drag-to-move with snap (uses parent lattice's `snap` attribute)
- Single-select on click, multi-select with shift/cmd-click
- Box-select on shift+drag of empty canvas (cmd+drag = add to selection)
- Empty-canvas click deselects all
- Group drag: select multiple nodes, drag any one of them, all move in lockstep
- Keyboard: Delete/Backspace, Esc deselect, ⌘A select all, Tab/Shift+Tab cycle, arrow keys nudge selected nodes (Shift = 10×)
- `<ax-lattice>` selection API: `nodes`, `selection`, `selectedIds()`, `selectAll()`, `deselectAll()`, `deleteSelected()`
- `<ax-node>` JS API: `x`, `y`, `position`, `selected`, `moveTo({x, y, snap})`, `getBox()`
- Events: `node-pointerdown` (cancellable), `node-select`, `node-movestart`, `node-moving`, `node-moveend`, `selection-change`, `nodes-delete-request` (cancellable), `nodes-deleted`
- New `--ax-node-*` CSS Custom Properties for theming
- New `--ax-marquee-fill` CSS Custom Property for box-select overlay

### Changed
- Shadow DOM restructured: pointer eater now sits behind the viewport; viewport is `pointer-events: none`; slotted nodes catch their own pointer events via `::slotted(*)` rule. Empty canvas clicks reach the eater, node clicks reach the node.

### Fixed
- Group-drag: clicking an already-selected node no longer collapses the multi-selection before the drag starts. The collapse-to-single is deferred to pointer-up and only happens on a plain click (no drag) — so dragging any node of a multi-selection moves the whole group.

### Testing
- Added `test/integration.mjs` — headless Playwright integration test covering registration, selection, single drag, group drag, and delete. `npm test` runs it against a throwaway static server. 10/10 passing.

## [0.1.0-alpha.0] — 2026-05

Phase A — Canvas foundation.

### Added
- `<ax-lattice>` Custom Element with Shadow DOM
- Infinite pan via pointer drag on empty canvas
- Zoom via wheel (zoom-toward-pointer math), pinch on touch, keyboard `+/-/0`
- Pan via keyboard arrows
- Background patterns: `dots`, `grid`, `none` (SVG `<pattern>` based)
- Snap-to-grid for client coordinate conversion
- `setViewport`, `getViewport`, `fitView`, `zoomIn`, `zoomOut`, `screenToLattice`, `latticeToScreen` programmatic API
- `viewport-change` CustomEvent, rAF-coalesced
- CSS Custom Property theming hooks (`--ax-bg`, `--ax-fg`, `--ax-grid`, `--ax-grid-size`, `--ax-accent`)
- Standalone demo at `packages/lattice/demo/index.html`
