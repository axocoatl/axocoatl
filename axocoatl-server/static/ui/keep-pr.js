import { adopt } from './sheets.js';

/**
 * Keep as PR for one loadout run, in the run outcome panel.
 *
 *   <ax-keep-pr session-id="ses-…" run-id="run-…" verdict="pass" loadout="fix">
 *
 * "Keep as branch" commits the run's changes to a new branch in the person's
 * repository with their own git; their checkout, index and current branch
 * stay as they are. "Open pull request…" is opt-in: a confirmation names the
 * remote, the branch and the base before anything is pushed. Both stay
 * disabled unless the run's verdict is `pass`. The daemon decides everything
 * (POST /api/sessions/{id}/keep-pr); this element shows its answer — the
 * branch, commit and pull request, or the refusal — and fires
 * `keep-pr-result` with it.
 *
 * Attributes: `session-id`, `run-id`, `verdict` (required); `loadout` (shows
 * the default branch name); `remote` (default `origin`); `branch` (default
 * `axocoatl/<loadout>-<run>`). The `result` property shows a Keep the run
 * record already holds (`{branch, commit, pull_request_url, error}`).
 */

const CSS = `
:host { display:block; color:var(--text); font:inherit; }
:host([hidden]) { display:none; }
.row { display:flex; flex-wrap:wrap; align-items:center; gap:8px; }
button { color:inherit; background:var(--panel); border:1px solid var(--border); border-radius:6px; padding:7px 12px; font:inherit; cursor:pointer; max-width:100%; }
button.primary { border-color:var(--accent); }
button:disabled { cursor:default; opacity:.55; }
button:focus-visible, input:focus-visible, a:focus-visible { outline:2px solid var(--accent-2); outline-offset:2px; }
.hint { margin:6px 0 0; font-size:12px; color:var(--muted); }
.result { margin-top:8px; font-size:13px; overflow-wrap:anywhere; }
.result:empty { display:none; }
.result p { margin:4px 0; }
.result ul { margin:4px 0; padding-left:18px; }
.result code, dialog code { font-family:var(--font-mono, ui-monospace, monospace); font-size:12px; }
a { color:var(--accent-2); }
.ok { color:var(--ok); }
.error { color:var(--err); white-space:pre-wrap; }
.warn { color:var(--warn); }
dialog { width:min(520px, 94vw); max-width:94vw; padding:0; border:1px solid var(--border-strong); border-radius:var(--r-lg, 12px); background:var(--panel); color:var(--text); box-shadow:var(--shadow-lg); }
dialog::backdrop { background:rgba(0,0,0,.5); }
form { display:flex; flex-direction:column; gap:12px; padding:18px 20px; margin:0; }
h2 { margin:0; font-size:16px; }
dialog p { margin:0; font-size:13px; line-height:1.45; }
label { display:flex; flex-direction:column; gap:4px; font-size:12px; color:var(--muted); }
input { color:var(--text); background:var(--bg, var(--panel)); border:1px solid var(--border); border-radius:6px; padding:7px; font:inherit; font-size:13px; }
.actions { display:flex; justify-content:flex-end; gap:8px; flex-wrap:wrap; }
@media (max-width:480px) { .row button { flex:1 1 100%; } form { padding:14px; } }
`;

const DEFAULT_REMOTE = 'origin';

/** The branch the daemon creates when none is named. */
export function defaultBranch(loadout, runId) {
  const id = String(runId || '').replace(/^run-/, '').slice(0, 8);
  return loadout && id ? `axocoatl/${loadout}-${id}` : '';
}

export class AxKeepPr extends HTMLElement {
  static get observedAttributes() { return ['session-id', 'run-id', 'verdict', 'loadout', 'remote', 'branch']; }

