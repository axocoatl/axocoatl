import { adopt } from './sheets.js';
import './session-control-plan.js';

const CSS = `
:host { display:block; color:var(--text); font:inherit; }
:host([hidden]) { display:none; }
.row { display:flex; align-items:center; flex-wrap:wrap; gap:8px; margin:8px 0; }
label { flex:1; min-width:180px; }
select,button { color:inherit; background:var(--panel); border:1px solid var(--border); border-radius:6px; padding:7px; font:inherit; max-width:100%; }
select { margin-left:8px; } button { cursor:pointer; } button:disabled { cursor:default; opacity:.6; }
.status { white-space:pre-wrap; font-size:12px; color:var(--text-muted); } .error { color:var(--danger); }
`;

/** Exact guidance shares the composer and the graph's command/receipt endpoint. */
export class AxSessionGuidance extends HTMLElement {
  constructor() {
    super(); this.attachShadow({ mode: 'open' });
    this.shadowRoot.innerHTML = `<div class="row"><label>Guide current work<select aria-label="Agent receiving guidance"></select></label><button class="plan-control" type="button">Plan a change</button><button class="controls" type="button">Turn controls</button><button class="retry" type="button" hidden>Check guidance receipt</button></div><p class="status" role="status"></p><ax-session-control-plan></ax-session-control-plan>`;
    void adopt(this.shadowRoot, CSS, ['/ui/tokens.css']);
    this.q('select').onchange = () => { this.selected = this.q('select').value; this.changed(); };
    this.q('.controls').onclick = () => this.dispatchEvent(new CustomEvent('open-current-turn-controls', {bubbles:true, composed:true, detail:{sessionId:this.sessionId,turnId:this.turnId}}));
    this.q('.retry').onclick = () => void this.retry();
    this.q('.plan-control').onclick = () => this.dispatchEvent(new CustomEvent('plan-current-work', {bubbles:true,composed:true}));
    this.sequence = 0; this.selected = ''; this.envelope = null; this.busy = false;
  }
  q(selector) { return this.shadowRoot.querySelector(selector); }
  get mode() { return this.envelope?.history_version === 'execution_v2' ? this.envelope.state : ''; }
  get pendingKey() { return `axocoatl:pending-guidance:${this.sessionId}:${this.turnId}`; }
  get selectedActivation() { return this.choices.find(choice => choice.activation.activation_id === this.selected); }
  get choices() {
    return (this.envelope?.nodes || []).flatMap(node => node.activations.filter(activation => activation.capabilities?.guide?.enabled === true && activation.reference?.kind === 'exact').map(activation => ({label:node.label || node.node_id, ...activation.reference})));
  }
  get canSend() { return this.mode === 'running' && !!this.selectedActivation && !this.busy && !this.pendingRequest && !this.envelope.stop_requested; }
  setIdentity(sessionId, turnId) {
    if (this.sessionId === sessionId && this.turnId === turnId) return;
    clearTimeout(this.refreshTimer); this.read?.controller.abort(); this.read = null;
    this.q('ax-session-control-plan').setIdentity(sessionId, turnId);
    this.sessionId = sessionId; this.turnId = turnId; this.sequence++; this.envelope = null; this.selected = ''; this.everSelected = false; this.pendingRequest = null; this.busy = false; this.hidden = true; this.canonicalRevision = null;
    try { const saved = JSON.parse(sessionStorage.getItem(this.pendingKey) || 'null'); if (saved?.session_id === sessionId && saved.turn_id === turnId && saved.action === 'guide') this.pendingRequest = saved; } catch {}
    this.q('.retry').hidden = !this.pendingRequest;
    if (this.pendingRequest) this.status('A guidance reply was not received. Check its saved receipt before sending another message.');
    if (sessionId && turnId) void this.refresh();
  }
  observeRetainedTurn(turn) {
    if (turn?.owner?.session_id !== this.sessionId || turn.turn_id !== this.turnId
        || !Number.isSafeInteger(turn.revision) || turn.revision === this.canonicalRevision
        || !['needs_attention', 'completed', 'cancelled', 'finished'].includes(turn.state)) return;
    this.canonicalRevision = turn.revision;
    // The final stream frame can precede canonical acceptance. A settled
    // transcript must refresh the exact capabilities even with no later frame.
    if (this.mode !== turn.state) void this.refresh();
  }
  acceptEnvelope(envelope) {
    if (envelope?.session_id !== this.sessionId || envelope.turn_id !== this.turnId || envelope.schema_version !== 1 || !Array.isArray(envelope.nodes)) return;
    this.envelope = envelope;
    this.q('ax-session-control-plan').observe(envelope);
    const select = this.q('select'), choices = this.choices; select.replaceChildren();
    if (!choices.some(choice => choice.activation.activation_id === this.selected)) this.selected = !this.everSelected && choices.length === 1 ? choices[0].activation.activation_id : '';
    if (this.selected) this.everSelected = true;
    const empty = document.createElement('option'); empty.value = ''; empty.textContent = choices.length ? 'Choose an Agent' : 'No Agent is accepting guidance'; select.append(empty);
    for (const choice of choices) { const option = document.createElement('option'); option.value = choice.activation.activation_id; option.textContent = `${choice.label} · generation ${choice.activation.generation}`; select.append(option); }
    select.value = this.selected; select.disabled = this.busy || this.mode !== 'running' || !choices.length;
    this.hidden = !['running','needs_attention'].includes(this.mode) && !this.pendingRequest && !this.q('ax-session-control-plan').hasPending;
    if (!this.busy && !this.pendingRequest) this.status(this.mode === 'needs_attention' ? 'This turn needs attention. Open Turn controls to inspect blockers and choose Continue or Finish.' : choices.length ? 'Your message goes to this exact Agent at its next safe boundary.' : 'Work is starting or has reached a boundary. Open Turn controls to inspect its state.');
    this.changed();
  }
  refresh() {
    clearTimeout(this.refreshTimer);
    if (!this.sessionId || !this.turnId) return Promise.resolve();
    if (this.read) { this.read.dirty = true; return this.read.promise; }
    const sequence = ++this.sequence, {sessionId,turnId} = this;
    const read = {controller:new AbortController(), dirty:false}; this.read = read;
    read.promise = (async () => {
      try {
        const response = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/turns/${encodeURIComponent(turnId)}/control-plane`, {signal:read.controller.signal});
        if (!response.ok) throw new Error(`Current work could not be read (${response.status}).`);
        const envelope = await response.json(); if (sequence === this.sequence) this.acceptEnvelope(envelope);
      } catch (error) { if (error.name !== 'AbortError' && sequence === this.sequence) { this.envelope = null; this.status(error.message, true); this.changed(); } }
      finally {
        if (this.read === read) {
          this.read = null;
          // Stream invalidations may outpace a retained read. Keep one trailing
          // read, never a queue of one request per token or history repaint.
          if (read.dirty) this.refreshTimer = setTimeout(() => { if (this.sessionId === sessionId && this.turnId === turnId) void this.refresh(); }, 100);
        }
      }
    })();
    return read.promise;
  }
  changed() { this.dispatchEvent(new CustomEvent('guidance-state-change', {bubbles:true,composed:true})); }
  status(text, error=false) { this.q('.status').textContent = text; this.q('.status').classList.toggle('error', error); }
  openPlanner(instruction, context) { const planner=this.q('ax-session-control-plan');planner.commandHandler=this.commandHandler;if(this.envelope)void planner.open(instruction,this.envelope,context,this.selectedActivation?.activation); }
  async send(instruction, context) {
    if (!this.canSend || !instruction.trim() || typeof this.commandHandler !== 'function') return false;
    const reference = structuredClone(this.selectedActivation), envelope = this.envelope;
    return this.deliver({kind:'guide', reference, model:{controlPlane:envelope}, instruction, context});
  }
  async retry() { if (this.pendingRequest && !this.busy) await this.deliver({request:this.pendingRequest}); }
  async deliver(input) {
    const sessionId = this.sessionId, turnId = this.turnId;
    this.busy = true; this.changed(); this.status('Sending guidance…');
    try {
      const record = await this.commandHandler(input);
      if (this.sessionId !== sessionId || this.turnId !== turnId) return false;
      if (!record) throw new Error('The command receipt is unavailable.');
      if (record.error) {
        if (record.errorStatus >= 400 && record.errorStatus < 500) {
          this.pendingRequest = null; sessionStorage.removeItem(this.pendingKey); this.q('.retry').hidden = true;
          this.status(record.error, true); return false;
        }
        this.pendingRequest = record.request; sessionStorage.setItem(this.pendingKey, JSON.stringify(record.request)); this.q('.retry').hidden = false;
        this.status(`${record.error}\nCheck the saved guidance receipt before sending different guidance.`, true); return false;
      }
      this.pendingRequest = null; sessionStorage.removeItem(this.pendingKey); this.q('.retry').hidden = true;
      const state = record.receipt?.state;
      if (['rejected','failed'].includes(state)) { this.status(`Guidance ${state}. Refresh the current Agent state before trying again.`, true); return false; }
      this.status(state === 'settled' ? 'Guidance received by the Agent.' : `Guidance ${state || 'recorded'}. Its durable receipt is available in Turn controls.`);
      return ['requested','accepted','applied','settled'].includes(state);
    } catch (error) { this.status(error.message, true); return false; }
    finally { if (this.sessionId === sessionId && this.turnId === turnId) { this.busy = false; this.changed(); } }
  }
}
customElements.define('ax-session-guidance', AxSessionGuidance);
