/**
 * <ax-comparison-row label="…" them="…" us="…"></ax-comparison-row>
 * <ax-comparison-row header label="…" them="…" us="…"></ax-comparison-row>
 *
 * One row of the "us vs. them" comparison table. Add `header` to render
 * as the table header. Names are anonymous on purpose ("framework" vs
 * "runtime"); we don't call out specific competitors on-site.
 */
class AxComparisonRow extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    const isHeader = this.hasAttribute('header');
    const label = this.getAttribute('label') || '';
    const them = this.getAttribute('them') || '';
    const us = this.getAttribute('us') || '';
    this.innerHTML = `
      <div class="label">${this._escape(label)}</div>
      <div class="them">${this._escape(them)}</div>
      <div class="us">${this._escape(us)}</div>
    `;
  }
  _escape(s) {
    return String(s).replace(/[&<>"']/g, c => ({ '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;' }[c]));
  }
}
customElements.define('ax-comparison-row', AxComparisonRow);
