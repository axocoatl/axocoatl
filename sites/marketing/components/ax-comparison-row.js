/**
 * <ax-comparison-row label="…" left="…" right="…"></ax-comparison-row>
 * <ax-comparison-row header label="…" left="…" right="…"></ax-comparison-row>
 *
 * One row of a three-column product-fact table. Add `header` to render
 * the column labels. The table describes Axocoatl's record/repository
 * contract rather than making unsupported claims about other products.
 */
class AxComparisonRow extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    const isHeader = this.hasAttribute('header');
    const label = this.getAttribute('label') || '';
    const left = this.getAttribute('left') || '';
    const right = this.getAttribute('right') || '';
    this.innerHTML = `
      <div class="label">${this._escape(label)}</div>
      <div class="left">${this._escape(left)}</div>
      <div class="right">${this._escape(right)}</div>
    `;
  }
  _escape(s) {
    return String(s).replace(/[&<>"']/g, c => ({ '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;' }[c]));
  }
}
customElements.define('ax-comparison-row', AxComparisonRow);
