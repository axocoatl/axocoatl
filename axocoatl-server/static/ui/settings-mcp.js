import { adopt } from './sheets.js';
import { SETTINGS_CSS, emit, h, jsonRequest } from './settings-common.js';

/**
 * `<ax-settings-mcp>` owns MCP server discovery, install, reconnect, removal,
 * tool inspection, and saved permission revocation.
 *
 * @element ax-settings-mcp
 * @fires mcp-change detail: {catalog, servers, tools, permissions}
 * @fires notify     detail: {title, body, kind}
 */

export function mcpIconFor(category) {
  return ({
    Development: '⌨', Web: '⌬', Data: '⛁', Productivity: '⌧', Memory: '◐',
    Reasoning: '✦', Utilities: '⌖', Reference: '?',
  })[category] || '◇';
}

const CSS = `${SETTINGS_CSS}
.detail { display: flex; flex: 1; min-height: 0; flex-direction: column; overflow: hidden; }
.detail-body { flex: 1; min-height: 0; overflow: auto; padding: var(--sp-4); }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: var(--sp-3); }
.server-card { display: flex; min-width: 0; min-height: 145px; flex-direction: column; padding: var(--sp-3); }
.server-card:hover { border-color: var(--border-strong); box-shadow: var(--shadow-sm); }
.card-head { display: flex; align-items: center; gap: var(--sp-2); }
.card-icon {
  display: grid; width: 31px; height: 31px; flex-shrink: 0; place-items: center;
  border-radius: var(--r-md); color: var(--accent-2); background: var(--bg-3); font-size: var(--fs-lg);
}
.card-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; font-size: var(--fs-body); font-weight: var(--fw-medium); }
.installed-dot { width: 8px; height: 8px; flex-shrink: 0; border-radius: 50%; background: var(--ok); box-shadow: 0 0 7px var(--ok); }
.recommended {
  padding: 2px 6px; border-radius: var(--r-pill); color: var(--axo-bronze);
  background: rgba(var(--axo-bronze-rgb), .14); font-size: var(--fs-xs);
}
.card-desc { flex: 1; margin: var(--sp-2) 0; color: var(--muted); font-size: var(--fs-xs); line-height: var(--lh-body); }
.card-foot { display: flex; align-items: center; gap: var(--sp-2); }
.category { color: var(--muted-2); font-size: var(--fs-xs); }
.segments { display: inline-flex; flex-shrink: 0; gap: 2px; padding: 2px; border: 1px solid var(--border); border-radius: var(--r-md); background: var(--bg-2); }
.segment { padding: 4px var(--sp-3); border: 0; border-radius: var(--r-sm); background: transparent; color: var(--muted); cursor: pointer; font-size: var(--fs-sm); }
.segment:hover { color: var(--text); background: var(--bg-3); }
.segment[aria-current="true"] { color: var(--text); background: var(--panel); box-shadow: var(--shadow-sm); }
.facts { display: grid; grid-template-columns: auto minmax(0, 1fr); gap: var(--sp-2) var(--sp-4); align-items: baseline; font-size: var(--fs-sm); }
.facts dt { color: var(--muted); font-size: var(--fs-xs); }
.facts dd { margin: 0; overflow-wrap: anywhere; }
.detail-actions { display: flex; flex-wrap: wrap; gap: var(--sp-2); margin-top: var(--sp-4); }
.permission-table { min-width: 690px; }
.decision.allow { color: var(--ok); } .decision.deny { color: var(--err); }
.modal-layer {
  position: absolute; z-index: 20; inset: 0; display: none; place-items: center;
  padding: var(--sp-4); background: rgba(0,0,0,.55);
}
.modal-layer[data-open] { display: grid; }
.modal {
  display: flex; width: min(500px, 100%); max-height: min(620px, 100%); flex-direction: column;
  overflow: hidden; border: 1px solid var(--border-strong); border-radius: var(--r-xl);
  background: var(--panel); box-shadow: var(--shadow-lg);
}
.modal-head { padding: var(--sp-3) var(--sp-4); border-bottom: 1px solid var(--border); font-size: var(--fs-body); font-weight: var(--fw-medium); }
.modal-body { overflow: auto; padding: var(--sp-4); }
.modal-copy { margin: 0 0 var(--sp-3); color: var(--muted); font-size: var(--fs-sm); line-height: var(--lh-body); white-space: pre-wrap; }
.modal-field { display: grid; gap: var(--sp-1); margin-top: var(--sp-3); }
.modal-field label { color: var(--muted); font-size: var(--fs-xs); }
.modal-field small { color: var(--muted-2); font-size: var(--fs-xs); line-height: var(--lh-body); }
.modal-error { min-height: 1.2em; margin-top: var(--sp-2); color: var(--err); font-size: var(--fs-xs); }
.modal-foot { display: flex; justify-content: flex-end; gap: var(--sp-2); padding: var(--sp-3) var(--sp-4); border-top: 1px solid var(--border); background: var(--panel-2); }
@media (max-width: 760px) { .segments { max-width: 100%; overflow-x: auto; } }
`;

