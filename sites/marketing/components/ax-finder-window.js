/**
 * <ax-finder-window title="…" [no-sidebar]>
 *   <slot name="sidebar">…</slot>
 *   <slot>…</slot>
 * </ax-finder-window>
 *
 * Renders a macOS Finder–style window: title bar with traffic lights,
 * optional left sidebar, content area. Pure light-DOM composition so
 * the host page's CSS (finder.css) controls everything.
 */
class AxFinderWindow extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    const title = this.getAttribute('title') || '';
    const noSidebar = this.hasAttribute('no-sidebar');
    // Capture existing children into named slots
    const sidebarHTML = this.querySelector('[slot="sidebar"]')?.outerHTML ?? '';
    const mainHTML = Array.from(this.children)
      .filter(c => c.getAttribute('slot') !== 'sidebar')
      .map(c => c.outerHTML)
      .join('');
    this.innerHTML = `
      <div class="finder-titlebar">
        <span class="finder-lights">
          <span class="finder-light red"></span>
          <span class="finder-light amber"></span>
          <span class="finder-light green"></span>
        </span>
        <span class="finder-title">${this._escape(title)}</span>
      </div>
      <div class="finder-body${noSidebar ? ' no-sidebar' : ''}">
        ${noSidebar ? '' : `<aside class="finder-sidebar">${sidebarHTML}</aside>`}
        <main class="finder-main">${mainHTML}</main>
      </div>
    `;
  }
  _escape(s) {
    return String(s).replace(/[&<>"']/g, c => ({ '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;' }[c]));
  }
}
customElements.define('ax-finder-window', AxFinderWindow);
