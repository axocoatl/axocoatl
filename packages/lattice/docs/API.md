# @axocoatl/lattice — API Reference (v1.1.0)

The public API below is **stable** since 1.0.0 and follows [SemVer](https://semver.org).
1.1.0 added the execution-state surface (additive — no breaking changes).
Breaking changes will only land in a 2.0.

- [Custom Elements](#custom-elements)
  - [`<ax-lattice>`](#ax-lattice)
  - [`<ax-node>`](#ax-node)
  - [`<ax-handle>`](#ax-handle)
  - [`<ax-edge>`](#ax-edge)
  - [`<ax-minimap>`](#ax-minimap)
  - [`<ax-controls>`](#ax-controls)
- [Events](#events)
- [CSS Custom Properties](#css-custom-properties)
- [Pure modules](#pure-modules)
- [Keyboard reference](#keyboard-reference)
- [Accessibility](#accessibility)

---

## Custom Elements

Importing the package registers all six elements:

```js
import '@axocoatl/lattice';
```

### `<ax-lattice>`

The canvas. Holds nodes and edges, owns pan/zoom, selection, history.

**Attributes**

| Attribute | Type | Default | Description |
|---|---|---|---|
| `zoom` | number | `1` | Current zoom level |
| `min-zoom` | number | `0.2` | Minimum zoom |
| `max-zoom` | number | `3` | Maximum zoom |
| `pan-x` / `pan-y` | number | `0` | Pan offset |
| `snap` | number | `0` | Grid snap size in lattice units; `0` disables |
| `background` | `dots`\|`grid`\|`none` | `dots` | Background pattern |
| `fit-view-on-init` | boolean | — | Auto fit-view once after first render |
| `virtualize` | boolean | — | Skip painting off-screen nodes |

**Methods**

| Method | Returns | Description |
|---|---|---|
| `getViewport()` | `{x,y,k}` | Current pan + zoom |
| `setViewport({x?,y?,k?})` | — | Set pan + zoom (k is clamped) |
| `fitView({padding?})` | — | Frame all nodes |
| `zoomIn(factor?)` / `zoomOut(factor?)` | — | Step zoom, center-anchored |
| `screenToLattice({x,y})` | `{x,y}` | Canvas-local px → lattice coords |
| `latticeToScreen({x,y})` | `{x,y}` | Lattice coords → canvas-local px |
| `snap(value)` | number | Snap a coordinate to the grid |
| `nodes` *(getter)* | `Set` | Registered `<ax-node>`s |
| `edges` *(getter)* | `Set` | Registered `<ax-edge>`s |
| `selection` *(getter)* | `Set` | Selected nodes |
| `selectedIds()` | `string[]` | Selected node ids |
| `selectedEdgeIds()` | `string[]` | Selected edge ids |
| `selectAll()` / `deselectAll()` | — | Selection ops |
| `deleteSelected()` | — | Delete selected nodes (undoable) |
| `deleteSelectedEdges()` | — | Delete selected edges (undoable) |
| `addEdge({from,to,label?})` | `<ax-edge>` | Create an edge (undoable) |
| `undo()` / `redo()` | — | History navigation |
| `canUndo()` / `canRedo()` | boolean | History state |
| `clearHistory()` | — | Drop all undo/redo state |
| `copy()` | number | Copy selection to the internal clipboard |
| `paste(offset?)` | `<ax-node>[]` | Paste the clipboard (undoable) |
| `autoLayout({direction?,gapMain?,gapCross?})` | — | Layered DAG layout (undoable) |
| `virtualization` *(getter)* | `{enabled,total,visible,culled}` | Virtualization stats |
| `announce(message)` | — | Speak a message via the live region |
| `setNodeStatus(id, status)` | — | Set a node's execution status |
| `setEdgeActive(idOr{from,to}, bool)` | — | Toggle an edge's flowing-active state |
| `resetStatuses()` | — | Clear all node statuses + active edges |

### `<ax-node>` execution state

`node.status` — one of `idle` / `pending` / `running` / `success` / `error`.
Set via the `status` attribute or the property. `running` pulses and sets
`aria-busy`; the minimap tints nodes by status.

### `<ax-edge>` execution state

`edge.active` (the `active` attribute) — renders the curve as an animated
flowing dash from source toward target.

### `<ax-node>`

A draggable, selectable node. Place any content inside; it renders in a slot.

**Attributes:** `data-x`, `data-y` (lattice position), `data-w`, `data-h`
(optional size hints), `selected`, `draggable` (`"false"` locks it).

**Properties:** `x`, `y`, `position`, `selected`, `moveTo({x,y,snap?})`, `getBox()`.

### `<ax-handle>`

A connection port. Place inside an `<ax-node>`.

**Attributes:** `type` (`source`\|`target`), `position` (`left`\|`right`\|`top`\|`bottom`),
`handle-id` (unique within the node — edges address it as `nodeId:handleId`).

### `<ax-edge>`

A declarative edge. Carries no visuals — the lattice renders it.

**Attributes:** `from`, `to` (endpoint refs: `"nodeId"` or `"nodeId:handleId"`),
`label`, `selected`.

### `<ax-minimap>`

Scaled overview with a viewport indicator. Click or drag to navigate.

**Attributes:** `for` (lattice id). **Property:** `target` (assign the element directly).

### `<ax-controls>`

Zoom / fit / undo / redo toolbar.

**Attributes:** `for` (lattice id). **Property:** `target`.

---

## Events

All bubble and are `composed`. Listen on the `<ax-lattice>`.

| Event | `detail` | When |
|---|---|---|
| `viewport-change` | `{x,y,k}` | Pan/zoom (rAF-coalesced) |
| `selection-change` | `{ids,count}` | Node selection changed |
| `edge-selection-change` | `{ids}` | Edge selection changed |
| `node-movestart` / `node-moving` / `node-moveend` | `{x,y,dx?,dy?}` | Node drag lifecycle |
| `nodes-delete-request` | `{ids}` | Before node delete — **cancelable** |
| `nodes-deleted` | `{ids}` | After node delete |
| `edges-delete-request` | `{ids}` | Before edge delete — **cancelable** |
| `edges-deleted` | `{ids}` | After edge delete |
| `edge-connect` | `{from,to}` | Drag-to-connect completed — **cancelable** (preventDefault to create the edge yourself) |
| `history-change` | `{canUndo,canRedo}` | History mutated |

---

## CSS Custom Properties

Set these on `<ax-lattice>` (or any ancestor):

| Property | Default | Effect |
|---|---|---|
| `--ax-bg` | `#0a0c11` | Canvas background |
| `--ax-fg` | `#e8ecf3` | Foreground / text |
| `--ax-grid` | `#232a37` | Grid dot/line color |
| `--ax-accent` | `#7c5cff` | Selection / highlight |
| `--ax-accent-2` | `#00d9b1` | Connection target highlight |
| `--ax-marquee-fill` | `rgba(124,92,255,.12)` | Box-select overlay |
| `--ax-edge-color` / `--ax-edge-color-sel` | `#4a536a` / accent | Edge stroke |
| `--ax-edge-color-active` | `--ax-accent-2` | Flowing-edge stroke |
| `--ax-edge-width` | `2` | Edge stroke width |
| `--ax-node-bg` / `-fg` / `-border` / `-border-sel` | — | Node theming |
| `--ax-node-pending` / `-running` / `-running-glow` / `-success` / `-error` | — | Per-status node theming |
| `--ax-handle-size` / `-bg` / `-border` | — | Handle theming |
| `--ax-minimap-*` / `--ax-controls-*` | — | Overlay theming |

---

## Pure modules

DOM-free, individually importable, unit-tested:

```js
import { clamp, zoomAt, fitView } from '@axocoatl/lattice/viewport';
import { bezierPath, autoAnchor } from '@axocoatl/lattice/geometry';
import { layeredLayout } from '@axocoatl/lattice/layout';
import { History } from '@axocoatl/lattice/history';
```

---

## Keyboard reference

The lattice must be focused (click it once).

| Keys | Action |
|---|---|
| Drag empty canvas | Pan |
| Wheel / pinch | Zoom |
| `+` `-` `0` | Zoom in / out / fit |
| Arrows | Pan — or nudge selected nodes |
| Shift+Arrows | Nudge ×10 |
| Click / Shift-click | Select / multi-select |
| Shift+drag empty | Box-select |
| `⌘/Ctrl+A` | Select all |
| `Esc` | Deselect |
| `Delete` / `Backspace` | Delete selection |
| `Tab` / `Shift+Tab` | Cycle selection |
| `⌘/Ctrl+Z` / `⌘/Ctrl+Shift+Z` | Undo / redo |
| `⌘/Ctrl+C` / `⌘/Ctrl+V` | Copy / paste |

---

## Accessibility

- `<ax-lattice>` is `role="application"` with `aria-roledescription="graph editor"`
  and a default `aria-label` (override by setting your own).
- Nodes are `role="button"` with `aria-selected` reflecting state and an
  `aria-label` derived from their text content.
- Handles carry descriptive `aria-label`s.
- A visually-hidden `aria-live` region announces selection changes, deletions,
  connections, undo/redo, and auto-layout. Call `lattice.announce(msg)` to add
  your own announcements.
