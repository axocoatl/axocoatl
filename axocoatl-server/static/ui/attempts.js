import { adopt } from './sheets.js';

/**
 * `<ax-attempts>` — persistent run state beside a session's chat.
 *
 * This dock composes preparation and observation: plan one instruction, check
 * that the configured models can act, watch every way, understand the cost,
 * then open Outcome and Route for the decision. Keeping or discarding work is
 * deliberately absent; `<ax-compare>` owns that focused review.
 *
 * @element ax-attempts
 *
 * @attr {string} session  Session whose draft and active attempts are shown.
 *
 * @fires attempt-instruction  detail: {task, instruction, plan}
 * @fires attempt-explore      detail: {session} — open Explore several ways configuration
 * @fires attempt-review       detail: {session, attempt_set_id}
 * @fires attempts-error       detail: {action, session, attempt_set_id, message, ...context}
 */

const TERMINAL_LANE_STATES = new Set(['completed', 'failed', 'cancelled', 'interrupted']);
const TERMINAL_SET_STATES = new Set(['ready', 'verified', 'judged', 'failed']);
const KEEP_RECOVERY_STATES = new Set(['applying', 'applied', 'transcript_recorded']);

const html = (value) => String(value ?? '')
  .replaceAll('&', '&amp;')
  .replaceAll('<', '&lt;')
  .replaceAll('>', '&gt;')
  .replaceAll('"', '&quot;')
  .replaceAll("'", '&#39;');

const words = (value) => {
  const state = String(value || '').replaceAll('_', ' ');
  return state === 'discarding' ? 'cleaning up' : state;
};
const usd = (value) => {
  const number = Number(value) || 0;
  return '$' + (number > 0 && number < 0.01 ? number.toFixed(4) : number.toFixed(2));
};

async function jsonRequest(url, options) {
  const response = await fetch(url, options);
  const text = await response.text();
  let body = null;
  if (text) {
    try { body = JSON.parse(text); } catch { body = text; }
  }
  if (!response.ok) {
    const message = body && typeof body === 'object' ? body.error : body;
    const error = new Error(message || `HTTP ${response.status}`);
    if (body && typeof body === 'object' && body.control_usage) {
      error.controlUsage = body.control_usage;
    }
    throw error;
  }
  return body;
}

const CSS = `
:host {
  display: flex; flex-direction: column; min-width: 0; min-height: 0; height: 100%;
  background: var(--bg-2); color: var(--text); font-family: var(--font-sans);
  border-left: 1px solid var(--border);
}
.top {
  display: flex; align-items: center; gap: var(--sp-2); flex-shrink: 0;
  padding: var(--sp-3); border-bottom: 1px solid var(--border);
}
.top-copy { min-width: 0; flex: 1; }
.top h2 { margin: 0; font-size: var(--fs-body); font-weight: var(--fw-medium); }
.top p { margin: 1px 0 0; color: var(--muted-2); font-size: var(--fs-xs); }
.scroll { flex: 1; min-height: 0; overflow: auto; }
.sr-only {
  position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
  overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
}
.section { padding: var(--sp-3); border-bottom: 1px solid var(--border); }
.section-head {
  display: flex; align-items: center; gap: var(--sp-2); margin-bottom: var(--sp-3);
}
.section-head h3 {
  margin: 0; flex: 1; font-size: var(--fs-sm); font-weight: var(--fw-medium);
}
.count { color: var(--muted-2); font: var(--fs-xs) var(--font-mono); }
.eyebrow {
  display: block; margin-bottom: 3px; color: var(--muted-2);
  font: var(--fw-bold) 9.5px var(--font-mono); letter-spacing: .07em;
  text-transform: uppercase;
}
.task {
  margin: 0 0 var(--sp-3); color: var(--text); font-size: var(--fs-sm);
  white-space: pre-wrap; overflow-wrap: anywhere;
}
.empty, .hint { color: var(--muted-2); font-size: var(--fs-xs); line-height: 1.45; }
.empty { padding: var(--sp-2) 0; }
.empty-action { display: grid; justify-items: start; gap: var(--sp-2); }
.loading { opacity: .65; }
.errors { flex-shrink: 0; }
.errors:empty { display: none; }
.error {
  display: grid; grid-template-columns: 1fr auto; gap: var(--sp-1) var(--sp-2);
  padding: var(--sp-2) var(--sp-3); color: var(--err); background: var(--bg-3);
  border-bottom: 1px solid var(--border); font-size: var(--fs-xs);
}
.error strong { font-weight: var(--fw-medium); }
.error span { grid-column: 1 / -1; white-space: pre-wrap; overflow-wrap: anywhere; }
.stale {
  margin-bottom: var(--sp-2); padding: var(--sp-2); color: var(--warn);
  border: 1px solid var(--warn); border-radius: var(--r-sm); font-size: var(--fs-xs);
}
.availability {
  margin-bottom: var(--sp-3); padding: var(--sp-2); color: var(--warn);
  border: 1px solid var(--warn); border-radius: var(--r-sm);
  font-size: var(--fs-xs); line-height: 1.45;
}
button, select, textarea { font-family: var(--font-sans); }
button.action, button.icon {
  background: none; border: 1px solid var(--border-strong); color: var(--text);
  border-radius: var(--r-md); cursor: pointer;
  font: var(--fw-medium) var(--fs-xs) var(--font-sans);
}
button.action { padding: 4px var(--sp-3); }
button.icon { width: 28px; height: 28px; padding: 0; color: var(--muted); }
button.action:hover:not(:disabled), button.icon:hover:not(:disabled) {
  border-color: var(--accent); color: var(--accent);
}
button.primary { background: var(--accent); border-color: var(--accent); color: var(--bg); }
button.primary:hover:not(:disabled) { color: var(--bg); filter: brightness(1.08); }
button.link {
  background: none; border: 0; padding: 0; color: inherit; cursor: pointer;
  font: inherit; text-decoration: underline;
}
button:disabled { cursor: not-allowed; opacity: .45; }
button:focus-visible, select:focus-visible, textarea:focus-visible {
  outline: none; box-shadow: var(--focus-ring);
}
.controls { display: grid; gap: var(--sp-2); }
.field { display: grid; gap: 3px; min-width: 0; }
.field > span { color: var(--muted); font-size: var(--fs-xs); }
select, textarea {
  width: 100%; min-width: 0; box-sizing: border-box; background: var(--bg-3);
  color: var(--text); border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 5px var(--sp-2); font-size: var(--fs-xs);
}
textarea {
  min-height: 11rem; resize: vertical; font-family: var(--font-mono);
  line-height: 1.45; tab-size: 2;
}
.actions { display: flex; align-items: center; flex-wrap: wrap; gap: var(--sp-2); }
.actions .hint { flex: 1; min-width: 9rem; }

.ways { display: grid; gap: var(--sp-1); margin-top: var(--sp-3); }
.way {
  display: grid; grid-template-columns: auto minmax(0, 1fr) auto;
  align-items: center; gap: var(--sp-2); padding: var(--sp-2);
  border: 1px solid var(--border); border-radius: var(--r-md); background: var(--bg-3);
}
.way-copy { min-width: 0; }
.way-title {
  display: flex; align-items: baseline; gap: var(--sp-1); min-width: 0;
  font-size: var(--fs-xs); font-weight: var(--fw-medium);
}
.way-title span {
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.way-meta {
  margin-top: 2px; color: var(--muted-2); font: 10px var(--font-mono);
  white-space: normal; overflow-wrap: anywhere;
}
.way-error {
  grid-column: 2 / -1; color: var(--err); font-size: var(--fs-xs);
  white-space: pre-wrap; overflow-wrap: anywhere;
}
.way-detail {
  grid-column: 2 / -1; color: var(--muted-2); font-size: var(--fs-xs);
  white-space: pre-wrap; overflow-wrap: anywhere;
}
.way-detail.bad { color: var(--err); }
.dot { width: 8px; height: 8px; border-radius: 50%; background: var(--muted-2); }
.dot.running, .dot.queued, .dot.checking, .dot.discarding {
  background: var(--warn); animation: pulse 1.4s ease-in-out infinite;
}
.dot.completed, .dot.usable { background: var(--ok); }
.dot.failed, .dot.cancelled, .dot.interrupted, .dot.unusable { background: var(--err); }
@keyframes pulse { 0%,100% { opacity: 1 } 50% { opacity: .3 } }
.state {
  color: var(--muted); font-size: 10px; text-transform: capitalize; text-align: right;
}
.state.good { color: var(--ok); }
.state.bad { color: var(--err); }
.state.live { color: var(--warn); }
.set-state {
  display: inline-flex; width: fit-content; align-items: center;
  border: 1px solid var(--border-strong); border-radius: 999px;
  padding: 1px 7px; color: var(--muted); font-size: var(--fs-xs); text-transform: capitalize;
}
.set-state[data-state="running"], .set-state[data-state="preparing"],
.set-state[data-state="checking"], .set-state[data-state="discarding"] {
  color: var(--warn); border-color: var(--warn);
}
.set-state[data-state="applying"], .set-state[data-state="applied"],
.set-state[data-state="transcript_recorded"] { color: var(--warn); border-color: var(--warn); }
.set-state[data-state="ready"], .set-state[data-state="verified"],
.set-state[data-state="judged"] { color: var(--ok); border-color: var(--ok); }
.set-state[data-state="failed"] { color: var(--err); border-color: var(--err); }

.plan {
  margin-top: var(--sp-3); padding: var(--sp-3); border: 1px solid var(--border);
  border-radius: var(--r-md); background: var(--panel);
}
.plan h4 { margin: 0 0 var(--sp-2); font-size: var(--fs-sm); font-weight: var(--fw-medium); }
.plan p { margin: 0; font-size: var(--fs-xs); line-height: 1.45; white-space: pre-wrap; }
.plan-group { margin-top: var(--sp-3); }
.plan ol, .plan ul { margin: var(--sp-1) 0 0; padding-left: var(--sp-4); }
.plan li { margin: 0 0 var(--sp-1); font-size: var(--fs-xs); line-height: 1.4; }
.plan code { color: var(--accent-2); font-family: var(--font-mono); overflow-wrap: anywhere; }
.instruction { margin-top: var(--sp-3); }

.cost-grid { display: grid; grid-template-columns: 1fr 1fr; gap: var(--sp-2); margin-top: var(--sp-3); }
.cost-card {
  min-width: 0; padding: var(--sp-2); border: 1px solid var(--border);
  border-radius: var(--r-md); background: var(--bg-3);
}
.cost-card.wide { grid-column: 1 / -1; }
.cost-value {
  margin-top: 3px; font: var(--fw-medium) var(--fs-body) var(--font-mono);
  overflow-wrap: anywhere;
}
.cost-detail { margin-top: 2px; color: var(--muted-2); font-size: 10px; line-height: 1.35; }
.cost-ways { margin-top: var(--sp-3); display: grid; gap: 3px; }
.cost-way {
  display: flex; align-items: baseline; gap: var(--sp-2); color: var(--muted);
  font-size: var(--fs-xs);
}
.cost-way span:first-child { flex: 1; }
.cost-way code { font-family: var(--font-mono); color: var(--text); }
.review {
  display: grid; gap: var(--sp-2); margin-top: var(--sp-3); padding-top: var(--sp-3);
  border-top: 1px solid var(--border);
}

@media (max-width: 340px) {
  .section, .top { padding-left: var(--sp-2); padding-right: var(--sp-2); }
  .cost-grid { grid-template-columns: 1fr; }
  .cost-card.wide { grid-column: auto; }
  .way { grid-template-columns: auto minmax(0, 1fr); }
  .way .state { grid-column: 2; text-align: left; }
}
@media (prefers-reduced-motion: reduce) {
  .dot { animation: none !important; }
}
`;

