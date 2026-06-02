/**
 * <ax-install-bar></ax-install-bar>
 *
 * Sticky bottom install bar. Slides up once the user scrolls past the
 * hero. Click anywhere to copy the install command. Stays hidden if
 * the user has already dismissed it (sessionStorage flag).
 */
const KEY = 'axo.install-bar.dismissed';
const CMD = 'curl -fsSL https://axocoatl.ai/install.sh | sh';

class AxInstallBar extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;

    if (sessionStorage.getItem(KEY)) return;

    this.innerHTML = `
      <div class="install-bar-inner">
        <code>${CMD}</code>
        <button class="btn btn-primary" data-action="copy">Copy</button>
        <button class="btn btn-ghost" data-action="dismiss" aria-label="Dismiss" style="padding:7px 10px;">×</button>
      </div>
    `;
    this.classList.add('install-bar');

    document.body.appendChild(this);

    const codeEl = this.querySelector('code');
    const copyBtn = this.querySelector('[data-action="copy"]');
    const dismiss = this.querySelector('[data-action="dismiss"]');

    const doCopy = async () => {
      try {
        await navigator.clipboard.writeText(CMD);
        const original = copyBtn.textContent;
        copyBtn.textContent = 'Copied';
        setTimeout(() => { copyBtn.textContent = original; }, 1400);
      } catch {}
    };
    copyBtn.addEventListener('click', doCopy);
    codeEl.addEventListener('click', doCopy);
    dismiss.addEventListener('click', () => {
      sessionStorage.setItem(KEY, '1');
      this.classList.remove('show');
      setTimeout(() => this.remove(), 280);
    });

    // Show after the user scrolls past ~80% of viewport height.
    const onScroll = () => {
      if (window.scrollY > window.innerHeight * 0.8) {
        this.classList.add('show');
      } else {
        this.classList.remove('show');
      }
    };
    window.addEventListener('scroll', onScroll, { passive: true });
    onScroll();
  }
}
customElements.define('ax-install-bar', AxInstallBar);
