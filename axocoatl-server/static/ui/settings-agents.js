import { adopt } from './sheets.js';
import {
  SETTINGS_CSS, emit, formatNumber, h, jsonRequest, teamClass,
} from './settings-common.js';

/**
 * `<ax-settings-agents>` owns agent status, token usage, editing and restart.
 * Agent edits remain in-memory because that is the server contract.
 *
 * @element ax-settings-agents
 * @fires agents-change detail: {agents, tokens, statuses}
 * @fires notify        detail: {title, body, kind}
 */

const CSS = `${SETTINGS_CSS}
.agent-table { min-width: 850px; }
.agent-table td:nth-child(6), .agent-table td:nth-child(7),
.agent-table th:nth-child(6), .agent-table th:nth-child(7) { text-align: right; }
.agent-table tr[aria-current="true"] { background: var(--bg-2); }
.status-note { color: var(--muted-2); font-size: var(--fs-xs); }
.deps { display: flex; flex-wrap: wrap; gap: var(--sp-1); }
.dep {
  display: inline-flex; align-items: center; gap: var(--sp-1);
  padding: 4px 9px; border: 1px solid var(--border); border-radius: var(--r-pill);
  background: var(--bg-3); cursor: pointer; font-size: var(--fs-xs);
}
.dep input { margin: 0; }
.prompt { min-height: 140px; font-family: var(--font-mono); }
.live-grid { display: grid; grid-template-columns: 112px minmax(0, 1fr); gap: var(--sp-2); align-items: center; }
.live-grid > span:nth-child(odd) { color: var(--muted); font-size: var(--fs-xs); }
.inline-action { padding: 3px var(--sp-2); }
.budget-switch { display: flex; align-items: center; gap: var(--sp-2); color: var(--text); font-size: var(--fs-xs); }
.budget-switch input { margin: 0; }
.budget-note { margin: calc(-1 * var(--sp-1)) 0 var(--sp-2) 120px; color: var(--muted-2); font-size: var(--fs-xs); }
.field:disabled { opacity: .55; }
`;

function normalizeStatus(value) {
  return String(value || '').replace(/[{}]/g, '').trim() || 'Unknown';
}

function statusClass(status) {
  if (status.startsWith('Idle')) return 'idle';
  if (status.startsWith('Running')) return 'running';
  return 'failed';
}

function dotClass(status) {
  if (status.startsWith('Running')) return 'run';
  if (status.startsWith('Idle')) return 'on';
  if (status === '—' || !status) return '';
  return 'err';
}