export class AxAttempts extends HTMLElement {
  static get observedAttributes() { return ['session']; }

  #root; #errorsHost; #draftHost; #activeHost; #costSelectorHost;
  #costBodyHost; #lifecycleHost;
  #agents = [];
  #sessionRecord = null;
  #plannerAgentId = '';
  #baselineModel = '';
  #baselineProvider = '';
  #draft = { task: '', lanes: [] };
  #plan = null;
  #planUsage = null;
  #instruction = '';
  #probes = new Map();
  #planning = false;
  #probing = false;
  #results = null;
  #resultsStale = '';
  #cost = null;
  #costLoading = false;
  #errors = new Map();
  #lastErrorEvents = new Map();
  #pollTimer = null;
  #contextRequestId = 0;
  #refreshRequestId = 0;
  #planRequestId = 0;
  #probeRequestId = 0;
  #costRequestId = 0;
  #costSelectorSignature = '';
  #lifecycleSignature = '';

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="top">
        <div class="top-copy"><h2>Attempts</h2><p>Prepare, watch, then review.</p></div>
        <button type="button" class="icon" data-action="refresh" aria-label="Refresh attempts"
          title="Refresh attempts">↻</button>
      </div>
      <div class="errors" aria-live="polite"></div>
      <div class="scroll">
        <section class="section" aria-labelledby="attempt-draft-title">
          <div class="section-head"><h3 id="attempt-draft-title">Plan &amp; preflight</h3></div>
          <div class="draft-host"></div>
        </section>
        <section class="section" aria-labelledby="attempt-run-title">
          <div class="section-head"><h3 id="attempt-run-title">Current attempts</h3></div>
          <div class="sr-only lifecycle-status" role="status" aria-live="polite"
            aria-atomic="true"></div>
          <div class="active-host"></div>
        </section>
        <section class="section" aria-labelledby="attempt-cost-title">
          <div class="section-head"><h3 id="attempt-cost-title">Attempt execution cost</h3></div>
          <div class="hint">Ways only; Plan first, model checks, and Judge are reported separately and not included.</div>
          <div class="cost-host">
            <div class="cost-selector-host"></div>
            <div class="cost-body-host"></div>
          </div>
        </section>
      </div>`;
    this.#errorsHost = this.#root.querySelector('.errors');
    this.#draftHost = this.#root.querySelector('.draft-host');
    this.#activeHost = this.#root.querySelector('.active-host');
    this.#costSelectorHost = this.#root.querySelector('.cost-selector-host');
    this.#costBodyHost = this.#root.querySelector('.cost-body-host');
    this.#lifecycleHost = this.#root.querySelector('.lifecycle-status');

    this.#root.addEventListener('click', (event) => {
      const button = event.target.closest('[data-action]');
      if (!button) return;
      const action = button.dataset.action;
      if (action === 'refresh') void this.refresh();
      if (action === 'plan') void this.#planFirst();
      if (action === 'use-plan') this.#usePlan();
      if (action === 'probe') void this.#checkModels();
      if (action === 'explore') this.#requestExplore();
      if (action === 'review') this.#reviewOutcomes();
      if (action === 'retry') this.#retry(button.dataset.retry);
      if (action === 'cost') void this.#loadCost();
    });
    this.#root.addEventListener('change', (event) => {
      if (event.target.matches('[data-planner]')) {
        const plannerChanged = this.#plannerAgentId !== event.target.value;
        this.#plannerAgentId = event.target.value;
        if (plannerChanged) {
          this.#planRequestId++;
          this.#planning = false;
          this.#plan = null;
          this.#planUsage = null;
          this.#instruction = '';
        }
        this.#clearError('plan');
        this.#chooseDefaults();
        this.#renderDraft();
        this.#renderCost();
      }
      if (event.target.matches('[data-baseline]')) {
        const choice = this.#baselineChoices()
          .find((candidate) => candidate.key === event.target.value);
        this.#baselineModel = choice?.model || '';
        this.#baselineProvider = choice?.provider || '';
        this.#cost = null;
        this.#costLoading = false;
        this.#costRequestId++;
        this.#clearError('cost');
        this.#renderCost();
        void this.#loadCost();
      }
    });
    this.#root.addEventListener('input', (event) => {
      if (!event.target.matches('[data-instruction]')) return;
      this.#instruction = event.target.value;
      const use = this.#draftHost.querySelector('[data-action="use-plan"]');
      if (use) use.disabled = !this.#instruction.trim();
    });
    adopt(this.#root, CSS);
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(value) { value ? this.setAttribute('session', value) : this.removeAttribute('session'); }

  connectedCallback() {
    this.#renderAll();
    if (!this.session) return;
    void this.#loadContext();
    void this.refresh();
  }

  disconnectedCallback() {
    this.#stopPolling();
    this.#contextRequestId++;
    this.#refreshRequestId++;
    this.#planRequestId++;
    this.#probeRequestId++;
    this.#costRequestId++;
    // A detached request is intentionally ignored. Clear its visual lock too,
    // so reattaching this same element cannot strand its controls as busy.
    this.#planning = false;
    this.#probing = false;
    this.#costLoading = false;
  }

  attributeChangedCallback(name, previous, next) {
    if (name !== 'session' || previous === next) return;
    // The shell can hand us a composer draft before it binds the first
    // session. Keep that draft on the initial bind, but never carry it across
    // an actual session switch.
    const keepInitialDraft = !previous && Boolean(next);
    this.#stopPolling();
    this.#contextRequestId++;
    this.#refreshRequestId++;
    this.#planRequestId++;
    this.#probeRequestId++;
    this.#costRequestId++;
    this.#sessionRecord = null;
    if (!keepInitialDraft) this.#draft = { task: '', lanes: [] };
    this.#plan = null;
    this.#planUsage = null;
    this.#instruction = '';
    this.#probes.clear();
    this.#planning = false;
    this.#probing = false;
    this.#results = null;
    this.#resultsStale = '';
    this.#cost = null;
    this.#costLoading = false;
    this.#errors.clear();
    this.#lastErrorEvents.clear();
    this.#renderAll();
    if (this.isConnected && this.session) {
      void this.#loadContext();
      void this.refresh();
    }
  }

  /** Supply the task and way configuration currently waiting in the composer. */
  setDraft(value = {}) {
    const task = String(value?.task || '');
    const lanes = Array.isArray(value?.lanes)
      ? value.lanes.map((lane) => ({
        ...(lane?.agent ? { agent: String(lane.agent) } : {}),
        ...(lane?.model ? { model: String(lane.model) } : {}),
      }))
      : [];
    const taskChanged = task !== this.#draft.task;
    const lanesChanged = JSON.stringify(lanes) !== JSON.stringify(this.#draft.lanes);
    this.#draft = { task, lanes };
    if (taskChanged) {
      this.#planRequestId++;
      this.#planning = false;
      this.#plan = null;
      this.#planUsage = null;
      this.#instruction = '';
      this.#clearError('plan');
    }
    if (taskChanged || lanesChanged) {
      this.#probes.clear();
      this.#clearError('probe');
      this.#probeRequestId++;
      this.#probing = false;
    }
    this.#renderDraft();
  }

  #draftSignature() {
    return JSON.stringify(this.#draft);
  }

  #attemptSet() { return this.#results?.attempt_set || null; }

  #attemptSetId() { return this.#attemptSet()?.id || ''; }

  #lanes() {
    const lanes = this.#attemptSet()?.lanes;
    return Array.isArray(lanes) ? lanes : (this.#results?.lanes || []);
  }

  #setState() {
    const state = this.#attemptSet()?.state;
    if (state) return state;
    if (!this.#lanes().length) return '';
    if (this.#results?.judgment) return 'judged';
    if ((this.#results?.verdicts || []).length) return 'verified';
    return 'ready';
  }

  #laneFact(index) {
    return (this.#results?.lane_states || []).find((lane) => lane.index === index) || null;
  }

  #laneState(index) {
    const state = this.#laneFact(index)?.state;
    if (state) return state;
    const setState = this.#setState();
    if (['ready', 'checking', 'verified', 'judged'].includes(setState)) return 'completed';
    if (setState === 'failed') return 'failed';
    if (setState === 'preparing') return 'queued';
    if (setState === 'discarding') return 'discarding';
    return setState === 'running' ? 'running' : 'queued';
  }

  #isTerminal() {
    if (!this.#attemptSetId()) return false;
    if (TERMINAL_SET_STATES.has(this.#setState())) return true;
    if (['checking', 'discarding'].includes(this.#setState())
        || KEEP_RECOVERY_STATES.has(this.#setState())) return true;
    const lanes = this.#lanes();
    return lanes.length > 0 && lanes.every((lane) => TERMINAL_LANE_STATES.has(this.#laneState(lane.index)));
  }

  #isRunning() {
    if (['preparing', 'running'].includes(this.#setState())) return true;
    return this.#lanes().some((lane) => ['queued', 'running'].includes(this.#laneState(lane.index)));
  }

  #shouldPoll() {
    return this.#isRunning()
      || ['checking', 'discarding'].includes(this.#setState())
      || KEEP_RECOVERY_STATES.has(this.#setState());
  }

  #primarySessionAgentId() {
    const mode = this.#sessionRecord?.mode;
    if (mode?.kind === 'single_agent') return mode.agent_id || '';
    if (mode?.kind === 'custom' && Array.isArray(mode.agents)) return mode.agents[0] || '';
    return '';
  }

  #supportsAttempts() {
    return this.#sessionRecord?.mode?.kind === 'single_agent';
  }

  #agent(id) { return this.#agents.find((agent) => agent.id === id) || null; }

  #resolveDraftWay(lane, index) {
    const agentId = lane.agent || this.#primarySessionAgentId();
    const agent = this.#agent(agentId);
    return {
      index,
      agentId,
      agentLabel: agent?.name || agentId || 'session agent',
      provider: agent?.provider || '',
      model: lane.model || agent?.model || '',
      inherited: !lane.agent,
    };
  }

  #draftResolutionSignature() {
    return JSON.stringify(this.#draft.lanes.map((lane, index) => {
      const way = this.#resolveDraftWay(lane, index);
      return [way.agentId, way.provider, way.model];
    }));
  }

  #resolveActiveWay(lane) {
    const agentId = lane.agent || this.#primarySessionAgentId();
    const agent = this.#agent(agentId);
    return {
      agentId,
      agentLabel: agent?.name || agentId || 'session agent',
      provider: lane.provider || agent?.provider || '',
      model: lane.model || agent?.model || '',
    };
  }

  #plannerAgent() { return this.#agent(this.#plannerAgentId); }

  #baselineChoices() {
    const models = new Map();
    for (const agent of this.#agents) {
      if (!agent.model) continue;
      const key = JSON.stringify([agent.provider || '', agent.model]);
      if (models.has(key)) continue;
      models.set(key, {
        key,
        model: agent.model,
        provider: agent.provider || '',
        label: `${agent.model}${agent.provider ? ` · ${agent.provider}` : ''}`,
      });
    }
    return [...models.values()];
  }

  #chooseDefaults() {
    const previousBaseline = `${this.#baselineProvider}\u0000${this.#baselineModel}`;
    if (!this.#plannerAgent()) {
      const primary = this.#agent(this.#primarySessionAgentId());
      const planner = primary || this.#agents[0];
      this.#plannerAgentId = planner?.id || '';
    }
    const choices = this.#baselineChoices();
    if (!choices.some((choice) => choice.model === this.#baselineModel
        && choice.provider === this.#baselineProvider)) {
      const plannerModel = this.#plannerAgent()?.model;
      const plannerProvider = this.#plannerAgent()?.provider || '';
      const selected = choices.find((choice) => choice.model === plannerModel
        && choice.provider === plannerProvider) || choices[0];
      this.#baselineModel = selected?.model || '';
      this.#baselineProvider = selected?.provider || '';
    }
    if (`${this.#baselineProvider}\u0000${this.#baselineModel}` !== previousBaseline) {
      this.#costRequestId++;
      this.#cost = null;
      this.#costLoading = false;
      this.#clearError('cost');
    }
  }

  async #loadContext() {
    const requestId = ++this.#contextRequestId;
    const sessionId = this.session;
    const resolutionBefore = this.#draftResolutionSignature();
    const plannerBefore = this.#plannerAgent();
    const plannerTargetBefore = JSON.stringify([
      this.#plannerAgentId,
      plannerBefore?.provider || '',
      plannerBefore?.model || '',
    ]);
    const [agentsResult, sessionsResult] = await Promise.allSettled([
      jsonRequest('/api/agents'),
      jsonRequest('/api/sessions'),
    ]);
    if (requestId !== this.#contextRequestId || sessionId !== this.session || !this.isConnected) return;

    const agentList = Array.isArray(agentsResult.value) ? agentsResult.value
      : (Array.isArray(agentsResult.value?.agents) ? agentsResult.value.agents : null);
    if (agentsResult.status === 'fulfilled' && agentList) {
      const list = agentList;
      this.#agents = list.filter((agent) => agent?.id).map((agent) => ({
        id: String(agent.id),
        name: String(agent.name || agent.id),
        provider: String(agent.provider || ''),
        model: String(agent.model || ''),
        role: String(agent.role || ''),
      })).filter((agent) => agent.role === 'autonomous');
      this.#clearError('agents');
    } else {
      const error = agentsResult.status === 'rejected'
        ? agentsResult.reason : new Error('The agents endpoint returned an invalid list.');
      this.#reportError('agents', error, {
        session: sessionId,
        attempt_set_id: null,
      });
    }
    if (sessionsResult.status === 'fulfilled' && Array.isArray(sessionsResult.value)) {
      const sessions = sessionsResult.value;
      this.#sessionRecord = sessions.find((session) => session.id === sessionId) || null;
      this.#clearError('sessions');
    } else {
      const error = sessionsResult.status === 'rejected'
        ? sessionsResult.reason : new Error('The sessions endpoint returned an invalid list.');
      this.#reportError('sessions', error, {
        session: sessionId,
        attempt_set_id: null,
      });
    }
    this.#chooseDefaults();
    if (resolutionBefore !== this.#draftResolutionSignature()) {
      this.#probeRequestId++;
      this.#probing = false;
      this.#probes.clear();
      this.#clearError('probe');
    }
    const planner = this.#plannerAgent();
    const plannerTarget = JSON.stringify([
      this.#plannerAgentId,
      planner?.provider || '',
      planner?.model || '',
    ]);
    if (plannerTarget !== plannerTargetBefore) {
      this.#planRequestId++;
      this.#planning = false;
      this.#plan = null;
      this.#planUsage = null;
      this.#instruction = '';
      this.#clearError('plan');
    }
    this.#renderAll();
    if (this.#attemptSetId() && this.#baselineModel) void this.#loadCost();
  }

  async #planFirst() {
    const task = this.#draft.task.trim();
    const planner = this.#plannerAgent();
    if (this.#planning || !this.session || !this.#supportsAttempts()
        || !task || !planner?.provider) return;
    const requestId = ++this.#planRequestId;
    const sessionId = this.session;
    const draftSignature = this.#draftSignature();
    const plannerId = planner.id;
    const plannerProvider = planner.provider;
    const plannerModel = planner.model;
    this.#planning = true;
    this.#clearError('plan');
    this.#renderDraft();
    try {
      const body = { task, agent_id: planner.id };
      const response = await jsonRequest(
        `/api/sessions/${encodeURIComponent(sessionId)}/variants/plan`, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(body),
        });
      if (requestId !== this.#planRequestId || sessionId !== this.session
          || draftSignature !== this.#draftSignature() || plannerId !== this.#plannerAgentId
          || plannerProvider !== this.#plannerAgent()?.provider
          || plannerModel !== this.#plannerAgent()?.model) return;
      if (!response || typeof response !== 'object' || typeof response.instruction !== 'string') {
        throw new Error('The planning agent returned an invalid plan.');
      }
      this.#plan = {
        summary: String(response.summary || ''),
        steps: Array.isArray(response.steps) ? response.steps.map((step) => ({
          path: String(step?.path || ''), change: String(step?.change || ''),
        })) : [],
        constraints: Array.isArray(response.constraints)
          ? response.constraints.map(String) : [],
        acceptance: Array.isArray(response.acceptance)
          ? response.acceptance.map(String) : [],
      };
      this.#planUsage = response.control_usage && typeof response.control_usage === 'object'
        ? response.control_usage : null;
      this.#instruction = response.instruction;
      this.#clearError('plan');
    } catch (error) {
      if (requestId !== this.#planRequestId) return;
      if (error?.controlUsage && typeof error.controlUsage === 'object') {
        this.#planUsage = error.controlUsage;
      }
      const usage = this.#controlUsageLabel(error?.controlUsage, 'Plan');
      const visibleError = usage
        ? new Error(`${String(error?.message || error)}\n${usage}`)
        : error;
      this.#reportError('plan', visibleError, {
        session: sessionId,
        attempt_set_id: null,
        task,
        planner_agent_id: plannerId,
        provider: plannerProvider,
        model: plannerModel || null,
      });
    } finally {
      if (requestId === this.#planRequestId) {
        this.#planning = false;
        this.#renderDraft();
      }
    }
  }

  #usePlan() {
    if (!this.#plan || !this.#instruction.trim()) return;
    const plan = JSON.parse(JSON.stringify(this.#plan));
    this.dispatchEvent(new CustomEvent('attempt-instruction', {
      detail: {
        task: this.#draft.task,
        instruction: this.#instruction.trim(),
        plan,
      },
      bubbles: true,
      composed: true,
    }));
  }

  async #checkModels() {
    if (this.#probing || !this.session || !this.#supportsAttempts()
        || !this.#draft.lanes.length) return;
    const requestId = ++this.#probeRequestId;
    const sessionId = this.session;
    const draftSignature = this.#draftSignature();
    const resolved = this.#draft.lanes.map((lane, index) => this.#resolveDraftWay(lane, index));
    const groups = new Map();
    this.#probes.clear();
    for (const way of resolved) {
      if (!way.provider || !way.model) {
        this.#probes.set(way.index, {
          status: 'failed',
          detail: !way.provider
            ? 'Its agent provider could not be resolved. Choose an explicit configured agent.'
            : 'Its model could not be resolved from the configured agent.',
          provider: way.provider,
          model: way.model,
        });
        continue;
      }
      if (!groups.has(way.provider)) groups.set(way.provider, new Map());
      const models = groups.get(way.provider);
      if (!models.has(way.model)) models.set(way.model, []);
      models.get(way.model).push(way.index);
    }
    this.#probing = true;
    this.#clearError('probe');
    this.#renderDraft();

    const requests = [...groups].map(async ([provider, models]) => {
      const params = new URLSearchParams({ provider, models: [...models.keys()].join(',') });
      try {
        const response = await jsonRequest(`/api/variants/probe?${params}`);
        if (!Array.isArray(response)) {
          throw new Error(`The ${provider} model check returned an invalid result.`);
        }
        return {
          provider,
          models,
          response,
          error: null,
        };
      } catch (error) {
        return { provider, models, response: null, error };
      }
    });
    const responses = await Promise.all(requests);
    if (requestId !== this.#probeRequestId || sessionId !== this.session
        || draftSignature !== this.#draftSignature()) return;

    for (const group of responses) {
      if (group.error) {
        this.#reportError('probe', group.error, {
          session: sessionId,
          attempt_set_id: null,
          provider: group.provider,
        });
        for (const [model, indexes] of group.models) {
          for (const index of indexes) {
            this.#probes.set(index, {
              status: 'failed',
              detail: `Could not check ${model}: ${group.error.message || group.error}`,
              provider: group.provider,
              model,
            });
          }
        }
        continue;
      }
      const list = Array.isArray(group.response) ? group.response : [];
      const byModel = new Map(list.map((probe) => [probe.model, probe]));
      for (const [model, indexes] of group.models) {
        const probe = byModel.get(model);
        for (const index of indexes) {
          this.#probes.set(index, {
            status: !probe ? 'failed' : (probe.usable === true ? 'usable' : 'unusable'),
            detail: String(probe?.detail || 'The provider returned no result for this model.'),
            provider: group.provider,
            model,
            control_usage: probe?.control_usage || null,
          });
        }
      }
    }
    this.#probing = false;
    this.#renderDraft();
  }

  /** Refresh the current durable attempt set. */
  async refresh() {
    const sessionId = this.session;
    if (!sessionId) {
      this.#results = null;
      this.#renderActive();
      this.#renderCost();
      return;
    }
    this.#stopPolling();
    const requestId = ++this.#refreshRequestId;
    const previousSetId = this.#attemptSetId();
    this.#renderActive();
    try {
      const results = await jsonRequest(
        `/api/sessions/${encodeURIComponent(sessionId)}/variants/results`);
      if (requestId !== this.#refreshRequestId || sessionId !== this.session || !this.isConnected) return;
      if (!results || typeof results !== 'object' || Array.isArray(results)
          || (results.lanes != null && !Array.isArray(results.lanes))
          || (results.attempt_set != null && (!results.attempt_set.id
            || !Array.isArray(results.attempt_set.lanes)
            || (results.attempt_set.session_id
              && results.attempt_set.session_id !== sessionId)))) {
        throw new Error('The attempts endpoint returned an invalid result.');
      }
      this.#results = results || {};
      this.#resultsStale = '';
      this.#clearError('results');
      if (previousSetId !== this.#attemptSetId()) {
        this.#costRequestId++;
        this.#cost = null;
        this.#costLoading = false;
        this.#clearError('cost');
      }
    } catch (error) {
      if (requestId !== this.#refreshRequestId || sessionId !== this.session) return;
      this.#resultsStale = String(error?.message || error);
      this.#reportError('results', error, {
        session: sessionId,
        attempt_set_id: previousSetId || null,
      });
    } finally {
      if (requestId !== this.#refreshRequestId || sessionId !== this.session) return;
      this.#renderActive();
      this.#renderCost();
    }

    const keepRecovery = KEEP_RECOVERY_STATES.has(this.#setState());
    const steadyPolling = keepRecovery || this.#setState() === 'checking';
    if (this.#setState() !== 'discarding' && this.#attemptSetId() && this.#baselineModel
        && (steadyPolling ? (!this.#cost && !this.#costLoading)
          : (!this.#costLoading || this.#isTerminal()))) {
      // Cost reads are independent of lifecycle reads. Do not let one slow
      // sandbox read pause the roster poll; a terminal refresh deliberately
      // supersedes any in-flight partial calculation with the final one.
      void this.#loadCost();
    }
    if (requestId !== this.#refreshRequestId || sessionId !== this.session) return;
    if ((this.#shouldPoll() || this.#resultsStale) && this.isConnected) {
      this.#pollTimer = setTimeout(() => {
        this.#pollTimer = null;
        void this.refresh();
      }, 1500);
    }
  }

  async #loadCost() {
    const sessionId = this.session;
    const attemptSetId = this.#attemptSetId();
    const baseline = this.#baselineModel;
    const baselineProvider = this.#baselineProvider;
    if (!sessionId || !attemptSetId || !baseline || this.#setState() === 'discarding') {
      this.#renderCost();
      return;
    }
    const requestId = ++this.#costRequestId;
    this.#costLoading = true;
    this.#renderCost();
    const params = new URLSearchParams({ attempt_set_id: attemptSetId, baseline });
    if (baselineProvider) params.set('baseline_provider', baselineProvider);
    try {
      const cost = await jsonRequest(
        `/api/sessions/${encodeURIComponent(sessionId)}/variants/cost?${params}`);
      if (requestId !== this.#costRequestId || sessionId !== this.session
          || attemptSetId !== this.#attemptSetId() || baseline !== this.#baselineModel
          || baselineProvider !== this.#baselineProvider) return;
      if (!cost || typeof cost !== 'object' || Array.isArray(cost)
          || !Array.isArray(cost.lanes)
          || (cost.baseline_model && cost.baseline_model !== baseline)) {
        throw new Error('The cost endpoint returned an invalid result.');
      }
      this.#cost = cost;
      this.#clearError('cost');
    } catch (error) {
      if (requestId !== this.#costRequestId) return;
      this.#reportError('cost', error, {
        session: sessionId,
        attempt_set_id: attemptSetId,
        baseline,
        baseline_provider: baselineProvider || null,
      });
    } finally {
      if (requestId === this.#costRequestId) {
        this.#costLoading = false;
        this.#renderCost();
      }
    }
  }

  #reviewOutcomes() {
    const attemptSetId = this.#attemptSetId();
    if (!this.session || !attemptSetId || !this.#isTerminal()) return;
    this.dispatchEvent(new CustomEvent('attempt-review', {
      detail: { session: this.session, attempt_set_id: attemptSetId },
      bubbles: true,
      composed: true,
    }));
  }

  #requestExplore() {
    if (!this.session || (this.#sessionRecord && !this.#supportsAttempts())) return;
    this.dispatchEvent(new CustomEvent('attempt-explore', {
      detail: { session: this.session }, bubbles: true, composed: true,
    }));
  }

  #stopPolling() {
    if (this.#pollTimer) clearTimeout(this.#pollTimer);
    this.#pollTimer = null;
  }

  #reportError(action, error, identity = {}) {
    const message = String(error?.message || error || 'Unknown attempts error');
    const {
      session = this.session,
      attempt_set_id: attemptSetId = this.#attemptSetId() || null,
      ...context
    } = identity;
    const stale = session !== this.session
      || (attemptSetId && attemptSetId !== this.#attemptSetId());
    if (!stale) this.#errors.set(action, { action, message, session, attemptSetId });
    const signature = JSON.stringify({ session, attemptSetId, message, context });
    if (this.#lastErrorEvents.get(action) !== signature) {
      this.#lastErrorEvents.set(action, signature);
      this.dispatchEvent(new CustomEvent('attempts-error', {
        detail: {
          action,
          session,
          attempt_set_id: attemptSetId,
          message,
          stale,
          ...context,
        },
        bubbles: true,
        composed: true,
      }));
    }
    this.#renderErrors();
  }

  #clearError(action) {
    this.#errors.delete(action);
    this.#lastErrorEvents.delete(action);
    this.#renderErrors();
  }

  #retry(action) {
    if (action === 'agents' || action === 'sessions') void this.#loadContext();
    if (action === 'results') void this.refresh();
    if (action === 'plan') void this.#planFirst();
    if (action === 'probe') void this.#checkModels();
    if (action === 'cost') void this.#loadCost();
  }

  #renderAll() {
    this.#renderErrors();
    this.#renderDraft();
    this.#renderActive();
    this.#renderCost();
  }

  #renderErrors() {
    const labels = {
      agents: 'Agents', sessions: 'Session', results: 'Attempts',
      plan: 'Plan', probe: 'Model check', cost: 'Cost',
    };
    this.#errorsHost.innerHTML = [...this.#errors.values()].map((error) => `
      <div class="error" role="alert">
        <strong>${html(labels[error.action] || error.action)}</strong>
        <button type="button" class="link" data-action="retry" data-retry="${html(error.action)}">Retry</button>
        <span>${html(error.message)}</span>
      </div>`).join('');
  }

  #renderDraft() {
    const focused = this.#root.activeElement?.matches?.('[data-instruction]');
    const start = focused ? this.#root.activeElement.selectionStart : null;
    const end = focused ? this.#root.activeElement.selectionEnd : null;
    const planner = this.#plannerAgent();
    const agentOptions = this.#agents.map((agent) => {
      const target = [agent.provider, agent.model].filter(Boolean).join(' / ');
      return `<option value="${html(agent.id)}"${agent.id === this.#plannerAgentId ? ' selected' : ''}>`
        + `${html(agent.name)}${target ? ` — ${html(target)}` : ''}</option>`;
    }).join('');
    const ways = this.#draft.lanes.map((lane, index) => {
      const resolved = this.#resolveDraftWay(lane, index);
      const probe = this.#probes.get(index);
      let state = this.#probing && !probe ? 'Checking…' : 'Not checked';
      let stateClass = this.#probing && !probe ? 'live' : '';
      let dot = this.#probing && !probe ? 'checking' : '';
      if (probe) {
        state = probe.status === 'usable' ? 'Usable'
          : (probe.status === 'unusable' ? 'Not usable' : 'Check failed');
        stateClass = probe.status === 'usable' ? 'good' : 'bad';
        dot = probe.status === 'usable' ? 'usable' : 'unusable';
      }
      const probeUsage = this.#controlUsageLabel(probe?.control_usage, 'Model check');
      const target = [resolved.provider || 'provider unresolved', resolved.model || 'model unresolved']
        .join(' · ');
      return `<div class="way"><span class="dot ${dot}" aria-hidden="true"></span>`
        + `<div class="way-copy"><div class="way-title"><span>Way ${index + 1} · `
        + `${html(resolved.agentLabel)}</span></div><div class="way-meta">${html(target)}</div></div>`
        + `<span class="state ${stateClass}">${html(state)}</span>`
        + `${probe?.detail ? `<div class="way-detail ${probe.status === 'usable' ? '' : 'bad'}">`
          + `${html(probe.detail)}${probeUsage ? `<br>${html(probeUsage)}` : ''}</div>` : ''}</div>`;
    }).join('');
    const plan = this.#plan ? this.#planMarkup() : '';
    const noTask = !this.#draft.task.trim();
    const plannerUnavailable = !planner?.provider;
    const contextLoading = !this.#sessionRecord;
    const unsupported = Boolean(this.#sessionRecord) && !this.#supportsAttempts();
    const availability = unsupported
      ? '<div class="availability" role="note">Explore several ways currently requires a single-agent session. '
        + 'This session’s multi-agent transcript remains available, but Plan, model preflight, and new attempts are disabled.</div>'
      : (contextLoading ? '<div class="hint" role="status">Reading session mode…</div>' : '');

    this.#draftHost.innerHTML = `
      ${availability}
      <span class="eyebrow">Draft task</span>
      <p class="task ${noTask ? 'empty' : ''}">${html(this.#draft.task || 'No draft task yet.')}</p>
      <div class="controls">
        <label class="field"><span>Planning agent</span>
          <select data-planner aria-label="Planning agent"${this.#planning || contextLoading || unsupported ? ' disabled' : ''}>
            ${agentOptions || '<option value="">No configured agents</option>'}
          </select>
        </label>
        <div class="actions">
          <button type="button" class="action" data-action="plan"
            ${this.#planning || !this.session || contextLoading || unsupported || noTask || plannerUnavailable ? 'disabled' : ''}>`
          + `${this.#planning ? 'Planning…' : 'Plan first'}</button>`
          + `<button type="button" class="action" data-action="probe"
            ${this.#probing || !this.session || contextLoading || unsupported || !this.#draft.lanes.length ? 'disabled' : ''}>`
          + `${this.#probing ? 'Checking…' : 'Check models'}</button>`
          + `<span class="hint">One reviewed plan can guide every way.</span>
        </div>
      </div>
      ${this.#draft.lanes.length
        ? `<div class="ways" aria-label="Draft ways">${ways}</div>`
        : '<div class="empty">Configure several ways to check their models before running.</div>'}
      ${plan}`;
    const textarea = this.#draftHost.querySelector('[data-instruction]');
    if (textarea) {
      textarea.value = this.#instruction;
      if (focused) {
        queueMicrotask(() => {
          textarea.focus();
          if (start != null && end != null) textarea.setSelectionRange(start, end);
        });
      }
    }
  }

  #planMarkup() {
    const steps = this.#plan.steps.length
      ? `<div class="plan-group"><span class="eyebrow">Steps</span><ol>`
        + this.#plan.steps.map((step) => `<li><code>${html(step.path || 'path unresolved')}</code>`
          + ` — ${html(step.change)}</li>`).join('') + '</ol></div>' : '';
    const constraints = this.#plan.constraints.length
      ? `<div class="plan-group"><span class="eyebrow">Constraints</span><ul>`
        + this.#plan.constraints.map((item) => `<li>${html(item)}</li>`).join('') + '</ul></div>' : '';
    const acceptance = this.#plan.acceptance.length
      ? `<div class="plan-group"><span class="eyebrow">Done when</span><ul>`
        + this.#plan.acceptance.map((item) => `<li>${html(item)}</li>`).join('') + '</ul></div>' : '';
    const usage = this.#controlUsageLabel(this.#planUsage, 'Plan');
    return `<div class="plan"><h4>Proposed plan</h4><p>${html(this.#plan.summary)}</p>`
      + `${usage ? `<div class="hint">${html(usage)} · current page; the Agent’s cumulative total remains in Settings.</div>` : ''}`
      + `${steps}${constraints}${acceptance}<label class="field instruction">`
      + '<span>Instruction every way will receive</span>'
      + '<textarea data-instruction aria-label="Attempt instruction"></textarea></label>'
      + `<div class="actions"><button type="button" class="action primary" data-action="use-plan"`
      + `${this.#instruction.trim() ? '' : ' disabled'}>Use this plan</button>`
      + '<span class="hint">You can edit the instruction before using it.</span></div></div>';
  }

  #controlUsageLabel(usage, label) {
    if (!usage || !Number(usage.calls || 0)) return '';
    const calls = Number(usage.calls || 0);
    const input = Number(usage.input_tokens || 0);
    const output = Number(usage.output_tokens || 0);
    const reasoning = Number(usage.reasoning_tokens || 0);
    const completeness = usage.token_usage_known === true
      ? 'exact' : 'known lower bound · incomplete';
    return `${label} · ${calls} call${calls === 1 ? '' : 's'} · `
      + `${input} in / ${output} out / ${reasoning} reasoning · `
      + `${input + output + reasoning} total tokens · ${completeness}`;
  }

  #announceLifecycle(message) {
    const signature = JSON.stringify([this.session, this.#attemptSetId(), message]);
    if (signature === this.#lifecycleSignature) return;
    this.#lifecycleSignature = signature;
    // Clear first so an identical summary in a newly selected session is a
    // fresh polite announcement, while repeated polls in one set stay quiet.
    this.#lifecycleHost.textContent = '';
    queueMicrotask(() => {
      if (signature === this.#lifecycleSignature) this.#lifecycleHost.textContent = message;
    });
  }

  #renderActive() {
    const restoreReviewFocus = this.#root.activeElement?.matches?.('[data-action="review"]');
    const restoreFocus = () => {
      if (!restoreReviewFocus) return;
      const sessionId = this.session;
      const attemptSetId = this.#attemptSetId();
      queueMicrotask(() => {
        if (sessionId !== this.session || attemptSetId !== this.#attemptSetId()
            || this.#root.activeElement) return;
        const review = this.#activeHost.querySelector('[data-action="review"]');
        if (review && !review.disabled) review.focus();
      });
    };
    if (!this.session) {
      this.#activeHost.innerHTML = '<div class="empty">Open a session to see its attempts.</div>';
      this.#announceLifecycle('No session is open.');
      restoreFocus();
      return;
    }
    if (!this.#results && !this.#resultsStale) {
      this.#activeHost.innerHTML = '<div class="empty loading" role="status">Reading current attempts…</div>';
      this.#announceLifecycle('Reading current attempts.');
      restoreFocus();
      return;
    }
    if (!this.#results && this.#resultsStale) {
      this.#activeHost.innerHTML = '<div class="empty">Current attempts could not be read. Retry above.</div>';
      this.#announceLifecycle('Current attempts could not be read.');
      restoreFocus();
      return;
    }
    const set = this.#attemptSet();
    if (!set || this.#lanes().length === 0) {
      const canExplore = !this.#sessionRecord || this.#supportsAttempts();
      this.#activeHost.innerHTML = '<div class="empty empty-action"><span>No attempts are active for this Session.</span>'
        + (canExplore
          ? '<button type="button" class="action primary" data-action="explore">Explore several ways</button>'
          : '')
        + '</div>';
      this.#announceLifecycle('No active attempts for this session.');
      restoreFocus();
      return;
    }
    const state = this.#setState();
    const roster = this.#lanes().map((lane) => {
      const fact = this.#laneFact(lane.index);
      const laneState = this.#laneState(lane.index);
      const resolved = this.#resolveActiveWay(lane);
      const target = [resolved.provider || 'provider unknown', resolved.model || 'model unknown'].join(' · ');
      const stateClass = ['queued', 'running', 'checking', 'discarding'].includes(laneState) ? 'live'
        : (laneState === 'completed' ? 'good' : 'bad');
      return `<div class="way"><span class="dot ${html(laneState)}" aria-hidden="true"></span>`
        + `<div class="way-copy"><div class="way-title"><span>Way ${lane.index + 1} · `
        + `${html(resolved.agentLabel)}</span></div><div class="way-meta">${html(target)}</div></div>`
        + `<span class="state ${stateClass}">${html(words(laneState))}</span>`
        + `${fact?.error ? `<div class="way-error">${html(fact.error)}</div>` : ''}</div>`;
    }).join('');
    let reviewReason = this.#isTerminal()
      ? 'Open Outcome and Route to compare the finished work.'
      : 'Outcome and Route are available after every way finishes.';
    if (state === 'checking') reviewReason = 'Review partial Checks and retry the interrupted check run.';
    if (state === 'discarding') reviewReason = 'Open review to finish cleaning up these attempts.';
    const counts = new Map();
    for (const lane of this.#lanes()) {
      const laneState = words(this.#laneState(lane.index));
      counts.set(laneState, (counts.get(laneState) || 0) + 1);
    }
    const lifecycle = [...counts]
      .map(([laneState, count]) => `${count} ${laneState}`)
      .join(', ');
    this.#announceLifecycle(`Current attempts ${words(state)}. ${lifecycle}.`);
    this.#activeHost.innerHTML = `
      ${this.#resultsStale ? `<div class="stale">Could not refresh; showing the last known state. `
        + `${html(this.#resultsStale)}</div>` : ''}
      <span class="eyebrow">Task</span><p class="task">${html(set.task || 'Task unavailable')}</p>
      <span class="set-state" data-state="${html(state)}">${html(words(state))}</span>
      <div class="ways" aria-label="Current attempt roster">${roster}</div>
      <div class="review"><button type="button" class="action primary" data-action="review"
        ${this.#isTerminal() ? '' : 'disabled'}>Review outcomes</button>
        <span class="hint">${html(reviewReason)}</span></div>`;
    restoreFocus();
  }

  #costHasEveryWay(cost) {
    const expected = new Set(this.#lanes().map((lane) => lane.index));
    const lanes = Array.isArray(cost?.lanes) ? cost.lanes : [];
    const recorded = new Set(lanes.map((lane) => lane.index));
    return expected.size > 0 && [...expected].every((index) => recorded.has(index));
  }

  #actualCost(cost, live) {
    const lanes = Array.isArray(cost?.lanes) ? cost.lanes : [];
    if (!lanes.length) return { value: 'Waiting', detail: 'Usage appears as each way finishes.', known: false };
    const explicit = typeof cost.actual_cost_known === 'boolean';
    const pricesKnown = explicit
      ? cost.actual_cost_known : lanes.every((lane) => lane.cost_known === true);
    const complete = live || this.#costHasEveryWay(cost);
    const known = pricesKnown && complete;
    if (known) {
      return {
        value: usd(cost.total_usd),
        detail: cost.all_local
          ? `Known local/free usage${live ? ' from finished ways' : ''}.`
          : `Every ${live ? 'finished ' : ''}way has a configured price.`,
        known: true,
      };
    }
    const priced = lanes.filter((lane) => lane.cost_known === true);
    const subtotal = priced.reduce((sum, lane) => sum + (Number(lane.cost_usd) || 0), 0);
    const usageUnknown = lanes.filter((lane) => lane.token_usage_known === false).length;
    const priceUnknown = lanes.filter((lane) =>
      lane.token_usage_known !== false && lane.cost_known !== true).length;
    const missing = Math.max(0, this.#lanes().length - lanes.length);
    const gaps = [
      usageUnknown ? `token usage for ${usageUnknown} way${usageUnknown === 1 ? ' is' : 's are'} unavailable` : '',
      priceUnknown ? `${priceUnknown} recorded way price${priceUnknown === 1 ? ' is' : 's are'} unknown` : '',
      !live && missing ? `usage for ${missing} way${missing === 1 ? ' is' : 's are'} unavailable` : '',
    ].filter(Boolean).join('; ');
    return {
      value: priced.length ? `${usd(subtotal)} known` : 'Unknown',
      detail: `${gaps || 'The full cost is unresolved'}; this is not a $0 run.`,
      known: false,
    };
  }

  #baselineCost(cost, live) {
    const lanes = Array.isArray(cost?.lanes) ? cost.lanes : [];
    if (!lanes.length) return { value: 'Waiting', detail: 'No token usage to compare yet.', known: false };
    const unknownUsage = lanes.filter((lane) => lane.token_usage_known === false).length;
    const missingWays = Math.max(0, this.#lanes().length - lanes.length);
    if (unknownUsage) {
      return {
        value: 'Unknown',
        detail: `Token usage was not recorded for ${unknownUsage} way${unknownUsage === 1 ? '' : 's'}, so no one-model baseline can be calculated.`,
        known: false,
      };
    }
    if (missingWays) {
      return {
        value: live ? 'Waiting' : 'Unknown',
        detail: `Token usage for ${missingWays} way${missingWays === 1 ? ' is' : 's are'} not available yet.`,
        known: false,
      };
    }
    const explicit = typeof cost.baseline_cost_known === 'boolean';
    const priceKnown = explicit ? cost.baseline_cost_known : Number(cost.baseline_usd) > 0;
    if (!priceKnown) {
      return {
        value: 'Unknown',
        detail: `${cost.baseline_model || this.#baselineModel} has no known price; $0 would be misleading.`,
        known: false,
      };
    }
    if (!live && !this.#costHasEveryWay(cost)) {
      return {
        value: `${usd(cost.baseline_usd)} known`,
        detail: 'Usage is missing for at least one way, so this is only a subtotal.',
        known: false,
      };
    }
    return {
      value: usd(cost.baseline_usd),
      detail: Number(cost.baseline_usd) === 0
        ? 'Known local/free baseline.'
        : `Same ${live ? 'recorded ' : ''}token volume on one model.`,
      known: true,
    };
  }

  #updateCostSelector(choices) {
    if (!choices) {
      if (this.#costSelectorSignature) this.#costSelectorHost.replaceChildren();
      this.#costSelectorSignature = '';
      return;
    }
    const signature = JSON.stringify(choices.map((choice) => [choice.key, choice.label]));
    if (signature !== this.#costSelectorSignature) {
      const options = choices.map((choice) => `<option value="${html(choice.key)}"`
        + `${choice.model === this.#baselineModel && choice.provider === this.#baselineProvider
          ? ' selected' : ''}>${html(choice.label)}</option>`).join('');
      this.#costSelectorHost.innerHTML = options
        ? `<label class="field"><span>One-model baseline</span><select data-baseline `
          + `aria-label="Counterfactual baseline model">${options}</select></label>`
        : '<div class="empty">No configured agent model is available as a baseline.</div>';
      this.#costSelectorSignature = signature;
    }
    const select = this.#costSelectorHost.querySelector('[data-baseline]');
    const selectedKey = JSON.stringify([this.#baselineProvider, this.#baselineModel]);
    if (select && select.value !== selectedKey
        && choices.some((choice) => choice.key === selectedKey)) {
      select.value = selectedKey;
    }
  }

  #renderCost() {
    const choices = this.#baselineChoices();
    if (!this.#attemptSetId()) {
      this.#updateCostSelector(null);
      this.#costBodyHost.innerHTML = '<div class="empty">Cost appears when attempts start.</div>';
      return;
    }
    if (this.#setState() === 'discarding') {
      this.#updateCostSelector(null);
      this.#costBodyHost.innerHTML = '<div class="empty">Cost is unavailable while attempt cleanup finishes.</div>';
      return;
    }
    this.#updateCostSelector(choices);
    if (this.#costLoading && !this.#cost) {
      this.#costBodyHost.innerHTML = '<div class="empty loading" role="status">Reading cost…</div>';
      return;
    }
    if (!this.#cost) {
      this.#costBodyHost.innerHTML = '<div class="empty">Cost is unavailable. '
        + '<button type="button" class="link" data-action="cost">Retry</button></div>';
      return;
    }
    const live = this.#isRunning();
    const actual = this.#actualCost(this.#cost, live);
    const baseline = this.#baselineCost(this.#cost, live);
    const difference = actual.known && baseline.known
      ? {
        value: usd(this.#cost.saved_usd),
        detail: Number(this.#cost.saved_usd) > 0
          ? 'Less than the selected one-model baseline.' : 'No priced saving against this baseline.',
      }
      : { value: 'Unknown', detail: 'Both actual and baseline prices must be known.' };
    const laneRows = (this.#cost.lanes || []).map((lane) => {
      const attempt = this.#lanes().find((item) => item.index === lane.index);
      const resolved = attempt ? this.#resolveActiveWay(attempt) : null;
      const label = resolved
        ? [resolved.provider, resolved.model].filter(Boolean).join(' · ')
        : lane.model || `Way ${lane.index + 1}`;
      const value = lane.cost_known === true
        ? `${usd(lane.cost_usd)}${Number(lane.cost_usd) === 0 ? ' · local/free' : ''}`
        : (lane.token_usage_known === false ? 'usage unknown' : 'unknown price');
      return `<div class="cost-way"><span>Way ${lane.index + 1} · ${html(label)}</span>`
        + `<code>${html(value)}</code></div>`;
    }).join('');
    const updating = this.#costLoading
      ? '<div class="empty loading" role="status">Updating cost…</div>' : '';
    const costError = this.#errors.get('cost');
    const stale = costError
      ? `<div class="stale">Could not update cost; showing the last known calculation. `
        + `${html(costError.message)}</div>` : '';
    this.#costBodyHost.innerHTML = `${stale}${updating}<div class="cost-grid">
      <div class="cost-card"><span class="eyebrow">${live ? 'Actual so far' : 'Actual'}</span>
        <div class="cost-value">${html(actual.value)}</div><div class="cost-detail">${html(actual.detail)}</div></div>
      <div class="cost-card"><span class="eyebrow">${live ? 'One model so far' : 'One model'}</span>
        <div class="cost-value">${html(baseline.value)}</div><div class="cost-detail">${html(baseline.detail)}</div></div>
      <div class="cost-card wide"><span class="eyebrow">${live ? 'Difference so far' : 'Difference'}</span>
        <div class="cost-value">${html(difference.value)}</div><div class="cost-detail">${html(difference.detail)}</div></div>
      </div>${laneRows ? `<div class="cost-ways">${laneRows}</div>` : ''}`;
  }
}

customElements.define('ax-attempts', AxAttempts);
