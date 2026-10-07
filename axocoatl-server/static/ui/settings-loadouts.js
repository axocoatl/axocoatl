import { adopt } from './sheets.js';
import { SETTINGS_CSS, emit, h, jsonRequest } from './settings-common.js';
import './loadout-graph.js';

/**
 * `<ax-settings-loadouts>` lists the built-in loadouts and the person's own
 * (`<config dir>/loadouts/*.yaml`), including user files that cannot be used,
 * with their path and error. Selecting one shows its parameters, warnings,
 * the display graph in a read-only lattice, its exact YAML (read-only) and the
 * `axocoatl run` command to copy. Nothing here runs a loadout: runs start from
 * `axocoatl run`.
 *
 * @element ax-settings-loadouts
 * @fires loadouts-change detail: {loadouts}
 * @fires notify          detail: {title, body, kind}
 */

const CSS = `${SETTINGS_CSS}
.detail { padding: var(--sp-3) var(--sp-4); display: grid; gap: var(--sp-4); }
.detail h3 { margin: 0; font-size: var(--fs-lg); font-weight: var(--fw-medium); }
.facts { display: flex; flex-wrap: wrap; gap: var(--sp-1); align-items: center; }
.badge.optin { color: var(--warn); background: rgba(var(--axo-bronze-rgb), .16); }
.badge.invalid { color: var(--err); background: rgba(226, 106, 106, .13); }
.warning { padding: var(--sp-2) var(--sp-3); border-radius: var(--r-md); color: var(--warn);
  background: rgba(var(--axo-bronze-rgb), .12); font-size: var(--fs-sm); }
.invalid-error { padding: var(--sp-2) var(--sp-3); border-radius: var(--r-md); color: var(--err);
  background: rgba(226, 106, 106, .1); font: var(--fs-sm) var(--font-mono); white-space: pre-wrap;
  overflow-wrap: anywhere; }
.command { display: flex; gap: var(--sp-2); align-items: center; }
.command input { flex: 1; min-width: 0; font: var(--fs-xs) var(--font-mono); padding: 6px var(--sp-2);
  border: 1px solid var(--border); border-radius: var(--r-sm); background: var(--bg); color: var(--text); }
pre.yaml { margin: 0; padding: var(--sp-3); max-height: 420px; overflow: auto; background: var(--bg);
  border: 1px solid var(--border); border-radius: var(--r-md); font: var(--fs-xs)/1.55 var(--font-mono);
  white-space: pre; user-select: text; }
.note { margin: 0; color: var(--muted); font-size: var(--fs-xs); line-height: var(--lh-body); }
.side-row .label small { color: var(--muted-2); margin-left: var(--sp-1); }
`;

/** The `axocoatl run` command for a loadout, with its required parameters. */
export function runCommand(summary) {
  const parts = ['axocoatl', 'run', summary.id, '--task', '"..."'];
  for (const param of summary.params || []) {
    if (!param.required) continue;
    if (param.kind === 'model' && param.name.endsWith('_model')) {
      parts.push('--model', `${param.name.slice(0, -'_model'.length)}=provider:model`);
    } else {
      parts.push('--param', `${param.name}=...`);
    }
  }
  return parts.join(' ');
}

