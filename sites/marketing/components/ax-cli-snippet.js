/**
 * <ax-cli-snippet>cargo install axocoatl-cli</ax-cli-snippet>
 *
 * Renders a keyboard-accessible terminal-style copy button. The
 * ::after pseudo-element flips to "copied" for 1.4s after success.
 */
class AxCliSnippet extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;

    this._command = (this.textContent || '').trim();
    this.setAttribute('role', 'button');
    if (!this.hasAttribute('tabindex')) this.tabIndex = 0;
    if (!this.hasAttribute('aria-label')) {
      this.setAttribute('aria-label', `Copy command: ${this._command}`);
    }

    this._commandText = document.createElement('span');
    this._commandText.className = 'cli-command';
    this._commandText.textContent = this._command;
    this._status = document.createElement('span');
    this._status.className = 'copy-status';
    this._status.setAttribute('aria-live', 'polite');
    this._status.setAttribute('aria-atomic', 'true');
    this.replaceChildren(this._commandText, this._status);

    this.addEventListener('click', (event) => {
      event.preventDefault();
      void this.copyCommand();
    });
    this.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter' && event.key !== ' ') return;
      event.preventDefault();
      void this.copyCommand();
    });
  }

  async copyCommand() {
    try {
      await navigator.clipboard.writeText(this._command);
      this.setAttribute('data-copied', '');
      this._status.textContent = 'Command copied.';
      clearTimeout(this._t);
      this._t = setTimeout(() => {
        this.removeAttribute('data-copied');
        this._status.textContent = '';
      }, 1400);
    } catch (error) {
      // Older browsers or denied clipboard permission: select only the
      // authored command, leaving the live status outside the selection.
      const range = document.createRange();
      range.selectNodeContents(this._commandText);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      this._status.textContent = 'Clipboard access failed. Command selected; press Control or Command plus C.';
    }
  }
}
customElements.define('ax-cli-snippet', AxCliSnippet);
