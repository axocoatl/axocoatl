import { adopt } from './sheets.js';

/**
 * `<ax-compare>` — several attempts at one task, side by side.
 *
 * Outcome is the scoreboard and decision surface. Route shows how attempts
 * arrived at their outcomes. Inspect files stays inside Outcome so reviewing a
 * candidate never turns into a second application shell.
 *
 * @element ax-compare
 *
 * @attr {string} session  Session whose attempts to show.
 * @attr {string} view     'outcome' | 'route'
 *
 * @fires attempt-keep     detail: {session, attempt_set_id, index, attempt, result}
 * @fires attempt-discard  detail: {session, attempt_set_id, result}
 * @fires attempt-set-changed detail: {session, previous_attempt_set_id, attempt_set_id}
 * @fires compare-error    detail: {action, session, attempt_set_id, message, ...context}
 */

const money = (n) => '$' + (n < 0.01 && n > 0 ? n.toFixed(4) : n.toFixed(2));
const secs = (ms) => (ms < 1000 ? `${ms}ms`
  : ms < 60000 ? `${(ms / 1000).toFixed(1)}s`
  : `${Math.floor(ms / 60000)}m ${Math.round((ms % 60000) / 1000)}s`);

const html = (value) => String(value ?? '')
  .replaceAll('&', '&amp;')
  .replaceAll('<', '&lt;')
  .replaceAll('>', '&gt;')
  .replaceAll('"', '&quot;')
  .replaceAll("'", '&#39;');

const sentence = (value) => {
  const state = String(value || '').replaceAll('_', ' ');
  return state === 'discarding' ? 'cleaning up' : state;
};
const KEEP_RECOVERY_STATES = new Set(['applying', 'applied', 'transcript_recorded']);

/** What an attempt is called: the agent, else the model, else its position. */
const attemptLabel = (attempt) => attempt.agent || attempt.model || `attempt ${attempt.index + 1}`;

async function jsonRequest(url, options) {
  const response = await fetch(url, options);
  const text = await response.text();
  let body = null;
  if (text) {
    try { body = JSON.parse(text); } catch { body = text; }
  }
  if (!response.ok) {
    const message = body && typeof body === 'object' ? body.error : body;
    throw new Error(message || `HTTP ${response.status}`);
  }
  return body;
}

