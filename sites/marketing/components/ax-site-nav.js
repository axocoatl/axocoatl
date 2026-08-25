const LINKS = [
  { id: 'product', href: '/concepts', label: 'Product' },
  { id: 'why', href: '/why', label: 'Why Axocoatl' },
  { id: 'examples', href: '/showcase', label: 'Demos' },
];

class AxSiteNav extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    const active = this.getAttribute('active') || '';
    const links = LINKS.map(({ id, href, label }) =>
      `<a href="${href}"${id === active ? ' class="active" aria-current="page"' : ''}>${label}</a>`
    ).join('');
    this.innerHTML = `
      <a class="skip-link" href="#main-content">Skip to content</a>
      <nav class="site-nav" aria-label="Primary navigation">
        <div class="container site-nav-inner">
          <a class="site-nav-brand unstyled" href="/" aria-label="Axocoatl home">
            <img src="/assets/favicon.png" alt="" width="28" height="28">
            <span>Axocoatl</span>
          </a>
          <button class="site-nav-menu" type="button" aria-expanded="false" aria-controls="site-nav-links" aria-label="Open navigation"><span></span></button>
          <div class="site-nav-links" id="site-nav-links">
            ${links}
            <a href="https://docs.axocoatl.ai">Docs</a>
            <a href="https://github.com/axocoatl/axocoatl">Source</a>
            <a class="site-nav-cta" href="/install">Install</a>
          </div>
        </div>
      </nav>`;

    const button = this.querySelector('.site-nav-menu');
    const panel = this.querySelector('.site-nav-links');
    const close = () => {
      button.setAttribute('aria-expanded', 'false');
      button.setAttribute('aria-label', 'Open navigation');
      panel.classList.remove('open');
      document.body.classList.remove('nav-open');
    };
    button.addEventListener('click', () => {
      const open = button.getAttribute('aria-expanded') !== 'true';
      button.setAttribute('aria-expanded', String(open));
      button.setAttribute('aria-label', open ? 'Close navigation' : 'Open navigation');
      panel.classList.toggle('open', open);
      document.body.classList.toggle('nav-open', open);
    });
    panel.addEventListener('click', (event) => { if (event.target.closest('a')) close(); });
    document.addEventListener('keydown', (event) => { if (event.key === 'Escape') close(); });
    matchMedia('(min-width: 821px)').addEventListener('change', (event) => { if (event.matches) close(); });
  }
}

customElements.define('ax-site-nav', AxSiteNav);
