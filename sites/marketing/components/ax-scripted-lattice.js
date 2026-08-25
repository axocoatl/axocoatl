/**
 * <ax-scripted-lattice></ax-scripted-lattice>
 *
 * A scripted event-flow illustration, not a product capture or workflow
 * executor. It shows a Skill publishing one typed lattice event and the
 * notification reaching two consumers. An Automation that receives the
 * event still executes its own explicit DAG; the lattice does not activate
 * agent nodes in sequence.
 *
 * Every phase fires a `phase` CustomEvent so an embedding can add its own
 * explanatory chrome without duplicating the timing.
 */
const SCRIPT_SRC = '/vendor/lattice/index.js';

if (!window.__axoLatticeLoaded) {
  window.__axoLatticeLoaded = import(SCRIPT_SRC).catch(err => {
    console.error('[ax-scripted-lattice] lattice module failed to load', err);
  });
}

// The 12-second loop. `at` is the millisecond offset within the cycle;
// `nodes` names the event-flow nodes to highlight at that moment.
const PHASES = [
  {
    at: 0,
    name: 'skill-publishes',
    nodes: ['publisher'],
    status: 'publishing',
    action: 'Skill completed',
    detail: 'Publishes ReviewReady with a typed payload',
    code: null,
  },
  {
    at: 3000,
    name: 'event-published',
    nodes: ['event'],
    status: 'published',
    action: 'ReviewReady',
    detail: 'The event feed records producer, payload, and timestamp',
    code: null,
  },
  {
    at: 6000,
    name: 'subscribers-notified',
    nodes: ['automation', 'webhook'],
    status: 'notified',
    action: 'Subscribers receive the event',
    detail: 'Automation trigger · optional webhook',
    code: null,
  },
  {
    at: 10000,
    name: 'idle',
    nodes: [],
    status: null,
    action: null,
    detail: null,
    code: null,
  },
];

class AxScriptedLattice extends HTMLElement {
  async connectedCallback() {
    if (this._wired) return;
    this._wired = true;

    this.innerHTML = `<ax-lattice background="dots" snap="20" min-zoom="0.4" max-zoom="2"></ax-lattice>`;

    await window.__axoLatticeLoaded;
    await customElements.whenDefined('ax-lattice');
    await customElements.whenDefined('ax-node');
    await customElements.whenDefined('ax-handle');
    await customElements.whenDefined('ax-edge');

    const lat = this.querySelector('ax-lattice');
    if (!lat) return;

    const mk = (id, x, y, title, sub) => {
      const n = document.createElement('ax-node');
      n.id = id;
      n.setAttribute('data-x', x);
      n.setAttribute('data-y', y);
      const t = document.createElement('div');
      t.style.cssText = 'font-weight:600;font-size:13px;color:var(--fg);letter-spacing:-.005em';
      t.textContent = title;
      const s = document.createElement('div');
      s.style.cssText = 'font-size:10.5px;color:var(--muted);margin-top:3px;font-family:JetBrains Mono,ui-monospace,monospace';
      s.textContent = sub;
      n.append(t, s);

      const ho = document.createElement('ax-handle');
      ho.setAttribute('type', 'source');
      ho.setAttribute('handle-id', 'out');
      ho.setAttribute('position', 'right');
      const hi = document.createElement('ax-handle');
      hi.setAttribute('type', 'target');
      hi.setAttribute('handle-id', 'in');
      hi.setAttribute('position', 'left');
      n.append(hi, ho);
      return n;
    };

    const publisher  = mk('publisher', -260,    0, 'Skill completed', 'publishes ReviewReady');
    const event      = mk('event',        0,    0, 'ReviewReady', 'typed lattice event');
    const automation = mk('automation', 260,  -70, 'Automation', 'starts an explicit DAG');
    const webhook    = mk('webhook',    260,   70, 'Webhook', 'optional delivery');

    const mkEdge = (from, to) => {
      const e = document.createElement('ax-edge');
      e.setAttribute('from', `${from}:out`);
      e.setAttribute('to',   `${to}:in`);
      return e;
    };
    lat.append(publisher, event, automation, webhook,
               mkEdge('publisher', 'event'),
               mkEdge('event', 'automation'),
               mkEdge('event', 'webhook'));

    // Wait two frames so the lattice has a real bounding box before
    // we ask it to fit the viewport. Then nudge the viewport downward
    // by a small fixed offset so the nodes sit lower in the canvas,
    // leaving more breathing room above (where the inspector pane
    // lives). Zoom (k) is preserved exactly.
    requestAnimationFrame(() => requestAnimationFrame(() => {
      lat.fitView?.();
      // Nudge the viewport so the nodes sit lower in the canvas without
      // changing zoom — gives more headroom for the inspector pane.
      if (typeof lat.getViewport === 'function' && typeof lat.setViewport === 'function') {
        const vp = lat.getViewport();
        lat.setViewport({ x: vp.x, y: vp.y + 60, k: vp.k });
      }
    }));

    const nodes = { publisher, event, automation, webhook };

    // Drive the loop entirely from PHASES so timing is declarative.
    const cycle = () => {
      // Reset visuals at cycle start
      Object.values(nodes).forEach(n =>
        n.classList.remove('axo-pulse', 'axo-active', 'axo-completed'));

      PHASES.forEach((phase, idx) => {
        setTimeout(() => {
          // Mark every node from the previous phase completed.
          if (idx > 0) {
            PHASES[idx - 1].nodes.forEach((id) => {
              const prev = nodes[id];
              prev.classList.remove('axo-active');
              prev.classList.add('axo-completed');
            });
          }
          // A published event fans out to its subscribers; these highlights
          // are notifications, not sequential agent activation.
          phase.nodes.forEach((id) => {
            const cur = nodes[id];
            cur.classList.remove('axo-pulse');
            void cur.offsetWidth;
            cur.classList.add('axo-pulse', 'axo-active');
          });
          // Clear all completed on idle so the cycle visual resets
          if (phase.name === 'idle') {
            Object.values(nodes).forEach(n => n.classList.remove('axo-completed'));
          }
          // Tell everyone listening what just happened
          this.dispatchEvent(new CustomEvent('phase', {
            detail: phase, bubbles: true,
          }));
        }, phase.at);
      });
    };

    cycle();
    setInterval(cycle, 12000);
  }
}
customElements.define('ax-scripted-lattice', AxScriptedLattice);