const CSS = `
:host {
  display: flex; flex-direction: column; min-height: 0;
  font-family: var(--font-sans); color: var(--text);
}
.bar {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-3); border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.seg {
  display: flex; border: 1px solid var(--border); border-radius: var(--r-md);
  overflow: hidden; flex-shrink: 0;
}
.seg button {
  background: none; border: 0; color: var(--muted); cursor: pointer;
  padding: 4px var(--sp-3); font: var(--fw-medium) var(--fs-xs) var(--font-sans);
  white-space: nowrap;
  transition: background var(--dur-fast) var(--ease), color var(--dur-fast) var(--ease);
}
.seg button + button { border-left: 1px solid var(--border); }
.seg button:hover { color: var(--text); }
.seg button[aria-pressed="true"] { background: var(--bg-3); color: var(--text); }
.seg button:focus-visible, button:focus-visible, select:focus-visible, input:focus-visible {
  outline: none; box-shadow: var(--focus-ring);
}
.gist { color: var(--muted-2); font-size: var(--fs-xs); margin-left: auto; }
button.run, button.action {
  background: none; border: 1px solid var(--border-strong); color: var(--text);
  border-radius: var(--r-md); padding: 3px var(--sp-3); cursor: pointer;
  font: var(--fw-medium) var(--fs-xs) var(--font-sans); flex-shrink: 0;
}
button.run:hover:not(:disabled), button.action:hover:not(:disabled) {
  border-color: var(--accent); color: var(--accent);
}
button:disabled { opacity: .45; cursor: not-allowed; }
.run.ghost { margin-left: auto; }
button.primary {
  background: var(--accent); border-color: var(--accent); color: var(--bg);
  padding: 5px var(--sp-3);
}
button.primary:hover:not(:disabled) { color: var(--bg); filter: brightness(1.08); }
button.danger { color: var(--err); }
button.danger:hover:not(:disabled) { border-color: var(--err); color: var(--err); }

/* Checks and judgment are the two arbiters. Both stay visible so a result can
   never look verified or ranked without saying what performed that work. */
.check, .decision {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-3); border-bottom: 1px solid var(--border);
  font-size: var(--fs-xs); color: var(--muted-2); flex-shrink: 0;
}
.check input {
  flex: 1; min-width: 10rem; background: var(--bg-3); color: var(--text);
  border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 3px var(--sp-2); font: var(--fs-xs) var(--font-mono);
}
.check.unset { color: var(--warn); }
.decision { flex-wrap: wrap; }
.decision label { display: flex; align-items: center; gap: var(--sp-2); }
.decision select {
  max-width: 22rem; background: var(--bg-3); color: var(--text);
  border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 3px var(--sp-2); font: var(--fs-xs) var(--font-sans);
}
.decision .spacer { flex: 1; }
.action-error { width: 100%; color: var(--err); }
.state-pill {
  display: inline-flex; align-items: center; width: fit-content;
  border: 1px solid var(--border-strong); border-radius: 999px;
  padding: 1px 7px; color: var(--muted); font: var(--fw-medium) var(--fs-xs) var(--font-sans);
  text-transform: capitalize;
}
.state-pill[data-state="running"], .state-pill[data-state="preparing"],
.state-pill[data-state="queued"], .state-pill[data-state="checking"],
.state-pill[data-state="discarding"], .state-pill[data-state="applying"],
.state-pill[data-state="applied"], .state-pill[data-state="transcript_recorded"] {
  color: var(--warn); border-color: var(--warn);
}
.state-pill[data-state="completed"], .state-pill[data-state="ready"],
.state-pill[data-state="verified"], .state-pill[data-state="judged"] {
  color: var(--ok); border-color: var(--ok);
}
.state-pill[data-state="failed"], .state-pill[data-state="cancelled"],
.state-pill[data-state="interrupted"] { color: var(--err); border-color: var(--err); }

.branches {
  border-bottom: 1px solid var(--border); padding: var(--sp-2) var(--sp-3);
  display: flex; flex-direction: column; gap: 2px;
}
.branches[hidden] { display: none; }
.brow { display: flex; align-items: baseline; gap: var(--sp-2); font-size: var(--fs-xs); }
.brow .bn { color: var(--muted-2); font-family: var(--font-mono); flex-shrink: 0; }
.brow code { font-family: var(--font-mono); color: var(--accent-2); }
.brow code.wt {
  color: var(--muted-2); overflow: hidden; text-overflow: ellipsis;
  white-space: nowrap; direction: rtl; text-align: left;
}

.body { flex: 1; overflow: auto; padding: var(--sp-3); min-height: 0; }
.empty { color: var(--muted-2); font-size: var(--fs-sm); padding: var(--sp-5); text-align: center; }
.empty.loading { opacity: .6; }
.empty.failed, .failed { color: var(--err); }
.retry {
  display: block; margin: var(--sp-2) auto 0; background: none;
  border: 1px solid var(--border); border-radius: var(--r-sm);
  color: var(--text); font: var(--fs-xs) var(--font-sans);
  padding: 2px var(--sp-2); cursor: pointer;
}
.retry:hover { border-color: var(--accent); }
.task, .judgment {
  border: 1px solid var(--border); border-radius: var(--r-md);
  background: var(--bg-2); padding: var(--sp-3); margin-bottom: var(--sp-3);
}
.task { display: grid; grid-template-columns: auto 1fr; gap: var(--sp-2); }
.eyebrow {
  color: var(--muted-2); font: var(--fw-bold) 9.5px var(--font-mono);
  letter-spacing: .07em; text-transform: uppercase;
}
.task p, .judgment p, .rationale p { margin: 0; white-space: pre-wrap; word-break: break-word; }
.judgment h3, .lane-card h3, .inspect h2 {
  margin: 0; font-size: var(--fs-sm); font-weight: var(--fw-medium);
}
.judgment h3 { margin-bottom: var(--sp-2); }

.table-wrap { overflow-x: auto; }
table { width: 100%; border-collapse: collapse; font-size: var(--fs-sm); }
th, td {
  text-align: left; padding: 5px var(--sp-3); border-bottom: 1px solid var(--border);
  vertical-align: top;
}
th { color: var(--muted); font-weight: var(--fw-medium); white-space: nowrap; }
td { font: var(--fs-xs) var(--font-mono); }
.rebase { background: none; border: 0; padding: 0; color: var(--text); font: inherit; cursor: pointer; }
.rebase:hover { color: var(--accent); text-decoration: underline; }
.base {
  color: var(--muted-2); font-size: var(--fs-xs); text-transform: uppercase;
  letter-spacing: .06em;
}
tr.same { opacity: .38; }
td.win { color: var(--accent); }
td.good { color: var(--ok); }
td.bad { color: var(--err); }
.opt {
  display: flex; gap: var(--sp-2); align-items: center; margin-top: var(--sp-3);
  font-size: var(--fs-xs); color: var(--muted);
}

.lane-grid {
  display: grid; grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
  gap: var(--sp-3); margin-top: var(--sp-4);
}
.lane-card {
  min-width: 0; border: 1px solid var(--border); border-radius: var(--r-md);
  background: var(--bg-2); padding: var(--sp-3); display: flex;
  flex-direction: column; gap: var(--sp-3);
}
.lane-head, .card-actions, .inspect-head, .path-row {
  display: flex; align-items: center; gap: var(--sp-2);
}
.lane-head h3, .path-row .path { flex: 1; min-width: 0; }
.lane-head .winner { color: var(--accent); font-size: var(--fs-xs); }
.lane-error { color: var(--err); font-size: var(--fs-xs); white-space: pre-wrap; }
.section-title {
  display: block; color: var(--muted); font: var(--fw-medium) var(--fs-xs) var(--font-sans);
  margin-bottom: 4px;
}
.verdict-meta, .muted, .why { color: var(--muted-2); font-size: var(--fs-xs); }
.verdict-output {
  max-height: 14rem; overflow: auto; margin: var(--sp-2) 0 0; padding: var(--sp-2);
  background: var(--bg-3); border-radius: var(--r-sm); color: var(--text);
  font: var(--fs-xs) var(--font-mono); white-space: pre-wrap; word-break: break-word;
}
.attempt-output {
  max-height: 18rem; overflow: auto; margin: var(--sp-2) 0 0; padding: var(--sp-2);
  background: var(--bg-3); border-radius: var(--r-sm); color: var(--text);
  font-size: var(--fs-xs); line-height: 1.5; white-space: pre-wrap; overflow-wrap: anywhere;
}
.paths { list-style: none; margin: 0; padding: 0; display: grid; gap: 3px; }
.path-row { min-width: 0; font: var(--fs-xs) var(--font-mono); }
.path-row .path {
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--text);
}
.path-row button.path {
  border: 0; background: none; padding: 0; text-align: left; cursor: pointer;
  font: inherit;
}
.path-row button.path:hover { color: var(--accent); text-decoration: underline; }
.file-state { color: var(--muted-2); text-transform: capitalize; }
.lines { color: var(--muted-2); white-space: nowrap; }
.test-warning { color: var(--warn); font-size: var(--fs-xs); }
.rationale { display: grid; gap: var(--sp-2); font-size: var(--fs-xs); }
.card-actions { margin-top: auto; flex-wrap: wrap; }
.card-actions .why { flex-basis: 100%; }

/* Route */
tr.diverged { background: rgba(var(--axo-bronze-rgb), .07); }
tr.diverged td.off { box-shadow: inset 2px 0 0 var(--warn); }
tr.diverged td.same-as-base { opacity: .45; }
.n { color: var(--muted-2); width: 1%; white-space: nowrap; }
.k {
  font: var(--fw-bold) 9.5px var(--font-mono); letter-spacing: .06em;
  text-transform: uppercase; border: 1px solid currentColor;
  border-radius: 3px; padding: 0 4px; margin-right: 5px;
}
.k.read, .k.list, .k.search { color: var(--muted-2); }
.k.edit, .k.write { color: var(--accent); }
.k.run { color: var(--warn); }
.k.other { color: var(--muted); }
.det {
  color: var(--muted-2); display: block; margin-top: 2px;
  white-space: pre-wrap; word-break: break-word;
}
.none { color: var(--muted-2); }
.fold td { padding: 3px var(--sp-3); }
.fold button {
  background: none; border: 0; color: var(--muted-2);
  font-size: var(--fs-xs); cursor: pointer; padding: 0;
}
.fold button:hover { color: var(--accent); }

/* File inspection */
.inspect { display: flex; flex-direction: column; gap: var(--sp-3); min-height: 100%; }
.inspect-head { flex-wrap: wrap; }
.inspect-head h2 { flex: 1; }
.back {
  background: none; border: 0; color: var(--muted); cursor: pointer;
  padding: 2px 0; font: var(--fs-xs) var(--font-sans);
}
.back:hover { color: var(--accent); }
.inspect-grid {
  display: grid; grid-template-columns: minmax(200px, 28%) minmax(0, 1fr);
  border: 1px solid var(--border); border-radius: var(--r-md); min-height: 22rem;
  overflow: hidden;
}
.file-list { border-right: 1px solid var(--border); overflow: auto; background: var(--bg-2); }
.file-list button {
  display: grid; width: 100%; gap: 2px; border: 0; border-bottom: 1px solid var(--border);
  background: none; color: var(--text); padding: var(--sp-2); text-align: left;
  cursor: pointer; font: var(--fs-xs) var(--font-mono);
}
.file-list button:hover, .file-list button[aria-current="true"] { background: var(--bg-3); }
.file-list .meta { color: var(--muted-2); text-transform: capitalize; }
.diff { min-width: 0; overflow: auto; }
.diff-empty { color: var(--muted-2); padding: var(--sp-5); text-align: center; font-size: var(--fs-sm); }
.diff-grid { display: grid; grid-template-columns: 1fr 1fr; min-width: 720px; min-height: 100%; }
.diff-side { min-width: 0; }
.diff-side + .diff-side { border-left: 1px solid var(--border); }
.diff-side h3 {
  position: sticky; top: 0; z-index: 1; margin: 0; padding: var(--sp-2);
  background: var(--bg-2); border-bottom: 1px solid var(--border);
  color: var(--muted); font-size: var(--fs-xs); font-weight: var(--fw-medium);
}
.diff-side pre {
  margin: 0; padding: var(--sp-3); color: var(--text); min-height: 100%;
  font: var(--fs-xs) var(--font-mono); white-space: pre; tab-size: 2;
}
.sr-only {
  position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
  overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
}

@media (max-width: 760px) {
  .bar { flex-wrap: wrap; }
  .gist { order: 3; width: 100%; margin-left: 0; }
  .run.ghost { margin-left: 0; }
  .check { flex-wrap: wrap; }
  .check input { flex-basis: 100%; }
  .inspect-grid { grid-template-columns: 1fr; }
  .file-list { border-right: 0; border-bottom: 1px solid var(--border); max-height: 13rem; }
}
`;

export class AxCompare extends HTMLElement {
  static get observedAttributes() { return ['session', 'view']; }

