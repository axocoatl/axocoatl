/**
 * `<ax-controls>` — a small toolbar of camera + history buttons for a lattice.
 *
 * Link it to a lattice by id:
 *   <ax-controls for="my-lattice"></ax-controls>
 *
 * Or assign directly: `controls.target = latticeElement`.
 *
 * Buttons: zoom in, zoom out, fit view, undo, redo. Undo/redo reflect the
 * lattice's history state (disabled when there is nothing to undo/redo).
 *
 * @element ax-controls
 *
 * @attr {string} for   Id of the `<ax-lattice>` to control
 *
 * @cssprop --ax-controls-bg       Toolbar background
 * @cssprop --ax-controls-border   Toolbar / button border
 * @cssprop --ax-controls-fg       Button glyph color
 */

const TEMPLATE = `
<style>
  :host {
    display: inline-flex;
    flex-direction: column;
    gap: 1px;
    background: var(--ax-controls-border, #232a37);
    border: 1px solid var(--ax-controls-border, #232a37);
    border-radius: 8px;
    overflow: hidden;
    box-shadow: 0 6px 18px rgba(0,0,0,.35);
    user-select: none;
  }
  button {
    width: 34px; height: 34px;
    display: flex; align-items: center; justify-content: center;
    background: var(--ax-controls-bg, #10131a);
    color: var(--ax-controls-fg, #e8ecf3);
    border: 0;
    cursor: pointer;
    font: 600 15px/1 'Inter', system-ui, sans-serif;
    padding: 0;
    transition: background .12s;
  }
  button:hover:not(:disabled) { background: var(--ax-controls-hover, #181d27); }
  button:active:not(:disabled) { transform: translateY(1px); }
  button:disabled { opacity: .35; cursor: default; }
  .sep { height: 1px; background: var(--ax-controls-border, #232a37); }
  svg { width: 16px; height: 16px; }
</style>
<button class="zoom-in"  title="Zoom in">+</button>
<button class="zoom-out" title="Zoom out">&minus;</button>
<button class="fit"      title="Fit view">
  <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
    <path d="M2 6V2h4M14 6V2h-4M2 10v4h4M14 10v4h-4"/>
  </svg>
</button>
<div class="sep"></div>
<button class="undo" title="Undo (Cmd/Ctrl+Z)">
  <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
    <path d="M6 4L2 8l4 4M2 8h7a4 4 0 1 1 0 8H6"/>
  </svg>
</button>
<button class="redo" title="Redo (Cmd/Ctrl+Shift+Z)">
  <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6">
    <path d="M10 4l4 4-4 4M14 8H7a4 4 0 1 0 0 8h3"/>
  </svg>
</button>
`;

export class AxControlsElement extends HTMLElement {
  static get observedAttributes() { return ['for']; }

  /** @type {ShadowRoot} */
  #root;
  /** @type {HTMLElement|null} */
  #target = null;
  /** @type {HTMLButtonElement} */
  #undoBtn;
  /** @type {HTMLButtonElement} */
  #redoBtn;
  #onHistory = (e) => this.#reflectHistory(e?.detail);

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = TEMPLATE;
    this.#undoBtn = /** @type {HTMLButtonElement} */ (this.#root.querySelector('.undo'));
    this.#redoBtn = /** @type {HTMLButtonElement} */ (this.#root.querySelector('.redo'));
  }

  connectedCallback() {
    this.#root.querySelector('.zoom-in').addEventListener('click', () => this.#target?.zoomIn?.());
    this.#root.querySelector('.zoom-out').addEventListener('click', () => this.#target?.zoomOut?.());
    this.#root.querySelector('.fit').addEventListener('click', () => this.#target?.fitView?.());
    this.#undoBtn.addEventListener('click', () => this.#target?.undo?.());
    this.#redoBtn.addEventListener('click', () => this.#target?.redo?.());
    this.#resolveTarget();
  }

  disconnectedCallback() {
    this.#detachTarget();
  }

  attributeChangedCallback(name) {
    if (name === 'for') this.#resolveTarget();
  }

  /** The lattice this toolbar controls. Assignable directly. */
  get target() { return this.#target; }
  set target(el) {
    this.#detachTarget();
    this.#target = el;
    this.#attachTarget();
  }

  #resolveTarget() {
    const id = this.getAttribute('for');
    const el = id ? document.getElementById(id) : null;
    if (el !== this.#target) {
      this.#detachTarget();
      this.#target = el;
      this.#attachTarget();
    }
  }

  #attachTarget() {
    if (!this.#target) return;
    this.#target.addEventListener('history-change', this.#onHistory);
    this.#reflectHistory({
      canUndo: this.#target.canUndo?.() ?? false,
      canRedo: this.#target.canRedo?.() ?? false,
    });
  }

  #detachTarget() {
    if (!this.#target) return;
    this.#target.removeEventListener('history-change', this.#onHistory);
  }

  #reflectHistory(state) {
    if (!state) return;
    this.#undoBtn.disabled = !state.canUndo;
    this.#redoBtn.disabled = !state.canRedo;
  }
}

if (!customElements.get('ax-controls')) {
  customElements.define('ax-controls', AxControlsElement);
}