  constructor() {
    super();
    this.attachShadow({ mode: 'open' });
    this.shadowRoot.innerHTML = `<div class="row">`
      + `<button class="keep primary" type="button">Keep as branch</button>`
      + `<button class="open" type="button" aria-haspopup="dialog">Open pull request…</button></div>`
      + `<p class="hint"></p>`
      + `<div class="result" role="status" aria-live="polite"></div>`
      + `<dialog aria-labelledby="keep-pr-title" aria-describedby="keep-pr-what"><form method="dialog">`
      + `<h2 id="keep-pr-title">Open a pull request?</h2>`
      + `<p id="keep-pr-what"></p>`
      + `<label>Branch<input class="branch" name="branch" autocomplete="off" spellcheck="false"></label>`
      + `<label>Remote<input class="remote" name="remote" autocomplete="off" spellcheck="false"></label>`
      + `<p class="never">Axocoatl never force-pushes and never pushes to the default branch. It refuses a branch that already exists, here or on the remote.</p>`
      + `<div class="actions"><button class="cancel" type="button">Cancel</button>`
      + `<button class="confirm primary" type="submit">Push and open pull request</button></div>`
      + `</form></dialog>`;
    void adopt(this.shadowRoot, CSS, ['/ui/tokens.css']);
    this.busy = false;
    this.q('.keep').addEventListener('click', () => void this.keep(false));
    this.q('.open').addEventListener('click', () => this.confirmOpen());
    const dialog = this.q('dialog');
    dialog.addEventListener('cancel', event => { event.preventDefault(); this.closeDialog(); });
    dialog.addEventListener('keydown', event => { if (event.key === 'Escape') event.stopPropagation(); });
    this.q('.cancel').addEventListener('click', () => this.closeDialog());
    this.q('form').addEventListener('submit', event => { event.preventDefault(); void this.confirmed(); });
    for (const input of [this.q('.branch'), this.q('.remote')]) input.addEventListener('input', () => this.describe());
  }

  q(selector) { return this.shadowRoot.querySelector(selector); }
  get sessionId() { return this.getAttribute('session-id') || ''; }
  get runId() { return this.getAttribute('run-id') || ''; }
  get verdict() { return this.getAttribute('verdict') || ''; }
  get passed() { return this.verdict === 'pass' && Boolean(this.sessionId) && Boolean(this.runId); }
  get suggestedBranch() { return this.getAttribute('branch') || defaultBranch(this.getAttribute('loadout'), this.runId); }
  get suggestedRemote() { return this.getAttribute('remote') || DEFAULT_REMOTE; }

  connectedCallback() { this.render(); }
  attributeChangedCallback(name, before, after) {
    if (before === after) return;
    if (name === 'run-id' || name === 'session-id') this.clearResult();
    this.render();
  }

  /** A Keep the run record already holds: `{branch, commit, pull_request_url, error}`. */
  set result(value) {
    this._result = value || null;
    if (!value) { this.clearResult(); return; }
    if (value.error) this.showError(value.error, value);
    else this.showKept({ branch: value.branch, commit: value.commit, pull_request_url: value.pull_request_url, paths: [] });
  }
  get result() { return this._result || null; }

  render() {
    const enabled = this.passed && !this.busy;
    this.q('.keep').disabled = !enabled;
    this.q('.open').disabled = !enabled;
    const hint = this.q('.hint');
    if (!this.sessionId || !this.runId) {
      hint.textContent = 'Keep is available once the run has finished.';
    } else if (this.verdict !== 'pass') {
      hint.textContent = `Keep is available only for a run that passed. This run's verdict is ${this.verdict ? this.verdict.replaceAll('_', ' ') : 'not known yet'}.`;
    } else {
      hint.textContent = 'Commits only the paths this run changed to a new branch with your own git. Your checkout, index and current branch stay as they are.';
    }
  }

  confirmOpen() {
    if (!this.passed || this.busy) return;
    this.q('.branch').value = this.suggestedBranch;
    this.q('.branch').placeholder = 'axocoatl/<loadout>-<run>';
    this.q('.remote').value = this.suggestedRemote;
    this.describe();
    const dialog = this.q('dialog');
    if (!dialog.open) dialog.showModal();
    this.q('.confirm').focus();
  }

  describe() {
    const branch = this.q('.branch').value.trim() || 'a new axocoatl/ branch';
    const remote = this.q('.remote').value.trim() || DEFAULT_REMOTE;
    const what = this.q('#keep-pr-what');
    what.replaceChildren(
      'Axocoatl commits this run\'s changes to branch ', code(branch),
      ', pushes it to remote ', code(remote),
      ' with your own git credentials, and opens a pull request with gh into the base branch ', code(`${remote}'s default branch`),
      '.',
    );
    this.q('.confirm').disabled = this.q('.remote').value.trim() === '';
  }

  closeDialog() {
    const dialog = this.q('dialog');
    if (dialog.open) dialog.close();
    this.q('.open').focus();
  }

  async confirmed() {
    const branch = this.q('.branch').value.trim();
    const remote = this.q('.remote').value.trim();
    if (!remote) return;
    this.closeDialog();
    await this.keep(true, { branch, remote });
  }

