import { adopt } from './sheets.js';
import { renderCoordinatorApproval, validateCoordinatorApproval } from './session-team-delegation.js';

const clone = value => structuredClone(value);
const node = (tag, text, attrs = {}) => { const element = document.createElement(tag); if (text != null) element.textContent = text; for (const [key, value] of Object.entries(attrs)) element.setAttribute(key, value); return element; };
const mayChange = scope => scope == null ? 'any file' : scope.length ? scope.join(', ') : 'nothing (read-only; write tools withheld)';
const pathLines = text => text.split('\n').map(line => line.trim()).filter(Boolean);
// A required check is an argv; one typed line runs as `sh -c <line>`.
const commandText = argv => argv.length === 3 && argv[0] === 'sh' && argv[1] === '-c' ? argv[2] : argv.join(' ');
const sameArgv = (a, b) => JSON.stringify(a) === JSON.stringify(b);
// `writes` is the whole value: null lets the Agent change any file, [] makes it
// a read-only helper. The chosen mode is remembered so an empty path list stays
// "Only these paths" until the person types one.
function renderWrites(host, panel, slot, update) {
  const label = node('label', 'May change'), mode = node('select', null, { class: 'writes', 'aria-label': 'May change' });
  for (const [value, text] of [['any', 'Any file'], ['paths', 'Only these paths'], ['none', 'Nothing – read-only helper']]) mode.append(node('option', text, { value }));
  mode.value = host.writesModes[slot.slot_id] || (slot.writes == null ? 'any' : slot.writes.length ? 'paths' : 'none');
  const pathsText = 'Paths it may change, one per line (for example lib/ or docs/*.md)', pathsLabel = node('label', pathsText), paths = node('textarea', null, { 'aria-label': pathsText });
  paths.value = (slot.writes || []).join('\n'); pathsLabel.hidden = mode.value !== 'paths'; mode.disabled = paths.disabled = host.mode !== 'edit' || host.busy;
  mode.onchange = () => { host.writesModes[slot.slot_id] = mode.value; pathsLabel.hidden = mode.value !== 'paths'; update('writes', mode.value === 'any' ? null : mode.value === 'none' ? [] : pathLines(paths.value)); };
  paths.oninput = () => update('writes', pathLines(paths.value));
  label.append(mode); pathsLabel.append(paths); panel.append(label, pathsLabel, node('p', 'Only these paths: its file tools refuse other paths, and every change it makes is checked when it finishes; a change outside them fails its work and is kept for review. With a shell this check is review evidence, not confinement. Nothing: it is not offered the file-writing tools, and its shell runs where it cannot change the repository, or does not run where the runtime cannot enforce that.'));
}
const CSS = `
:host{display:block;color:var(--text);font:var(--fs-sm)/1.5 var(--font-sans)}*{box-sizing:border-box}button,input,select,textarea{font:inherit;color:inherit}button{border:1px solid var(--border);border-radius:var(--r-md);padding:6px 10px;background:var(--panel-2);cursor:pointer}button:disabled{opacity:.45;cursor:default}button:focus-visible,input:focus-visible,select:focus-visible,textarea:focus-visible{outline:2px solid var(--accent);outline-offset:2px}.open{margin:4px 0}.primary{background:var(--accent);color:var(--bg-0,#111)}dialog{width:min(1120px,96vw);max-height:92vh;padding:0;border:1px solid var(--border);border-radius:var(--r-lg);background:var(--panel);color:var(--text)}dialog[open]{display:flex;flex-direction:column;height:min(900px,92vh)}.head,.toolbar,.foot,.help{flex-shrink:0}.status{margin:0;flex-shrink:0}.review{flex-shrink:1;min-height:0}.body{flex:1;min-height:0;overflow:hidden}.canvas{min-height:0}ax-lattice{min-height:0}.panel{max-height:none;min-height:0}dialog::backdrop{background:#0008}.head,.foot,.toolbar{display:flex;align-items:center;flex-wrap:wrap;gap:8px;padding:12px;border-bottom:1px solid var(--border)}.head h2{margin:0;flex:1;font-size:var(--fs-lg)}.body{display:grid;grid-template-columns:minmax(260px,1fr) minmax(260px,340px);min-height:420px}.canvas{min-width:0;position:relative;min-height:420px}ax-lattice{display:block;width:100%;height:100%;min-height:420px;--ax-bg:var(--bg-2);--ax-fg:var(--text);--ax-grid:var(--border);--ax-node-bg:var(--panel);--ax-node-fg:var(--text);--ax-node-border:var(--border);--ax-accent:var(--accent)}ax-node{min-width:160px;padding:12px}.node-model{color:var(--muted);font-size:var(--fs-xs);max-width:220px;overflow-wrap:anywhere}.panel{padding:14px;border-left:1px solid var(--border);max-height:58vh;overflow:auto}.panel h3{margin:0 0 8px}.panel label{display:flex;flex-direction:column;gap:4px;margin:9px 0}.panel label[hidden]{display:none}.panel input,.panel textarea,.panel select{width:100%;background:var(--bg-2);border:1px solid var(--border);border-radius:var(--r-sm);padding:7px}.panel textarea{min-height:90px;resize:vertical}.panel label.check{flex-direction:row}.panel input[type=checkbox]{width:auto}.help{color:var(--muted);margin:0;padding:8px 12px}.status{padding:8px 12px;white-space:pre-wrap;overflow-wrap:anywhere}.status.error{color:var(--err)}.review{margin:0;padding:12px 30px;max-height:170px;overflow:auto}.foot{border-top:1px solid var(--border);border-bottom:0;justify-content:flex-end}.checks{flex-shrink:0;padding:8px 12px;border-top:1px solid var(--border)}.checks summary{cursor:pointer}.checks p{margin:6px 0;color:var(--muted)}.checks label{display:flex;flex-direction:column;gap:4px}.checks textarea{width:100%;min-height:60px;resize:vertical;background:var(--bg-2);border:1px solid var(--border);border-radius:var(--r-sm);padding:7px;font-family:var(--font-mono,monospace)}.checks .add-detected{margin-top:6px;overflow-wrap:anywhere;text-align:left}.toolbar [aria-pressed=true]{border-color:var(--accent)}.body{flex:1;min-height:0;overflow:hidden}.canvas,ax-lattice{min-height:0}.panel{max-height:none;min-height:0}@media(max-width:720px){.body{grid-template-columns:1fr;grid-template-rows:150px minmax(110px,1fr)}.canvas,ax-lattice{min-height:0;height:150px}.panel{max-height:none;border-left:0;border-top:1px solid var(--border)}dialog{max-height:96vh}.review{max-height:100px;padding:8px 24px}.status{padding:6px 12px}.head h2{font-size:16px}.head,.toolbar,.foot{padding:8px}.help{font-size:var(--fs-xs)}}`;

