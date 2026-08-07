import { adopt } from './sheets.js';

/**
 * `<ax-compare>` — several attempts at one task, side by side.
 *
 * Two views of the same run, because they answer different questions:
 *
 * **Outcome** is the scoreboard. Measures are rows and attempts are columns —
 * transposed, because comparing four attempts across seven measures reads down a
 * row, where four stacked cards make you hold values in your head. The leftmost
 * column is the baseline everything else is read against; clicking a header
 * re-bases onto it. "Only differences" folds away every row they agree on, since
 * when they mostly match the few rows that differ are the entire comparison.
 *
 * **Route** answers what the scoreboard cannot: when two attempts both pass, the
 * numbers tie and the difference is in how each got there. Steps are aligned
 * against the same baseline, stretches everyone walked identically fold away,
 * and on a divergent row the attempts that matched the baseline recede so the
 * one that went its own way comes forward.
 *
 * Nothing here names a worktree or a branch. What the user asked for was several
 * attempts at a task; the version control underneath is how that is done, and
 * naming it would tie the surface to one implementation of it.
 *
 * @element ax-compare
 *
 * @attr {string} session  Session whose attempts to show.
 * @attr {string} view     'outcome' | 'route'
 *
 * @fires attempt-keep  detail: {index} — keep this attempt's work
 */

const money = (n) => '$' + (n < 0.01 && n > 0 ? n.toFixed(4) : n.toFixed(2));
const secs = (ms) => (ms < 1000 ? `${ms}ms`
  : ms < 60000 ? `${(ms / 1000).toFixed(1)}s`
  : `${Math.floor(ms / 60000)}m ${Math.round((ms % 60000) / 1000)}s`);

/** What an attempt is called: the agent, else the model, else its position. */
const attemptLabel = (a) => a.agent || a.model || `attempt ${a.index + 1}`;

const CSS = `
:host { display: flex; flex-direction: column; min-height: 0; font-family: var(--font-sans); color: var(--text); }
.bar {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-3); border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.seg { display: flex; border: 1px solid var(--border); border-radius: var(--r-md); overflow: hidden; flex-shrink: 0; }
.seg button {
  background: none; border: 0; color: var(--muted); cursor: pointer;
  padding: 4px var(--sp-3); font: var(--fw-medium) var(--fs-xs) var(--font-sans);
  white-space: nowrap;
  transition: background var(--dur-fast) var(--ease), color var(--dur-fast) var(--ease);
}
/* A divider, so two adjacent labels do not read as one word. */
.seg button + button { border-left: 1px solid var(--border); }
.seg button:hover { color: var(--text); }
.seg button[aria-pressed="true"] { background: var(--bg-3); color: var(--text); }
.seg button:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.gist { color: var(--muted-2); font-size: var(--fs-xs); margin-left: auto; }
button.run {
  background: none; border: 1px solid var(--border-strong); color: var(--text);
  border-radius: var(--r-md); padding: 3px var(--sp-3); cursor: pointer;
  font: var(--fw-medium) var(--fs-xs) var(--font-sans); flex-shrink: 0;
}
button.run:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
button.run:disabled { opacity: .45; cursor: not-allowed; }
button.run:focus-visible { outline: none; box-shadow: var(--focus-ring); }
/* The arbiter, stated. It decides which attempts survive, so it should never be
   something the user has to guess at or discover by being surprised. */
.check {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: var(--sp-2) var(--sp-3); border-bottom: 1px solid var(--border);
  font-size: var(--fs-xs); color: var(--muted-2); flex-shrink: 0;
}
.check input {
  flex: 1; min-width: 0; background: var(--bg-3); color: var(--text);
  border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 3px var(--sp-2); font: var(--fs-xs) var(--font-mono);
}
.check input:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.check.unset { color: var(--warn); }
.body { flex: 1; overflow: auto; padding: var(--sp-3); min-height: 0; }
.empty { color: var(--muted-2); font-size: var(--fs-sm); padding: var(--sp-5); text-align: center; }
.empty.loading { opacity: .6; }
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
.run.ghost { margin-left: auto; }
.empty.failed { color: var(--err); }
.retry {
  display: block; margin: var(--sp-2) auto 0; background: none;
  border: 1px solid var(--border); border-radius: var(--r-sm);
  color: var(--text); font: var(--fs-xs) var(--font-sans);
  padding: 2px var(--sp-2); cursor: pointer;
}
.retry:hover { border-color: var(--accent); }
.retry:focus-visible { outline: none; box-shadow: var(--focus-ring); }

table { width: 100%; border-collapse: collapse; font-size: var(--fs-sm); }
th, td { text-align: left; padding: 5px var(--sp-3); border-bottom: 1px solid var(--border); vertical-align: top; }
th { color: var(--muted); font-weight: var(--fw-medium); white-space: nowrap; }
td { font: var(--fs-xs) var(--font-mono); }
.rebase { background: none; border: 0; padding: 0; color: var(--text); font: inherit; cursor: pointer; }
.rebase:hover { color: var(--accent); text-decoration: underline; }
.base { color: var(--muted-2); font-size: var(--fs-xs); text-transform: uppercase; letter-spacing: .06em; }
tr.same { opacity: .38; }
td.win { color: var(--accent); }
td.good { color: var(--ok); }
td.bad { color: var(--err); }
.opt { display: flex; gap: var(--sp-2); align-items: center; margin-top: var(--sp-3); font-size: var(--fs-xs); color: var(--muted); }

tr.diverged { background: rgba(var(--axo-bronze-rgb), .07); }
tr.diverged td.off { box-shadow: inset 2px 0 0 var(--warn); }
tr.diverged td.same-as-base { opacity: .45; }
.n { color: var(--muted-2); width: 1%; white-space: nowrap; }
.k {
  font: var(--fw-bold) 9.5px var(--font-mono); letter-spacing: .06em; text-transform: uppercase;
  border: 1px solid currentColor; border-radius: 3px; padding: 0 4px; margin-right: 5px;
}
.k.read, .k.list, .k.search { color: var(--muted-2); }
.k.edit, .k.write { color: var(--accent); }
.k.run { color: var(--warn); }
.k.other { color: var(--muted); }
.det { color: var(--muted-2); display: block; margin-top: 2px; white-space: pre-wrap; word-break: break-word; }
.none { color: var(--muted-2); }
.failed { color: var(--err); }
.fold td { padding: 3px var(--sp-3); }
.fold button { background: none; border: 0; color: var(--muted-2); font-size: var(--fs-xs); cursor: pointer; padding: 0; }
.fold button:hover { color: var(--accent); }
`;

