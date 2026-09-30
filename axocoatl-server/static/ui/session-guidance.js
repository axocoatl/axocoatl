import { adopt } from './sheets.js';

const CSS = `
:host { display:block; color:var(--text); font:inherit; }
:host([hidden]) { display:none; }
.row { display:flex; align-items:center; flex-wrap:wrap; gap:8px; margin:8px 0; }
label { flex:1; min-width:180px; }
select,button { color:inherit; background:var(--panel); border:1px solid var(--border); border-radius:6px; padding:7px; font:inherit; max-width:100%; }
select { margin-left:8px; } button { cursor:pointer; } button:disabled { cursor:default; opacity:.6; }
.status { white-space:pre-wrap; font-size:12px; color:var(--muted); } .error { color:var(--err); }
.attention { border:1px solid var(--border-strong); border-radius:8px; padding:10px 12px; margin:8px 0; background:var(--panel); }
.attention[hidden] { display:none; } .attention:focus { outline:none; } .attention:focus-visible { outline:2px solid var(--accent-2); outline-offset:2px; }
.attention h3 { margin:0 0 8px; font-size:13px; font-weight:600; }
.choice { margin:0 0 10px; } .choice button { width:100%; text-align:left; }
.choice p { margin:4px 0 0; font-size:12px; color:var(--muted); }
`;

// A request the daemon revalidates is available even when the read could not
// establish a live runtime; the command POST applies or refuses it.
const available = capability => capability?.enabled === true || capability?.requires_revalidation === true;
const listed = items => items.length < 3 ? items.join(' and ') : `${items.slice(0, -1).join(', ')} and ${items.at(-1)}`;

/** What finishing the turn as it is keeps, loses and skips, from its exact review. */
function finishDetail(envelope) {
  const partial = envelope?.turn_controls?.partial_finish;
  if (!partial) return 'Finishing this turn as it is is unavailable here. Open Turn controls.';
  if (!available(partial.capability)) return partial.capability?.reason || 'Finishing this turn is unavailable right now.';
  const nodes = envelope.nodes || [];
  const label = id => nodes.find(node => node.node_id === id)?.label || id;
  const sinks = partial.available_sinks || [];
  const carried = new Set(sinks.map(item => item.activation_id));
  const latest = nodes.map(node => ({node, activation: node.activations.at(-1)})).filter(item => item.activation);
  const unfinished = latest.filter(item => ['failed', 'interrupted', 'running'].includes(item.activation.state)).map(item => label(item.node.node_id));
  const held = latest.filter(item => item.activation.state === 'accepted'
    && !carried.has(item.activation.reference?.activation?.activation_id)).map(item => label(item.node.node_id));
  const parts = [sinks.length
    ? `Closes this turn as Finished, then sends your message as a new request. The accepted ${sinks.length === 1 ? 'answer' : 'answers'} of ${listed(sinks.map(item => label(item.node_id)))} ${sinks.length === 1 ? 'carries' : 'carry'} into its conversation, as after a completed turn.`
    : 'Closes this turn as Finished with no result, then sends your message as a new request. No Agent has an accepted answer in this turn, so the new request starts from the conversation before it.'];
  if (unfinished.length) parts.push(`What ${listed(unfinished)} did in this turn does not carry forward: only accepted answers do.`);
  if (held.length) parts.push(`Only final answers carry, so ${listed(held)} ${held.length === 1 ? 'keeps its' : 'keep their'} conversation from before this turn.`);
  const review = partial.review || {};
  if (review.stop_activations?.length) parts.push(`Running work is stopped: ${listed(review.stop_activations.map(item => label(item.node_id)))}.`);
  if (review.unrun_nodes?.length) parts.push(`Work that never started is skipped: ${listed(review.unrun_nodes.map(label))}.`);
  if (review.missing_conditions?.length) parts.push('Required checks that have not passed stay unmet.');
  return parts.join(' ');
}

/**
 * Exact guidance shares the composer and the graph's command/receipt endpoint.
 * While the turn needs attention, a message is either sent to an Agent with an
 * accepted answer as a Revise that continues the turn, or the turn is finished
 * as it is and the message becomes a new request. The person chooses each time.
 */
