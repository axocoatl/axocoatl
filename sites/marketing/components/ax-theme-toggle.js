/**
 * <ax-theme-toggle></ax-theme-toggle>
 *
 * Three-state segmented control: light / dark / system. Persists the
 * choice in localStorage under `axo.marketing.theme`. Applies via the
 * [data-theme] attribute on <html>.
 */
const KEY = 'axo.marketing.theme';

function apply(pref) {
  const root = document.documentElement;
  if (pref === 'light' || pref === 'dark') {
    root.setAttribute('data-theme', pref);
  } else {
    root.removeAttribute('data-theme');
  }
}

class AxThemeToggle extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    const initial = localStorage.getItem(KEY) || 'system';
    apply(initial);
    this.innerHTML = `
      <div role="group" aria-label="Theme" style="display:inline-flex; gap:1px; padding:2px; background:var(--bg-2); border:1px solid var(--border); border-radius:7px;">
        <button data-pref="light"  title="Light">☀</button>
        <button data-pref="dark"   title="Dark">☾</button>
        <button data-pref="system" title="System">⊙</button>
      </div>
    `;
    this.querySelectorAll('button').forEach(b => {
      const isActive = b.dataset.pref === initial;
      if (isActive) b.style.background = 'var(--panel)';
      Object.assign(b.style, {
        background: isActive ? 'var(--panel)' : 'transparent',
        border: '0', padding: '4px 8px', borderRadius: '5px',
        color: isActive ? 'var(--accent)' : 'var(--muted)',
        cursor: 'pointer', fontSize: '13px', lineHeight: '1',
      });
      b.addEventListener('click', () => {
        localStorage.setItem(KEY, b.dataset.pref);
        apply(b.dataset.pref);
        this.querySelectorAll('button').forEach(x => {
          x.style.background = x === b ? 'var(--panel)' : 'transparent';
          x.style.color = x === b ? 'var(--accent)' : 'var(--muted)';
        });
      });
    });
  }
}
customElements.define('ax-theme-toggle', AxThemeToggle);
