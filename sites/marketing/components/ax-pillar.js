/**
 * <ax-pillar glyph="◉">
 *   <h3>Built for production</h3>
 *   <p>…</p>
 * </ax-pillar>
 *
 * Light-DOM pillar block. The `glyph` attribute renders as a small
 * monospace mark above the title, matching the dashboard's typographic
 * inline-glyph language.
 */
class AxPillar extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    this.classList.add('pillar');
    const glyph = this.getAttribute('glyph');
    if (glyph) {
      const g = document.createElement('span');
      g.className = 'pillar-glyph';
      g.textContent = glyph;
      this.prepend(g);
    }
  }
}
customElements.define('ax-pillar', AxPillar);
