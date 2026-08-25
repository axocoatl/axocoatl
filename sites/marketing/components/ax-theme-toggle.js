const THEME_KEY = 'axo.marketing.theme';
const THEMES = [
  { value: 'light', label: 'Light', mark: 'L' },
  { value: 'dark', label: 'Dark', mark: 'D' },
  { value: 'system', label: 'System', mark: 'A' },
];

function applyTheme(theme) {
  if (theme === 'light' || theme === 'dark') document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
}

class AxThemeToggle extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    const saved = localStorage.getItem(THEME_KEY);
    let current = THEMES.some(({ value }) => value === saved) ? saved : 'system';
    applyTheme(current);
    this.innerHTML = `<div class="theme-toggle" role="group" aria-label="Color theme">${THEMES.map(({ value, label, mark }) => `<button type="button" data-theme-choice="${value}" aria-label="${label} theme" aria-pressed="${value === current}">${mark}</button>`).join('')}</div>`;
    this.addEventListener('click', (event) => {
      const button = event.target.closest('[data-theme-choice]');
      if (!button) return;
      current = button.dataset.themeChoice;
      localStorage.setItem(THEME_KEY, current);
      applyTheme(current);
      this.querySelectorAll('[data-theme-choice]').forEach((item) => item.setAttribute('aria-pressed', String(item === button)));
    });
  }
}

customElements.define('ax-theme-toggle', AxThemeToggle);
