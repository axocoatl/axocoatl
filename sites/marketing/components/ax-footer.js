/**
 * <ax-footer></ax-footer>
 *
 * Standard site footer: brand row, four nav columns, meta + theme
 * toggle. Lives in light DOM so global CSS controls everything.
 */
class AxFooter extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    this.innerHTML = `
      <footer>
        <div class="container">
          <div class="footer-grid">
            <div class="footer-col">
              <div class="footer-brand-row">
                <img src="/assets/mark.png" alt="" aria-hidden="true">
                <span>Axocoatl</span>
              </div>
              <p class="muted small" style="max-width: 28ch; margin: 0;">
                An open-source agentic runtime, written in Rust.
                Apache-2.0.
              </p>
            </div>
            <div class="footer-col">
              <h5>Product</h5>
              <ul>
                <li><a href="/why">Why Axocoatl</a></li>
                <li><a href="/concepts">Concepts</a></li>
                <li><a href="/showcase">Showcase</a></li>
                <li><a href="/integrations/openrouter">OpenRouter</a></li>
                <li><a href="/install">Install</a></li>
              </ul>
            </div>
            <div class="footer-col">
              <h5>Docs</h5>
              <ul>
                <li><a href="https://docs.axocoatl.ai/getting-started/" target="_blank" rel="noopener">Getting started</a></li>
                <li><a href="https://docs.axocoatl.ai/concepts/lattice/" target="_blank" rel="noopener">The lattice</a></li>
                <li><a href="https://docs.axocoatl.ai/api/http/" target="_blank" rel="noopener">HTTP API</a></li>
                <li><a href="https://docs.axocoatl.ai/api/cli/" target="_blank" rel="noopener">CLI</a></li>
              </ul>
            </div>
            <div class="footer-col">
              <h5>Community</h5>
              <ul>
                <li><a href="https://github.com/axocoatl/axocoatl" target="_blank" rel="noopener">GitHub</a></li>
                <li><a href="https://github.com/axocoatl/axocoatl/discussions" target="_blank" rel="noopener">Discussions</a></li>
                <li><a href="/changelog">Changelog</a></li>
                <li><a href="https://github.com/axocoatl/axocoatl/blob/main/CONTRIBUTING.md" target="_blank" rel="noopener">Contribute</a></li>
              </ul>
            </div>
            <div class="footer-col">
              <h5>Legal</h5>
              <ul>
                <li><a href="/pricing">Pricing</a></li>
                <li><a href="https://github.com/axocoatl/axocoatl/blob/main/LICENSE" target="_blank" rel="noopener">License</a></li>
                <li><a href="https://github.com/axocoatl/axocoatl/blob/main/SECURITY.md" target="_blank" rel="noopener">Security</a></li>
              </ul>
            </div>
          </div>
          <div class="footer-meta">
            <span>© 2026 Axocoatl contributors · Apache-2.0</span>
            <ax-theme-toggle></ax-theme-toggle>
          </div>
        </div>
      </footer>
    `;
  }
}
customElements.define('ax-footer', AxFooter);