export class AxCompare extends HTMLElement {
  static get observedAttributes() { return ['session', 'view']; }

  #root; #bar; #body; #gist; #check; #branches;
  /** The project's own check command, as the session records it. */
  #checkCmd = null;
  #busy = false;
  #results = null;
  #error = null;
  #loading = false;
  #alignment = null;
  #base = 0;
  #diffOnly = false;
  #openFolds = new Set();

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="bar">
        <div class="seg">
          <button data-view="outcome">Outcome</button>
          <button data-view="route">Route</button>
        </div>
        <button class="run" data-act="verify" title="Run the project's own checks in every attempt">Run checks</button>
        <button class="run" data-act="judge" title="Rank the survivors and say why">Judge</button>
        <span class="gist"></span>
        <button class="run ghost" data-act="branches" title="Show the git branch and worktree behind each attempt">Show the branches</button>
      </div>
      <div class="check"></div>
      <div class="branches" hidden></div>
      <div class="body"></div>`;
    this.#bar = this.#root.querySelector('.bar');
    this.#body = this.#root.querySelector('.body');
    this.#gist = this.#root.querySelector('.gist');
    this.#check = this.#root.querySelector('.check');
    this.#branches = this.#root.querySelector('.branches');
    this.#bar.addEventListener('click', (e) => {
      const v = e.target.closest('[data-view]');
      if (v) { this.view = v.dataset.view; return; }
      const a = e.target.closest('[data-act]');
      if (!a) return;
      if (a.dataset.act === 'branches') { this.#toggleBranches(); return; }
      this.#run(a.dataset.act);
    });
    adopt(this.#root, CSS);
  }

  get session() { return this.getAttribute('session') || ''; }
  set session(v) { v ? this.setAttribute('session', v) : this.removeAttribute('session'); }

  get view() { return this.getAttribute('view') || 'outcome'; }
  set view(v) { this.setAttribute('view', v); }

  connectedCallback() { if (this.session) this.refresh(); }

  attributeChangedCallback(name, prev, next) {
    if (prev === next) return;
    if (name === 'session') { this.#base = 0; this.#openFolds.clear(); this.refresh(); }
    if (name === 'view') this.render();
  }

  /**
   * Run the project's checks, or the judge, across the attempts.
   *
   * Verify sends no command: the session holds the project's own, so the arbiter
   * is a property of the repository rather than something retyped — and a typo
   * here would decide which attempt survives.
   */
  async #run(act) {
    if (this.#busy || !this.session) return;
    this.#busy = true; this.#syncButtons();
    const s = encodeURIComponent(this.session);
    try {
      if (act === 'verify') {
        const r = await fetch(`/api/sessions/${s}/variants/verify`, {
          method: 'POST', headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ check: this.#checkCmd || '' }),
        });
        if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || 'verify failed');
      } else {
        const r = await fetch(`/api/sessions/${s}/variants/judge`, {
          method: 'POST', headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ task: 'the task these attempts share', provider: 'ollama' }),
        });
        if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || 'judge failed');
      }
      await this.refresh();
    } catch (e) {
      this.dispatchEvent(new CustomEvent('compare-error', {
        detail: { message: String(e.message || e) }, bubbles: true, composed: true,
      }));
    } finally { this.#busy = false; this.#syncButtons(); }
  }

  #syncButtons() {
    for (const b of this.#bar.querySelectorAll('[data-act]')) {
      b.disabled = this.#busy || (this.#results?.lanes || []).length < 2;
    }
    const v = this.#bar.querySelector('[data-act="verify"]');
    if (v) v.textContent = this.#busy ? 'Running…' : 'Run checks';
  }

  /**
   * The git truth, one click away.
   *
   * The surface deliberately says "attempts" and never names a worktree — the
   * decision recorded in the plan is that the product must not spell
   * git-specific nouns, because it has to work with other version control
   * later. But hiding it entirely reads as a toy to the person this is built
   * for, who wants to know it is real git underneath and scriptable. So the
   * branch and worktree of every attempt are exactly one click from the
   * scoreboard, and nowhere else on the surface.
   */
  #toggleBranches() {
    const btn = this.#bar.querySelector('[data-act="branches"]');
    if (!this.#branches.hidden) {
      this.#branches.hidden = true;
      btn.textContent = 'Show the branches';
      return;
    }
    const lanes = this.#results?.lanes || [];
    this.#branches.textContent = '';
    if (!lanes.length) {
      const e = document.createElement('div');
      e.className = 'empty';
      e.textContent = 'No attempts yet — nothing to show.';
      this.#branches.append(e);
    }
    for (const l of lanes) {
      const row = document.createElement('div');
      row.className = 'brow';
      const n = document.createElement('span');
      n.className = 'bn';
      n.textContent = `#${(l.index ?? 0) + 1}`;
      const br = document.createElement('code');
      br.textContent = l.branch || '—';
      const wt = document.createElement('code');
      wt.className = 'wt';
      wt.textContent = l.worktree || '—';
      wt.title = l.worktree || '';
      row.append(n, br, wt);
      this.#branches.append(row);
    }
    this.#branches.hidden = false;
    btn.textContent = 'Hide the branches';
  }

  /** Show the arbiter, and let it be corrected in place. */
  #renderCheck() {
    if ((this.#results?.lanes || []).length < 2) { this.#check.innerHTML = ''; return; }
    this.#check.classList.toggle('unset', !this.#checkCmd);
    this.#check.innerHTML = this.#checkCmd
      ? '<span>Checks</span><input spellcheck="false">'
      : '<span>No check command — attempts cannot be ruled out until you set one</span><input spellcheck="false" placeholder="npm test">';
    const input = this.#check.querySelector('input');
    input.value = this.#checkCmd || '';
    const commit = async () => {
      const next = input.value.trim();
      if (next === (this.#checkCmd || '')) return;
      await fetch(`/api/sessions/${encodeURIComponent(this.session)}/check`, {
        method: 'PUT', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ check_command: next || null }),
      }).catch(() => {});
      this.#checkCmd = next || null;
      this.#renderCheck();
      this.#syncButtons();
    };
    input.addEventListener('blur', commit);
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); input.blur(); } });
  }

  /** Re-read the run. Safe to call whenever the attempts change. */
  async refresh() {
    if (!this.session) return;
    const s = encodeURIComponent(this.session);
    // Say that we are reading before we read, so the pane is never blank in a
    // way that means "there is nothing" while it actually means "not yet".
    if (!this.#results) {
      this.#loading = true;
      this.render();
    }
    try {
      const list = await fetch('/api/sessions').then((r) => r.json());
      this.#checkCmd = (Array.isArray(list) ? list : [])
        .find((x) => x.id === this.session)?.check_command || null;
    } catch { /* keep what we had */ }
    // A failed read and "no attempts yet" are opposite facts. Conflating them
    // showed the invitation to explore a task several ways at exactly the
    // moment the app could not tell whether you already had.
    this.#error = null;
    try {
      const r = await fetch(`/api/sessions/${s}/variants/results`);
      const body = await r.json().catch(() => null);
      if (!r.ok) throw new Error(body?.error || `HTTP ${r.status}`);
      this.#results = body;
    } catch (e) {
      this.#results = null;
      this.#error = String(e.message || e);
    }
    this.#loading = false;
    // The route view is only meaningful once there is something to align, and a
    // failed fetch must not blank a scoreboard that did load.
    try {
      this.#alignment = await fetch(
        `/api/sessions/${s}/variants/trajectories?baseline=${this.#base}`).then((r) => r.json());
    } catch { this.#alignment = null; }
    this.render();
    this.#renderCheck();
    this.#syncButtons();
  }

  render() {
    for (const b of this.#bar.querySelectorAll('[data-view]')) {
      b.setAttribute('aria-pressed', String(b.dataset.view === this.view));
    }
    const attempts = this.#results?.lanes || [];
    if (this.#loading) {
      this.#gist.textContent = '';
      this.#body.innerHTML = '<div class="empty loading">Reading the attempts…</div>';
      return;
    }
    if (this.#error) {
      this.#gist.textContent = '';
      this.#body.textContent = '';
      const box = document.createElement('div');
      box.className = 'empty failed';
      box.textContent = `Could not read the attempts: ${this.#error}`;
      const retry = document.createElement('button');
      retry.className = 'retry';
      retry.textContent = 'Try again';
      retry.addEventListener('click', () => void this.refresh());
      box.append(retry);
      this.#body.append(box);
      return;
    }
    if (attempts.length < 2) {
      this.#gist.textContent = '';
      this.#body.innerHTML = '<div class="empty">Explore a task several ways to compare the attempts.</div>';
      return;
    }
    if (this.view === 'route') this.#renderRoute(); else this.#renderOutcome();
  }

  // ── Outcome ──────────────────────────────────────────────────────────
  #cell(a) {
    const v = (this.#results.verdicts || []).find((x) => x.index === a.index);
    const u = (this.#results.usage || []).find((x) => x.index === a.index);
    const c = (this.#results.judgment?.candidates || []).find((x) => x.index === a.index);
    return {
      outcome: v ? (v.changed_files === 0 ? 'changed nothing' : (v.passed ? 'passed' : 'ruled out')) : 'working',
      rank: c ? `#${c.rank}` : '—',
      files: v ? String(v.changed_files) : '—',
      tests: v?.touched_tests?.length ? 'rewrote tests' : 'untouched',
      duration: u?.duration_ms ? secs(u.duration_ms) : '—',
      cost: u ? money(u.cost_usd) : '—',
      tokens: u ? String(u.input_tokens + u.output_tokens) : '—',
    };
  }

  #renderOutcome() {
    const METRICS = [
      ['outcome', 'Outcome'], ['rank', 'Rank'], ['files', 'Files changed'],
      ['tests', 'Tests'], ['duration', 'Duration'], ['cost', 'Cost'], ['tokens', 'Tokens'],
    ];
    const order = [...this.#results.lanes].sort((a, b) =>
      (a.index === this.#base ? -1 : 0) - (b.index === this.#base ? -1 : 0));
    const cells = order.map((a) => this.#cell(a));

    const head = `<tr><th></th>${order.map((a, i) =>
      `<th><button class="rebase" data-i="${a.index}">${attemptLabel(a)}</button>` +
      `${i === 0 ? '<div class="base">baseline</div>' : ''}</th>`).join('')}</tr>`;

    let hidden = 0;
    const rows = METRICS.map(([k, name]) => {
      const vals = cells.map((c) => c[k]);
      const same = vals.every((v) => v === vals[0]);
      if (same) hidden++;
      if (this.#diffOnly && same) return '';
      return `<tr class="${same ? 'same' : ''}"><th>${name}</th>` + vals.map((v) => {
        let cls = '';
        if (k === 'outcome') cls = v === 'passed' ? 'good' : (v === 'ruled out' || v === 'changed nothing' ? 'bad' : '');
        if (k === 'rank' && v === '#1') cls = 'win';
        if (k === 'tests' && v === 'rewrote tests') cls = 'bad';
        return `<td class="${cls}">${v}</td>`;
      }).join('') + '</tr>';
    }).join('');

    this.#gist.textContent = `${order.length} attempts`;
    this.#body.innerHTML = `<table>${head}${rows}</table>
      <label class="opt"><input type="checkbox" ${this.#diffOnly ? 'checked' : ''}>
        only differences${this.#diffOnly && hidden ? ` — ${hidden} identical row${hidden === 1 ? '' : 's'} hidden` : ''}</label>`;
    this.#body.querySelector('.opt input').onchange = (e) => {
      this.#diffOnly = e.target.checked; this.render();
    };
    for (const b of this.#body.querySelectorAll('.rebase')) {
      b.onclick = () => { this.#base = Number(b.dataset.i); this.#openFolds.clear(); this.refresh(); };
    }
  }

  // ── Route ────────────────────────────────────────────────────────────
  #step(a, rel) {
    const cls = [rel, a?.failed ? 'failed' : ''].filter(Boolean).join(' ');
    if (!a) return `<td class="none ${cls}">—</td>`;
    return `<td class="${cls}"><span class="k ${a.kind}" title="${a.tool}">${a.kind}</span>${a.target}`
      + (a.detail ? `<span class="det">${a.detail}</span>` : '')
      + (a.failed ? '<span class="det">failed</span>' : '') + '</td>';
  }

  #renderRoute() {
    const al = this.#alignment;
    if (!al?.rows?.length) {
      this.#gist.textContent = '';
      this.#body.innerHTML = '<div class="empty">No steps recorded for these attempts yet.</div>';
      return;
    }
    const byIndex = new Map(this.#results.lanes.map((a) => [a.index, a]));
    const head = `<tr><th class="n"></th>${al.lanes.map((i, n) =>
      `<th>${attemptLabel(byIndex.get(i) || { index: i })}` +
      `${n === 0 ? '<div class="base">baseline</div>' : ''}</th>`).join('')}</tr>`;

    // Consecutive agreeing rows fold as one unit: that stretch carries no
    // information about how the attempts differ.
    const groups = [];
    al.rows.forEach((r, i) => {
      const last = groups[groups.length - 1];
      if (r.agree && last?.agree) last.rows.push([i, r]);
      else groups.push({ agree: r.agree, rows: [[i, r]] });
    });

    const body = groups.map((g, gi) => {
      const foldable = g.agree && g.rows.length > 1;
      const open = this.#openFolds.has(gi);
      const fold = foldable
        ? `<tr class="fold"><td class="n"></td><td colspan="${al.lanes.length}">
             <button data-fold="${gi}">${open ? '▾' : '▸'} ${g.rows.length} identical steps</button></td></tr>`
        : '';
      if (foldable && !open) return fold;
      return fold + g.rows.map(([i, r]) =>
        `<tr class="${r.agree ? '' : 'diverged'}"><td class="n">${i + 1}</td>`
        + r.cells.map((c, n) => this.#step(c,
            n === 0 ? '' : ((r.agree || r.matches_baseline?.[n]) ? 'same-as-base' : 'off'))).join('')
        + '</tr>').join('');
    }).join('');

    const diverged = al.rows.length - al.agreed;
    this.#gist.textContent = diverged === 0
      ? `${al.rows.length} steps, identical throughout`
      : `${diverged} of ${al.rows.length} steps diverged`;
    this.#body.innerHTML = `<table>${head}${body}</table>`;
    for (const b of this.#body.querySelectorAll('[data-fold]')) {
      b.onclick = () => {
        const k = Number(b.dataset.fold);
        this.#openFolds.has(k) ? this.#openFolds.delete(k) : this.#openFolds.add(k);
        this.render();
      };
    }
  }
}

customElements.define('ax-compare', AxCompare);