export class AxSettingsLoadouts extends HTMLElement {
  #root;
  #loadouts = [];
  #selected = '';
  #view = null;
  #error = '';
  #generation = 0;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="shell">
        <aside class="side">
          <div class="side-scroll">
            <section class="side-section" data-group="builtin">
              <div class="side-head" role="heading" aria-level="3">Built-in</div>
              <div class="side-list builtin-list"></div>
            </section>
            <section class="side-section" data-group="user">
              <div class="side-head" role="heading" aria-level="3">Yours</div>
              <div class="side-list user-list"></div>
            </section>
          </div>
        </aside>
        <div class="main">
          <div class="toolbar"><h2 class="title">Loadouts</h2><span class="sub">display only · runs start from axocoatl run</span><span class="grow"></span>
            <button class="action refresh" type="button">Refresh</button></div>
          <div class="errors"></div>
          <div class="content"><div class="detail"></div></div>
        </div>
      </div>`;
    this.#root.querySelector('.refresh').addEventListener('click', () => void this.refresh());
    adopt(this.#root, CSS);
  }

  get loadouts() { return this.#loadouts; }
  get selected() { return this.#selected; }

  connectedCallback() {
    if (!this.#loadouts.length && !this.hidden) void this.refresh();
  }

  /** Read the registry again (built-in and user loadouts, no cache). */
  async refresh() {
    const generation = ++this.#generation;
    try {
      const loadouts = await jsonRequest('/api/loadouts');
      if (generation !== this.#generation) return;
      this.#loadouts = Array.isArray(loadouts) ? loadouts : [];
      this.#error = '';
      emit(this, 'loadouts-change', { loadouts: this.#loadouts });
      const still = this.#loadouts.find((row) => this.#key(row) === this.#selected);
      const first = still || this.#loadouts.find((row) => !row.error) || this.#loadouts[0];
      this.#renderList();
      if (first) await this.select(this.#key(first));
      else this.#renderDetail();
    } catch (error) {
      if (generation !== this.#generation) return;
      this.#error = error.message || String(error);
      this.#renderList();
      this.#renderDetail();
    }
  }

  #key(row) { return row.error ? `invalid:${row.path || row.id}` : row.id; }

  /** Show one loadout: its file, parameters and display graph. */
  async select(key) {
    this.#selected = key;
    this.#view = null;
    this.#renderList();
    const row = this.#loadouts.find((item) => this.#key(item) === key);
    if (row && !row.error) {
      try {
        const view = await jsonRequest(`/api/loadouts/${encodeURIComponent(row.id)}`);
        if (this.#selected !== key) return;
        this.#view = view;
      } catch (error) {
        this.#error = error.message || String(error);
      }
    }
    this.#renderDetail();
  }

  #renderList() {
    const errors = this.#root.querySelector('.errors');
    errors.replaceChildren();
    if (this.#error) {
      const box = h('div', 'error');
      box.append(h('strong', '', 'Loadouts could not be read'), h('span', '', this.#error));
      errors.append(box);
    }
    for (const [group, filter] of [['builtin', (row) => row.builtin], ['user', (row) => !row.builtin]]) {
      const list = this.#root.querySelector(`.${group}-list`);
      list.replaceChildren();
      const rows = this.#loadouts.filter(filter);
      if (!rows.length) {
        list.append(h('div', 'side-empty', group === 'user'
          ? 'No loadouts of your own. Add *.yaml files to the loadouts folder next to your configuration file.'
          : 'No built-in loadouts.'));
      }
      for (const row of rows) {
        const key = this.#key(row);
        const button = h('button', 'side-row');
        button.type = 'button';
        button.dataset.key = key;
        button.setAttribute('aria-current', String(key === this.#selected));
        const dot = h('span', `dot${row.error ? ' err' : ''}`);
        const label = h('span', 'label', row.error ? row.name : row.name || row.id);
        if (!row.error) label.append(h('small', '', `${row.id}@${row.version}`));
        button.append(dot, label);
        if (row.error) button.append(h('span', 'badge invalid', 'invalid'));
        else if (row.opt_in) button.append(h('span', 'badge optin', 'opt-in'));
        button.addEventListener('click', () => void this.select(key));
        list.append(button);
      }
    }
  }

  #renderDetail() {
    const detail = this.#root.querySelector('.detail');
    detail.replaceChildren();
    const row = this.#loadouts.find((item) => this.#key(item) === this.#selected);
    if (!row) {
      detail.append(h('p', 'empty', this.#error ? '' : 'Select a loadout.'));
      return;
    }
    if (row.error) {
      detail.append(h('h3', '', row.name));
      detail.append(h('p', 'note', `File: ${row.path || 'unknown'}`));
      detail.append(h('div', 'invalid-error', row.error));
      detail.append(h('p', 'note', 'This file cannot be used until it is fixed. `axocoatl loadouts validate <file>` checks it without the daemon.'));
      return;
    }
    const view = this.#view;
    const summary = view?.summary || row;
    detail.append(h('h3', '', summary.name || summary.id));
    const facts = h('div', 'facts');
    facts.append(
      h('span', 'badge', `${summary.id}@${summary.version}`),
      h('span', 'badge', summary.kind),
      h('span', 'badge', summary.builtin ? 'built-in' : 'yours'),
    );
    if (summary.opt_in) facts.append(h('span', 'badge optin', 'opt-in: runs only when named'));
    facts.append(h('span', 'badge mono', `sha256:${String(summary.digest).slice(0, 12)}…`));
    detail.append(facts);
    if (summary.description) detail.append(h('p', 'note', summary.description));
    if (summary.path) detail.append(h('p', 'note', `File: ${summary.path}`));
    for (const warning of summary.warnings || []) {
      const box = h('div', 'warning', `${warning.message} (${warning.field})`);
      box.dataset.code = warning.code;
      detail.append(box);
    }
    const command = h('div', 'section');
    command.append(h('h4', '', 'Run it'));
    const line = h('div', 'command');
    const input = h('input');
    input.readOnly = true;
    input.value = runCommand(summary);
    input.setAttribute('aria-label', 'axocoatl run command');
    const copy = h('button', 'action', 'Copy');
    copy.type = 'button';
    copy.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(input.value);
        emit(this, 'notify', { title: 'Copied', body: input.value, kind: 'info' });
      } catch {
        input.select();
      }
    });
    line.append(input, copy);
    command.append(line, h('p', 'note', 'Runs start from the command line; this view never runs a loadout.'));
    detail.append(command);
    if (summary.params?.length) {
      const params = h('div', 'section');
      params.append(h('h4', '', 'Parameters'));
      const table = h('table', 'params');
      table.innerHTML = '<thead><tr><th>Name</th><th>Kind</th><th>Required</th><th>Default</th><th>Description</th></tr></thead><tbody></tbody>';
      for (const param of summary.params) {
        const tr = h('tr');
        tr.append(h('td', 'mono', param.name), h('td', '', param.kind), h('td', '', param.required ? 'yes' : 'no'),
          h('td', 'mono', param.default ?? ''), h('td', 'muted', param.description || ''));
        table.tBodies[0].append(tr);
      }
      params.append(table);
      detail.append(params);
    }
    const graph = h('div', 'section');
    graph.append(h('h4', '', 'Graph'));
    const lattice = document.createElement('ax-loadout-graph');
    lattice.graph = view?.graph || null;
    graph.append(lattice);
    detail.append(graph);
    const file = h('div', 'section');
    file.append(h('h4', '', 'File'));
    file.append(h('pre', 'yaml', view?.text ?? 'Loading…'));
    detail.append(file);
  }
}

if (!customElements.get('ax-settings-loadouts')) customElements.define('ax-settings-loadouts', AxSettingsLoadouts);