  /** POST the Keep request; `options` holds the confirmed branch and remote. */
  async keep(openPr, options = {}) {
    if (!this.passed || this.busy) return;
    const body = { run_id: this.runId, open_pr: openPr };
    const branch = options.branch ?? this.getAttribute('branch') ?? '';
    if (branch) body.branch = branch;
    if (openPr) {
      const remote = options.remote || this.suggestedRemote;
      if (remote !== DEFAULT_REMOTE) body.remote = remote;
    }
    this.busy = true;
    this.render();
    this.showStatus(openPr ? 'Committing, pushing and opening the pull request…' : 'Committing to a new branch…');
    let response = null;
    let payload = null;
    try {
      response = await fetch(`/api/sessions/${encodeURIComponent(this.sessionId)}/keep-pr`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      payload = await response.json().catch(() => null);
    } catch (error) {
      payload = { error: `The daemon could not be reached: ${error?.message || error}` };
    } finally {
      this.busy = false;
      this.render();
    }
    if (response?.ok && payload && typeof payload.branch === 'string') {
      this.showKept(payload);
      this.emit({ response: payload });
    } else {
      const message = payload?.error || (response ? `Keep failed (HTTP ${response.status}).` : 'Keep failed.');
      this.showError(message, null, response?.status);
      this.emit({ error: message, status: response?.status ?? null });
    }
  }

  emit(detail) {
    this.dispatchEvent(new CustomEvent('keep-pr-result', {
      bubbles: true, composed: true,
      detail: { sessionId: this.sessionId, runId: this.runId, ...detail },
    }));
  }

  clearResult() { this.q('.result').replaceChildren(); }

  showStatus(text) {
    const line = document.createElement('p');
    line.textContent = text;
    this.q('.result').replaceChildren(line);
  }

  showKept(kept) {
    const out = [];
    const head = document.createElement('p');
    head.className = 'ok kept';
    const short = String(kept.commit || '').slice(0, 12);
    const count = Array.isArray(kept.paths) ? kept.paths.length : 0;
    head.append('Kept as branch ', code(kept.branch), ' at ', code(short));
    if (count) head.append(` (${count} ${count === 1 ? 'path' : 'paths'})`);
    head.append('.');
    out.push(head);
    if (kept.pushed_to) {
      const pushed = document.createElement('p');
      pushed.className = 'pushed';
      pushed.append('Pushed to ', code(kept.pushed_to));
      if (kept.base) pushed.append(', base ', code(kept.base));
      pushed.append('.');
      out.push(pushed);
    }
    if (kept.pull_request_url) {
      const pr = document.createElement('p');
      pr.className = 'pull-request';
      const url = String(kept.pull_request_url);
      if (/^https:\/\/[^\s]+$/.test(url)) {
        const link = document.createElement('a');
        link.href = url;
        link.target = '_blank';
        link.rel = 'noopener noreferrer';
        link.textContent = url;
        pr.append('Pull request: ', link);
      } else {
        pr.append('Pull request: ', url);
      }
      out.push(pr);
    }
    if (count) {
      const list = document.createElement('ul');
      list.className = 'paths';
      list.setAttribute('aria-label', 'Committed paths');
      for (const path of kept.paths.slice(0, 50)) {
        const item = document.createElement('li');
        item.append(code(path));
        list.append(item);
      }
      if (kept.paths.length > 50) {
        const more = document.createElement('li');
        more.textContent = `…and ${kept.paths.length - 50} more`;
        list.append(more);
      }
      out.push(list);
    }
    for (const warning of Array.isArray(kept.warnings) ? kept.warnings : []) {
      const line = document.createElement('p');
      line.className = 'warn';
      line.textContent = warning;
      out.push(line);
    }
    this.q('.result').replaceChildren(...out);
  }

  showError(message, recorded = null, status = null) {
    const line = document.createElement('p');
    line.className = 'error refusal';
    line.setAttribute('role', 'alert');
    const prefix = status === 409 || status === 422 ? 'Refused: ' : '';
    line.textContent = `${prefix}${String(message).replace(/^(Session conflict: )?keep as PR: /, '')}`;
    const out = [line];
    if (recorded?.branch && recorded?.commit) {
      const partial = document.createElement('p');
      partial.append('Branch ', code(recorded.branch), ' exists at ', code(String(recorded.commit).slice(0, 12)), '; keeping again continues from it.');
      out.push(partial);
    }
    this.q('.result').replaceChildren(...out);
  }
}

function code(text) {
  const element = document.createElement('code');
  element.textContent = String(text ?? '');
  return element;
}

if (!customElements.get('ax-keep-pr')) customElements.define('ax-keep-pr', AxKeepPr);
