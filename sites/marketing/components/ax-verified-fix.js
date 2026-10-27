/** Manual navigation for an actual-run excerpt. All evidence stays visible without JS. */
class AxVerifiedFix extends HTMLElement {
  connectedCallback() {
    if (this._initialized) return;
    const panels = [...this.children].filter((child) => child.matches('section[data-step]'));
    if (panels.length !== 4 || panels.some((panel) => !panel.id || !panel.querySelector('h3'))) return;

    const controls = document.createElement('div');
    controls.className = 'verified-fix-controls';
    controls.setAttribute('role', 'group');
    controls.setAttribute('aria-label', 'Choose an evidence step');
    const status = document.createElement('p');
    status.className = 'verified-fix-status';
    status.setAttribute('role', 'status');
    status.setAttribute('aria-live', 'polite');
    status.setAttribute('aria-atomic', 'true');
    const buttons = panels.map((panel, index) => {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'verified-fix-step';
      button.textContent = `${index + 1}. ${panel.dataset.step}`;
      button.setAttribute('aria-controls', panel.id);
      button.addEventListener('click', () => select(index));
      controls.append(button);
      return button;
    });
    const select = (selected) => {
      panels.forEach((panel, index) => {
        panel.hidden = index !== selected;
        buttons[index].setAttribute('aria-pressed', String(index === selected));
      });
      status.textContent = `Step ${selected + 1} of ${panels.length}: ${panels[selected].dataset.step}`;
    };

    this.prepend(controls);
    this.append(status);
    select(0);
    this._initialized = true;
  }
}
customElements.define('ax-verified-fix', AxVerifiedFix);