function validList(value, label) {
  if (!Array.isArray(value)) throw new Error(`${label} returned an invalid list.`);
  return value;
}

export class AxSettingsMcp extends HTMLElement {
  #root;
  #catalog = { servers: [] };
  #servers = [];
  #tools = [];
  #permissions = [];
  #available = { catalog: false, servers: false, tools: false, permissions: false };
  #phase = 'idle';
  #issues = [];
  #operationError = '';
  #section = 'servers';
  #serverTab = 'overview';
  #category = 'All';
  #query = '';
  #generation = 0;
  #busy = '';
  #modal = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="shell">
        <aside class="side">
          <div class="side-search"><input class="search side-query" type="search" placeholder="Filter servers, tools…" spellcheck="false"></div>
          <div class="side-scroll">
            <section class="side-section">
              <button class="side-head" type="button"><span class="tri">▾</span><span>Servers</span></button>
              <div class="side-list server-list"></div>
            </section>
            <section class="side-section">
              <button class="side-head" type="button"><span class="tri">▾</span><span>Catalog</span></button>
              <div class="side-list catalog-list"></div>
            </section>
            <section class="side-section">
              <button class="side-head" type="button"><span class="tri">▾</span><span>Permissions</span></button>
              <div class="side-list permission-list"></div>
            </section>
          </div>
        </aside>
        <main class="main">
          <div class="errors" role="status" aria-live="polite"></div>
          <div class="detail"></div>
          <div class="modal-layer"><form class="modal" role="dialog" aria-modal="true" aria-labelledby="mcp-modal-title"><div class="modal-head" id="mcp-modal-title"></div><div class="modal-body"></div><div class="modal-foot"><button class="action cancel" type="button">Cancel</button><button class="action primary confirm" type="submit">Continue</button></div></form></div>
        </main>
      </div>`;
    adopt(this.#root, CSS);
    this.#wire();
  }

  connectedCallback() {
    if (this.#phase === 'idle') void this.refresh();
  }

  get data() {
    return {
      catalog: this.#catalog,
      servers: this.#servers.map((server) => ({ ...server })),
      tools: this.#tools.map((tool) => ({ ...tool })),
      permissions: this.#permissions.map((permission) => ({ ...permission })),
    };
  }

  async refresh() {
    const generation = ++this.#generation;
    this.#phase = 'loading';
    this.#issues = [];
    this.#renderErrors();
    this.#root.querySelector('.shell').classList.add('loading');
    const specs = [
      ['catalog', '/api/mcp/catalog'],
      ['servers', '/api/mcp/servers'],
      ['tools', '/api/mcp/tools'],
      ['permissions', '/api/mcp/permissions'],
    ];
    const results = await Promise.allSettled(specs.map(([, url]) => jsonRequest(url)));
    if (generation !== this.#generation) return;
    results.forEach((result, index) => {
      const [key] = specs[index];
      if (result.status === 'rejected') {
        this.#available[key] = false;
        if (key === 'catalog') this.#catalog = { servers: [] };
        if (key === 'servers') this.#servers = [];
        if (key === 'tools') this.#tools = [];
        if (key === 'permissions') this.#permissions = [];
        this.#issues.push(`${key[0].toUpperCase() + key.slice(1)}: ${result.reason?.message || result.reason}`);
        return;
      }
      try {
        if (key === 'catalog') {
          if (!result.value || typeof result.value !== 'object') throw new Error('Catalog returned an invalid document.');
          validList(result.value.servers, 'Catalog');
          this.#catalog = result.value;
        } else {
          const value = validList(result.value, `MCP ${key}`);
          if (key === 'servers') this.#servers = value;
          if (key === 'tools') this.#tools = value;
          if (key === 'permissions') this.#permissions = value;
        }
        this.#available[key] = true;
      } catch (error) {
        this.#available[key] = false;
        if (key === 'catalog') this.#catalog = { servers: [] };
        if (key === 'servers') this.#servers = [];
        if (key === 'tools') this.#tools = [];
        if (key === 'permissions') this.#permissions = [];
        this.#issues.push(String(error?.message || error));
      }
    });
    if (this.#section.startsWith('server:')
        && !this.#servers.some((server) => server.name === this.#section.slice(7))) {
      this.#section = 'servers';
    }
    this.#phase = 'ready';
    this.#root.querySelector('.shell').classList.remove('loading');
    this.#render();
    emit(this, 'mcp-change', this.data);
  }

  openSection(section) {
    if (!section) return;
    this.#section = section;
    if (!section.startsWith('server:')) this.#serverTab = 'overview';
    this.#renderSidebar();
    this.#renderDetail();
  }

  #wire() {
    this.#root.querySelector('.side-query').addEventListener('input', (event) => {
      this.#query = event.target.value.trim().toLowerCase();
      this.#renderSidebar();
    });
    this.#root.querySelectorAll('.side-head').forEach((button) => {
      button.addEventListener('click', () => button.closest('.side-section').toggleAttribute('data-collapsed'));
    });
    this.#root.querySelector('.modal .cancel').addEventListener('click', () => this.#finishModal(false));
    this.#root.querySelector('.modal').addEventListener('submit', (event) => {
      event.preventDefault();
      this.#finishModal(true);
    });
    this.#root.querySelector('.modal-layer').addEventListener('click', (event) => {
      if (event.target === event.currentTarget) this.#finishModal(false);
    });
    this.#root.addEventListener('keydown', (event) => {
      if (event.key === 'Escape' && this.#modal) {
        event.preventDefault();
        event.stopPropagation();
        this.#finishModal(false);
      } else if (event.key === 'Tab' && this.#modal) {
        const focusable = [...this.#root.querySelectorAll('.modal button:not(:disabled), .modal input:not(:disabled)')];
        if (!focusable.length) return;
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (event.shiftKey && this.#root.activeElement === first) {
          event.preventDefault(); last.focus();
        } else if (!event.shiftKey && this.#root.activeElement === last) {
          event.preventDefault(); first.focus();
        }
      }
    });
  }

  #render() {
    this.#renderErrors();
    this.#renderSidebar();
    this.#renderDetail();
  }

  #renderErrors() {
    const host = this.#root.querySelector('.errors');
    host.replaceChildren();
    this.#issues.forEach((issue) => {
      const row = h('div', 'error');
      row.append(h('strong', '', 'MCP data is incomplete'));
      const retry = h('button', '', 'Retry all');
      retry.type = 'button';
      retry.addEventListener('click', () => void this.refresh());
      row.append(retry, h('span', '', issue));
      host.append(row);
    });
    if (this.#operationError) {
      const row = h('div', 'error');
      row.append(h('strong', '', 'MCP action failed'), h('span', '', this.#operationError));
      host.append(row);
    }
  }

  #renderSidebar() {
    const matches = (value) => !this.#query || String(value || '').toLowerCase().includes(this.#query);
    const serverHost = this.#root.querySelector('.server-list');
    serverHost.replaceChildren(this.#sideRow('⌥', 'All servers', this.#servers.length, this.#section === 'servers', () => this.openSection('servers')));
    this.#servers.filter((server) => {
      return matches(server.name) || this.#tools.some((tool) => tool.server === server.name && matches(tool.name));
    }).forEach((server) => {
      const row = this.#sideRow('', server.name, server.tool_count, this.#section === `server:${server.name}`, () => this.openSection(`server:${server.name}`));
      row.querySelector('.ico').replaceWith(h('span', 'dot on'));
      serverHost.append(row);
    });
    if (!this.#servers.length) {
      serverHost.append(h('div', 'side-empty', this.#available.servers ? 'No servers connected.' : 'Server list unavailable.'));
    }

    const entries = this.#catalog.servers || [];
    const categories = [...new Set(entries.map((entry) => entry.category))].sort();
    const catalogHost = this.#root.querySelector('.catalog-list');
    catalogHost.replaceChildren(this.#sideRow('⌬', 'Browse all', entries.length,
      this.#section === 'catalog' && this.#category === 'All', () => {
        this.#category = 'All'; this.openSection('catalog');
      }));
    categories.filter(matches).forEach((category) => {
      const count = entries.filter((entry) => entry.category === category).length;
      catalogHost.append(this.#sideRow(mcpIconFor(category), category, count,
        this.#section === 'catalog' && this.#category === category, () => {
          this.#category = category; this.openSection('catalog');
        }));
    });

    const permissionHost = this.#root.querySelector('.permission-list');
    permissionHost.replaceChildren(this.#sideRow('⌘', 'All decisions', this.#permissions.length,
      this.#section === 'permissions', () => this.openSection('permissions')));
  }

  #sideRow(icon, label, count, current, action) {
    const row = h('button', 'side-row');
    row.type = 'button';
    row.setAttribute('aria-current', String(current));
    row.append(h('span', 'ico', icon), h('span', 'label', label), h('span', 'count', count));
    row.addEventListener('click', action);
    return row;
  }

  #renderDetail() {
    const host = this.#root.querySelector('.detail');
    host.replaceChildren();
    if (this.#phase === 'loading' && !this.#servers.length && !(this.#catalog.servers || []).length) {
      host.append(h('div', 'empty', 'Loading MCP servers…'));
      return;
    }
    if (this.#section === 'catalog') this.#renderCatalog(host);
    else if (this.#section === 'permissions') this.#renderPermissions(host);
    else if (this.#section.startsWith('server:')) this.#renderServer(host, this.#section.slice(7));
    else this.#renderServers(host);
  }

  #toolbar(host, title, subtitle, ...actions) {
    const toolbar = h('div', 'toolbar');
    toolbar.append(h('h2', '', title));
    if (subtitle) toolbar.append(h('span', 'sub', subtitle));
    toolbar.append(h('span', 'grow'));
    actions.forEach((action) => toolbar.append(action));
    host.append(toolbar);
  }

  #renderServers(host) {
    this.#toolbar(host, 'Servers', `${this.#servers.length} connected · until daemon restart`);
    const body = h('div', 'detail-body');
    if (!this.#servers.length) {
      body.append(h('div', 'empty', this.#available.servers
        ? 'No MCP servers connected yet. Open Catalog in the sidebar to connect one for this daemon run.'
        : 'The connected-server list could not be loaded. Retry above.'));
    } else {
      const grid = h('div', 'grid');
      this.#servers.forEach((server) => {
        const catalog = (this.#catalog.servers || []).find((entry) => entry.slug === server.name);
        const card = h('article', 'card server-card');
        const head = h('div', 'card-head');
        head.append(h('span', 'card-icon', mcpIconFor(catalog?.category)), h('span', 'card-name', server.name), h('span', 'installed-dot'));
        card.append(head, h('p', 'card-desc', catalog?.description || `Transport: ${server.transport}`));
        const foot = h('div', 'card-foot');
        foot.append(h('span', 'category', `${server.tool_count} tool${server.tool_count === 1 ? '' : 's'}`), h('span', 'grow'));
        const open = h('button', 'action', 'Open →'); open.type = 'button';
        open.addEventListener('click', () => this.openSection(`server:${server.name}`));
        foot.append(open); card.append(foot); grid.append(card);
      });
      body.append(grid);
    }
    host.append(body);
  }

  #renderServer(host, name) {
    const server = this.#servers.find((candidate) => candidate.name === name);
    if (!server) { this.openSection('servers'); return; }
    const catalog = (this.#catalog.servers || []).find((entry) => entry.slug === server.name);
    const back = h('button', 'action', '← Servers'); back.type = 'button';
    back.addEventListener('click', () => this.openSection('servers'));
    const segments = h('div', 'segments');
    [['overview', 'Overview'], ['tools', 'Tools'], ['permissions', 'Permissions']].forEach(([id, label]) => {
      const button = h('button', 'segment', label); button.type = 'button';
      button.setAttribute('aria-current', String(this.#serverTab === id));
      button.addEventListener('click', () => { this.#serverTab = id; this.#renderDetail(); });
      segments.append(button);
    });
    const toolbar = h('div', 'toolbar');
    toolbar.append(back, h('h2', '', server.name), h('span', 'sub', `${server.tool_count} tool${server.tool_count === 1 ? '' : 's'} · ${server.transport}`), h('span', 'grow'), segments);
    host.append(toolbar);
    const body = h('div', 'detail-body');
    if (this.#serverTab === 'overview') {
      const facts = h('dl', 'facts');
      const fact = (label, value) => facts.append(h('dt', '', label), h('dd', '', value));
      fact('Name', server.name); fact('Transport', server.transport); fact('Tools', server.tool_count);
      if (catalog) { fact('Category', catalog.category); fact('Description', catalog.description); }
      body.append(facts);
      const actions = h('div', 'detail-actions');
      const reconnect = h('button', 'action', this.#busy === `reconnect:${name}` ? 'Reconnecting…' : '↻ Reconnect');
      reconnect.type = 'button'; reconnect.disabled = Boolean(this.#busy);
      reconnect.addEventListener('click', () => void this.#reconnect(server));
      const remove = h('button', 'action danger', this.#busy === `remove:${name}` ? 'Removing…' : '✕ Remove');
      remove.type = 'button'; remove.disabled = Boolean(this.#busy);
      remove.addEventListener('click', () => void this.#remove(server));
      actions.append(reconnect, remove); body.append(actions);
    } else if (this.#serverTab === 'tools') {
      const tools = this.#tools.filter((tool) => tool.server === server.name);
      if (!tools.length) body.append(h('div', 'empty', this.#available.tools ? 'No tools discovered for this server.' : 'Tools could not be loaded. Retry above.'));
      else {
        const card = h('div', 'card');
        const table = h('table'); const head = h('thead'); const headRow = h('tr');
        headRow.append(h('th', '', 'Tool'), h('th', '', 'Description')); head.append(headRow);
        const rows = h('tbody');
        tools.forEach((tool) => { const row = h('tr'); row.append(h('td', 'mono', tool.name), h('td', 'muted', tool.description || '')); rows.append(row); });
        table.append(head, rows); card.append(table); body.append(card);
      }
    } else {
      const permissions = this.#permissions.filter((permission) => permission.server === server.name);
      if (!permissions.length) body.append(h('div', 'empty', this.#available.permissions ? 'No recorded permissions for this server.' : 'Permissions could not be loaded. Retry above.'));
      else body.append(this.#permissionsTable(permissions));
    }
    host.append(body);
  }

  #renderCatalog(host) {
    const entries = this.#catalog.servers || [];
    const filtered = this.#category === 'All' ? entries : entries.filter((entry) => entry.category === this.#category);
    this.#toolbar(host, 'Catalog', `${filtered.length} ${this.#category === 'All' ? 'available' : `in ${this.#category}`}`);
    const body = h('div', 'detail-body');
    if (!entries.length) {
      body.append(h('div', 'empty', this.#available.catalog ? 'The catalog is empty.' : 'The catalog could not be loaded. Retry above.'));
    } else {
    const connected = new Set(this.#servers.map((server) => server.name));
      const sorted = [...filtered].sort((a, b) => a.recommended === b.recommended
        ? String(a.name).localeCompare(String(b.name)) : (a.recommended ? -1 : 1));
      const grid = h('div', 'grid');
      sorted.forEach((entry) => grid.append(this.#catalogCard(entry, connected.has(entry.slug))));
      body.append(grid);
    }
    host.append(body);
  }

  #catalogCard(entry, connected) {
    const card = h('article', 'card server-card');
    const head = h('div', 'card-head');
    head.append(h('span', 'card-icon', mcpIconFor(entry.category)), h('span', 'card-name', entry.name));
    if (entry.recommended) head.append(h('span', 'recommended', 'recommended'));
    card.append(head, h('p', 'card-desc', entry.description || ''));
    const foot = h('div', 'card-foot');
    foot.append(h('span', 'category', entry.category || ''), h('span', 'grow'));
    if (connected) foot.append(h('span', 'installed-dot'), h('span', 'muted', 'connected'));
    else {
      const connect = h('button', 'action primary', this.#busy === `install:${entry.slug}` ? 'Connecting…' : '+ Connect');
      connect.type = 'button'; connect.disabled = Boolean(this.#busy);
      connect.addEventListener('click', () => void this.#install(entry));
      foot.append(connect);
    }
    card.append(foot);
    return card;
  }

  #renderPermissions(host) {
    this.#toolbar(host, 'Permissions', `${this.#permissions.length} recorded`);
    const body = h('div', 'detail-body');
    if (!this.#permissions.length) {
      body.append(h('div', 'empty', this.#available.permissions
        ? 'No recorded permissions yet. Saved “Allow always” and “Deny always” decisions appear here.'
        : 'Permissions could not be loaded. Retry above.'));
    } else body.append(this.#permissionsTable(this.#permissions));
    host.append(body);
  }

  #permissionsTable(permissions) {
    const card = h('div', 'card');
    const table = h('table', 'permission-table');
    const head = h('thead'); const headRow = h('tr');
    ['Agent', 'Server', 'Tool', 'Decision', 'Recorded', ''].forEach((label) => headRow.append(h('th', '', label)));
    head.append(headRow);
    const body = h('tbody');
    permissions.forEach((permission) => {
      const row = h('tr');
      row.append(h('td', '', permission.agent_id || '(any agent)'), h('td', 'mono', permission.server), h('td', 'mono', permission.tool || '(any tool)'));
      row.append(h('td', `mono decision ${permission.decision === 'allow' ? 'allow' : 'deny'}`, permission.decision));
      const recorded = Number(permission.recorded_at);
      row.append(h('td', 'muted', Number.isFinite(recorded) ? new Date(recorded * 1000).toLocaleString() : '—'));
      const actions = h('td');
      const revoke = h('button', 'action danger', 'Revoke'); revoke.type = 'button'; revoke.disabled = Boolean(this.#busy);
      revoke.addEventListener('click', () => void this.#revoke(permission));
      actions.append(revoke); row.append(actions); body.append(row);
    });
    table.append(head, body); card.append(table);
    return card;
  }

  async #reconnect(server) {
    if (this.#busy) return;
    this.#busy = `reconnect:${server.name}`; this.#renderDetail();
    try {
      const result = await jsonRequest(`/api/mcp/servers/${encodeURIComponent(server.name)}`, { method: 'POST' });
      this.#operationError = '';
      emit(this, 'notify', { title: 'Reconnected', body: `${server.name} · ${result?.tools ?? 0} tool${result?.tools === 1 ? '' : 's'}`, kind: 'ok' });
      await this.refresh();
    } catch (error) { this.#actionFailed('Reconnect failed', server.name, error); }
    finally { this.#busy = ''; this.#renderDetail(); }
  }

  async #remove(server) {
    if (this.#busy) return;
    const confirmed = await this.#ask({
      title: `Remove ${server.name}?`,
      body: 'Disconnect from this MCP server. Its tools become unavailable. The catalog entry remains so you can connect it again.',
      okLabel: 'Remove', danger: true,
    });
    if (!confirmed) return;
    this.#busy = `remove:${server.name}`; this.#renderDetail();
    try {
      await jsonRequest(`/api/mcp/servers/${encodeURIComponent(server.name)}`, { method: 'DELETE' });
      this.#operationError = ''; this.#section = 'servers'; this.#serverTab = 'overview';
      emit(this, 'notify', { title: 'Removed', body: server.name, kind: 'ok' });
      await this.refresh();
    } catch (error) { this.#actionFailed('Remove failed', server.name, error); }
    finally { this.#busy = ''; this.#renderDetail(); }
  }

  async #install(entry) {
    if (this.#busy) return;
    const fields = (entry.requires || []).map((requirement) => ({
      key: requirement.key,
      label: requirement.label,
      type: requirement.kind === 'secret' ? 'password' : 'text',
      placeholder: requirement.placeholder || '',
      help: requirement.help || '',
      required: true,
    }));
    const values = await this.#ask({
      title: `Connect ${entry.name}`,
      body: `${entry.description || ''}\n\nThis connection lasts until the Axocoatl daemon restarts. Add it to axocoatl.yaml when it should reconnect on future launches.`,
      fields,
      okLabel: 'Connect',
    });
    if (!values) return;
    this.#busy = `install:${entry.slug}`; this.#renderDetail();
    try {
      const result = await jsonRequest('/api/mcp/install', {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ slug: entry.slug, values: fields.length ? values : {} }),
      });
      this.#operationError = '';
      emit(this, 'notify', { title: 'Connected', body: `${result.name} · ${result.tools} tool${result.tools === 1 ? '' : 's'} · until daemon restart`, kind: 'ok' });
      await this.refresh();
    } catch (error) { this.#actionFailed('Connection failed', entry.name, error); }
    finally { this.#busy = ''; this.#renderDetail(); }
  }

  async #revoke(permission) {
    if (this.#busy) return;
    const confirmed = await this.#ask({
      title: 'Revoke this permission?',
      body: `Next time ${permission.agent_id || 'any agent'} calls ${permission.tool || 'any tool'} on ${permission.server}, you will be asked again.`,
      okLabel: 'Revoke', danger: true,
    });
    if (!confirmed) return;
    const params = new URLSearchParams({ server: permission.server });
    if (permission.agent_id) params.set('agent_id', permission.agent_id);
    if (permission.tool) params.set('tool', permission.tool);
    this.#busy = 'revoke'; this.#renderDetail();
    try {
      await jsonRequest(`/api/mcp/permissions?${params}`, { method: 'DELETE' });
      this.#operationError = '';
      emit(this, 'notify', { title: 'Permission revoked', body: permission.server, kind: 'ok' });
      await this.refresh();
    } catch (error) { this.#actionFailed('Revoke failed', permission.server, error); }
    finally { this.#busy = ''; this.#renderDetail(); }
  }

  #actionFailed(title, subject, error) {
    const message = String(error?.message || error);
    this.#operationError = `${subject}: ${message}`;
    this.#renderErrors();
    emit(this, 'notify', { title, body: message, kind: 'err' });
  }

  #ask({ title, body, fields = [], okLabel = 'Continue', danger = false }) {
    if (this.#modal) this.#finishModal(false);
    return new Promise((resolve) => {
      this.#modal = {
        title, body, fields, okLabel, danger, resolve,
        returnFocus: this.#root.activeElement,
      };
      this.#renderModal();
    });
  }

  #renderModal() {
    const layer = this.#root.querySelector('.modal-layer');
    layer.toggleAttribute('data-open', Boolean(this.#modal));
    if (!this.#modal) {
      layer.querySelector('.modal-head').textContent = '';
      layer.querySelector('.modal-body').replaceChildren();
      return;
    }
    layer.querySelector('.modal-head').textContent = this.#modal.title;
    const body = layer.querySelector('.modal-body');
    body.replaceChildren(h('p', 'modal-copy', this.#modal.body));
    this.#modal.fields.forEach((field) => {
      const row = h('div', 'modal-field');
      const id = `mcp-field-${field.key}`;
      const label = h('label', '', field.label); label.htmlFor = id;
      const input = h('input', 'field'); input.id = id; input.name = field.key;
      input.type = field.type; input.placeholder = field.placeholder; input.required = field.required;
      input.autocomplete = 'off';
      row.append(label, input);
      if (field.help) row.append(h('small', '', field.help));
      body.append(row);
    });
    body.append(h('div', 'modal-error'));
    const confirm = layer.querySelector('.confirm');
    confirm.textContent = this.#modal.okLabel;
    confirm.classList.toggle('danger', this.#modal.danger);
    queueMicrotask(() => (body.querySelector('input') || confirm).focus());
  }

  #finishModal(confirmed) {
    if (!this.#modal) return;
    const modal = this.#modal;
    let result = false;
    if (confirmed && modal.fields.length) {
      const values = {};
      for (const field of modal.fields) {
        const value = this.#root.querySelector(`#mcp-field-${globalThis.CSS.escape(field.key)}`)?.value.trim() || '';
        if (field.required && !value) {
          this.#root.querySelector('.modal-error').textContent = `“${field.label}” is required.`;
          return;
        }
        values[field.key] = value;
      }
      result = values;
    } else if (confirmed) result = true;
    this.#modal = null;
    this.#renderModal();
    modal.resolve(result);
    queueMicrotask(() => modal.returnFocus?.focus?.());
  }
}

if (!customElements.get('ax-settings-mcp')) {
  customElements.define('ax-settings-mcp', AxSettingsMcp);
}