export class AxSettingsAgents extends HTMLElement {
  #root;
  #agents = [];
  #tokens = null;
  #statuses = {};
  #phase = 'idle';
  #error = '';
  #warnings = [];
  #operationError = '';
  #team = '';
  #query = '';
  #sideQuery = '';
  #selected = '';
  #generation = 0;
  #busy = '';
  #dirty = false;
  #draftRevision = 0;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="shell">
        <aside class="side">
          <div class="side-search"><input class="search side-query" type="search" placeholder="Filter teams, agents…" spellcheck="false"></div>
          <div class="side-scroll">
            <section class="side-section">
              <button class="side-head" type="button"><span class="tri">▾</span><span>Teams</span></button>
              <div class="side-list teams"></div>
            </section>
            <section class="side-section">
              <button class="side-head" type="button"><span class="tri">▾</span><span>All agents</span></button>
              <div class="side-list agents"></div>
            </section>
          </div>
        </aside>
        <main class="main">
          <div class="toolbar"><h2>Agents</h2><span class="sub count-label"></span><span class="grow"></span><input class="search row-query" type="search" placeholder="Filter…" spellcheck="false"></div>
          <div class="errors" role="status" aria-live="polite"></div>
          <div class="content table-host"></div>
          <aside class="drawer" aria-label="Agent details">
            <div class="drawer-head"><span class="muted">Agent</span><h3></h3><button class="icon-button close" type="button" title="Close">×</button></div>
            <div class="drawer-body"></div>
            <div class="drawer-foot"><span class="unsaved"></span><button class="action restart" type="button">↻ Restart only</button><button class="action primary save" type="button">Save &amp; restart</button></div>
          </aside>
        </main>
      </div>`;
    adopt(this.#root, CSS);
    this.#wire();
  }

  connectedCallback() {
    if (this.#phase === 'idle') void this.refresh();
  }

  get agents() {
    return this.#agents.map((agent) => ({
      ...agent, depends_on: [...(agent.depends_on || [])],
    }));
  }

  async refresh() {
    const generation = ++this.#generation;
    this.#phase = 'loading';
    this.#error = '';
    this.#warnings = [];
    this.#renderErrors();
    this.#root.querySelector('.shell').classList.add('loading');
    try {
      const agents = await jsonRequest('/api/agents');
      if (!Array.isArray(agents)) throw new Error('The Agents endpoint returned an invalid list.');

      const [tokensResult, ...statusResults] = await Promise.allSettled([
        jsonRequest('/api/tokens/report'),
        ...agents.map((agent) => jsonRequest(`/api/agents/${encodeURIComponent(agent.id)}/status`)),
      ]);
      if (generation !== this.#generation) return;

      this.#agents = agents;
      this.#tokens = tokensResult.status === 'fulfilled' ? tokensResult.value : null;
      if (tokensResult.status === 'rejected') {
        this.#warnings.push(`Token usage is unavailable: ${tokensResult.reason?.message || tokensResult.reason}`);
      }
      this.#statuses = {};
      let statusFailures = 0;
      statusResults.forEach((result, index) => {
        const id = agents[index].id;
        if (result.status === 'fulfilled') this.#statuses[id] = normalizeStatus(result.value?.status);
        else { this.#statuses[id] = '—'; statusFailures += 1; }
      });
      if (statusFailures) {
        this.#warnings.push(`Live status could not be read for ${statusFailures} agent${statusFailures === 1 ? '' : 's'}.`);
      }
      if (this.#selected && !agents.some((agent) => agent.id === this.#selected)) this.#selected = '';
      this.#phase = 'ready';
      if (this.#dirty && this.#selected) {
        // A background/section refresh may update roster and live facts, but
        // it must not rebuild the open form and erase the maintainer's draft.
        this.#renderErrors();
        this.#renderSidebar();
        this.#renderTable();
      } else {
        this.#render();
      }
      emit(this, 'agents-change', {
        agents: this.agents,
        tokens: this.#tokens,
        statuses: { ...this.#statuses },
      });
    } catch (error) {
      if (generation !== this.#generation) return;
      this.#phase = 'error';
      this.#error = String(error?.message || error);
      this.#renderErrors();
    } finally {
      if (generation === this.#generation) this.#root.querySelector('.shell').classList.remove('loading');
    }
  }

  openAgent(agentOrId) {
    const id = typeof agentOrId === 'object' ? agentOrId?.id : agentOrId;
    if (!id) return false;
    if (id === this.#selected) return true;
    const known = this.#agents.some((agent) => agent.id === id);
    if (!known && typeof agentOrId !== 'object') return false;
    if (!this.#mayLeaveDraft(`open ${id}`)) return false;
    if (!known) this.#agents = [...this.#agents, agentOrId];
    this.#selected = id;
    this.#dirty = false;
    this.#draftRevision += 1;
    this.#renderSidebar();
    this.#renderTable();
    this.#renderDrawer();
    queueMicrotask(() => this.#root.querySelector('.drawer input:not(:disabled)')?.focus());
    return true;
  }

  closeAgent() {
    if (!this.#selected) return true;
    if (!this.#mayLeaveDraft('close the agent editor')) return false;
    this.#selected = '';
    this.#dirty = false;
    this.#draftRevision += 1;
    this.#renderSidebar();
    this.#renderTable();
    this.#renderDrawer();
    return true;
  }

  #mayLeaveDraft(action) {
    if (this.#busy) {
      emit(this, 'notify', {
        title: 'Agent action in progress',
        body: `Wait for the current agent action before you ${action}.`,
        kind: 'warn',
      });
      return false;
    }
    if (!this.#dirty) return true;
    return globalThis.confirm?.(
      `Discard unsaved changes to ${this.#selected}?`,
    ) === true;
  }

  async restartAgent(agentOrId) {
    const id = typeof agentOrId === 'object' ? agentOrId?.id : agentOrId;
    if (!id || this.#busy) return false;
    this.#busy = `restart:${id}`;
    this.#setBusyButtons();
    try {
      await jsonRequest(`/api/agents/${encodeURIComponent(id)}/restart`, { method: 'POST' });
      this.#operationError = '';
      emit(this, 'notify', {
        title: 'Agent restarted',
        body: `${id} session restored from checkpoint`,
        kind: 'ok',
      });
      await this.#refreshLiveData();
      return true;
    } catch (error) {
      this.#operationError = `Could not restart ${id}: ${error?.message || error}`;
      this.#renderErrors();
      emit(this, 'notify', { title: 'Restart failed', body: String(error?.message || error), kind: 'err' });
      return false;
    } finally {
      this.#busy = '';
      this.#setBusyButtons();
      this.#renderTable();
    }
  }

  async #refreshLiveData() {
    const generation = ++this.#generation;
    const [tokensResult, ...statusResults] = await Promise.allSettled([
      jsonRequest('/api/tokens/report'),
      ...this.#agents.map((agent) => jsonRequest(`/api/agents/${encodeURIComponent(agent.id)}/status`)),
    ]);
    if (generation !== this.#generation) return;
    this.#tokens = tokensResult.status === 'fulfilled' ? tokensResult.value : this.#tokens;
    statusResults.forEach((result, index) => {
      this.#statuses[this.#agents[index].id] = result.status === 'fulfilled'
        ? normalizeStatus(result.value?.status) : '—';
    });
    // Live refresh deliberately leaves the drawer DOM alone: restarting one
    // actor must not erase unsaved edits in this or another agent's draft.
    this.#renderSidebar();
    this.#renderTable();
  }

  #wire() {
    this.#root.querySelector('.side-query').addEventListener('input', (event) => {
      this.#sideQuery = event.target.value.trim().toLowerCase();
      this.#renderSidebar();
    });
    this.#root.querySelector('.row-query').addEventListener('input', (event) => {
      this.#query = event.target.value.trim().toLowerCase();
      this.#renderTable();
    });
    this.#root.querySelectorAll('.side-head').forEach((button) => {
      button.addEventListener('click', () => button.closest('.side-section').toggleAttribute('data-collapsed'));
    });
    this.#root.querySelector('.drawer .close').addEventListener('click', () => this.closeAgent());
    this.#root.querySelector('.drawer .restart').addEventListener('click', () => {
      if (this.#selected) void this.restartAgent(this.#selected);
    });
    this.#root.querySelector('.drawer .save').addEventListener('click', () => void this.#saveSelected());
    this.#root.addEventListener('keydown', (event) => {
      if (event.key === 'Escape' && this.#selected) {
        event.preventDefault();
        event.stopPropagation();
        this.closeAgent();
      }
    });
  }

  #render() {
    this.#renderErrors();
    this.#renderSidebar();
    this.#renderTable();
    this.#renderDrawer();
  }

  #renderErrors() {
    const host = this.#root.querySelector('.errors');
    host.replaceChildren();
    if (this.#error) {
      const row = h('div', 'error');
      row.append(h('strong', '', 'Agents could not be loaded'));
      const retry = h('button', '', 'Retry');
      retry.type = 'button';
      retry.addEventListener('click', () => void this.refresh());
      row.append(retry, h('span', '', this.#error));
      host.append(row);
    }
    this.#warnings.forEach((warning) => {
      const row = h('div', 'error');
      row.append(h('strong', '', 'Some live data is unavailable'));
      const retry = h('button', '', 'Retry');
      retry.type = 'button';
      retry.addEventListener('click', () => void this.refresh());
      row.append(retry, h('span', '', warning));
      host.append(row);
    });
    if (this.#operationError) {
      const row = h('div', 'error');
      row.append(h('strong', '', 'Agent action failed'), h('span', '', this.#operationError));
      host.append(row);
    }
  }

  #renderSidebar() {
    const match = (value) => !this.#sideQuery || String(value || '').toLowerCase().includes(this.#sideQuery);
    const teams = new Map();
    this.#agents.forEach((agent) => {
      if (agent.team) teams.set(agent.team, (teams.get(agent.team) || 0) + 1);
    });
    const teamsHost = this.#root.querySelector('.teams');
    teamsHost.replaceChildren(this.#sideRow('All teams', this.#agents.length, !this.#team, () => {
      this.#team = ''; this.#renderSidebar(); this.#renderTable();
    }, null));
    [...teams.keys()].sort().filter(match).forEach((team) => {
      teamsHost.append(this.#sideRow(team, teams.get(team), this.#team === team, () => {
        this.#team = this.#team === team ? '' : team;
        this.#renderSidebar(); this.#renderTable();
      }, team));
    });

    const agentsHost = this.#root.querySelector('.agents');
    agentsHost.replaceChildren();
    const visible = this.#agents.filter((agent) => match(agent.id) || match(agent.team));
    if (!visible.length) agentsHost.append(h('div', 'side-empty', this.#sideQuery ? 'No matches.' : 'No agents.'));
    visible.forEach((agent) => {
      const row = h('button', 'side-row');
      row.type = 'button';
      row.setAttribute('aria-current', String(this.#selected === agent.id));
      row.append(h('span', `dot ${dotClass(this.#statuses[agent.id] || '')}`), h('span', 'label', agent.id));
      row.addEventListener('click', () => this.openAgent(agent.id));
      agentsHost.append(row);
    });
  }

  #sideRow(label, count, current, action, team) {
    const row = h('button', 'side-row');
    row.type = 'button';
    row.setAttribute('aria-current', String(current));
    if (team) row.append(h('span', `team-dot ${teamClass(team)}`));
    else row.append(h('span', 'ico', '⊕'));
    row.append(h('span', 'label', label), h('span', 'count', count));
    row.addEventListener('click', action);
    return row;
  }

  #visibleAgents() {
    return this.#agents.filter((agent) => {
      if (this.#team && agent.team !== this.#team) return false;
      if (!this.#query) return true;
      return [agent.id, agent.name, agent.team, agent.provider, agent.model]
        .some((value) => String(value || '').toLowerCase().includes(this.#query));
    });
  }

  #renderTable() {
    const host = this.#root.querySelector('.table-host');
    host.replaceChildren();
    const visible = this.#visibleAgents();
    this.#root.querySelector('.count-label').textContent = `${visible.length} of ${this.#agents.length}`;
    if (!visible.length) {
      host.append(h('div', 'empty', this.#phase === 'loading' ? 'Loading agents…' : 'No agents match.'));
      return;
    }
    const table = h('table', 'agent-table');
    const head = h('thead');
    const headRow = h('tr');
    ['ID', 'Team', 'Provider', 'Model', 'Status', 'Input', 'Output', ''].forEach((label) => headRow.append(h('th', '', label)));
    head.append(headRow);
    const body = h('tbody');
    visible.forEach((agent) => {
      const row = h('tr', 'clickable');
      row.tabIndex = 0;
      row.setAttribute('aria-current', String(this.#selected === agent.id));
      row.append(h('td', 'mono', agent.id));
      const team = h('td'); team.append(h('span', `badge team ${teamClass(agent.team)}`, agent.team || '—')); row.append(team);
      row.append(h('td', '', agent.provider || '—'), h('td', 'mono', agent.model || '—'));
      const status = this.#statuses[agent.id] || 'Unknown';
      const statusCell = h('td'); statusCell.append(h('span', `badge ${statusClass(status)}`, status)); row.append(statusCell);
      const usage = this.#tokens?.per_agent?.find((item) => item.agent_id === agent.id);
      row.append(h('td', 'mono', formatNumber(usage?.input_tokens)), h('td', 'mono', formatNumber(usage?.output_tokens)));
      const actions = h('td');
      const restart = h('button', 'action inline-action', this.#busy === `restart:${agent.id}` ? '…' : 'Restart');
      restart.type = 'button';
      restart.disabled = Boolean(this.#busy);
      restart.addEventListener('click', (event) => { event.stopPropagation(); void this.restartAgent(agent.id); });
      actions.append(restart); row.append(actions);
      row.addEventListener('click', () => this.openAgent(agent.id));
      row.addEventListener('keydown', (event) => {
        if (event.target !== row) return;
        if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); this.openAgent(agent.id); }
      });
      body.append(row);
    });
    table.append(head, body);
    host.append(table);
  }

  #renderDrawer() {
    const drawer = this.#root.querySelector('.drawer');
    const agent = this.#agents.find((candidate) => candidate.id === this.#selected);
    drawer.toggleAttribute('data-open', Boolean(agent));
    if (!agent) return;
    drawer.querySelector('h3').textContent = agent.id;
    const body = drawer.querySelector('.drawer-body');
    body.replaceChildren();

    const identity = this.#section('Identity');
    identity.append(
      this.#field('ID', this.#input('agent-id', agent.id, { disabled: true, mono: true })),
      this.#field('Name', this.#input('agent-name', agent.name || agent.id)),
      this.#field('Provider', this.#input('agent-provider', agent.provider || '', { disabled: true })),
      this.#field('Model', this.#input('agent-model', agent.model || '', { mono: true })),
    );
    const teamRow = h('div', 'field-row');
    teamRow.append(h('span', 'field-label', 'Team'));
    const teamValue = h('div'); teamValue.append(h('span', `badge team ${teamClass(agent.team)}`, agent.team || '—'));
    teamRow.append(teamValue); identity.append(teamRow);
    body.append(identity);

    const prompt = this.#section('System prompt');
    const promptArea = h('textarea', 'field prompt');
    promptArea.dataset.field = 'agent-prompt';
    promptArea.value = agent.system_prompt || '';
    prompt.append(promptArea); body.append(prompt);

    const budget = this.#section('Token budget');
    const hasBudget = agent.per_call_budget != null
      || agent.per_execution_budget != null
      || Boolean(agent.overflow_policy);
    const budgetSwitch = h('label', 'budget-switch');
    const budgetEnabled = h('input');
    budgetEnabled.type = 'checkbox';
    budgetEnabled.dataset.field = 'agent-budget-enabled';
    budgetEnabled.checked = hasBudget;
    budgetSwitch.append(budgetEnabled, h('span', '', 'Use a token spend cap'));
    budget.append(
      this.#field('Budget', budgetSwitch),
      h('p', 'budget-note', hasBudget ? 'Turn this off to remove the configured spend cap.' : 'No spend cap is configured for this agent.'),
      this.#field('Per call', this.#input('agent-per-call', agent.per_call_budget ?? 4096, { type: 'number', mono: true })),
      this.#field('Per execution', this.#input('agent-per-exec', agent.per_execution_budget ?? 16000, { type: 'number', mono: true })),
    );
    const policy = h('select', 'field');
    policy.dataset.field = 'agent-policy';
    ['abort', 'warn', 'summarize'].forEach((value) => {
      const option = h('option', '', value); option.value = value;
      option.selected = String(agent.overflow_policy || 'warn').toLowerCase() === value;
      policy.append(option);
    });
    budget.append(this.#field('Overflow policy', policy)); body.append(budget);
    const syncBudgetControls = () => {
      const enabled = budgetEnabled.checked;
      body.querySelectorAll('[data-field="agent-per-call"], [data-field="agent-per-exec"], [data-field="agent-policy"]')
        .forEach((control) => { control.disabled = !enabled; });
      body.querySelector('.budget-note').textContent = enabled
        ? 'Turn this off to remove the configured spend cap.'
        : 'No spend cap is configured for this agent.';
    };
    budgetEnabled.addEventListener('change', syncBudgetControls);
    syncBudgetControls();

    const dependencies = this.#section('Depends on');
    const deps = h('div', 'deps');
    this.#agents.filter((candidate) => candidate.id !== agent.id).forEach((candidate) => {
      const label = h('label', 'dep');
      const checkbox = h('input'); checkbox.type = 'checkbox'; checkbox.value = candidate.id;
      checkbox.dataset.dependency = '';
      checkbox.checked = (agent.depends_on || []).includes(candidate.id);
      label.append(checkbox, h('span', '', candidate.id)); deps.append(label);
    });
    if (!deps.children.length) deps.append(h('span', 'muted', 'No other agents.'));
    dependencies.append(deps); body.append(dependencies);

    const live = this.#section('Live');
    const grid = h('div', 'live-grid');
    const usage = this.#tokens?.per_agent?.find((item) => item.agent_id === agent.id);
    const status = this.#statuses[agent.id] || 'Unknown';
    grid.append(h('span', '', 'Status'));
    const statusValue = h('span'); statusValue.append(h('span', `badge ${statusClass(status)}`, status)); grid.append(statusValue);
    grid.append(h('span', '', 'Input tokens'), h('span', 'mono', formatNumber(usage?.input_tokens)));
    grid.append(h('span', '', 'Output tokens'), h('span', 'mono', formatNumber(usage?.output_tokens)));
    grid.append(h('span', '', 'Total'), h('span', 'mono', formatNumber((usage?.input_tokens || 0) + (usage?.output_tokens || 0))));
    live.append(grid, h('p', 'status-note', 'Agent edits are live for this daemon process; YAML is unchanged.'));
    body.append(live);

    body.querySelectorAll('input, textarea, select').forEach((control) => {
      control.addEventListener('input', () => this.#markDirty());
      control.addEventListener('change', () => this.#markDirty());
    });
    this.#setBusyButtons();
    this.#root.querySelector('.unsaved').textContent = this.#dirty ? '● unsaved' : '';
  }

  #section(title) {
    const section = h('section', 'section');
    section.append(h('h4', '', title));
    return section;
  }

  #field(label, control) {
    const row = h('div', 'field-row');
    row.append(h('label', '', label), control);
    return row;
  }

  #input(field, value, { disabled = false, mono = false, type = 'text' } = {}) {
    const input = h('input', `field${mono ? ' mono' : ''}`);
    input.dataset.field = field;
    input.type = type;
    input.value = value;
    input.disabled = disabled;
    if (type === 'number') input.min = '0';
    return input;
  }

  #markDirty() {
    this.#dirty = true;
    this.#draftRevision += 1;
    this.#root.querySelector('.unsaved').textContent = '● unsaved';
  }

  #readDraft() {
    const drawer = this.#root.querySelector('.drawer');
    const read = (name) => drawer.querySelector(`[data-field="${name}"]`)?.value ?? '';
    const perCall = read('agent-per-call');
    const perExecution = read('agent-per-exec');
    const parseBudget = (value, label) => {
      if (value === '') return null;
      const number = Number(value);
      if (!Number.isInteger(number) || number < 0) throw new Error(`${label} must be a non-negative whole number.`);
      return number;
    };
    const draft = {
      name: read('agent-name').trim() || null,
      model: read('agent-model').trim() || null,
      system_prompt: read('agent-prompt'),
      depends_on: [...drawer.querySelectorAll('[data-dependency]:checked')].map((input) => input.value),
      restart_now: true,
    };
    const budgetEnabled = drawer.querySelector('[data-field="agent-budget-enabled"]')?.checked === true;
    if (!budgetEnabled) {
      draft.clear_token_budget = true;
    } else {
      const perCallBudget = parseBudget(perCall, 'Per-call budget');
      const perExecutionBudget = parseBudget(perExecution, 'Per-execution budget');
      const overflowPolicy = read('agent-policy');
      if (perCallBudget === null) throw new Error('Per-call budget is required when the spend cap is enabled.');
      if (perExecutionBudget === null) throw new Error('Per-execution budget is required when the spend cap is enabled.');
      if (perCallBudget > perExecutionBudget) throw new Error('Per-call budget cannot exceed the per-execution budget.');
      if (!overflowPolicy) throw new Error('Overflow policy is required when the spend cap is enabled.');
      draft.per_call_budget = perCallBudget;
      draft.per_execution_budget = perExecutionBudget;
      draft.overflow_policy = overflowPolicy;
    }
    return draft;
  }

  async #saveSelected() {
    const id = this.#selected;
    if (!id || this.#busy) return;
    let body;
    try { body = this.#readDraft(); }
    catch (error) {
      this.#operationError = String(error?.message || error);
      this.#renderErrors();
      return;
    }
    const savedRevision = this.#draftRevision;
    this.#busy = `save:${id}`;
    this.#setBusyButtons();
    try {
      const result = await jsonRequest(`/api/agents/${encodeURIComponent(id)}`, {
        method: 'PATCH',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
      // The request owns only the snapshot it read. If the user typed again
      // while it was in flight, or another agent somehow became selected, the
      // response must not bless that newer draft as saved.
      if (this.#selected === id && this.#draftRevision === savedRevision) {
        this.#dirty = false;
        this.#draftRevision += 1;
      }
      this.#operationError = '';
      emit(this, 'notify', { title: 'Agent saved', body: result?.message || id, kind: 'ok' });
      await this.refresh();
    } catch (error) {
      this.#operationError = `Could not save ${id}: ${error?.message || error}`;
      this.#renderErrors();
      emit(this, 'notify', { title: 'Save failed', body: String(error?.message || error), kind: 'err' });
    } finally {
      this.#busy = '';
      this.#setBusyButtons();
      this.#renderTable();
    }
  }

  #setBusyButtons() {
    const restart = this.#root.querySelector('.drawer .restart');
    const save = this.#root.querySelector('.drawer .save');
    if (!restart || !save) return;
    restart.disabled = Boolean(this.#busy);
    save.disabled = Boolean(this.#busy);
    restart.textContent = this.#busy.startsWith('restart:') ? 'Restarting…' : '↻ Restart only';
    save.textContent = this.#busy.startsWith('save:') ? 'Saving…' : 'Save & restart';
  }
}

if (!customElements.get('ax-settings-agents')) {
  customElements.define('ax-settings-agents', AxSettingsAgents);
}
