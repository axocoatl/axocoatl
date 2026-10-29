import { adopt } from './sheets.js';

/** Signed evidence for one exact native Session turn. Reads never replace the
 * recorded result with the present working tree. The daemon signs captured
 * terminal evidence on the first download and retains that envelope. */
const terminal = new Set(['completed', 'finished', 'cancelled']);
const words = value => String(value || 'unknown').replaceAll('_', ' ');
const node = (tag, text, className) => { const item = document.createElement(tag); if (text != null) item.textContent = text; if (className) item.className = className; return item; };
const CSS = `
:host{display:block;margin:10px 0;color:var(--text);font:var(--fs-sm)/1.5 var(--font-sans)}
*{box-sizing:border-box}.receipt{padding:12px;border:1px solid var(--border);border-radius:var(--r-md);background:var(--panel)}
h3{font-size:var(--fs-sm);margin:0 0 6px}p{margin:5px 0;overflow-wrap:anywhere}.muted{color:var(--muted)}.error{color:var(--err)}
.actions{display:flex;align-items:center;gap:12px;flex-wrap:wrap;margin-top:8px}a{color:var(--text);text-decoration-color:var(--accent);text-underline-offset:3px}button{font:inherit;color:var(--text);background:var(--panel-2);border:1px solid var(--border);border-radius:var(--r-sm);padding:5px 8px;cursor:pointer}button:disabled{opacity:.5;cursor:default}
a:focus-visible,button:focus-visible{outline:2px solid var(--accent);outline-offset:3px}.key{font-family:var(--font-mono);font-size:var(--fs-xs)}
`;

export function turnHasReceipt(state) { return terminal.has(state); }

