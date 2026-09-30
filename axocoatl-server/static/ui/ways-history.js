import { adopt } from './sheets.js';

const fields = [
  ['field_bytes', 'Each review detail (KiB)', 1024],
  ['record_bytes', 'Each decision (MiB)', 1024 * 1024],
  ['aggregate_bytes', 'Total retained storage (MiB)', 1024 * 1024],
  ['records', 'Total decisions, including deleted records', 1],
  ['candidates', 'Candidates in each decision', 1],
  ['items_per_field', 'Paths, tools, and cleanup details per list', 1],
];
const css = `
:host {color:var(--text);font:var(--fs-body)/var(--lh-body) var(--font-sans)}
*{box-sizing:border-box} dialog{color:var(--text);background:var(--panel);border:1px solid var(--border);border-radius:var(--r-lg);width:min(1000px,96vw);max-height:92vh;padding:0}
dialog::backdrop{background:#0009} header,.toolbar{display:flex;gap:var(--sp-2);align-items:center;padding:var(--sp-3);border-bottom:1px solid var(--border);flex-wrap:wrap}
h2{font-size:var(--fs-lg);margin:0;flex:1} h3,h4{margin:0 0 var(--sp-2)} main{padding:var(--sp-3);overflow:auto;max-height:70vh}.status{padding:var(--sp-2) var(--sp-3);color:var(--muted)}
button,input{font:inherit;color:inherit;border:1px solid var(--border);border-radius:var(--r-sm);padding:var(--sp-2);background:var(--bg-2)}button{cursor:pointer}button:disabled{opacity:.5;cursor:default}button:focus-visible,input:focus-visible,summary:focus-visible{outline:2px solid var(--accent);outline-offset:2px}
input[type=search]{flex:1;min-width:180px}details{border:1px solid var(--border);border-radius:var(--r-md);padding:var(--sp-3);margin-bottom:var(--sp-3)}summary{cursor:pointer;font-weight:var(--fw-medium)}
pre{white-space:pre-wrap;overflow-wrap:anywhere;background:var(--bg);padding:var(--sp-2);max-height:340px;overflow:auto}p{overflow-wrap:anywhere}small{display:block;color:var(--muted)}.actions{display:flex;gap:var(--sp-2);flex-wrap:wrap;margin:var(--sp-2) 0}.error{color:var(--err)}
form{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:var(--sp-3)}label{display:grid;gap:var(--sp-1)}.wide{grid-column:1/-1}.muted{color:var(--muted)}
@media(max-width:600px){form{grid-template-columns:1fr}dialog{width:98vw}main{max-height:68vh}}
`;
function node(tag, text, className) { const value = document.createElement(tag); if (text !== undefined) value.textContent = text; if (className) value.className = className; return value; }
function button(text, action) { const value = node('button', text); value.type = 'button'; value.addEventListener('click', action); return value; }
function textEvidence(parent, title, value) {
  const section = node('section'); section.append(node('h4', title));
  if (!value || value.state === 'unavailable') section.append(node('p', value?.detail || 'Not recorded', 'muted'));
  else { section.append(node('pre', value.text || '')); if (value.state === 'truncated') section.append(node('small', `Truncated: ${new TextEncoder().encode(value.text).length} of ${value.original_bytes} bytes retained, starting at byte ${value.offset_bytes}.`)); }
  parent.append(section);
}
function usageText(usage) {
  const tokens = usage.tokens.usage || usage.tokens.known_subtotal || {};
  return `${tokens.input_tokens || 0} input / ${tokens.output_tokens || 0} output tokens${usage.tokens.kind === 'unknown' ? ' (known subtotal; total unknown)' : ''}. Cost: $${usage.cost_usd_known_subtotal}${usage.cost_complete ? '' : ' known subtotal; total unknown'}.`;
}
function saveJson(name, data) { const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' })); const link = node('a'); link.href = url; link.download = name; link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000); }
export class AxWaysHistory extends HTMLElement {
  constructor() {
    super(); this.root = this.attachShadow({ mode: 'open' });
    this.root.innerHTML = `<dialog aria-labelledby="ways-history-title"><header><h2 id="ways-history-title">Ways decisions</h2><button type="button" class="close" aria-label="Close Ways decisions">Close</button></header><div class="toolbar"><input type="search" aria-label="Search retained decisions" placeholder="Search task, outcome, route, or files"><button type="button" class="refresh">Refresh</button><button type="button" class="storage">Storage</button></div><div class="status" role="status" aria-live="polite"></div><main></main></dialog>`;
    adopt(this.root, css, []);
    this.dialog = this.root.querySelector('dialog'); this.main = this.root.querySelector('main');
    this.root.querySelector('.close').onclick = () => this.dialog.close();
    this.root.querySelector('.refresh').onclick = () => this.load();
    this.root.querySelector('.storage').onclick = () => this.renderConfiguration();
    this.root.querySelector('input[type=search]').oninput = () => this.renderRecords();
  }
  async show({ sessionId, decisionId, configure = false }) {
    this.sessionId = sessionId; this.selected = decisionId; this.data = null;
    if (!this.dialog.open) this.dialog.showModal();
    await this.load(); if (configure && this.data && !this.data.limits) this.renderConfiguration();
  }
  async request(suffix = '', options = {}) {
    const response = await fetch(`/api/sessions/${encodeURIComponent(this.sessionId)}/ways-history${suffix}`, options);
    if (!response.ok) { let message; try { const body = await response.json(); message = body.error || body.message; } catch {} throw new Error(message || `Request failed (${response.status})`); }
    return response.status === 204 ? null : response.json();
  }
  status(text, error = false) { const target = this.root.querySelector('.status'); target.textContent = text; target.classList.toggle('error', error); }
  async load() {
    const generation = this.generation = (this.generation || 0) + 1; this.status('Loading retained decisions…');
    try { const data = await this.request(); if (generation !== this.generation) return; this.data = data; if (data.supports_retention === false) {this.main.replaceChildren(node('p','Retained Ways decisions require the upgraded Session history format.'));this.status('Legacy history remains available in Session History.');return;} this.status('Retained evidence stays available after attempt cleanup.'); if (data.limits) this.renderRecords(); else this.renderConfiguration(); }
    catch (error) { if (generation === this.generation) { this.main.replaceChildren(); this.status(error.message, true); } }
  }
  renderConfiguration() {
    this.main.replaceChildren(); const form = node('form');
    const explanation = node('p', 'Choose how much decision evidence this Session can retain. These limits include protected patches. If storage is full, Axocoatl preserves the current attempts and asks you to free space before cleanup.', 'wide'); form.append(explanation);
    for (const [key, label, factor] of fields) { const control = node('label', label); const input = node('input'); input.name = key; input.type = 'number'; input.min = '1'; input.step = '1'; input.required = true; if (this.data?.limits) input.value = this.data.limits[key] / factor; if (key === 'candidates') input.max = '100'; control.append(input); form.append(control); }
    {
      if (this.data?.limits) form.append(node('p', 'Changing these limits must preserve every retained decision and unfinished cleanup reservation.', 'wide'));
      const save = node('button', 'Approve storage limits'); save.type = 'submit'; save.className = 'wide'; form.append(save);
      form.onsubmit = async event => { event.preventDefault(); if (!form.reportValidity()) return; const limits = { version: 1 }; for (const [key, , factor] of fields) limits[key] = Number(form.elements[key].value) * factor;
        if (Object.values(limits).some(value => !Number.isSafeInteger(value) || value <= 0) || limits.field_bytes > limits.record_bytes || limits.record_bytes > limits.aggregate_bytes) { this.status('Each detail must fit within a decision, and each decision within the total storage limit.', true); return; }
        save.disabled = true; try { this.data = await this.request('/configuration', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ limits }) }); this.status('Storage limits approved. Your unsent request is preserved.'); this.renderRecords(); this.dispatchEvent(new CustomEvent('ways-storage-configured', { bubbles: true, composed: true })); } catch (error) { this.status(error.message, true); save.disabled = false; }
      };
    }
    this.main.append(form);
  }
  renderRecords() {
    if (!this.data) return; this.main.replaceChildren(); const query = this.root.querySelector('input[type=search]').value.toLowerCase().trim();
    const records = this.data.decisions.filter(record => !query || JSON.stringify(record).toLowerCase().includes(query));
    if (!records.length) this.main.append(node('p', query ? 'No retained decision matches this search.' : 'No Ways decision has been closed yet.'));
    for (const record of [...records].reverse()) {
      const card = node('details'); card.open = this.selected === record.decision_id;
      const chosen = record.human_decision.choice; const outcome = chosen.kind === 'keep' ? `Keep attempt ${chosen.patch.candidate.index + 1}` : 'No keep';
      card.append(node('summary', `${outcome} · ${record.application.state.replaceAll('_', ' ')} · ${record.task.text || record.decision_id}`));
      card.append(node('small', `${record.decision_id} · ${new Date(record.human_decision.decided_at_unix_ms).toLocaleString()} · ${record.cleanup.completed_at_unix_ms ? 'Cleanup complete' : 'Cleanup pending'}`));
      const actions = node('div', undefined, 'actions');
      actions.append(button('Open decision graph', () => { this.dispatchEvent(new CustomEvent('open-ways-graph', {bubbles:true, composed:true, detail:{sessionId:this.sessionId,setId:record.set_id,record}})); this.dialog.close(); }));
      actions.append(button('Attach to request', () => { this.dispatchEvent(new CustomEvent('attach-ways-decision', { bubbles: true, composed: true, detail: { reference: { reference_id: record.decision_id, display_name: `Ways decision: ${record.task.text || outcome}`, kind: 'ways_decision', scope: 'this_turn', metadata: { source_session_id: this.sessionId, decision_id: record.decision_id } }, preview: outcome } })); this.dialog.close(); }));
      actions.append(button('Export evidence and patches', async () => { try { saveJson(`${record.decision_id}.json`, await this.request(`/${encodeURIComponent(record.decision_id)}`)); } catch (error) { this.status(error.message, true); } }));
      if (record.cleanup.completed_at_unix_ms) actions.append(button('Delete retained evidence…', () => this.confirmDelete(record, card)));
      else actions.append(button('Retry pending completion', async () => {
        const keep = chosen.kind === 'keep';
        try {
          const response = await fetch(`/api/sessions/${encodeURIComponent(this.sessionId)}/variants/${keep ? 'adopt' : 'discard'}`, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({attempt_set_id:record.set_id,...(keep ? {index:chosen.patch.candidate.index} : {})})});
          if (!response.ok) {const failure=await response.json();throw new Error(failure.error || failure.message || 'Completion is still pending.');}
          await this.load();
        } catch(error) {this.status(error.message,true);}
      }));
      card.append(actions); textEvidence(card, 'Task', record.task);
      for (const candidate of record.candidates) {
        const detail = node('details'); const model = candidate.model.value;
        detail.append(node('summary', `Attempt ${candidate.id.index + 1} · ${model ? `${model.provider_id}/${model.model_id}` : 'Model not recorded'} · ${candidate.terminal}`));
        detail.append(node('p', usageText(candidate.usage)));
        for (const [title, value] of [['Outcome', candidate.outcome], ['Route', candidate.route], ['Diff', candidate.reviewable_diff]]) textEvidence(detail, title, value);
        detail.append(node('p', `Changed paths (${candidate.changed_paths.items.length}/${candidate.changed_paths.original_count} retained): ${candidate.changed_paths.items.join(', ') || 'None'}`));
        for (const check of candidate.checks) { const checkView = node('details'); checkView.append(node('summary', `Check · ${typeof check.outcome === 'string' ? check.outcome : 'verification rejected'}`)); checkView.append(node('p', check.command.state === 'available' ? check.command.value : 'Command not recorded')); checkView.append(node('small', `Exit: ${check.exit_code.value ?? 'unknown'} · Duration: ${check.duration_ms.value ?? 'unknown'} ms`)); textEvidence(checkView, 'Observed output', check.output); detail.append(checkView); }
        card.append(detail);
      }
      for (const usage of record.shared_usage || []) card.append(node('p', `Shared preparation: ${usageText(usage)}`));
      if (record.judge) { textEvidence(card, 'Judge criteria', record.judge.criteria); textEvidence(card, 'Judge result', record.judge.result); card.append(node('p', `Judge: ${usageText(record.judge.usage)}`)); }
      this.main.append(card);
    }
    for (const deleted of this.data.deleted || []) if (!query || deleted.decision_id.includes(query)) this.main.append(node('p', `${deleted.decision_id} · Evidence explicitly deleted; it cannot be reattached.`, 'muted'));
  }
  confirmDelete(record, card) {
    if (card.querySelector('[data-delete-confirmation]')) return; const confirmation = node('div'); confirmation.dataset.deleteConfirmation = '';
    confirmation.append(node('p', 'Delete this retained decision and its unshared protected patch bytes? Previously captured request context remains in its own history. This cannot be undone.'));
    confirmation.append(button('Cancel', () => confirmation.remove()), button('Delete this evidence', async () => { try { await this.request(`/${encodeURIComponent(record.decision_id)}`, { method: 'DELETE' }); await this.load(); } catch (error) { this.status(error.message, true); } })); card.append(confirmation);
  }
}
customElements.define('ax-ways-history', AxWaysHistory);
