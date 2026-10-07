import { adopt } from './sheets.js';

/**
 * `<ax-loadout-graph>` shows a loadout's display graph — its Agents, its
 * required checks and its reviewer, in the order the host runs them — in a
 * read-only lattice. The lattice only displays a loadout: nothing here can
 * drag, connect, delete, paste, undo or run anything. Runs start from
 * `axocoatl run`.
 *
 * @element ax-loadout-graph
 * @prop {{nodes: Array<{id,kind,label,detail}>, edges: Array<{from,to}>}} graph
 */

const CSS = `
:host { display: block; min-height: 240px; }
ax-lattice {
  display: block; height: 300px; background: var(--bg);
  border: 1px solid var(--border); border-radius: var(--r-md);
  --ax-accent: var(--accent, var(--axo-jade));
}
ax-node {
  width: 220px; min-height: 90px; padding: var(--sp-2) var(--sp-3);
  background: var(--panel); color: var(--text); border: 1px solid var(--border);
  border-radius: var(--r-md); font: var(--fs-sm) var(--font-sans);
}
ax-node[data-kind="check"] { border-style: dashed; }
ax-node[data-kind="review"] { border-color: var(--axo-blue, var(--accent)); }
ax-node[data-kind="area_workers"] { box-shadow: 4px 4px 0 var(--border), 8px 8px 0 var(--border); }
ax-node strong { display: block; font-weight: var(--fw-medium); overflow-wrap: anywhere; }
ax-node small { display: block; color: var(--muted); font: var(--fs-xs) var(--font-mono); overflow-wrap: anywhere; }
.kind { color: var(--muted-2); font-size: var(--fs-xs); text-transform: uppercase; letter-spacing: .06em; }
.empty { color: var(--muted); padding: var(--sp-3); }
`;

const el = (tag, text) => {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = String(text);
  return node;
};

/** A lattice node id from a graph id (`agent:writer` → `agent-writer`). */
export const latticeId = (id) => String(id).replace(/[^A-Za-z0-9_-]/g, '-');

const KIND_LABEL = {
  agent: 'Agent', check: 'Required check', review: 'Required review', area_workers: 'Area workers',
};

export class AxLoadoutGraph extends HTMLElement {
  #root;
  #graph = null;
  #ready = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS);
  }

  get graph() { return this.#graph; }
  set graph(value) {
    this.#graph = value || null;
    void this.#render();
  }

  /** The lattice element, for tests and the host. */
  get lattice() { return this.#root.querySelector('ax-lattice'); }

  async #render() {
    this.#ready ||= import('/lattice/index.js');
    const lattice = await this.#ready;
    const graph = this.#graph;
    this.#root.replaceChildren();
    if (!graph || !graph.nodes?.length) {
      this.#root.append(el('p', 'This loadout has no graph to show.'));
      this.#root.lastChild.className = 'empty';
      return;
    }
    const canvas = el('ax-lattice');
    // Display only: read-only keeps the lattice in View for good.
    canvas.setAttribute('readonly', '');
    canvas.setAttribute('mode', 'view');
    canvas.setAttribute('background', 'dots');
    canvas.setAttribute('tabindex', '0');
    canvas.setAttribute('aria-label', 'Loadout graph (display only)');
    const nodes = graph.nodes.map((node) => ({ id: latticeId(node.id), width: 220, height: 110 }));
    const edges = graph.edges.map((edge) => ({ from: latticeId(edge.from), to: latticeId(edge.to) }));
    const positions = lattice.layout.layeredLayout(nodes, edges, { direction: 'LR', gapMain: 80, gapCross: 30 });
    for (const node of graph.nodes) {
      const item = el('ax-node');
      item.id = latticeId(node.id);
      item.dataset.kind = node.kind;
      const at = positions.get(item.id) || { x: 0, y: 0 };
      item.setAttribute('data-x', String(Math.round(at.x)));
      item.setAttribute('data-y', String(Math.round(at.y)));
      item.append(el('span', KIND_LABEL[node.kind] || node.kind));
      item.lastChild.className = 'kind';
      item.append(el('strong', node.label));
      for (const fact of node.detail || []) item.append(el('small', fact));
      for (const [type, position, id] of [['target', 'left', 'in'], ['source', 'right', 'out']]) {
        const handle = el('ax-handle');
        handle.setAttribute('type', type);
        handle.setAttribute('position', position);
        handle.setAttribute('handle-id', id);
        item.append(handle);
      }
      canvas.append(item);
    }
    for (const edge of edges) {
      const line = el('ax-edge');
      line.setAttribute('from', `${edge.from}:out`);
      line.setAttribute('to', `${edge.to}:in`);
      canvas.append(line);
    }
    this.#root.append(canvas);
    requestAnimationFrame(() => { if (this.isConnected) canvas.fitView?.({ maxZoom: 1 }); });
  }
}

if (!customElements.get('ax-loadout-graph')) customElements.define('ax-loadout-graph', AxLoadoutGraph);
