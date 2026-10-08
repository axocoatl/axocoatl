import { adopt } from './sheets.js';

/**
 * `<ax-run-outcome>` — the Outcome of one loadout run, as `axocoatl run`
 * reports it: verdict and exit code, the required checks with their report
 * test cases, the review rounds, the writer's adjudications (missing ones in
 * red), findings with their reproduction classification ("fails on clean
 * build" as its own label), everything not covered and why, the run's notes
 * (what was neither a gap nor a warning), warnings (the same-model reviewer
 * warning included), usage (a known subtotal when
 * incomplete, and a cost the run does not know as such), the network summary, a link to download the record bundle and
 * Keep as PR (`<ax-keep-pr>`, workstream keep).
 *
 * @element ax-run-outcome
 * @attr {string} run-id  The run to show; the panel reads `/api/runs/{id}`.
 * @prop {object} status  A `RunStatusView` to show without fetching.
 */

const CSS = `
:host { display: block; color: var(--text); font: var(--fs-sm)/var(--lh-body) var(--font-sans); }
* { box-sizing: border-box; }
section { margin: 0 0 var(--sp-4); }
h3 { margin: 0 0 var(--sp-2); font-size: var(--fs-xs); letter-spacing: .06em; text-transform: uppercase; color: var(--muted); }
.verdict { display: flex; flex-wrap: wrap; align-items: center; gap: var(--sp-2); margin-bottom: var(--sp-3); }
.verdict strong { font-size: var(--fs-lg); font-weight: var(--fw-medium); }
.badge { display: inline-flex; padding: 2px 8px; border-radius: var(--r-pill); background: var(--bg-3); color: var(--muted); font-size: var(--fs-xs); }
.badge.pass { color: var(--ok); background: rgba(var(--axo-jade-rgb), .16); }
.badge.fail, .badge.error { color: var(--err); background: rgba(226, 106, 106, .13); }
.badge.attention, .badge.interrupted { color: var(--warn); background: rgba(var(--axo-bronze-rgb), .16); }
.badge.clean { color: var(--muted); background: var(--bg-3); }
ul { margin: 0; padding-left: 1.2em; }
li { margin: 2px 0; overflow-wrap: anywhere; }
table { width: 100%; border-collapse: collapse; font-size: var(--fs-sm); }
th, td { padding: 5px var(--sp-2); border-bottom: 1px solid var(--border); text-align: left; vertical-align: top; overflow-wrap: anywhere; }
th { color: var(--muted); font-size: var(--fs-xs); font-weight: var(--fw-medium); }
tr.missing td { color: var(--err); }
tr.missing td.decision { font-weight: var(--fw-medium); }
.mono { font-family: var(--font-mono); font-size: var(--fs-xs); }
.muted { color: var(--muted); }
.warning { padding: var(--sp-2) var(--sp-3); border-radius: var(--r-md); color: var(--warn); background: rgba(var(--axo-bronze-rgb), .12); margin-bottom: var(--sp-1); }
.error { padding: var(--sp-2) var(--sp-3); border-radius: var(--r-md); color: var(--err); background: rgba(226, 106, 106, .1); white-space: pre-wrap; overflow-wrap: anywhere; }
details { margin: var(--sp-1) 0; }
summary { cursor: pointer; }
pre { white-space: pre-wrap; overflow-wrap: anywhere; background: var(--bg); padding: var(--sp-2); border-radius: var(--r-sm); max-height: 240px; overflow: auto; font: var(--fs-xs) var(--font-mono); }
.actions { display: flex; flex-wrap: wrap; gap: var(--sp-2); align-items: center; }
a.download { color: var(--accent); }
`;

const el = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
};

const VERDICT = {
  pass: ['Pass', 'pass'],
  checks_failed: ['Checks failed', 'fail'],
  needs_attention: ['Needs attention', 'attention'],
  interrupted: ['Interrupted', 'interrupted'],
  error: ['Could not run', 'error'],
};

