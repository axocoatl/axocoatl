/**
 * <ax-scripted-lattice></ax-scripted-lattice>
 *
 * The brand-asset demo: a realistic three-agent bug-fix workflow,
 * running as a passive loop. Used identically in the homepage hero
 * (inside <ax-studio-mock>) and on /concepts.
 *
 * Story: a small game called Serpent Run has a bug — the snake's
 * jump arc is too short to clear the wider boulders on level 4 and
 * the sprite visibly clips through them. The bug enters the lattice,
 * a QA agent reproduces it, an engineer patches the code, a reviewer
 * approves. Three agents, real outputs, real diff.
 *
 * Every phase fires a `phase` CustomEvent with the full inspector
 * payload so the surrounding chrome (sidebar, inspector pane, pearls)
 * stays in lockstep without re-implementing the timing.
 */
const SCRIPT_SRC = '/vendor/lattice/index.js';

if (!window.__axoLatticeLoaded) {
  window.__axoLatticeLoaded = import(SCRIPT_SRC).catch(err => {
    console.error('[ax-scripted-lattice] lattice module failed to load', err);
  });
}

// The 12-second loop. `at` is the millisecond offset within the cycle;
// `node` names the lattice node to activate at that moment.
const PHASES = [
  {
    at: 0,
    name: 'qa-active',
    node: 'qa',
    agent: 'qa-verifier',
    status: 'reproducing',
    action: 'Reproducing #273',
    detail: 'Level 4 · wide boulder · sprite clips through at frame 14',
    code: null,
  },
  {
    at: 3500,
    name: 'engineer-active',
    node: 'engineer',
    agent: 'engineer',
    status: 'patching',
    action: 'Writing patch',
    detail: 'serpent-run/jump.ts:42',
    code:
      '- const JUMP_HEIGHT = 64;\n' +
      '+ const JUMP_HEIGHT = (b) => Math.max(64, b.width * 1.2);',
  },
  {
    at: 7500,
    name: 'reviewer-active',
    node: 'reviewer',
    agent: 'reviewer',
    status: 'approving',
    action: 'Reviewing diff',
    detail: '12 LOC · 4 tests pass · approved',
    code: null,
  },
  {
    at: 11000,
    name: 'idle',
    node: null,
    agent: null,
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

    const qa       = mk('qa',       -220, 0, 'qa-verifier', 'reproduces bugs');
    const engineer = mk('engineer',    0, 0, 'engineer',    'fixes the code');
    const reviewer = mk('reviewer',  220, 0, 'reviewer',    'approves diffs');

    const mkEdge = (from, to) => {
      const e = document.createElement('ax-edge');
      e.setAttribute('from', `${from}:out`);
      e.setAttribute('to',   `${to}:in`);
      return e;
    };
    lat.append(qa, engineer, reviewer,
               mkEdge('qa', 'engineer'),
               mkEdge('engineer', 'reviewer'));

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

    const nodes = { qa, engineer, reviewer };

    // Drive the loop entirely from PHASES so timing is declarative.
    const cycle = () => {
      // Reset visuals at cycle start
      Object.values(nodes).forEach(n =>
        n.classList.remove('axo-pulse', 'axo-active', 'axo-completed'));

      PHASES.forEach((phase, idx) => {
        setTimeout(() => {
          // Mark previous node completed
          if (idx > 0 && PHASES[idx - 1].node) {
            const prev = nodes[PHASES[idx - 1].node];
            prev.classList.remove('axo-active');
            prev.classList.add('axo-completed');
          }
          // Activate current node (if any)
          if (phase.node) {
            const cur = nodes[phase.node];
            cur.classList.remove('axo-pulse');
            void cur.offsetWidth;
            cur.classList.add('axo-pulse', 'axo-active');
          }
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
