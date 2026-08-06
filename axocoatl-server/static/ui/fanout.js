import { adopt } from './sheets.js';

/**
 * `<ax-fanout>` — decide what the several attempts at a task will be.
 *
 * The control used to offer a count and nothing else, which can only express one
 * of the three comparisons and the least useful one. Running the same agent on
 * the same model N times measures *variance*, and we measured that: attempts
 * failed in correlated ways, because a model with a blind spot reproduces it
 * every time. What is worth comparing is *difference* — a different model, or a
 * different agent, meaning a different prompt, tools and memory.
 *
 * So an attempt is a row you configure, not a number you increase. The three
 * modes fall out of one mechanism rather than being separate features:
 *
 * | rows differ by | you are comparing |
 * |---|---|
 * | nothing        | variance — rarely what you want |
 * | model          | model diversity |
 * | agent          | design: the prompt, tools and memory |
 *
 * @element ax-fanout
 *
 * @attr {boolean} open     Popover visible.
 * @attr {boolean} enabled  Fan-out is on for the next send.
 *
 * @fires change  detail: {enabled, lanes} — the configuration changed
 *
 * @prop {Array<{agent?: string, model?: string}>} lanes
 */

/** Beyond a handful, local models make this slow rather than parallel. */
const MAX = 100;
const RECOMMENDED = 3;

const CSS = `
:host { display: contents; }
.pop {
  position: absolute; bottom: calc(100% + var(--sp-2)); right: 0;
  width: min(420px, 92vw); z-index: 400;
  background: var(--panel-2); border: 1px solid var(--border-strong);
  border-radius: var(--r-lg); box-shadow: var(--shadow-lg);
  font-family: var(--font-sans); color: var(--text);
  padding: var(--sp-3);
  animation: rise var(--dur-base) var(--ease);
}
@keyframes rise { from { opacity: 0; transform: translateY(4px); } }
:host(:not([open])) .pop { display: none; }
.head { display: flex; align-items: center; gap: var(--sp-2); margin-bottom: var(--sp-2); }
.head h3 { margin: 0; font-size: var(--fs-sm); font-weight: var(--fw-medium); }
/* Fan-out is a mode you switch on, not something a send implies. Without this
   the configured rows are discarded by the enabled guard and no run starts. */
.sw {
  display: inline-flex; align-items: center; gap: var(--sp-2);
  cursor: pointer; user-select: none; font-size: var(--fs-xs); color: var(--muted);
}
.sw input { position: absolute; opacity: 0; width: 0; height: 0; }
.track {
  width: 30px; height: 17px; border-radius: 999px; flex-shrink: 0;
  background: var(--bg-3); border: 1px solid var(--border);
  transition: background var(--dur-fast) var(--ease), border-color var(--dur-fast) var(--ease);
  position: relative;
}
.track::after {
  content: ''; position: absolute; top: 2px; left: 2px;
  width: 11px; height: 11px; border-radius: 50%; background: var(--muted-2);
  transition: transform var(--dur-fast) var(--ease), background var(--dur-fast) var(--ease);
}
:host([enabled]) .track { background: var(--accent); border-color: var(--accent); }
:host([enabled]) .track::after { transform: translateX(13px); background: var(--c-ink, #0A0A0A); }
:host([enabled]) .sw { color: var(--text); }
.sw input:focus-visible + .track { box-shadow: var(--focus-ring); }
/* The rows are configuration for a mode that is off — show that, do not hide it. */
:host(:not([enabled])) .rows, :host(:not([enabled])) .acts { opacity: .45; }
.head .x { margin-left: auto; background: none; border: 0; color: var(--muted); cursor: pointer; font-size: var(--fs-lg); line-height: 1; }
.rows { display: flex; flex-direction: column; gap: var(--sp-1); }
.row {
  display: flex; align-items: center; gap: var(--sp-2);
  padding: var(--sp-1); border-radius: var(--r-md); background: var(--bg-3);
}
.n { color: var(--muted-2); font: var(--fs-xs) var(--font-mono); width: 16px; text-align: center; flex-shrink: 0; }
select {
  flex: 1; min-width: 0; background: var(--bg-2); color: var(--text);
  border: 1px solid var(--border); border-radius: var(--r-sm);
  padding: 3px var(--sp-2); font: var(--fs-xs) var(--font-sans);
}
select:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.drop { background: none; border: 0; color: var(--muted-2); cursor: pointer; padding: 0 var(--sp-1); flex-shrink: 0; }
.drop:hover { color: var(--err); }
.acts { display: flex; align-items: center; gap: var(--sp-2); margin-top: var(--sp-2); }
button.add {
  background: none; border: 1px dashed var(--border-strong); color: var(--muted);
  border-radius: var(--r-md); padding: 4px var(--sp-3); cursor: pointer;
  font: var(--fs-xs) var(--font-sans);
}
button.add:hover { color: var(--text); border-color: var(--accent); }
button.add:disabled { opacity: .4; cursor: not-allowed; }
.note { color: var(--muted-2); font-size: var(--fs-xs); margin-top: var(--sp-2); line-height: 1.45; }
.warn { color: var(--warn); }
`;