export class AxSessionTeam extends HTMLElement {
  static observedAttributes = ['session'];
  constructor() {
    super(); this.attachShadow({ mode: 'open' });
    this.shadowRoot.innerHTML = `<button class="open" type="button">Team and budget</button><dialog aria-labelledby="team-title"><div class="head"><h2 id="team-title">Session team</h2><button class="close" aria-label="Close team review">Close</button></div><p class="help">Changes apply to future turns in this Session. Current work and saved History stay as they are.</p><div class="toolbar"><button data-mode="view" aria-pressed="true">View</button><button data-mode="edit" aria-pressed="false">Edit</button><button data-action="add">Add Agent</button><button data-action="undo">Undo</button><button data-action="redo">Redo</button><button data-action="copy">Copy</button><button data-action="paste">Paste</button><button data-action="layout">Arrange</button></div><div class="body"><div class="canvas"><ax-lattice mode="view" background="dots" snap="20" max-zoom="1.5" aria-label="Future Session team"></ax-lattice></div><form class="panel" novalidate></form></div><details class="checks"><summary>Required checks</summary><p>Commands the host runs in this Session's repository after the required Agents of every turn finish. A failure leaves the turn needing attention; the turn completes only when every check passes and leaves the repository unchanged. They run on the allowance of the first required Agent with the bash tool.</p><label>Commands, one per line<textarea class="check-lines" aria-label="Required checks, one command per line"></textarea></label><button type="button" class="add-detected" hidden></button></details><p class="status" role="status"></p><ul class="review" hidden></ul><div class="foot"><button class="refresh">Refresh</button><button class="cancel">Cancel changes</button><button class="preview">Preview changes</button><button class="apply primary" disabled>Apply to this Session</button></div></dialog>`;
    void adopt(this.shadowRoot, CSS, ['/ui/tokens.css']);
    this.q('.open').onclick = () => void this.open(); this.q('.close').onclick = () => this.close();
    this.q('dialog').addEventListener('cancel', event => { event.preventDefault(); this.close(); });
    this.q('.refresh').onclick = () => void this.load(); this.q('.cancel').onclick = () => void this.cancel();
    this.q('.preview').onclick = () => void this.preview(); this.q('.apply').onclick = () => void this.apply();
    this.q('.panel').onsubmit = event => event.preventDefault();
    this.q('.check-lines').oninput = () => this.setChecks(pathLines(this.q('.check-lines').value).map(line => ['sh', '-c', line]), false);
    this.q('.add-detected').onclick = () => { if (this.view?.suggested_check) this.setChecks([...(this.draft?.required_checks || []), clone(this.view.suggested_check)], true); };
    for (const button of this.shadowRoot.querySelectorAll('[data-mode]')) button.onclick = () => this.setMode(button.dataset.mode);
    for (const button of this.shadowRoot.querySelectorAll('[data-action]')) button.onclick = () => this.action(button.dataset.action);
    const lattice = this.q('ax-lattice');
    lattice.addEventListener('selection-change', event => { this.selected = event.detail.ids?.[0] || null; this.renderPanel(); });
    lattice.addEventListener('history-change', () => queueMicrotask(() => this.syncGraph()));
    lattice.addEventListener('keydown', event => { if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'z') { event.preventDefault(); event.stopImmediatePropagation(); this.action(event.shiftKey ? 'redo' : 'undo'); } }, true);
    this.busy = false; this.mode = 'view'; this.undo = []; this.redo = []; this.writesModes = {};
  }
  q(selector) { return this.shadowRoot.querySelector(selector); }
  attributeChangedCallback(name, oldValue, value) { if (oldValue !== value) { this.epoch = (this.epoch || 0) + 1; this.q('dialog').close(); this.draft = null; this.review = null; } }
  get session() { return this.getAttribute('session') || ''; }
  get storageKey() { return `axocoatl-session-team-apply:${this.session}`; }
  async request(suffix = '', body) {
    const response = await fetch(`/api/sessions/${encodeURIComponent(this.session)}/team${suffix}`, body === undefined ? {} : { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    const result = await response.json(); if (!response.ok) { const error = new Error(result.error || `Request failed (${response.status})`); error.status = response.status; throw error; } return result;
  }
  status(message, error = false) { this.q('.status').textContent = message; this.q('.status').classList.toggle('error', error); }
  async open() { if (!this.session) return; this.q('dialog').showModal(); await import('/lattice/index.js'); await this.load(); }
  close() { if (!this.busy) { this.q('dialog').close(); this.q('.open').focus(); } }
  async load() {
    if (this.busy) return; this.busy = true; const epoch = this.epoch; this.controls(); this.status('Loading the current Session team…');
    try {
      const view = await this.request(); if (epoch !== this.epoch) return;
      this.dispatchEvent(new CustomEvent('session-team-loaded', {bubbles:true, composed:true, detail:{session_id:this.session,team:view}}));
      this.view = view;
      if (view.history_version === 'legacy_v1') {
        this.draft = null; this.review = null; this.mode = 'view'; this.selected = null;
        this.q('ax-lattice').replaceChildren(); this.q('ax-lattice').setAttribute('mode', 'view'); this.q('.panel').replaceChildren(); this.q('.review').hidden = true; this.q('.checks').hidden = true;
        this.status('This Session uses legacy history. Native team editing and budget approvals are unavailable; its existing Session workflow remains available.');
        return;
      }
      this.q('.checks').hidden = false;
      this.draft = { command_id: `team-edit-${crypto.randomUUID()}`, expected_configuration_revision: view.configuration_revision, slots: clone(view.slots), dependencies: clone(view.dependencies), layout: clone(view.layout), required_checks: clone(view.required_checks || []) };
      this.undo = []; this.redo = []; this.review = null; this.writesModes = {}; this.selected = this.draft.slots[0]?.slot_id || null;
      const pending = JSON.parse(localStorage.getItem(this.storageKey) || 'null');
      if (pending?.edit && typeof pending.review_digest === 'string') { this.draft = pending.edit; this.review = pending; this.status('An Apply reply was not received. Retry that exact Apply to obtain its saved result.'); this.mode = 'view'; }
      else this.status(view.approved ? `Saved Session configuration ${view.configuration_revision}. Select Edit to make changes.` : 'Approve explicit budgets before sending the first request. Select Edit, then choose an Agent to enter its limits.');
      this.renderGraph(); this.setMode(this.mode);
    } catch (error) { if (epoch === this.epoch) this.status(error.message, true); }
    finally { this.busy = false; this.controls(); }
  }
  setMode(mode) { if (this.busy) return; if (mode === 'edit' && localStorage.getItem(this.storageKey)) { this.status('Resolve the pending Apply before editing this team.', true); return; } this.mode = mode; this.q('ax-lattice').setAttribute('mode', mode); for (const button of this.shadowRoot.querySelectorAll('[data-mode]')) button.setAttribute('aria-pressed', String(button.dataset.mode === mode)); for (const item of this.q('ax-lattice').querySelectorAll('ax-node')) { if (mode === 'edit') item.removeAttribute('draggable'); else item.setAttribute('draggable', 'false'); } this.renderPanel(); this.controls(); }
  controls() {
    for (const button of this.shadowRoot.querySelectorAll('button')) button.disabled = this.busy;
    for (const input of this.q('.panel').querySelectorAll('input,select,textarea')) input.disabled = this.busy || this.mode !== 'edit';
    for (const button of this.shadowRoot.querySelectorAll('[data-action]')) button.disabled ||= this.mode !== 'edit' || !this.draft;
    this.q('[data-action="undo"]').disabled ||= !this.undo.length; this.q('[data-action="redo"]').disabled ||= !this.redo.length;
    this.q('[data-mode="edit"]').disabled ||= this.view?.history_version === 'legacy_v1';
    this.q('.cancel').disabled ||= !this.draft;
    this.q('.preview').disabled ||= !this.draft || this.mode !== 'edit'; this.q('.apply').disabled ||= !this.review;
    const locked = this.busy || this.mode !== 'edit' || !this.draft; this.q('.check-lines').disabled = locked; this.q('.add-detected').disabled = locked;
  }
  // The detected command is only offered; it runs only after an Apply that
  // includes it. `rewrite` refreshes the typed lines as well.
  renderChecks(rewrite = true) {
    const checks = this.draft?.required_checks || [], suggested = this.view?.suggested_check, button = this.q('.add-detected');
    this.q('.checks summary').textContent = checks.length ? `Required checks (${checks.length})` : 'Required checks: none';
    if (rewrite) this.q('.check-lines').value = checks.map(commandText).join('\n');
    button.hidden = !Array.isArray(suggested) || checks.some(check => sameArgv(check, suggested));
    if (!button.hidden) button.textContent = `Add detected: ${commandText(suggested)}`;
  }
  setChecks(checks, rewrite) { if (this.busy || this.mode !== 'edit' || !this.draft) return; const next = clone(this.draft); next.required_checks = checks; this.changed(next); this.renderChecks(rewrite); }
  changed(next) { if (JSON.stringify(next) === JSON.stringify(this.draft)) return; this.undo.push(clone(this.draft)); this.redo = []; this.draft = next; this.review = null; this.q('.review').hidden = true; this.controls(); }
  renderGraph() {
    if (!this.draft) return; const lattice = this.q('ax-lattice'), selected = this.selected; this.rendering = true; lattice.replaceChildren();
    this.draft.slots.forEach((slot, index) => {
      // Lattice handles dragging itself. HTML draggable="true" cancels its pointer gesture.
      const element = node('ax-node', null, { id: slot.slot_id, ...(this.mode === 'edit' ? {} : { draggable: 'false' }) }); element.dataset.slot = JSON.stringify(slot);
      const position = this.draft.layout.find(position => position.slot_id === slot.slot_id) || { x: index * 240, y: 40 };
      element.setAttribute('data-x', position.x); element.setAttribute('data-y', position.y);
      element.append(node('strong', slot.name), node('div', `${slot.provider} · ${slot.model}`, { class: 'node-model' }), node('ax-handle', null, { type: 'target', 'handle-id': 'in', position: 'left' }), node('ax-handle', null, { type: 'source', 'handle-id': 'out', position: 'right' })); lattice.append(element);
    });
    for (const [index, edge] of this.draft.dependencies.entries()) lattice.append(node('ax-edge', null, { id: `team-edge-${index}`, from: `${edge.parent}:out`, to: `${edge.child}:in` }));
    lattice.clearHistory(); this.rendering = false; this.renderChecks();
    if (this.draft.slots.some(slot => slot.slot_id === selected)) lattice.setSelection([selected]);
    this.renderPanel(); requestAnimationFrame(() => lattice.fitView({ padding: 36 }));
  }
  syncGraph() {
    if (this.rendering || this.mode !== 'edit' || !this.draft || this.busy) return;
    const lattice = this.q('ax-lattice'), next = clone(this.draft); next.slots = []; next.layout = [];
    for (const element of lattice.querySelectorAll('ax-node')) {
      const slot = JSON.parse(element.dataset.slot); if (slot.slot_id !== element.id) { slot.source_slot_id = slot.source_slot_id || slot.slot_id; slot.slot_id = element.id; slot.reset_history = true; element.dataset.slot = JSON.stringify(slot); }
      next.slots.push(slot); next.layout.push({ slot_id: slot.slot_id, x: element.x, y: element.y });
    }
    next.dependencies = [...lattice.querySelectorAll('ax-edge')].map(edge => ({ parent: edge.getAttribute('from').replace(/:out$/, ''), child: edge.getAttribute('to').replace(/:in$/, '') }));
    this.changed(next); this.renderPanel();
  }
  action(action) {
    if (this.busy || this.mode !== 'edit' || !this.draft) return; const lattice = this.q('ax-lattice');
    if (action === 'undo' || action === 'redo') { const from = action === 'undo' ? this.undo : this.redo, to = action === 'undo' ? this.redo : this.undo; if (!from.length) return; to.push(clone(this.draft)); this.draft = from.pop(); this.review = null; this.q('.review').hidden = true; this.renderGraph(); this.controls(); return; }
    if (action === 'add') { const template = this.view.templates[0]; if (!template) return; const slot = clone(template); slot.slot_id = `slot-${crypto.randomUUID()}`; const next = clone(this.draft); next.slots.push(slot); this.changed(next); this.selected = slot.slot_id; this.renderGraph(); return; }
    if (action === 'layout') lattice.autoLayout({ direction: 'LR' }); else lattice[action]?.();
    queueMicrotask(() => this.syncGraph());
  }
  renderPanel() {
    const panel = this.q('.panel'); panel.replaceChildren(); const slot = this.draft?.slots.find(slot => slot.slot_id === this.selected);
    if (!slot) { panel.append(node('p', 'Select an Agent in the graph to inspect its configuration.')); return; }
    panel.append(node('h3', slot.name));
    const update = (key, value) => { const next = clone(this.draft), changedSlot = next.slots.find(item => item.slot_id === slot.slot_id); changedSlot[key] = value; this.changed(next); const element = [...this.q('ax-lattice').querySelectorAll('ax-node')].find(item => item.id === slot.slot_id); if (element) { element.dataset.slot = JSON.stringify(changedSlot); element.querySelector('strong').textContent = changedSlot.name; element.querySelector('.node-model').textContent = `${changedSlot.provider} · ${changedSlot.model}`; } };
    const field = (label, key, type = 'text', value = slot[key]) => { const wrapper = node('label', label), input = node(type === 'textarea' ? 'textarea' : 'input', null, { type: type === 'textarea' ? 'text' : type }); input.value = value ?? ''; input.disabled = this.mode !== 'edit' || this.busy; input.oninput = () => update(key, type === 'number' ? (input.value === '' ? null : Number(input.value)) : (key === 'instructions' ? input.value || null : input.value)); wrapper.append(input); panel.append(wrapper); return input; };
    const templateLabel = node('label', 'Agent template'), templates = node('select'); templates.append(node('option', 'Keep this Session definition', { value: '' }));
    for (const template of this.view.templates) templates.append(node('option', `${template.name} · ${template.provider}`, { value: template.template_id })); templates.value = slot.template_id || ''; templates.disabled = this.mode !== 'edit'; templates.onchange = () => { const selected = this.view.templates.find(item => item.template_id === templates.value); if (!selected) return; const next = clone(this.draft), index = next.slots.findIndex(item => item.slot_id === slot.slot_id); next.slots[index] = { ...clone(selected), slot_id: slot.slot_id, limits: slot.limits, expires_at_ms: slot.expires_at_ms, reset_history: true }; delete this.writesModes[slot.slot_id]; this.changed(next); this.renderGraph(); }; templateLabel.append(templates); panel.append(templateLabel);
    field('Name', 'name'); field('Provider', 'provider'); field('Model', 'model'); field('Instructions', 'instructions', 'textarea'); const output = field('Maximum output tokens per request', 'max_output_tokens', 'number'); output.min = '1'; output.step = '1';
    renderWrites(this, panel, slot, update);
    panel.append(node('p', 'Limits are explicit. Axocoatl stops admitting work when its allowance is exhausted. Zero cost is valid for a local model.'));
    panel.append(node('p', 'The total token limit is an admission allowance: each call reserves a conservative token bound, and unused reservations are not returned. Actual token usage is reported separately.'));
    for (const [key, label, min] of [['activations', 'Activation limit', 1], ['invocations', 'Provider and tool invocation limit', 1], ['tokens', 'Total token limit', 1], ['cost_microunits', 'Cost limit (USD)', 0]]) {
      const wrapper = node('label', label), input = node('input', null, { type: 'number', min, step: key === 'cost_microunits' ? '0.000001' : '1', required: '' }); input.value = slot.limits?.[key] == null ? '' : key === 'cost_microunits' ? slot.limits[key] / 1e6 : slot.limits[key]; input.disabled = this.mode !== 'edit'; input.oninput = () => { const limits = { ...this.draft.slots.find(item => item.slot_id === slot.slot_id)?.limits }; limits[key] = input.value === '' ? null : key === 'cost_microunits' ? Math.round(Number(input.value) * 1e6) : Number(input.value); update('limits', limits); slot.limits = limits; }; wrapper.append(input); panel.append(wrapper);
    }
    const expiryLabel = node('label', 'Budget expires (your local time)'), expiry = node('input', null, { type: 'datetime-local', required: '' }); if (slot.expires_at_ms) { const date = new Date(slot.expires_at_ms); expiry.value = new Date(date.getTime() - date.getTimezoneOffset() * 60000).toISOString().slice(0, 16); } expiry.disabled = this.mode !== 'edit'; expiry.oninput = () => update('expires_at_ms', expiry.value ? new Date(expiry.value).getTime() : null); expiryLabel.append(expiry); panel.append(expiryLabel);
    renderCoordinatorApproval(this, panel, slot, update);
    for (const [key, text] of [['required', 'Required for the result'], ['reset_history', 'Start a new conversation for this Agent']]) { const label = node('label', text, { class: 'check' }), checkbox = node('input', null, { type: 'checkbox' }); checkbox.checked = slot[key]; checkbox.disabled = this.mode !== 'edit'; checkbox.onchange = () => update(key, checkbox.checked); label.prepend(checkbox); panel.append(label); }
    if (this.mode === 'edit') { const remove = node('button', 'Remove from future team', { type: 'button' }); remove.onclick = () => { const next = clone(this.draft); next.slots = next.slots.filter(item => item.slot_id !== slot.slot_id); next.dependencies = next.dependencies.filter(edge => edge.parent !== slot.slot_id && edge.child !== slot.slot_id); next.layout = next.layout.filter(item => item.slot_id !== slot.slot_id); this.changed(next); this.renderGraph(); }; panel.append(remove); }
  }
  validate() {
    for (const slot of this.draft.slots) { if (!slot.limits || !['activations','invocations','tokens','cost_microunits'].every(key => Number.isSafeInteger(slot.limits[key]) && slot.limits[key] >= (key === 'cost_microunits' ? 0 : 1)) || !Number.isSafeInteger(slot.expires_at_ms) || slot.expires_at_ms <= Date.now()) { this.selected = slot.slot_id; this.renderPanel(); throw new Error(`Enter all limits and a future expiry for ${slot.name}.`); } if (this.writesModes[slot.slot_id] === 'paths' && !slot.writes?.length) { this.selected = slot.slot_id; this.renderPanel(); throw new Error(`Enter at least one path ${slot.name} may change, or choose Nothing – read-only helper.`); } validateCoordinatorApproval(slot); }
  }
  async preview() {
    if (this.busy) return; try { this.syncGraph(); this.validate(); this.busy = true; this.controls(); this.status('Validating the whole team and exact model configuration…'); const epoch = this.epoch, review = await this.request('/preview', this.draft); if (epoch !== this.epoch) return; this.review = review; const list = this.q('.review'); list.replaceChildren(); for (const change of review.changes) list.append(node('li', `${this.draft.slots.find(slot => slot.slot_id === change.slot_id)?.name || change.slot_id}: ${change.kind}; ${change.history}.`)); for (const slot of this.draft.slots) list.append(node('li', `${slot.name}: ${slot.limits.activations} activations, ${slot.limits.invocations} invocations, ${slot.limits.tokens} tokens, $${slot.limits.cost_microunits / 1e6}; expires ${new Date(slot.expires_at_ms).toLocaleString()}.`)); for (const profile of review.profiles || []) list.append(node('li', `${profile.provider} · ${profile.model}; isolation: ${profile.isolation}; tools: ${profile.tools.join(', ') || 'none'}; may change: ${mayChange(profile.write_scope)}.`)); const checks = review.edit?.required_checks || []; for (const check of checks) list.append(node('li', `Required check after each turn: ${commandText(check)}`)); if (checks.length) list.append(node('li', 'The host runs these checks after the required Agents finish. A failure leaves the turn needing attention; the turn completes only when every check passes and leaves the repository unchanged.')); for (const [id, policy] of review.coordinators || []) { list.append(node('li', `Coordinator ${this.draft.slots.find(slot => slot.slot_id === id)?.name || id}: ${policy.max_nodes} nodes, ${policy.max_edges} connections; operations: ${policy.operations.join(', ')}. Session environment ${policy.resource.environment_generation}; ${policy.resource.backend}, network ${policy.resource.network}; setup ${policy.resource.setup_command || 'none'}.`)); for (const worker of policy.workers) list.append(node('li', `Worker ${worker.template_id}: ${worker.limits.activations} activations, ${worker.limits.invocations} invocations, ${worker.limits.tokens} tokens, $${worker.limits.cost_microunits / 1e6}; ad hoc selection ${worker.adhoc_allowed ? 'allowed' : 'not allowed'}.`)); } list.hidden = false; this.status('Review these changes. Apply authorizes the shown limits for future turns in this Session.'); }
    catch (error) { this.review = null; this.status(error.message, true); } finally { this.busy = false; this.controls(); }
  }
  async apply() {
    if (this.busy || !this.review) return; this.busy = true; this.controls(); const request = { edit: this.review.edit, review_digest: this.review.review_digest }, epoch = this.epoch;
    try { localStorage.setItem(this.storageKey, JSON.stringify(request)); const receipt = await this.request('/apply', request); if (epoch !== this.epoch) return; localStorage.removeItem(this.storageKey); this.review = null; this.status(`Session configuration ${receipt.configuration_revision} applied. The next turn will use this team and its approved limits.`); this.dispatchEvent(new CustomEvent('session-team-applied', { bubbles: true, composed: true, detail: { session_id: this.session, configuration_revision: receipt.configuration_revision } })); this.busy = false; await this.load(); }
    catch (error) { if (error.status >= 400 && error.status < 500) { localStorage.removeItem(this.storageKey); this.review = null; this.status(`${error.message}\nRefresh the Session team and review your changes again.`, true); } else this.status(`${error.message}\nThe exact Apply is retained. Retry Apply to resolve its result.`, true); } finally { this.busy = false; this.controls(); }
  }
  async cancel() { if (this.busy || !this.draft) return; if (localStorage.getItem(this.storageKey)) { this.status('Resolve the pending Apply before starting a different edit. Retry Apply to obtain its saved result.', true); return; } this.busy = true; this.controls(); try { await this.request('/cancel', { command_id: this.draft.command_id }); this.busy = false; await this.load(); this.close(); } catch (error) { this.status(error.message, true); } finally { this.busy = false; this.controls(); } }
}
customElements.define('ax-session-team', AxSessionTeam);
