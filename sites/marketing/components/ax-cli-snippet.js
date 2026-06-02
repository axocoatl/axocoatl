/**
 * <ax-cli-snippet>cargo install axocoatl-cli</ax-cli-snippet>
 *
 * Renders a terminal-style copy block. Click anywhere to copy the
 * inner text; the ::after pseudo-element flips to "copied" for 1.4s.
 */
class AxCliSnippet extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    this.addEventListener('click', async (e) => {
      e.preventDefault();
      const text = (this.textContent || '').trim();
      try {
        await navigator.clipboard.writeText(text);
        this.setAttribute('data-copied', '');
        clearTimeout(this._t);
        this._t = setTimeout(() => this.removeAttribute('data-copied'), 1400);
      } catch (err) {
        // Older browsers / no permission — fall back to a selection
        const range = document.createRange();
        range.selectNodeContents(this);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(range);
      }
    });
  }
}
customElements.define('ax-cli-snippet', AxCliSnippet);