export class AxFanout extends HTMLElement {
  static get observedAttributes() { return ['open', 'enabled']; }

  #root; #rows; #note; #add; #sw; #swLabel;
  #agents = [];
  #models = [];
  /** One entry per attempt. `{}` means "the session's own agent and model". */
  #lanes = [{}, {}, {}];

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="pop" role="dialog" aria-label="Configure attempts">
        <div class="head">
          <h3>Explore several ways</h3>
          <label class="sw">
            <input type="checkbox" class="on" />
            <span class="track"></span>
            <span class="sw-label">Off</span>
          </label>
          <button class="x" title="Close">×</button>
        </div>
        <div class="rows"></div>
        <div class="acts"><button class="add">+ Add an attempt</button></div>
        <div class="note"></div>
      </div>`;
    this.#rows = this.#root.querySelector('.rows');
    this.#note = this.#root.querySelector('.note');
    this.#add = this.#root.querySelector('.add');
    this.#sw = this.#root.querySelector('.on');
    this.#swLabel = this.#root.querySelector('.sw-label');
    // The only way fan-out turns on. It was lost when this component was
    // extracted from the shell, which left `enabled` write-only: the rows were
    // configurable, the `lanes` getter returned [] regardless, and no run could
    // ever start. Configuration you cannot act on is not a control.
    this.#sw.onchange = () => { this.enabled = this.#sw.checked; this.#emit(); };
    this.#root.querySelector('.x').onclick = () => { this.open = false; };
    this.#add.onclick = () => {
      if (this.#lanes.length >= MAX) return;
      // A new row copies the last, because the usual next move is to change one
      // thing about it rather than start from nothing.
      this.#lanes.push({ ...this.#lanes[this.#lanes.length - 1] });
      this.#render(); this.#emit();
    };
    adopt(this.#root, CSS);
  }

  get open() { return this.hasAttribute('open'); }
  set open(v) { v ? this.setAttribute('open', '') : this.removeAttribute('open'); }

  get enabled() { return this.hasAttribute('enabled'); }
  set enabled(v) { v ? this.setAttribute('enabled', '') : this.removeAttribute('enabled'); }

  /** The configured attempts. Empty when fan-out is off. */
  get lanes() { return this.enabled ? this.#lanes.map((l) => ({ ...l })) : []; }
  set lanes(v) {
    this.#lanes = Array.isArray(v) && v.length ? v.map((l) => ({ ...l })) : [{}, {}, {}];
    this.#render();
  }

  get count() { return this.enabled ? this.#lanes.length : 1; }

  /** Keep the switch and its label showing the truth of the attribute. */
  #syncSwitch() {
    if (!this.#sw) return;
    this.#sw.checked = this.enabled;
    this.#swLabel.textContent = this.enabled ? `On — ${this.#lanes.length} attempts` : 'Off';
  }

  async connectedCallback() {
    await this.loadChoices();
    this.#render();
  }