export class AxTurnReceipt extends HTMLElement {
  static observedAttributes = ['session-id', 'turn-id'];
  constructor() { super(); this.attachShadow({mode:'open'}); void adopt(this.shadowRoot, CSS, ['/ui/tokens.css']); this.generation = 0; this.downloadGeneration = 0; }
  get sessionId() { return this.getAttribute('session-id') || ''; }
  get turnId() { return this.getAttribute('turn-id') || ''; }
  get filename() { return `${this.turnId.replace(/[^a-zA-Z0-9._-]/g, '_') || 'turn'}.receipt.dsse.json`; }
  get path() { return `/api/sessions/${encodeURIComponent(this.sessionId)}/turns/${encodeURIComponent(this.turnId)}/receipt`; }
  connectedCallback() {
    this.activated = false;
    this.render();
    this.observer = new IntersectionObserver(entries => {
      if (!entries.some(entry => entry.isIntersecting) || !this.isConnected) return;
      this.observer?.disconnect(); this.activated = true; void this.refresh();
    });
    this.observer.observe(this);
  }
  disconnectedCallback() { this.observer?.disconnect(); this.activated = false; this.generation++; this.downloadGeneration++; this.downloading = false; clearTimeout(this.timer); }
  attributeChangedCallback(_name, oldValue, value) { if (oldValue !== value) { this.value = null; this.pendingReads = 0; this.downloadGeneration++; this.downloading = false; if (this.isConnected && this.activated) void this.refresh(); } }
  async refresh() {
    clearTimeout(this.timer);
    if (!this.sessionId || !this.turnId) return;
    const generation = ++this.generation; this.error = ''; this.loading = true; this.render();
    try {
      const response = await fetch(`${this.path}/status`, {cache:'no-store'});
      const value = await response.json();
      if (generation !== this.generation || !this.isConnected) return;
      if (!response.ok) throw new Error(value.error || `Receipt status could not be read (${response.status}).`);
      if (value.session_id !== this.sessionId || value.turn_id !== this.turnId || !['ready','available','pending','unavailable','not_applicable'].includes(value.state)) throw new Error('The receipt status does not identify this Session turn.');
      this.value = value;
    } catch (error) { if (generation !== this.generation) return; this.value = null; this.error = error.message || String(error); }
    if (generation !== this.generation || !this.isConnected) return;
    this.loading = false; this.render();
    // A completion read can race the final evidence flush. Bound retries to the
    // mounted exact identity; reconnect/history reconstruction reads again.
    if (this.value?.state === 'pending' && (this.pendingReads = (this.pendingReads || 0) + 1) < 6) this.timer = setTimeout(() => void this.refresh(), 1000);
  }
  async download() {
    if (this.downloading) return;
    const generation = this.generation, path = this.path, downloadGeneration = ++this.downloadGeneration;
    this.downloading = true; this.render();
    try {
      const response = await fetch(path, {cache:'no-store'});
      if (!response.ok) { const value = await response.json().catch(() => ({})); throw new Error(value.error || `Receipt download failed (${response.status}).`); }
      if (!response.headers.get('content-type')?.startsWith('application/vnd.dsse.envelope.v1+json')) throw new Error('The server did not return a signed receipt.');
      const blob = await response.blob();
      if (generation !== this.generation || !this.isConnected || path !== this.path) return;
      const url = URL.createObjectURL(blob), anchor = document.createElement('a');
      anchor.href = url; anchor.download = this.filename; anchor.hidden = true; this.shadowRoot.append(anchor); anchor.click(); anchor.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      this.downloading = false; await this.refresh();
    } catch (error) { if (generation === this.generation && this.isConnected) { this.error = error.message || String(error); this.render(); } }
    finally { if (downloadGeneration === this.downloadGeneration) { this.downloading = false; if (this.isConnected) this.render(); } }
  }
  render() {
    const root = this.shadowRoot, focused = root.activeElement?.matches('a.download') ? 'a.download' : root.activeElement?.matches('button') ? 'button' : null, expanded = root.querySelector('details')?.open;
    root.replaceChildren();
    const box = node('section', null, 'receipt'); box.setAttribute('aria-label', 'Signed receipt'); root.append(box);
    const value = this.value;
    this.dataset.state = this.error ? 'error' : this.loading && !value ? 'loading' : value?.state || 'loading';
    box.append(node('h3', 'Signed receipt'));
    if (this.error) box.append(node('p', `Receipt unavailable: ${this.error}`, 'error'));
    else if (!value) box.append(node('p', 'Reading this turn’s recorded evidence…', 'muted'));
    else {
      const usable = ['ready','available'].includes(value.state);
      box.append(node('p', value.message || (usable ? 'A signed record of this turn’s captured evidence is available.' : value.state === 'pending' ? 'This turn has not finished. Its receipt is not available yet.' : 'This turn has no signed receipt.'), usable ? '' : 'muted'));
      if (value.outcome) box.append(node('p', `Recorded outcome: ${words(value.outcome)}`));
      const facts = [];
      if (value.checks) facts.push(`Checks: ${words(value.checks.state)}${Number.isInteger(value.checks.count) ? ` (${value.checks.count})` : ''}`);
      if (value.review) facts.push(`Review: ${words(value.review.state)}`);
      if (facts.length) box.append(node('p', facts.join(' · ')));
      const receipt = value.receipt;
      if (receipt) {
        box.append(node('p', `Signing key: ${receipt.key_id}`, 'key'));
        box.append(node('p', `${receipt.files} file${receipt.files === 1 ? '' : 's'} covered · ${receipt.removed} removed · ${words(receipt.source)}${receipt.complete ? '' : ' · coverage incomplete'}`));
      }
      if (usable) {
        box.append(node('p', 'The receipt records what the host observed. A valid signature identifies its signing key and protects the signed content; it does not prove the change is correct. Unconfigured or unrun checks and reviews are not passes.', 'muted'));
        const actions = node('div', null, 'actions');
        const download = node('a', 'Download signed receipt', 'download'); download.href = this.path; download.download = this.filename;
        download.setAttribute('aria-disabled', String(Boolean(this.downloading)));
        if (this.downloading) download.textContent = 'Preparing receipt…';
        download.addEventListener('click', event => { event.preventDefault(); void this.download(); });
        actions.append(download); box.append(actions);
        const verification = node('details'); verification.open = Boolean(expanded); verification.append(node('summary', 'What verification means'));
        verification.append(node('p', 'Verify this receipt with an updated Axocoatl verifier and a signing key you independently trust. File verification compares the covered file hashes with a checkout; later edits can differ. This receipt covers this recorded turn, not the whole Session.', 'muted')); box.append(verification);
      }
    }
    if (this.error || value && !['available'].includes(value.state)) {
      const retry = node('button', this.loading ? 'Checking…' : 'Refresh receipt'); retry.type = 'button'; retry.disabled = this.loading || this.downloading; retry.addEventListener('click', () => void this.refresh()); box.append(retry);
    }
    if (focused) root.querySelector(focused)?.focus({preventScroll:true});
  }
}
if (!customElements.get('ax-turn-receipt')) customElements.define('ax-turn-receipt', AxTurnReceipt);

export function turnReceipt(sessionId, turnId) {
  const receipt = document.createElement('ax-turn-receipt');
  receipt.setAttribute('session-id', sessionId); receipt.setAttribute('turn-id', turnId); return receipt;
}