export class AxSessionGuidance extends HTMLElement {
  constructor() {
    super(); this.attachShadow({ mode: 'open' });
    this.shadowRoot.innerHTML = `<div class="row"><label><span class="target-label">Guide current work</span><select aria-label="Agent receiving guidance"></select></label><button class="controls" type="button">Turn controls</button><button class="retry" type="button" hidden>Check guidance receipt</button></div>`
      + `<section class="attention" aria-labelledby="attention-title" tabindex="-1" hidden><h3 id="attention-title">This turn needs attention. Where should your message go?</h3>`
      + `<div class="choice"><button class="continue-with-message" type="button">Continue this turn with your message</button><p class="continue-detail"></p></div>`
      + `<div class="choice"><button class="finish-and-send" type="button">Finish this turn as it is and send as a new request</button><p class="finish-detail"></p></div>`
      + `<button class="keep-editing" type="button">Keep editing</button></section><p class="status" role="status"></p>`;
    void adopt(this.shadowRoot, CSS, ['/ui/tokens.css']);
    this.q('select').onchange = () => { this.selected = this.q('select').value; this.changed(); this.renderAttention(); };
    this.q('.controls').onclick = () => this.dispatchEvent(new CustomEvent('open-current-turn-controls', {bubbles:true, composed:true, detail:{sessionId:this.sessionId,turnId:this.turnId}}));
    this.q('.retry').onclick = () => void this.retry();
    const choose = choice => () => this.dispatchEvent(new CustomEvent('attention-choice', {bubbles:true, composed:true, detail:{choice, sessionId:this.sessionId, turnId:this.turnId}}));
    this.q('.continue-with-message').onclick = choose('continue');
    this.q('.finish-and-send').onclick = choose('finish');
    this.q('.keep-editing').onclick = () => this.hideChoice();
    this.sequence = 0; this.selected = ''; this.envelope = null; this.busy = false; this.choiceOpen = false;
  }
  q(selector) { return this.shadowRoot.querySelector(selector); }
  get mode() { return this.envelope?.history_version === 'execution_v2' ? this.envelope.state : ''; }
  get pendingKey() { return `axocoatl:pending-guidance:${this.sessionId}:${this.turnId}`; }
  get selectedActivation() { return this.choices.find(choice => choice.activation.activation_id === this.selected); }
  /** Guide targets while running; while the turn needs attention, the Agents whose accepted answer a Revise can continue. */
  get choices() {
    const attention = this.mode === 'needs_attention';
    const offered = activation => attention ? available(activation.capabilities?.revise) : activation.capabilities?.guide?.enabled === true;
    return (this.envelope?.nodes || []).flatMap(node => node.activations.filter(activation => offered(activation) && activation.reference?.kind === 'exact').map(activation => ({label:node.label || node.node_id, ...activation.reference})));
  }
  get canSend() { return this.mode === 'running' && !!this.selectedActivation && !this.busy && !this.pendingRequest && !this.envelope.stop_requested; }
  setIdentity(sessionId, turnId) {
    if (this.sessionId === sessionId && this.turnId === turnId) return;
    clearTimeout(this.refreshTimer); this.read?.controller.abort(); this.read = null;
    this.sessionId = sessionId; this.turnId = turnId; this.sequence++; this.envelope = null; this.selected = ''; this.everSelected = false; this.pendingRequest = null; this.busy = false; this.hidden = true; this.canonicalRevision = null; this.choiceMode = ''; this.choiceOpen = false; this.renderAttention();
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
    // Guide and Revise targets are different choices; one never carries over to the other.
    if (this.choiceMode !== this.mode) { this.choiceMode = this.mode; this.selected = ''; this.everSelected = false; }
    const attention = this.mode === 'needs_attention';
    const select = this.q('select'), choices = this.choices; select.replaceChildren();
    if (!choices.some(choice => choice.activation.activation_id === this.selected)) this.selected = !this.everSelected && choices.length === 1 ? choices[0].activation.activation_id : '';
    if (this.selected) this.everSelected = true;
    this.q('.target-label').textContent = attention ? 'Agent receiving your message' : 'Guide current work';
    select.setAttribute('aria-label', attention ? 'Agent receiving your message' : 'Agent receiving guidance');
    const empty = document.createElement('option'); empty.value = ''; empty.textContent = choices.length ? 'Choose an Agent' : attention ? 'No Agent has an accepted answer' : 'No Agent is accepting guidance'; select.append(empty);
    for (const choice of choices) { const option = document.createElement('option'); option.value = choice.activation.activation_id; option.textContent = `${choice.label} · generation ${choice.activation.generation}`; select.append(option); }
    select.value = this.selected; select.disabled = this.busy || !['running','needs_attention'].includes(this.mode) || !choices.length;
    this.hidden = !['running','needs_attention'].includes(this.mode) && !this.pendingRequest;
    if (!this.busy && !this.pendingRequest) this.status(attention ? 'This turn needs attention. Send a message to continue it or to finish it and start a new request, or open Turn controls.' : choices.length ? 'Your message goes to this exact Agent at its next safe boundary.' : 'Work is starting or has reached a boundary. Open Turn controls to inspect its state.');
    this.renderAttention();
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
  async send(instruction, context) {
    if (!this.canSend || !instruction.trim() || typeof this.commandHandler !== 'function') return false;
    const reference = structuredClone(this.selectedActivation), envelope = this.envelope;
    return this.deliver({kind:'guide', reference, model:{controlPlane:envelope}, instruction, context});
  }
  /** Show the two ways a message can go while this turn needs attention. */
  offerChoice() {
    if (this.mode !== 'needs_attention') return false;
    this.choiceOpen = true; this.renderAttention();
    // Focus the question, not an action: each way out needs its own explicit click.
    this.q('.attention').focus();
    return true;
  }
  hideChoice() { this.choiceOpen = false; this.renderAttention(); }
  renderAttention() {
    const section = this.q('.attention');
    section.hidden = !(this.choiceOpen && this.mode === 'needs_attention');
    if (section.hidden) return;
    const target = this.selectedActivation, choices = this.choices;
    this.q('.continue-with-message').disabled = this.busy || !target;
    this.q('.continue-detail').textContent = !choices.length
      ? 'No Agent in this turn has an accepted answer to revise with your message; an Agent that failed or was interrupted has none. Open Turn controls to Continue its work without a message, or finish this turn below.'
      : !target ? 'Choose the Agent receiving your message above.'
      : `${target.label} revises its accepted answer, with your message as the instruction and that answer as context. Work that depends on it and the required checks run again.`;
    this.q('.finish-and-send').disabled = this.busy || !available(this.envelope?.turn_controls?.partial_finish?.capability);
    this.q('.finish-detail').textContent = finishDetail(this.envelope);
    this.q('.keep-editing').disabled = this.busy;
  }
  /** Continue this turn: Revise the selected Agent with the message. */
  async continueWithMessage(instruction, context) {
    const target = this.selectedActivation;
    if (this.mode !== 'needs_attention' || !target || this.busy || !instruction?.trim() || typeof this.commandHandler !== 'function') return false;
    const state = await this.deliverChoice({kind:'revise', reference:structuredClone(target), model:{controlPlane:this.envelope},
      instruction, includePreviousOutput:true, context}, `Sending your message to ${target.label}…`);
    if (!state) return false;
    this.hideChoice(); this.status(`Your message went to ${target.label}; this turn continues.`);
    return true;
  }
  /**
   * Finish this turn as it is: the exact offered partial-Finish review, with
   * every accepted final answer selected so it carries into the next turn.
   * True only once the turn has closed, so the caller can send a new request.
   */
  async finishAsItIs() {
    const envelope = this.envelope, partial = envelope?.turn_controls?.partial_finish;
    if (this.mode !== 'needs_attention' || this.busy || !available(partial?.capability) || typeof this.commandHandler !== 'function') return false;
    const review = structuredClone(partial.review);
    review.selected_activations = structuredClone(partial.available_sinks || []);
    review.confirmed = true;
    const state = await this.deliverChoice({kind:'finish', partialFinish:review, model:{controlPlane:envelope}}, 'Finishing this turn…');
    if (!state) return false;
    if (!['applied', 'settled'].includes(state)) {
      this.status('Finish was recorded, but this turn has not closed yet. Your message is still in the composer; send it once the turn is finished.');
      return false;
    }
    this.hideChoice(); this.status('This turn is finished. Sending your message as a new request…');
    return true;
  }
  async deliverChoice(input, sending) {
    const {sessionId, turnId} = this;
    this.busy = true; this.changed(); this.renderAttention(); this.status(sending);
    try {
      const record = await this.commandHandler(input);
      if (this.sessionId !== sessionId || this.turnId !== turnId) return '';
      if (!record) throw new Error('The command receipt is unavailable. Open Turn controls to check it.');
      if (record.error) throw new Error(record.error);
      const state = record.receipt?.state;
      if (!['requested', 'accepted', 'applied', 'settled'].includes(state)) {
        throw new Error(record.receipt?.last_transition?.failure?.message || `The request was ${state || 'not recorded'}. Refresh this turn before trying again.`);
      }
      return state;
    } catch (error) {
      if (this.sessionId === sessionId && this.turnId === turnId) this.status(error.message || String(error), true);
      return '';
    } finally {
      if (this.sessionId === sessionId && this.turnId === turnId) { this.busy = false; this.changed(); this.renderAttention(); }
    }
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