  #root; #bar; #body; #gist; #check; #decision; #branches;
  #checkCmd = null;
  #savedCheckCmd = null;
  #checkSaveId = 0;
  #checkSavePending = 0;
  #checkSaveChain = Promise.resolve();
  #sessionEpoch = 0;
  #sessionAgentId = '';
  #busy = null;
  #results = null;
  #statuses = [];
  #statusError = null;
  #agents = [];
  #agentsError = null;
  #judgeAgentId = '';
  #error = null;
  #actionError = null;
  #loading = false;
  #alignment = null;
  #alignmentError = null;
  #base = 0;
  #diffOnly = false;
  #openFolds = new Set();
  #inspect = null;
  #refreshId = 0;
  #pollTimer = null;
  #actionId = 0;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="bar">
        <div class="seg" aria-label="Comparison view">
          <button type="button" data-view="outcome">Outcome</button>
          <button type="button" data-view="route">Route</button>
        </div>
        <button type="button" class="run" data-act="verify"
          title="Run the project's own checks in every completed attempt">Run checks</button>
        <button type="button" class="run" data-act="judge"
          title="Rank the attempts that passed checks and say why">Judge</button>
        <span class="gist"></span>
        <button type="button" class="run ghost" data-act="branches"
          title="Show the git branch and worktree behind each attempt">Show the branches</button>
      </div>
      <div class="check"></div>
      <div class="decision"></div>
      <div class="branches" hidden></div>
      <div class="body"></div>`;
    this.#bar = this.#root.querySelector('.bar');
    this.#body = this.#root.querySelector('.body');
    this.#gist = this.#root.querySelector('.gist');
    this.#check = this.#root.querySelector('.check');
    this.#decision = this.#root.querySelector('.decision');
    this.#branches = this.#root.querySelector('.branches');

    this.#bar.addEventListener('click', (event) => {
      const view = event.target.closest('[data-view]');
      if (view) {
        this.#inspect = null;
        this.view = view.dataset.view;
        return;
      }
      const action = event.target.closest('[data-act]');
      if (!action) return;
      if (action.dataset.act === 'branches') { this.#toggleBranches(); return; }
      void this.#run(action.dataset.act);
    });
    this.#decision.addEventListener('change', (event) => {
      const select = event.target.closest('[data-judge-agent]');
      if (!select) return;
      this.#judgeAgentId = select.value;
      this.#actionError = null;
      this.#renderDecision();
      this.#syncButtons();
    });
    this.#decision.addEventListener('click', (event) => {
      if (event.target.closest('[data-discard]')) void this.#discard();
    });
    this.#body.addEventListener('change', (event) => {
      if (!event.target.matches('[data-diff-only]')) return;
      this.#diffOnly = event.target.checked;
      this.render();
    });
    this.#body.addEventListener('click', (event) => {
      const retry = event.target.closest('[data-retry]');
      if (retry) { void this.refresh(); return; }
      const back = event.target.closest('[data-inspect-back]');
      if (back) { this.#inspect = null; this.render(); return; }
      const keep = event.target.closest('[data-keep]');
      if (keep) { void this.#keep(Number(keep.dataset.keep)); return; }
      const inspect = event.target.closest('[data-inspect]');
      if (inspect) {
        const index = Number(inspect.dataset.inspect);
        const file = inspect.dataset.file === undefined ? null : Number(inspect.dataset.file);
        this.#openInspect(index, file);
        return;
      }
      const rebase = event.target.closest('[data-rebase]');
      if (rebase) {
        this.#base = Number(rebase.dataset.rebase);
        this.#openFolds.clear();
        void this.refresh();
        return;
      }
      const fold = event.target.closest('[data-fold]');
      if (fold) {
        const key = Number(fold.dataset.fold);
        this.#openFolds.has(key) ? this.#openFolds.delete(key) : this.#openFolds.add(key);
        this.render();
      }
    });
    adopt(this.#root, CSS);
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(value) { value ? this.setAttribute('session', value) : this.removeAttribute('session'); }
  get attemptSetId() { return this.#attemptSetId(); }

  get view() { return this.getAttribute('view') || 'outcome'; }
  set view(value) { this.setAttribute('view', value); }

  connectedCallback() { if (this.session) void this.refresh(); }

  disconnectedCallback() {
    if (this.#pollTimer) clearTimeout(this.#pollTimer);
    this.#pollTimer = null;
  }

  attributeChangedCallback(name, previous, next) {
    if (previous === next) return;
    if (name === 'session') {
      if (this.#pollTimer) clearTimeout(this.#pollTimer);
      this.#pollTimer = null;
      this.#refreshId++;
      this.#actionId++;
      this.#busy = null;
      this.#base = 0;
      this.#openFolds.clear();
      this.#inspect = null;
      this.#results = null;
      this.#statuses = [];
      this.#alignment = null;
      this.#alignmentError = null;
      this.#error = null;
      this.#actionError = null;
      this.#branches.hidden = true;
      this.#branches.textContent = '';
      this.#bar.querySelector('[data-act="branches"]').textContent = 'Show the branches';
      this.#checkCmd = null;
      this.#savedCheckCmd = null;
      this.#checkSaveId++;
      this.#checkSavePending = 0;
      this.#sessionEpoch++;
      this.#sessionAgentId = '';
      this.#judgeAgentId = '';
      if (this.isConnected && this.session) void this.refresh();
      else this.render();
    }
    if (name === 'view') {
      this.#inspect = null;
      this.render();
    }
  }

  #attemptSet() { return this.#results?.attempt_set || null; }

  #attemptSetId() { return this.#attemptSet()?.id || ''; }

  #isKeepRecovery() { return KEEP_RECOVERY_STATES.has(this.#setState()); }

  #keptIndex() {
    const index = this.#attemptSet()?.kept_index;
    return Number.isInteger(index) ? index : null;
  }

  #actionCurrent(actionId, sessionId, attemptSetId) {
    return actionId === this.#actionId && sessionId === this.session
      && attemptSetId === this.#attemptSetId();
  }

  #lanes() {
    const current = this.#attemptSet()?.lanes;
    return Array.isArray(current) ? current : (this.#results?.lanes || []);
  }

  #setState() {
    if (this.#attemptSet()?.state) return this.#attemptSet().state;
    if (!this.#lanes().length) return '';
    if ((this.#results?.judgment?.candidates || []).length) return 'judged';
    if ((this.#results?.verdicts || []).length) return 'verified';
    return 'ready';
  }

  #laneFact(index) {
    return (this.#results?.lane_states || []).find((lane) => lane.index === index) || null;
  }

  #laneState(index) {
    const fact = this.#laneFact(index);
    if (fact?.state) return fact.state;
    const setState = this.#setState();
    if (['ready', 'checking', 'verified', 'judged'].includes(setState)) return 'completed';
    if (setState === 'failed') return 'failed';
    if (setState === 'preparing') return 'queued';
    if (setState === 'discarding') return 'discarding';
    return setState === 'running' ? 'running' : 'queued';
  }

  #allTerminal() {
    const terminal = new Set(['completed', 'failed', 'cancelled', 'interrupted']);
    const lanes = this.#lanes();
    return lanes.length >= 2 && lanes.every((lane) => terminal.has(this.#laneState(lane.index)));
  }

  #isVerified() {
    if (['verified', 'judged'].includes(this.#setState())) return true;
    // Old persisted results do not have attempt_set.state. Keep them readable,
    // but new state-bearing sets remain authoritative.
    return !this.#attemptSet() && (this.#results?.verdicts || []).length > 0;
  }

  #verdict(index) {
    return (this.#results?.verdicts || []).find((verdict) => verdict.index === index) || null;
  }

  #usage(index) {
    return (this.#results?.usage || []).find((usage) => usage.index === index) || null;
  }

  #output(index) {
    const output = (this.#results?.outputs || []).find((item) => item.index === index);
    return typeof output?.content === 'string' ? output.content : '';
  }

  #rationale(index) {
    return (this.#results?.judgment?.candidates || [])
      .find((candidate) => candidate.index === index) || null;
  }

  #variantStatus(index) {
    return this.#statuses.find((status) => status.index === index) || null;
  }

  #files(index) {
    const files = this.#variantStatus(index)?.status?.files;
    return Array.isArray(files) ? files : null;
  }

  #judgeAgent() {
    return this.#agents.find((agent) => agent.id === this.#judgeAgentId) || null;
  }

  #selectJudgeAgent() {
    if (this.#judgeAgent()) return;
    const preferred = this.#agents.find((agent) => agent.id === this.#sessionAgentId)
      || this.#agents.find((agent) => agent.role === 'coordinator')
      || this.#agents[0];
    this.#judgeAgentId = preferred?.id || '';
  }

  #checkValue() {
    const input = this.#check.querySelector('input');
    return input ? input.value.trim() : (this.#checkCmd || '');
  }

  #canVerify() {
    return Boolean(!this.#isKeepRecovery() && this.#attemptSetId()
      && this.#checkValue() && this.#allTerminal()
      && ['ready', 'checking', 'verified', 'judged'].includes(this.#setState()));
  }

  #canJudge() {
    const agent = this.#judgeAgent();
    const survivors = (this.#results?.verdicts || [])
      .filter((verdict) => verdict.passed && verdict.changed_files > 0).length;
    return Boolean(!this.#isKeepRecovery() && this.#attemptSetId()
      && this.#allTerminal() && this.#isVerified()
      && survivors >= 2 && agent?.provider);
  }

  #keepReason(index) {
    if (!this.#attemptSetId()) return 'This comparison predates attempt sets; start a new exploration to keep one safely.';
    if (this.#setState() === 'discarding') return 'Attempt cleanup has started; finish it before doing anything else.';
    if (this.#setState() === 'checking') return 'Checks are incomplete. Retry Checks before keeping an attempt.';
    if (this.#isKeepRecovery()) {
      const kept = this.#keptIndex();
      if (kept == null) return 'Keep recovery is missing its selected attempt.';
      if (index !== kept) return `Keep is already in progress for attempt ${kept + 1}. Finish that attempt.`;
      return '';
    }
    if (!this.#allTerminal()) return 'Wait for every attempt to finish.';
    if (this.#laneState(index) !== 'completed') return `This attempt ${sentence(this.#laneState(index))}.`;
    if (!this.#isVerified()) return 'Run checks before keeping an attempt.';
    const verdict = this.#verdict(index);
    if (!verdict) return 'This attempt has no check result.';
    if (!verdict.passed) return 'This attempt did not pass checks.';
    if (verdict.changed_files === 0) return 'This attempt changed nothing.';
    return '';
  }

  #emitError(action, error, context = {}) {
    const message = String(error?.message || error || 'Unknown comparison error');
    const {
      session = this.session,
      attempt_set_id: attemptSetId = this.#attemptSetId() || null,
      action_id: actionId = null,
      ...details
    } = context;
    const stale = session !== this.session
      || (attemptSetId && attemptSetId !== this.#attemptSetId())
      || (actionId != null && actionId !== this.#actionId);
    if (!stale) this.#actionError = message;
    this.dispatchEvent(new CustomEvent('compare-error', {
      detail: {
        action,
        session,
        attempt_set_id: attemptSetId,
        message,
        stale,
        ...details,
      },
      bubbles: true,
      composed: true,
    }));
  }

  async #run(action) {
    if (this.#busy || !this.session) return;
    if (action === 'verify' && !this.#canVerify()) return;
    if (action === 'judge' && !this.#canJudge()) return;

    const attemptSetId = this.#attemptSetId();
    const sessionId = this.session;
    const session = encodeURIComponent(sessionId);
    const actionId = ++this.#actionId;
    const judgeAgent = action === 'judge' ? this.#judgeAgent() : null;
    const judgeAgentId = this.#judgeAgentId;
    this.#busy = action;
    this.#actionError = null;
    this.#renderCheck();
    this.#renderDecision();
    this.#syncButtons();
    if (action === 'verify' && this.isConnected) {
      if (this.#pollTimer) clearTimeout(this.#pollTimer);
      this.#pollTimer = setTimeout(() => {
        this.#pollTimer = null;
        if (this.#actionCurrent(actionId, sessionId, attemptSetId)) void this.refresh();
      }, 500);
    }
    try {
      if (action === 'verify') {
        const check = this.#checkValue();
        await jsonRequest(`/api/sessions/${session}/variants/verify`, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ attempt_set_id: attemptSetId, check }),
        });
      } else {
        const body = { attempt_set_id: attemptSetId, provider: judgeAgent.provider };
        if (judgeAgent.model) body.model = judgeAgent.model;
        await jsonRequest(`/api/sessions/${session}/variants/judge`, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(body),
        });
      }
      if (this.#actionCurrent(actionId, sessionId, attemptSetId)) await this.refresh();
    } catch (error) {
      this.#emitError(action, error, action === 'judge'
        ? {
          session: sessionId,
          attempt_set_id: attemptSetId,
          action_id: actionId,
          judge_agent_id: judgeAgentId,
        }
        : { session: sessionId, attempt_set_id: attemptSetId, action_id: actionId });
      if (this.#actionCurrent(actionId, sessionId, attemptSetId)) await this.refresh();
    } finally {
      if (!this.#actionCurrent(actionId, sessionId, attemptSetId)) return;
      this.#busy = null;
      this.#renderCheck();
      this.#renderDecision();
      this.#syncButtons();
    }
  }

  async #keep(index) {
    if (this.#busy || this.#keepReason(index)) return;
    const lane = this.#lanes().find((attempt) => attempt.index === index);
    if (!lane) return;
    const attemptSetId = this.#attemptSetId();
    const sessionId = this.session;
    const actionId = ++this.#actionId;
    this.#busy = `keep:${index}`;
    this.#actionError = null;
    this.#renderDecision();
    this.#syncButtons();
    if (this.isConnected) {
      if (this.#pollTimer) clearTimeout(this.#pollTimer);
      this.#pollTimer = setTimeout(() => {
        this.#pollTimer = null;
        if (this.#actionCurrent(actionId, sessionId, attemptSetId)) void this.refresh();
      }, 500);
    }
    try {
      const result = await jsonRequest(
        `/api/sessions/${encodeURIComponent(sessionId)}/variants/adopt`, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ attempt_set_id: attemptSetId, index }),
        });
      this.dispatchEvent(new CustomEvent('attempt-keep', {
        detail: {
          session: sessionId,
          attempt_set_id: attemptSetId,
          index,
          attempt: { ...lane },
          result,
        },
        bubbles: true,
        composed: true,
      }));
      if (this.#actionCurrent(actionId, sessionId, attemptSetId)) {
        this.#inspect = null;
        this.#branches.hidden = true;
        await this.refresh();
      }
    } catch (error) {
      this.#emitError('keep', error, {
        session: sessionId,
        attempt_set_id: attemptSetId,
        action_id: actionId,
        index,
        attempt: attemptLabel(lane),
      });
      if (this.#actionCurrent(actionId, sessionId, attemptSetId)) await this.refresh();
    } finally {
      if (!this.#actionCurrent(actionId, sessionId, attemptSetId)) return;
      this.#busy = null;
      this.#renderDecision();
      this.#syncButtons();
    }
  }