const REPRO = {
  confirmed: ['confirmed', 'fail'],
  fails_on_clean_build: ['fails on clean build', 'clean'],
  reproduced: ['reproduced (no reference build)', 'attention'],
  not_reproduced: ['not reproduced', 'clean'],
  repro_error: ['reproduction error', 'error'],
  missing: ['no reproduction', 'error'],
};

const CHECK = {
  passed: ['passed', 'pass'],
  failed: ['failed', 'fail'],
  timed_out: ['timed out', 'fail'],
  not_run: ['not run on the final result', 'attention'],
  unavailable: ['record unavailable', 'attention'],
};

/**
 * The cost in words: what the run cost, or, when a call's cost is not known
 * (`cost_known: false`, such as a Codex writer, which reports no cost), what
 * the run's grants reserved for it, never shown as a price.
 */
export function costText(usage = {}) {
  const cost = Number(usage.cost_microunits) || 0;
  const dollars = `$${(cost / 1e6).toFixed(4)}`;
  if (usage.cost_known !== false) return dollars;
  return cost > 0 ? `cost unknown (reserved up to ${dollars})` : 'cost unknown';
}

/** Usage in words, saying when the numbers are only a known subtotal. */
export function usageText(usage = {}) {
  const text = `${usage.input_tokens || 0} input + ${usage.output_tokens || 0} output tokens, ${costText(usage)}`;
  const retries = usage.retries ? `, ${usage.retries} provider retr${usage.retries === 1 ? 'y' : 'ies'}` : '';
  return usage.complete === false ? `${text}${retries} (known subtotal: some usage was not reported)` : `${text}${retries}`;
}

const asciiLower = (text) => text.replace(/[A-Z]/g, (letter) => letter.toLowerCase());
const asClass = (text) => text.replace(/[ -]/g, '_');

/**
 * Why an area was not covered, in one line: `<class>: <detail>`. This
 * mirrors `NotCovered::reason` (crates/axocoatl-session/src/run_outcome.rs),
 * the one rendering the run summary, its progress lines, JUnit and Keep as
 * PR use: the class is said once (a detail that already starts with it is
 * not prefixed again), and an empty detail leaves the class alone.
 */
export function notCoveredReason(entry = {}) {
  const cls = String(entry.class || 'other');
  let detail = String(entry.detail ?? '').trim();
  const colon = detail.indexOf(':');
  if (colon >= 0 && asClass(asciiLower(detail.slice(0, colon).trim())) === cls) {
    detail = detail.slice(colon + 1).trim();
  } else if (asciiLower(asClass(detail)) === asciiLower(cls)) {
    detail = '';
  }
  return detail ? `${cls}: ${detail}` : cls;
}

export class AxRunOutcome extends HTMLElement {
  static get observedAttributes() { return ['run-id']; }

  #root;
  #status = null;
  #error = '';
  #generation = 0;
  #keepLoaded = null;

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    adopt(this.#root, CSS);
  }

  get runId() { return this.getAttribute('run-id') || ''; }
  set runId(value) { if (value) this.setAttribute('run-id', value); else this.removeAttribute('run-id'); }

