/**
 * <ax-site-nav active="why"></ax-site-nav>
 *
 * Sticky top navigation. Pass `active` to highlight the current page.
 * Light-DOM render so finder.css styles apply.
 */
const LINKS = [
  { id: 'why',       href: '/why',       label: 'Why Axocoatl' },
  { id: 'concepts',  href: '/concepts',  label: 'Concepts' },
  { id: 'showcase',  href: '/showcase',  label: 'Showcase' },
];

class AxSiteNav extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    const active = this.getAttribute('active') || '';
    const linkHtml = LINKS.map(l =>
      `<a href="${l.href}" ${l.id === active ? 'class="active"' : ''}>${l.label}</a>`
    ).join('');
    this.innerHTML = `
      <nav class="site-nav">
        <div class="container site-nav-inner">
          <a class="site-nav-brand unstyled" href="/">
            <img src="/assets/mark.png" alt="" aria-hidden="true">
            <span>Axocoatl</span>
          </a>
          <div class="site-nav-links">
            ${linkHtml}
            <a href="https://docs.axocoatl.ai" target="_blank" rel="noopener">Docs</a>
            <a href="https://github.com/axocoatl/axocoatl" target="_blank" rel="noopener">GitHub</a>
            <a class="site-nav-cta" href="/install">Install</a>
          </div>
        </div>
      </nav>
    `;
  }
}
customElements.define('ax-site-nav', AxSiteNav);