  async #discard() {
    // Discard is also the cancellation control for a long Checks request. The
    // daemon removes the exact check container out of band before it waits for
    // the workspace lease, so allowing this one overlapping request is what
    // makes the durable `checking` state escapable from the page that started it.
    if ((this.#busy && this.#busy !== 'verify') || !this.#attemptSetId()
        || this.#isKeepRecovery()) return;
    const attemptSetId = this.#attemptSetId();
    const sessionId = this.session;
    const actionId = ++this.#actionId;
    this.#busy = 'discard';
    this.#actionError = null;
    this.#renderDecision();
    this.#syncButtons();
    if (this.isConnected) {
      if (this.#pollTimer) clearTimeout(this.#pollTimer);
      this.#pollTimer = setTimeout(() => {
        this.#pollTimer = null;
        if (this.#actionCurrent(actionId, sessionId, attemptSetId)) void this.refresh();
      }, 500);
    }
    try {
      const result = await jsonRequest(
        `/api/sessions/${encodeURIComponent(sessionId)}/variants/discard`, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ attempt_set_id: attemptSetId }),
        });
      this.dispatchEvent(new CustomEvent('attempt-discard', {
        detail: { session: sessionId, attempt_set_id: attemptSetId, result },
        bubbles: true,
        composed: true,
      }));
      if (this.#actionCurrent(actionId, sessionId, attemptSetId)) {
        this.#inspect = null;
        this.#branches.hidden = true;
        await this.refresh();
      }
    } catch (error) {
      this.#emitError('discard', error, {
        session: sessionId,
        attempt_set_id: attemptSetId,
        action_id: actionId,
      });
      if (this.#actionCurrent(actionId, sessionId, attemptSetId)) await this.refresh();
    } finally {
      if (!this.#actionCurrent(actionId, sessionId, attemptSetId)) return;
      this.#busy = null;
      this.#renderDecision();
      this.#syncButtons();
    }
  }

  #syncButtons() {
    const lanes = this.#lanes();
    const verify = this.#bar.querySelector('[data-act="verify"]');
    const judge = this.#bar.querySelector('[data-act="judge"]');
    const branches = this.#bar.querySelector('[data-act="branches"]');
    verify.disabled = Boolean(this.#busy) || !this.#canVerify();
    judge.disabled = Boolean(this.#busy) || !this.#canJudge();
    const discarding = this.#setState() === 'discarding';
    branches.disabled = lanes.length < 2 || discarding;
    verify.textContent = this.#busy === 'verify' ? 'Running checks…'
      : (this.#setState() === 'checking' ? 'Retry checks' : 'Run checks');
    judge.textContent = this.#busy === 'judge' ? 'Judging…' : 'Judge';
    this.toggleAttribute('aria-busy', Boolean(this.#busy));

    const discard = this.#decision.querySelector('[data-discard]');
    if (discard) {
      const recovery = this.#isKeepRecovery();
      const canCancelBusyChecks = this.#busy === 'verify';
      const cancellingChecks = canCancelBusyChecks
        || (!this.#busy && this.#setState() === 'checking');
      discard.disabled = (Boolean(this.#busy) && !canCancelBusyChecks)
        || !this.#attemptSetId() || recovery;
      discard.textContent = this.#busy === 'discard' ? 'Finishing…'
        : (cancellingChecks ? 'Stop checks & finish without keeping'
          : (discarding ? 'Finish cleanup' : 'Finish without keeping'));
      discard.title = recovery
        ? 'Keep has started; finish the selected attempt instead of discarding its recovery state'
        : (cancellingChecks ? 'Stop the running checks and remove every attempt'
          : (discarding ? 'Retry cleanup for these attempts' : 'Remove every attempt without keeping one'));
    }
    const select = this.#decision.querySelector('[data-judge-agent]');
    // While lifecycle polling is active, render() replaces the decision DOM.
    // Keep this selector unavailable until every way is terminal so keyboard
    // focus cannot be destroyed mid-selection by the next poll.
    if (select) select.disabled = Boolean(this.#busy) || this.#isKeepRecovery()
      || !this.#allTerminal() || ['checking', 'discarding'].includes(this.#setState());
    const check = this.#check.querySelector('input');
    if (check) check.disabled = Boolean(this.#busy) || discarding;
    for (const button of this.#body.querySelectorAll('[data-keep]')) {
      const index = Number(button.dataset.keep);
      const reason = this.#keepReason(index);
      button.disabled = Boolean(this.#busy) || Boolean(reason);
      button.title = reason || 'Keep this attempt and remove the others';
      button.textContent = this.#busy === `keep:${index}` ? 'Finishing Keep…'
        : (this.#isKeepRecovery() && index === this.#keptIndex() ? 'Finish Keep' : 'Keep this one');
    }
  }

  #toggleBranches() {
    const button = this.#bar.querySelector('[data-act="branches"]');
    if (!this.#branches.hidden) {
      this.#branches.hidden = true;
      button.textContent = 'Show the branches';
      return;
    }
    const lanes = this.#lanes();
    this.#branches.textContent = '';
    if (!lanes.length) {
      const empty = document.createElement('div');
      empty.className = 'empty';
      empty.textContent = 'No attempts yet — nothing to show.';
      this.#branches.append(empty);
    }
    for (const lane of lanes) {
      const row = document.createElement('div');
      row.className = 'brow';
      const number = document.createElement('span');
      number.className = 'bn';
      number.textContent = `#${(lane.index ?? 0) + 1}`;
      const branch = document.createElement('code');
      branch.textContent = lane.branch || '—';
      const worktree = document.createElement('code');
      worktree.className = 'wt';
      worktree.textContent = lane.worktree || '—';
      worktree.title = lane.worktree || '';
      row.append(number, branch, worktree);
      this.#branches.append(row);
    }
    this.#branches.hidden = false;
    button.textContent = 'Hide the branches';
  }

  #renderCheck() {
    if (this.#lanes().length < 2) {
      this.#check.textContent = '';
      return;
    }
    let input = this.#check.querySelector('input');
    if (!input) {
      this.#check.innerHTML = '<span></span><input aria-label="Check command" spellcheck="false">';
      input = this.#check.querySelector('input');
      input.value = this.#checkCmd || '';
      const commit = async () => {
        const next = input.value.trim();
        if (next === (this.#checkCmd || '')) return;
        const sessionId = this.session;
        const attemptSetId = this.#attemptSetId();
        const sessionEpoch = this.#sessionEpoch;
        const saveId = ++this.#checkSaveId;
        this.#checkCmd = next || null;
        this.#checkSavePending++;
        this.#renderCheck();
        this.#syncButtons();
        const save = this.#checkSaveChain.catch(() => {}).then(() =>
          jsonRequest(`/api/sessions/${encodeURIComponent(sessionId)}/check`, {
            method: 'PUT',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ check_command: next || null }),
          }));
        this.#checkSaveChain = save;
        try {
          await save;
          if (sessionEpoch === this.#sessionEpoch && sessionId === this.session) {
            this.#savedCheckCmd = next || null;
          }
        } catch (error) {
          if (sessionEpoch !== this.#sessionEpoch || saveId !== this.#checkSaveId
              || sessionId !== this.session
              || attemptSetId !== this.#attemptSetId()) return;
          this.#checkCmd = this.#savedCheckCmd;
          this.#emitError('set-check', error, {
            session: sessionId,
            attempt_set_id: attemptSetId,
            check: next,
            check_save_id: saveId,
          });
          this.#renderCheck();
          this.#renderDecision();
          this.#syncButtons();
        } finally {
          if (sessionEpoch === this.#sessionEpoch && sessionId === this.session) {
            this.#checkSavePending = Math.max(0, this.#checkSavePending - 1);
          }
        }
      };
      input.addEventListener('blur', () => void commit());
      input.addEventListener('keydown', (event) => {
        if (event.key === 'Enter') { event.preventDefault(); input.blur(); }
      });
      input.addEventListener('input', () => {
        this.#renderCheck();
        this.#renderDecision();
        this.#syncButtons();
      });
    }
    if (this.#root.activeElement !== input) input.value = this.#checkCmd || '';
    const check = this.#checkValue();
    this.#check.classList.toggle('unset', !check);
    this.#check.querySelector('span').textContent = check
      ? 'Checks' : 'No check command — attempts cannot be verified until you set one';
    input.placeholder = check ? '' : 'npm test';
    input.disabled = Boolean(this.#busy) || this.#setState() === 'discarding';
  }

  #renderDecision() {
    if (this.#lanes().length < 2) {
      this.#decision.textContent = '';
      return;
    }
    this.#selectJudgeAgent();
    const state = this.#setState() || 'preparing';
    const options = this.#agents.map((agent) => {
      const target = [agent.provider, agent.model].filter(Boolean).join(' / ');
      const label = `${agent.name || agent.id}${target ? ` — ${target}` : ''}`;
      return `<option value="${html(agent.id)}"${agent.id === this.#judgeAgentId ? ' selected' : ''}>`
        + `${html(label)}</option>`;
    }).join('');
    const selector = options
      ? `<label>Judge agent <select data-judge-agent aria-label="Judge agent">${options}</select></label>`
      : `<span class="muted">${html(this.#agentsError || 'No configured judge agent is available.')}</span>`;
    let next = 'Attempts are still running.';
    if (state === 'ready') next = this.#checkValue()
      ? 'Ready to run checks.' : 'Set a check command to continue.';
    if (state === 'checking') next = 'Checks were interrupted or are still in progress. Partial results are shown; retry Checks or finish without keeping.';
    if (state === 'verified') {
      const survivors = (this.#results?.verdicts || [])
        .filter((verdict) => verdict.passed && verdict.changed_files > 0).length;
      next = survivors >= 2
        ? 'Checks are complete. Judge the survivors or keep one.'
        : (survivors === 1
          ? 'One keepable attempt remains. Keep it, inspect it, or finish without keeping; Judge needs two.'
          : 'Checks left no keepable attempt. Finish without keeping, then explore again.');
    }
    if (state === 'judged') next = 'Review the recommendation, then keep one.';
    if (state === 'failed') next = 'Every attempt ended without completing.';
    if (state === 'applying') next = 'Keep started. Finish the selected attempt to complete the resumable apply.';
    if (state === 'applied') next = 'The changes are applied. Finish Keep to record the turn and clean up.';
    if (state === 'transcript_recorded') next = 'The turn is recorded. Finish Keep to complete cleanup.';
    if (state === 'discarding') next = 'Attempt cleanup is incomplete. Finish cleanup; other actions stay unavailable.';
    this.#decision.innerHTML = `
      <span class="state-pill" data-state="${html(state)}">${html(sentence(state))}</span>
      <span class="muted">${html(next)}</span>
      ${selector}
      <span class="spacer"></span>
      <button type="button" class="action danger" data-discard
        title="Remove every attempt without keeping one">Finish without keeping</button>
      ${this.#actionError ? `<span class="action-error" role="alert">${html(this.#actionError)}</span>` : ''}`;
  }

  /** Re-read the current attempt set and its review material. */
  async refresh() {
    const sessionId = this.session;
    if (!sessionId) { this.render(); return; }
    if (this.#pollTimer) clearTimeout(this.#pollTimer);
    this.#pollTimer = null;
    const refreshId = ++this.#refreshId;
    const session = encodeURIComponent(sessionId);
    if (!this.#results) {
      this.#loading = true;
      this.render();
    }
    this.#error = null;

    const previousSetId = this.#attemptSetId();
    const [sessionsResult, agentsResult, resultsResult] = await Promise.allSettled([
      jsonRequest('/api/sessions'),
      jsonRequest('/api/agents'),
      jsonRequest(`/api/sessions/${session}/variants/results`),
    ]);
    if (refreshId !== this.#refreshId || sessionId !== this.session) return;

    if (sessionsResult.status === 'fulfilled') {
      const current = (Array.isArray(sessionsResult.value) ? sessionsResult.value : [])
        .find((item) => item.id === sessionId);
      if (!this.#checkSavePending) {
        this.#checkCmd = current?.check_command || null;
        this.#savedCheckCmd = this.#checkCmd;
      }
      this.#sessionAgentId = current?.mode?.agent_id || current?.agent_id || '';
    }
    if (agentsResult.status === 'fulfilled') {
      this.#agents = Array.isArray(agentsResult.value) ? agentsResult.value : [];
      this.#agentsError = null;
      this.#selectJudgeAgent();
    } else {
      this.#agentsError = `Could not load judge agents: ${agentsResult.reason?.message || agentsResult.reason}`;
    }
    if (resultsResult.status === 'fulfilled') {
      this.#results = resultsResult.value || {};
    } else {
      this.#error = String(resultsResult.reason?.message || resultsResult.reason);
    }

    const attemptSetId = this.#attemptSetId();
    if (previousSetId && previousSetId !== attemptSetId) {
      // Results is the lifecycle authority. This can precede (or permanently
      // outlive) the original Keep/Discard response when a connection drops,
      // so tell the shell immediately instead of waiting for that promise to
      // reject before it releases the transcript/composer.
      this.dispatchEvent(new CustomEvent('attempt-set-changed', {
        detail: {
          session: sessionId,
          previous_attempt_set_id: previousSetId,
          attempt_set_id: attemptSetId || null,
        },
        bubbles: true,
        composed: true,
      }));
    }
    if (previousSetId !== attemptSetId) {
      this.#actionId++;
      this.#busy = null;
      this.#actionError = null;
      this.#base = this.#lanes()[0]?.index || 0;
      this.#inspect = null;
      this.#alignment = null;
      this.#alignmentError = null;
      this.#openFolds.clear();
      this.#branches.hidden = true;
      this.#branches.textContent = '';
      this.#bar.querySelector('[data-act="branches"]').textContent = 'Show the branches';
    }
    if (this.#setState() === 'discarding') {
      this.#inspect = null;
      this.#alignment = null;
      this.#alignmentError = null;
      this.#branches.hidden = true;
      this.#branches.textContent = '';
      this.#bar.querySelector('[data-act="branches"]').textContent = 'Show the branches';
    }

    this.#statuses = [];
    this.#statusError = null;
    const materializationUnavailable = ['checking', 'discarding'].includes(this.#setState())
      || this.#isKeepRecovery();
    if (resultsResult.status === 'fulfilled' && this.#results && this.#lanes().length
        && !materializationUnavailable) {
      const statusParams = new URLSearchParams();
      const routeParams = new URLSearchParams({ baseline: String(this.#base) });
      if (attemptSetId) {
        statusParams.set('attempt_set_id', attemptSetId);
        routeParams.set('attempt_set_id', attemptSetId);
      }
      const statusSuffix = statusParams.toString() ? `?${statusParams}` : '';
      const [statusResult, routeResult] = await Promise.allSettled([
        jsonRequest(`/api/sessions/${session}/variants/status${statusSuffix}`),
        jsonRequest(`/api/sessions/${session}/variants/trajectories?${routeParams}`),
      ]);
      if (refreshId !== this.#refreshId || sessionId !== this.session) return;
      if (statusResult.status === 'fulfilled') {
        this.#statuses = Array.isArray(statusResult.value) ? statusResult.value : [];
      } else {
        this.#statusError = String(statusResult.reason?.message || statusResult.reason);
      }
      if (routeResult.status === 'fulfilled') {
        this.#alignment = routeResult.value;
        this.#alignmentError = null;
      } else {
        this.#alignment = null;
        this.#alignmentError = String(routeResult.reason?.message || routeResult.reason);
      }
    } else {
      this.#alignment = null;
      this.#alignmentError = null;
    }
    if (refreshId !== this.#refreshId || sessionId !== this.session) return;
    this.#loading = false;
    this.render();
    if (this.isConnected
        && (this.#error
          || ['verify', 'discard'].includes(this.#busy)
          || String(this.#busy || '').startsWith('keep:')
          || ['preparing', 'running', 'checking', 'discarding'].includes(this.#setState())
          || this.#isKeepRecovery())) {
      this.#pollTimer = setTimeout(() => {
        this.#pollTimer = null;
        void this.refresh();
      }, 1500);
    }
  }

  #preservePolledActionFocus() {
    const active = this.#root.activeElement;
    let selector = '';
    if (active?.matches?.('[data-discard]')) selector = '[data-discard]';
    else if (active?.matches?.('[data-keep]')) selector = `[data-keep="${active.dataset.keep}"]`;
    else if (active?.matches?.('[data-rebase]')) selector = `[data-rebase="${active.dataset.rebase}"]`;
    else if (active?.matches?.('[data-diff-only]')) selector = '[data-diff-only]';
    else if (active?.matches?.('[data-fold]')) selector = `[data-fold="${active.dataset.fold}"]`;
    else if (active?.matches?.('[data-inspect-back]')) selector = '[data-inspect-back]';
    else if (active?.matches?.('[data-inspect]')) {
      selector = `[data-inspect="${active.dataset.inspect}"]`;
      if (active.dataset.file !== undefined) selector += `[data-file="${active.dataset.file}"]`;
      else selector += ':not([data-file])';
    }
    if (!selector) return;
    const sessionId = this.session;
    const attemptSetId = this.#attemptSetId();
    queueMicrotask(() => {
      if (sessionId !== this.session || attemptSetId !== this.#attemptSetId()
          || this.#root.activeElement) return;
      const replacement = this.#root.querySelector(selector);
      if (replacement && !replacement.disabled) replacement.focus();
    });
  }

  render() {
    this.#preservePolledActionFocus();
    for (const button of this.#bar.querySelectorAll('[data-view]')) {
      button.setAttribute('aria-pressed', String(button.dataset.view === this.view));
    }
    this.#renderCheck();
    this.#renderDecision();
    this.#syncButtons();
    const attempts = this.#lanes();
    if (this.#loading) {
      this.#gist.textContent = '';
      this.#body.innerHTML = '<div class="empty loading" role="status">Reading the attempts…</div>';
      return;
    }
    if (this.#error) {
      this.#gist.textContent = '';
      this.#body.innerHTML = `<div class="empty failed">Could not read the attempts: ${html(this.#error)}`
        + '<button type="button" class="retry" data-retry>Try again</button></div>';
      return;
    }
    if (attempts.length < 2) {
      this.#gist.textContent = '';
      this.#body.innerHTML = '<div class="empty">Explore a task several ways to compare the attempts.</div>';
      return;
    }
    if (this.#inspect) { this.#renderInspect(); return; }
    if (this.view === 'route') this.#renderRoute(); else this.#renderOutcome();
    this.#syncButtons();
  }

  // ── Outcome ──────────────────────────────────────────────────────────
  #cell(attempt) {
    const verdict = this.#verdict(attempt.index);
    const usage = this.#usage(attempt.index);
    const rationale = this.#rationale(attempt.index);
    const files = this.#files(attempt.index);
    const state = this.#laneState(attempt.index);
    let outcome = sentence(state);
    if (verdict) {
      outcome = !verdict.passed
        ? 'ruled out' : (verdict.changed_files === 0 ? 'changed nothing' : 'passed');
    }
    return {
      lifecycle: sentence(state),
      outcome,
      rank: rationale ? `#${rationale.rank}` : '—',
      files: files ? String(files.length) : (verdict ? String(verdict.changed_files) : 'unknown'),
      tests: verdict ? (verdict.touched_tests?.length
        ? `changed ${verdict.touched_tests.length}` : 'untouched') : '—',
      duration: usage ? secs(usage.duration_ms || 0) : '—',
      cost: usage ? (usage.cost_known === true ? money(usage.cost_usd || 0) : 'unknown') : '—',
      tokens: usage
        ? (usage.token_usage_known === false
          ? 'unknown' : String((usage.input_tokens || 0) + (usage.output_tokens || 0)))
        : '—',
    };
  }

  #renderOutcome() {
    const metrics = [
      ['lifecycle', 'Lifecycle'], ['outcome', 'Outcome'], ['rank', 'Rank'],
      ['files', 'Changed paths'], ['tests', 'Tests'], ['duration', 'Duration'],
      ['cost', 'Execution cost'], ['tokens', 'Tokens'],
    ];
    const order = [...this.#lanes()].sort((left, right) =>
      (left.index === this.#base ? -1 : 0) - (right.index === this.#base ? -1 : 0));
    const cells = order.map((attempt) => this.#cell(attempt));
    const head = `<tr><th></th>${order.map((attempt, position) =>
      `<th><button type="button" class="rebase" data-rebase="${attempt.index}">`
      + `${html(attemptLabel(attempt))}</button>`
      + `${position === 0 ? '<div class="base">baseline</div>' : ''}</th>`).join('')}</tr>`;

    let hidden = 0;
    const rows = metrics.map(([key, name]) => {
      const values = cells.map((cell) => cell[key]);
      const same = values.every((value) => value === values[0]);
      if (same) hidden++;
      if (this.#diffOnly && same) return '';
      return `<tr class="${same ? 'same' : ''}"><th>${html(name)}</th>`
        + values.map((value) => {
          let className = '';
          if (key === 'outcome') {
            className = value === 'passed' ? 'good'
              : (['ruled out', 'changed nothing', 'failed', 'cancelled', 'interrupted'].includes(value)
                ? 'bad' : '');
          }
          if (key === 'rank' && value === '#1') className = 'win';
          if (key === 'tests' && value.startsWith('changed')) className = 'bad';
          return `<td class="${className}">${html(value)}</td>`;
        }).join('') + '</tr>';
    }).join('');
    const task = this.#attemptSet()?.task;
    const judgment = this.#results?.judgment;
    const taskBlock = task ? `<section class="task"><span class="eyebrow">Task</span>`
      + `<p>${html(task)}</p></section>` : '';
    const judgmentBlock = judgment
      ? `<section class="judgment" aria-labelledby="judge-reasoning"><h3 id="judge-reasoning">`
        + `Judge’s reasoning</h3><p>${html(judgment.reasoning || 'No overall reasoning returned.')}</p></section>`
      : '';

    this.#gist.textContent = `${order.length} attempts · ${sentence(this.#setState())}`;
    this.#body.innerHTML = `${taskBlock}${judgmentBlock}<div class="table-wrap"><table>`
      + `<caption class="sr-only">Attempt outcomes</caption>${head}${rows}</table></div>`
      + `<label class="opt"><input type="checkbox" data-diff-only ${this.#diffOnly ? 'checked' : ''}>`
      + ` only differences${this.#diffOnly && hidden
        ? ` — ${hidden} identical row${hidden === 1 ? '' : 's'} hidden` : ''}</label>`
      + `<div class="lane-grid">${order.map((attempt) => this.#laneCard(attempt)).join('')}</div>`;
  }

  #laneCard(attempt) {
    const index = attempt.index;
    const fact = this.#laneFact(index);
    const state = this.#laneState(index);
    const verdict = this.#verdict(index);
    const rationale = this.#rationale(index);
    const output = this.#output(index);
    const files = this.#files(index);
    const winner = this.#results?.judgment?.winner === index;
    const keepReason = this.#keepReason(index);
    let verdictTitle = 'Checks have not run for this attempt.';
    if (verdict) {
      verdictTitle = !verdict.passed
        ? `Checks failed with exit code ${verdict.exit_code}.`
        : (verdict.changed_files === 0
          ? 'Checks passed, but this attempt changed nothing.' : 'Checks passed.');
    }
    const verdictBlock = verdict
      ? `<div><span class="section-title">Check verdict</span>`
        + `<div class="${verdict.passed && verdict.changed_files > 0 ? 'good' : 'failed'}">`
        + `${html(verdictTitle)}</div><div class="verdict-meta">Exit ${html(verdict.exit_code)}</div>`
        + `<pre class="verdict-output">${html(verdict.output || 'No output returned.')}</pre></div>`
      : `<div><span class="section-title">Check verdict</span><span class="muted">${html(verdictTitle)}</span></div>`;

    const cleanupOnly = this.#setState() === 'discarding';
    let filesBlock = `<div><span class="section-title">Changed paths</span>`;
    if (cleanupOnly) {
      filesBlock += '<span class="muted">Unavailable while attempt cleanup finishes.</span>';
    } else if (files?.length) {
      filesBlock += `<ul class="paths">${files.map((file, fileIndex) => {
        const counts = file.added == null || file.removed == null
          ? '' : `<span class="lines">+${file.added} −${file.removed}</span>`;
        return `<li class="path-row"><button type="button" class="path" data-inspect="${index}" `
          + `data-file="${fileIndex}" title="Inspect ${html(file.path)}">${html(file.path)}</button>`
          + `<span class="file-state">${html(sentence(file.state))}</span>${counts}</li>`;
      }).join('')}</ul>`;
    } else if (files) {
      filesBlock += '<span class="muted">No changed paths.</span>';
    } else {
      filesBlock += `<span class="muted">${this.#statusError
        ? `Changed paths unavailable: ${html(this.#statusError)}` : 'Changed paths are still loading.'}</span>`;
    }
    if (!cleanupOnly) {
      filesBlock += `<div class="card-actions"><button type="button" class="action" data-inspect="${index}">`
        + 'Inspect files</button></div>';
    }
    filesBlock += '</div>';

    const tests = verdict?.touched_tests || [];
    const testsBlock = tests.length
      ? `<div class="test-warning"><span class="section-title">Tests changed by this attempt</span>`
        + `${tests.map((path) => html(path)).join('<br>')}</div>` : '';
    const rationaleBlock = rationale
      ? `<div class="rationale"><div><span class="section-title">Approach · rank #${rationale.rank}</span>`
        + `<p>${html(rationale.approach || 'No approach summary returned.')}</p></div>`
        + `<div><span class="section-title">Tradeoffs</span>`
        + `<p>${html(rationale.tradeoffs || 'No tradeoffs returned.')}</p></div></div>` : '';
    const outputBlock = output
      ? `<div><span class="section-title">Outcome</span>`
        + `<div class="attempt-output">${html(output)}</div></div>`
      : `<div><span class="section-title">Outcome</span>`
        + '<span class="muted">No durable answer was recorded for this attempt.</span></div>';

    return `<article class="lane-card"><div class="lane-head"><h3>${html(attemptLabel(attempt))}</h3>`
      + `${winner ? '<span class="winner">Recommended</span>' : ''}`
      + `<span class="state-pill" data-state="${html(state)}">${html(sentence(state))}</span></div>`
      + `${fact?.error ? `<div class="lane-error">${html(fact.error)}</div>` : ''}`
      + `${outputBlock}${verdictBlock}${testsBlock}${filesBlock}${rationaleBlock}`
      + `<div class="card-actions"><button type="button" class="action primary" data-keep="${index}"`
      + `${keepReason ? ' disabled' : ''}>Keep this one</button>`
      + `${keepReason ? `<span class="why">${html(keepReason)}</span>` : ''}</div></article>`;
  }

  // ── File inspection ──────────────────────────────────────────────────
  #openInspect(index, fileIndex = null) {
    if (this.#setState() === 'discarding') return;
    const files = this.#files(index) || [];
    this.#inspect = { index, fileIndex: null, diff: null, loading: false, error: null };
    this.render();
    if (fileIndex != null && files[fileIndex]) void this.#loadDiff(index, fileIndex);
  }

  async #loadDiff(index, fileIndex) {
    if (this.#setState() === 'discarding') return;
    const file = (this.#files(index) || [])[fileIndex];
    if (!file || !this.#inspect || this.#inspect.index !== index) return;
    const attemptSetId = this.#attemptSetId();
    const sessionId = this.session;
    this.#inspect = { index, fileIndex, diff: null, loading: true, error: null };
    this.#renderInspect();
    const params = new URLSearchParams({
      attempt_set_id: attemptSetId,
      index: String(index),
      path: file.path,
    });
    try {
      const diff = await jsonRequest(
        `/api/sessions/${encodeURIComponent(sessionId)}/variants/diff?${params}`);
      if (sessionId !== this.session || this.#attemptSetId() !== attemptSetId
          || this.#inspect?.index !== index || this.#inspect?.fileIndex !== fileIndex) return;
      this.#inspect = { index, fileIndex, diff, loading: false, error: null };
    } catch (error) {
      if (sessionId !== this.session || this.#attemptSetId() !== attemptSetId
          || this.#inspect?.index !== index || this.#inspect?.fileIndex !== fileIndex) return;
      this.#inspect = {
        index, fileIndex, diff: null, loading: false, error: String(error?.message || error),
      };
      this.#emitError('inspect-files', error, {
        session: sessionId,
        attempt_set_id: attemptSetId,
        index,
        path: file.path,
      });
    }
    this.render();
  }

  #renderInspect() {
    const index = this.#inspect?.index;
    const attempt = this.#lanes().find((lane) => lane.index === index);
    if (!attempt) { this.#inspect = null; this.render(); return; }
    const files = this.#files(index) || [];
    const reason = this.#keepReason(index);
    const keepLabel = this.#isKeepRecovery() && index === this.#keptIndex()
      ? 'Finish Keep' : 'Keep this one';
    const fileList = files.length
      ? files.map((file, fileIndex) => `<button type="button" data-inspect="${index}" data-file="${fileIndex}" `
        + `aria-current="${String(this.#inspect.fileIndex === fileIndex)}"><span>${html(file.path)}</span>`
        + `<span class="meta">${html(sentence(file.state))}`
        + `${file.added == null || file.removed == null ? '' : ` · +${file.added} −${file.removed}`}`
        + '</span></button>').join('')
      : `<div class="diff-empty">${this.#statusError
        ? `Could not read changed paths: ${html(this.#statusError)}` : 'This attempt has no changed paths.'}</div>`;
    let diff = '<div class="diff-empty">Choose a changed path to inspect it.</div>';
    if (this.#inspect.loading) diff = '<div class="diff-empty" role="status">Reading this change…</div>';
    if (this.#inspect.error) {
      diff = `<div class="diff-empty failed" role="alert">Could not read this change: `
        + `${html(this.#inspect.error)}</div>`;
    }
    if (this.#inspect.diff) {
      const value = this.#inspect.diff;
      if (value.binary) {
        diff = '<div class="diff-empty">This is a binary file, so it cannot be shown inline.</div>';
      } else if (value.too_large) {
        diff = '<div class="diff-empty">This file is too large to show inline.</div>';
      } else {
        diff = `<div class="diff-grid"><section class="diff-side"><h3>Before</h3>`
          + `<pre>${html(value.old)}</pre></section><section class="diff-side"><h3>After</h3>`
          + `<pre>${html(value.new)}</pre></section></div>`;
      }
    }
    this.#gist.textContent = `Inspecting ${attemptLabel(attempt)}`;
    this.#body.innerHTML = `<section class="inspect"><div class="inspect-head">`
      + '<button type="button" class="back" data-inspect-back>← Back to Outcome</button>'
      + `<h2>${html(attemptLabel(attempt))} · changed paths</h2>`
      + `<span class="state-pill" data-state="${html(this.#laneState(index))}">`
      + `${html(sentence(this.#laneState(index)))}</span>`
      + `<button type="button" class="action primary" data-keep="${index}"${reason ? ' disabled' : ''} `
      + `title="${html(reason || 'Keep this attempt and remove the others')}">${keepLabel}</button></div>`
      + `<div class="inspect-grid"><nav class="file-list" aria-label="Changed paths">${fileList}</nav>`
      + `<div class="diff">${diff}</div></div></section>`;
  }

  // ── Route ────────────────────────────────────────────────────────────
  #step(action, relation) {
    const className = [relation, action?.failed ? 'failed' : ''].filter(Boolean).join(' ');
    if (!action) return `<td class="none ${className}">—</td>`;
    const kinds = new Set(['read', 'list', 'search', 'edit', 'write', 'run']);
    const kind = kinds.has(action.kind) ? action.kind : 'other';
    return `<td class="${className}"><span class="k ${kind}" title="${html(action.tool)}">`
      + `${html(action.kind || 'other')}</span>${html(action.target)}`
      + (action.detail ? `<span class="det">${html(action.detail)}</span>` : '')
      + (action.failed ? '<span class="det">failed</span>' : '') + '</td>';
  }

  #renderRoute() {
    if (this.#setState() === 'discarding') {
      this.#gist.textContent = '';
      this.#body.innerHTML = '<div class="empty">Route is unavailable while attempt cleanup finishes.</div>';
      return;
    }
    if (this.#alignmentError) {
      this.#gist.textContent = '';
      this.#body.innerHTML = `<div class="empty failed">Could not read attempt routes: `
        + `${html(this.#alignmentError)}<button type="button" class="retry" data-retry>`
        + 'Try again</button></div>';
      return;
    }
    const alignment = this.#alignment;
    if (!alignment?.rows?.length) {
      this.#gist.textContent = '';
      this.#body.innerHTML = '<div class="empty">No steps recorded for these attempts yet.</div>';
      return;
    }
    const byIndex = new Map(this.#lanes().map((attempt) => [attempt.index, attempt]));
    const head = `<tr><th class="n"></th>${alignment.lanes.map((index, position) =>
      `<th>${html(attemptLabel(byIndex.get(index) || { index }))}`
      + `${position === 0 ? '<div class="base">baseline</div>' : ''}</th>`).join('')}</tr>`;

    const groups = [];
    alignment.rows.forEach((row, index) => {
      const last = groups[groups.length - 1];
      if (row.agree && last?.agree) last.rows.push([index, row]);
      else groups.push({ agree: row.agree, rows: [[index, row]] });
    });

    const body = groups.map((group, groupIndex) => {
      const foldable = group.agree && group.rows.length > 1;
      const open = this.#openFolds.has(groupIndex);
      const fold = foldable
        ? `<tr class="fold"><td class="n"></td><td colspan="${alignment.lanes.length}">`
          + `<button type="button" data-fold="${groupIndex}">${open ? '▾' : '▸'} `
          + `${group.rows.length} identical steps</button></td></tr>`
        : '';
      if (foldable && !open) return fold;
      return fold + group.rows.map(([index, row]) =>
        `<tr class="${row.agree ? '' : 'diverged'}"><td class="n">${index + 1}</td>`
        + row.cells.map((cell, position) => this.#step(cell,
          position === 0 ? ''
            : ((row.agree || row.matches_baseline?.[position]) ? 'same-as-base' : 'off'))).join('')
        + '</tr>').join('');
    }).join('');

    const diverged = alignment.rows.length - alignment.agreed;
    this.#gist.textContent = diverged === 0
      ? `${alignment.rows.length} steps, identical throughout`
      : `${diverged} of ${alignment.rows.length} steps diverged`;
    this.#body.innerHTML = `<div class="table-wrap"><table><caption class="sr-only">`
      + `Attempt routes</caption>${head}${body}</table></div>`;
  }
}

customElements.define('ax-compare', AxCompare);