  get status() { return this.#status; }
  set status(value) {
    this.#status = value || null;
    this.#error = '';
    this.#render();
  }

  attributeChangedCallback() { if (this.isConnected) void this.refresh(); }
  connectedCallback() { if (this.runId && !this.#status) void this.refresh(); else this.#render(); }

  /** Read the run again. */
  async refresh() {
    const id = this.runId;
    if (!id) return;
    const generation = ++this.#generation;
    try {
      const response = await fetch(`/api/runs/${encodeURIComponent(id)}`);
      const body = await response.json().catch(() => null);
      if (generation !== this.#generation) return;
      if (!response.ok) throw new Error(body?.error || `HTTP ${response.status}`);
      this.#status = body;
      this.#error = '';
    } catch (error) {
      if (generation !== this.#generation) return;
      this.#error = error.message || String(error);
    }
    this.#render();
  }

  #section(title) {
    const section = el('section');
    section.dataset.section = title.toLowerCase().replace(/[^a-z]+/g, '-');
    section.append(el('h3', '', title));
    this.#root.append(section);
    return section;
  }

  #render() {
    this.#root.replaceChildren();
    if (this.#error) {
      this.#root.append(el('div', 'error', `The run could not be read: ${this.#error}`));
      return;
    }
    const status = this.#status;
    if (!status) {
      this.#root.append(el('p', 'muted', 'No run selected.'));
      return;
    }
    const outcome = status.outcome;
    const head = el('div', 'verdict');
    if (!outcome) {
      head.append(el('strong', '', 'Running'), el('span', 'badge', status.state), el('span', 'muted', status.phase || ''));
      this.#root.append(head);
      return;
    }
    const [label, tone] = VERDICT[outcome.verdict] || [outcome.verdict, ''];
    head.append(el('strong', '', label), el('span', `badge ${tone}`, `exit ${outcome.exit_code}`),
      el('span', 'badge', `${outcome.loadout.id}@${outcome.loadout.version}`));
    head.dataset.verdict = outcome.verdict;
    this.#root.append(head);
    if (outcome.error) this.#root.append(el('div', 'error', outcome.error));
    if (outcome.attention?.length) {
      const list = el('ul');
      for (const reason of outcome.attention) list.append(el('li', '', reason));
      this.#section('Needs attention').append(list);
    }
    if (outcome.warnings?.length) {
      const section = this.#section('Warnings');
      for (const warning of outcome.warnings) {
        const box = el('div', 'warning', warning.message);
        box.dataset.code = warning.code;
        section.append(box);
      }
    }
    const checks = this.#section('Checks');
    if (!outcome.checks?.length) checks.append(el('p', 'muted', 'No required checks.'));
    for (const check of outcome.checks || []) {
      const [state, tone] = CHECK[check.state] || [check.state, ''];
      const details = el('details');
      details.dataset.check = check.name;
      const summary = el('summary');
      summary.append(el('span', 'mono', check.name), ' ', el('span', `badge ${tone}`, state));
      if (check.exit_code !== undefined && check.exit_code !== null) summary.append(' ', el('span', 'muted', `exit ${check.exit_code}`));
      if (check.report) {
        summary.append(' ', el('span', 'muted', `report: ${check.report.passed} passed, ${check.report.failed} failed, ${check.report.skipped} skipped, ${check.report.errors} errors`));
      }
      details.append(summary, el('p', 'mono', check.argv.join(' ')));
      if (check.reason) details.append(el('p', 'muted', check.reason));
      if (check.report?.tests?.length) {
        const table = el('table');
        table.innerHTML = '<thead><tr><th>Suite</th><th>Test</th><th>Status</th><th>Message</th></tr></thead><tbody></tbody>';
        for (const test of check.report.tests) {
          const tr = el('tr');
          tr.append(el('td', '', test.suite), el('td', '', test.name), el('td', '', test.status), el('td', 'muted', test.message || ''));
          table.tBodies[0].append(tr);
        }
        details.append(table);
      }
      if (check.stdout_tail || check.stderr_tail) details.append(el('pre', '', `${check.stdout_tail || ''}${check.stderr_tail ? `\n${check.stderr_tail}` : ''}`));
      checks.append(details);
    }
    const review = this.#section('Review');
    if (!outcome.review) review.append(el('p', 'muted', 'No required review.'));
    else {
      const r = outcome.review;
      review.append(el('p', '', `${r.state} after ${r.rounds.length} of ${r.max_rounds} rounds by ${r.reviewer.provider}:${r.reviewer.model}. ${r.reason}`));
      for (const round of r.rounds) {
        const details = el('details');
        details.append(el('summary', '', `Round ${round.round}: ${round.verdict}${round.continued ? ' · sent back to the writer' : ''}`));
        details.append(el('pre', '', round.findings_text || '(no findings)'));
        review.append(details);
      }
    }
    if (outcome.adjudications?.length) {
      const section = this.#section('Adjudications');
      const table = el('table', 'adjudications');
      table.innerHTML = '<thead><tr><th>Finding</th><th>Decision</th><th>Reason</th></tr></thead><tbody></tbody>';
      for (const item of outcome.adjudications) {
        const tr = el('tr', item.decision === 'missing' ? 'missing' : '');
        tr.append(el('td', '', `Round ${item.round} ${item.finding_id}: ${item.finding}`),
          el('td', 'decision', item.decision),
          el('td', '', item.decision === 'missing' ? 'The writer did not answer this finding.' : item.reason));
        table.tBodies[0].append(tr);
      }
      section.append(table);
    }
    if (outcome.findings?.length) {
      const section = this.#section('Findings');
      const list = el('ul');
      for (const finding of outcome.findings) {
        const li = el('li');
        li.dataset.finding = finding.id;
        const repro = finding.repro?.classification;
        const [label, tone] = REPRO[repro] || [finding.source, ''];
        li.append(el('strong', '', `${finding.id} ${finding.title}`), ' ', el('span', `badge ${tone}`, label));
        if (finding.area) li.append(' ', el('span', 'muted', finding.area));
        if (finding.detail) li.append(el('div', 'muted', finding.detail));
        if (finding.repro?.path) li.append(el('div', 'mono', finding.repro.path));
        list.append(li);
      }
      section.append(list);
    }
    if (outcome.not_covered?.length) {
      const section = this.#section('Not covered');
      const list = el('ul');
      for (const entry of outcome.not_covered) {
        const li = el('li', '', `${entry.area}: ${notCoveredReason(entry)}`);
        li.dataset.class = entry.class;
        list.append(li);
      }
      section.append(list);
    }
    if (outcome.notes?.length) {
      const list = el('ul');
      for (const note of outcome.notes) list.append(el('li', 'note', note));
      this.#section('Notes').append(list);
    }
    this.#section('Usage').append(el('p', 'usage', usageText(outcome.usage)));
    const network = outcome.network || {};
    const net = this.#section('Network');
    net.append(el('p', '', `${network.events || 0} recorded events: ${network.allowed_connections || 0} allowed and ${network.refused_connections || 0} refused connections, ${network.route_requests || 0} route requests.`));
    if (network.routes?.length) {
      const list = el('ul');
      for (const [host, count] of network.routes) list.append(el('li', 'mono', `${host}: ${count}`));
      net.append(list);
    }
    const actions = el('div', 'actions');
    const download = el('a', 'download', 'Download record');
    download.href = `/api/runs/${encodeURIComponent(outcome.run_id)}/record`;
    download.setAttribute('download', `${outcome.run_id}.axorecord.jsonl`);
    actions.append(download, el('span', 'muted', `Record ${outcome.run_id}`));
    const keep = document.createElement('ax-keep-pr');
    keep.setAttribute('session-id', outcome.session_id);
    keep.setAttribute('run-id', outcome.run_id);
    keep.setAttribute('verdict', outcome.verdict);
    if (outcome.loadout?.id) keep.setAttribute('loadout', outcome.loadout.id);
    // The run record's latest Keep (the Outcome is written before any Keep).
    // Set once the element is defined, so its own setter shows it.
    if (status.keep) void customElements.whenDefined('ax-keep-pr').then(() => { keep.result = status.keep; });
    keep.addEventListener('keep-pr-result', () => { void this.refresh(); });
    actions.append(keep);
    this.#section('Record').append(actions);
    // Keep as PR belongs to workstream keep; the panel works without it.
    this.#keepLoaded ||= import('./keep-pr.js').catch(() => null);
  }
}

if (!customElements.get('ax-run-outcome')) customElements.define('ax-run-outcome', AxRunOutcome);