  attributeChangedCallback(name) {
    if (name === 'open' && this.open) this.#render();
    if (name === 'enabled') this.#syncSwitch();
  }

  /** Read the agents and models actually installed, so the menus offer reality. */
  async loadChoices() {
    try {
      const a = await fetch('/api/agents').then((r) => r.json());
      const list = Array.isArray(a) ? a : (a?.agents || []);
      this.#agents = list.map((x) => ({ id: x.id, model: x.model }));
    } catch { this.#agents = []; }
    try {
      const m = await fetch('/api/llm/models').then((r) => r.json());
      this.#models = Array.isArray(m) ? m.map((x) => (typeof x === 'string' ? x : x.id || x.name)) : [];
    } catch { this.#models = []; }
    // With no model list from the provider, offer the models the agents already
    // name — better than an empty menu that implies there is no choice.
    if (!this.#models.length) {
      this.#models = [...new Set(this.#agents.map((a) => a.model).filter(Boolean))];
    }
  }

  #emit() {
    this.dispatchEvent(new CustomEvent('change', {
      detail: { enabled: this.enabled, lanes: this.lanes },
      bubbles: true, composed: true,
    }));
  }

  #render() {
    this.#syncSwitch();
    this.#rows.textContent = '';
    this.#lanes.forEach((lane, i) => {
      const row = document.createElement('div');
      row.className = 'row';

      const n = document.createElement('span');
      n.className = 'n';
      n.textContent = String(i + 1);

      const agent = document.createElement('select');
      agent.title = 'Which agent runs this attempt';
      agent.append(new Option('session agent', ''));
      for (const a of this.#agents) {
        agent.append(new Option(a.id, a.id, false, lane.agent === a.id));
      }
      agent.value = lane.agent || '';
      agent.onchange = () => {
        lane.agent = agent.value || undefined;
        this.#render(); this.#emit();
      };

      const model = document.createElement('select');
      model.title = 'Which model this attempt runs on';
      model.append(new Option('agent default', ''));
      for (const m of this.#models) {
        model.append(new Option(m, m, false, lane.model === m));
      }
      model.value = lane.model || '';
      model.onchange = () => {
        lane.model = model.value || undefined;
        this.#render(); this.#emit();
      };

      const drop = document.createElement('button');
      drop.className = 'drop';
      drop.textContent = '×';
      drop.title = 'Remove this attempt';
      // Two is the floor: one attempt is not a comparison, it is a send.
      drop.disabled = this.#lanes.length <= 2;
      drop.onclick = () => {
        this.#lanes.splice(i, 1);
        this.#render(); this.#emit();
      };

      row.append(n, agent, model, drop);
      this.#rows.append(row);
    });

    this.#add.disabled = this.#lanes.length >= MAX;
    this.#note.innerHTML = this.#describe();
  }

  /**
   * Say what this configuration actually compares.
   *
   * Naming it matters: all-identical rows look like the obvious default and are
   * the one setup that measures nothing useful, so the control should say so
   * rather than let someone spend three runs finding out.
   */
  #describe() {
    const n = this.#lanes.length;
    const agents = new Set(this.#lanes.map((l) => l.agent || ''));
    const models = new Set(this.#lanes.map((l) => l.model || ''));
    const slow = n > 8
      ? ` <span class="warn">${n} at once is a lot for local models — expect it to be slow.</span>` : '';

    if (agents.size > 1 && models.size > 1) {
      return `Comparing different agents on different models.${slow}`;
    }
    if (agents.size > 1) {
      return `Comparing <strong>agents</strong> — different prompts, tools and memory on the same model. This is the comparison that says something about design.${slow}`;
    }
    if (models.size > 1) {
      return `Comparing <strong>models</strong> on the same agent.${slow}`;
    }
    return `Every attempt is identical, so this measures <strong>variance</strong> — how much the same setup varies run to run. Attempts tend to fail the same way, so changing the agent or the model usually tells you more.${slow}`;
  }
}

customElements.define('ax-fanout', AxFanout);
