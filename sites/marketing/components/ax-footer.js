class AxFooter extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    this.innerHTML = `
      <footer>
        <div class="container">
          <div class="footer-grid">
            <div class="footer-col">
              <div class="footer-brand-row"><img src="/assets/favicon.png" alt="" width="28" height="28"><span>Axocoatl</span></div>
              <p class="muted small">An open-source, local-first coding workbench backed by a durable Rust runtime.</p>
            </div>
            <div class="footer-col">
              <h2>Product</h2>
              <ul><li><a href="/concepts">How it works</a></li><li><a href="/why">Why Axocoatl</a></li><li><a href="/showcase">Demos</a></li><li><a href="/pricing">Pricing</a></li></ul>
            </div>
            <div class="footer-col">
              <h2>Start</h2>
              <ul><li><a href="/install">Install</a></li><li><a href="https://docs.axocoatl.ai/start/install/">Getting started</a></li><li><a href="https://docs.axocoatl.ai/operate/troubleshooting/">Troubleshooting</a></li><li><a href="/integrations/openrouter">OpenRouter</a></li></ul>
            </div>
            <div class="footer-col">
              <h2>Project</h2>
              <ul><li><a href="https://github.com/axocoatl/axocoatl">Source</a></li><li><a href="https://github.com/axocoatl/axocoatl/discussions">Discussions</a></li><li><a href="/changelog">Changelog</a></li><li><a href="https://github.com/axocoatl/axocoatl/blob/main/SECURITY.md">Security</a></li></ul>
            </div>
          </div>
          <div class="footer-meta"><span>© 2026 Axocoatl contributors · Apache-2.0</span><ax-theme-toggle></ax-theme-toggle></div>
        </div>
      </footer>`;
  }
}

customElements.define('ax-footer', AxFooter);
