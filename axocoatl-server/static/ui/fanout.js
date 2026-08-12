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
:host([disabled]) .pop { opacity: .72; }
:host([disabled]) .rows, :host([disabled]) .acts { opacity: .35; }
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
.model-field { flex: 1; min-width: 0; }
.model-field select { width: 100%; }
.model-state {
  display: flex; align-items: center; gap: var(--sp-1); margin-top: 2px;
  color: var(--muted-2); font-size: 10px; line-height: 1.25;
}
.model-state.error { color: var(--warn); }
.model-state button {
  border: 0; padding: 0; background: none; color: inherit; cursor: pointer;
  font: inherit; text-decoration: underline;
}
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
.choice-status { color: var(--warn); font-size: var(--fs-xs); margin-top: var(--sp-2); line-height: 1.4; }
.choice-status[hidden] { display: none; }
.warn { color: var(--warn); }
`;

export class AxFanout extends HTMLElement {
  static get observedAttributes() { return ['open', 'enabled', 'disabled']; }

  #root; #rows; #note; #choiceStatus; #add; #sw; #swLabel;
  #agents = [];
  /** Agent id -> {models, error}. Both successes and failures are cached. */
  #modelsByAgent = new Map();
  /** Agent id -> in-flight discovery promise, so duplicate rows share one read. */
  #modelLoads = new Map();
  #choicesError = '';
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
        <div class="choice-status" role="status" hidden></div>
      </div>`;
    this.#rows = this.#root.querySelector('.rows');
    this.#note = this.#root.querySelector('.note');
    this.#choiceStatus = this.#root.querySelector('.choice-status');
    this.#add = this.#root.querySelector('.add');
    this.#sw = this.#root.querySelector('.on');
    this.#swLabel = this.#root.querySelector('.sw-label');
    // The only way fan-out turns on. It was lost when this component was
    // extracted from the shell, which left `enabled` write-only: the rows were
    // configurable, the `lanes` getter returned [] regardless, and no run could
    // ever start. Configuration you cannot act on is not a control.
    this.#sw.onchange = () => {
      if (this.disabled) return;
      this.enabled = this.#sw.checked;
      this.#emit();
    };
    this.#root.querySelector('.x').onclick = () => { this.open = false; };
    this.#add.onclick = () => {
      if (this.disabled || this.#lanes.length >= MAX) return;
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

  get disabled() { return this.hasAttribute('disabled'); }
  set disabled(v) {
    v ? this.setAttribute('disabled', '') : this.removeAttribute('disabled');
    if (v) this.enabled = false;
  }

  /** The configured attempts. Empty when fan-out is off. */
  get lanes() { return this.enabled && !this.disabled ? this.#lanes.map((l) => ({ ...l })) : []; }
  set lanes(v) {
    this.#lanes = Array.isArray(v) && v.length ? v.map((l) => ({ ...l })) : [{}, {}, {}];
    this.#render();
  }

  get count() { return this.enabled && !this.disabled ? this.#lanes.length : 1; }

  /** Keep the switch and its label showing the truth of the attribute. */
  #syncSwitch() {
    if (!this.#sw) return;
    this.#sw.checked = this.enabled;
    this.#sw.disabled = this.disabled;
    this.#swLabel.textContent = this.disabled
      ? 'Unavailable in multi-agent sessions'
      : (this.enabled ? `On — ${this.#lanes.length} attempts` : 'Off');
  }

  async connectedCallback() {
    await this.loadChoices();
    this.#render();
  }

  attributeChangedCallback(name) {
    if (name === 'open' && this.open) this.#render();
    if (name === 'enabled' || name === 'disabled') this.#syncSwitch();
  }

  /** Read the installed agents, preserving what resolves each lane's models. */
  async loadChoices() {
    this.#choicesError = '';
    try {
      const response = await fetch('/api/agents');
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const a = await response.json();
      const list = Array.isArray(a) ? a : (a?.agents || []);
      this.#agents = list
        .filter((x) => x?.id)
        .map((x) => ({ id: x.id, provider: x.provider || '', model: x.model || '' }));
    } catch {
      this.#agents = [];
      this.#choicesError = 'Could not load configured agents. Existing attempt choices are preserved.';
    }

    // Restore externally-supplied lanes without issuing one global model query.
    // Only agents a lane actually selected need discovery.
    const selected = [...new Set(this.#lanes.map((lane) => lane.agent).filter(Boolean))];
    await Promise.all(selected.map((agentId) => this.#loadModels(agentId)));
  }

  /** Discover one agent's provider-scoped models and cache the result. */
  #loadModels(agentId) {
    if (!agentId) return Promise.resolve({ models: [], error: '' });
    if (this.#modelsByAgent.has(agentId)) {
      return Promise.resolve(this.#modelsByAgent.get(agentId));
    }
    if (this.#modelLoads.has(agentId)) return this.#modelLoads.get(agentId);

    const agent = this.#agents.find((a) => a.id === agentId);
    const fallback = agent?.model ? [agent.model] : [];
    const request = (async () => {
      let result;
      try {
        const response = await fetch(`/api/llm/models?agent=${encodeURIComponent(agentId)}`);
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        const body = await response.json();
        if (!Array.isArray(body)) throw new Error('invalid model list');
        const discovered = body
          .map((x) => (typeof x === 'string' ? x : x?.id || x?.name))
          .filter(Boolean);
        const models = [...new Set([...fallback, ...discovered])];
        result = { models, error: '' };
      } catch {
        result = {
          models: fallback,
          error: agent?.model
            ? `Could not load ${agentId} models; using its configured default.`
            : `Could not load ${agentId} models; its agent default still applies.`,
        };
      }
      this.#modelsByAgent.set(agentId, result);
      return result;
    })().finally(() => this.#modelLoads.delete(agentId));
    this.#modelLoads.set(agentId, request);
    return request;
  }

  /** Start discovery without making rendering wait on the network. */
  #ensureModels(agentId) {
    if (!agentId || this.#modelsByAgent.has(agentId) || this.#modelLoads.has(agentId)) return;
    void this.#loadModels(agentId).then(() => this.#render());
  }

  #emit() {
    this.dispatchEvent(new CustomEvent('change', {
      detail: { enabled: this.enabled, lanes: this.lanes },
      bubbles: true, composed: true,
    }));
  }

  #render() {
    this.#syncSwitch();
    this.#choiceStatus.textContent = this.#choicesError;
    this.#choiceStatus.hidden = !this.#choicesError;
    this.#rows.textContent = '';
    this.#lanes.forEach((lane, i) => {
      this.#ensureModels(lane.agent);
      const row = document.createElement('div');
      row.className = 'row';

      const n = document.createElement('span');
      n.className = 'n';
      n.textContent = String(i + 1);

      const agent = document.createElement('select');
      agent.title = 'Which agent runs this attempt';
      agent.append(new Option('session agent', ''));
      for (const a of this.#agents) {
        const label = a.provider ? `${a.id} · ${a.provider}` : a.id;
        agent.append(new Option(label, a.id, false, lane.agent === a.id));
      }
      // A failed agents read must not silently erase an externally supplied id.
      if (lane.agent && !this.#agents.some((a) => a.id === lane.agent)) {
        agent.append(new Option(`${lane.agent} · selected`, lane.agent, true, true));
      }
      agent.value = lane.agent || '';
      agent.disabled = this.disabled;
      agent.onchange = () => {
        lane.agent = agent.value || undefined;
        // A model belongs to its provider. Switching agents returns this lane to
        // the newly-selected agent's configured default instead of carrying an
        // incompatible override across providers.
        lane.model = undefined;
        this.#render(); this.#emit();
      };

      const selectedAgent = this.#agents.find((a) => a.id === lane.agent);
      const modelField = document.createElement('div');
      modelField.className = 'model-field';
      const model = document.createElement('select');
      model.title = selectedAgent?.provider
        ? `Which model this attempt runs on (${selectedAgent.provider})`
        : 'Which model this attempt runs on';
      const defaultLabel = selectedAgent?.model
        ? `agent default — ${selectedAgent.model}`
        : (lane.agent ? 'agent default' : 'session agent default');
      model.append(new Option(defaultLabel, ''));

      const modelState = lane.agent ? this.#modelsByAgent.get(lane.agent) : null;
      const choices = [...(modelState?.models || [])];
      if (selectedAgent?.model && !choices.includes(selectedAgent.model)) {
        choices.unshift(selectedAgent.model);
      }
      // Preserve an existing override even if discovery is unavailable or the
      // provider stopped advertising it. The emitted contract remains lossless.
      if (lane.model && !choices.includes(lane.model)) choices.push(lane.model);
      for (const m of choices) {
        model.append(new Option(m, m, false, lane.model === m));
      }
      model.value = lane.model || '';
      model.disabled = this.disabled;
      model.onchange = () => {
        lane.model = model.value || undefined;
        this.#render(); this.#emit();
      };
      modelField.append(model);

      if (lane.agent && this.#modelLoads.has(lane.agent)) {
        const state = document.createElement('div');
        state.className = 'model-state';
        state.textContent = 'Loading models…';
        modelField.append(state);
      } else if (modelState?.error) {
        const state = document.createElement('div');
        state.className = 'model-state error';
        const message = document.createElement('span');
        message.textContent = modelState.error;
        const retry = document.createElement('button');
        retry.type = 'button';
        retry.textContent = 'Retry';
        retry.onclick = () => {
          this.#modelsByAgent.delete(lane.agent);
          this.#render();
        };
        state.append(message, retry);
        modelField.append(state);
      }

      const drop = document.createElement('button');
      drop.className = 'drop';
      drop.textContent = '×';
      drop.title = 'Remove this attempt';
      // Two is the floor: one attempt is not a comparison, it is a send.
      drop.disabled = this.disabled || this.#lanes.length <= 2;
      drop.onclick = () => {
        this.#lanes.splice(i, 1);
        this.#render(); this.#emit();
      };

      row.append(n, agent, modelField, drop);
      this.#rows.append(row);
    });

    this.#add.disabled = this.disabled || this.#lanes.length >= MAX;
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
    const models = new Set(this.#lanes.map((l) => {
      const agent = this.#agents.find((a) => a.id === l.agent);
      return l.model || agent?.model || '';
    }));
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
