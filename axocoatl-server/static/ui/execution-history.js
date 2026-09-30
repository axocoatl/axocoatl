// Read-only presentation of versioned history. This module never reconstructs
// checkpoints, control authority, accepted state, or a legacy run from v2 data.
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const id = value => typeof value === 'string' && value.length > 0;
const exactKey = value => JSON.stringify([value?.session_id, value?.turn_id, value?.execution_epoch_id,
  value?.node_id, value?.generation, value?.activation_id]);
function requireValue(condition, message) { if (!condition) throw new Error(message); }

// Optional only when no Stop was recorded. Never interpret malformed evidence
// as absence or use it to fabricate an activation identity.
export function stopIntentProblem(intent, {revision, state, nodes = null, startedNodes = []}) {
  const boundedId = value => id(value) && value.trim() === value && !/[\u0000-\u001f\u007f]/.test(value)
    && new TextEncoder().encode(value).length <= 256;
  if (!object(intent) || !boundedId(intent.command_id) || !boundedId(intent.evidence)
      || !Number.isSafeInteger(intent.requested_revision) || intent.requested_revision < 1
      || !Number.isSafeInteger(revision) || intent.requested_revision > revision
      || !(intent.partial_finish ? ['running', 'needs_attention', 'finished'] : ['running', 'needs_attention', 'cancelled']).includes(state)
      || !Array.isArray(intent.unrun_nodes) || intent.unrun_nodes.length > 128
      || intent.unrun_nodes.some(node => !boundedId(node))
      || new Set(intent.unrun_nodes).size !== intent.unrun_nodes.length) return 'Stop request evidence is malformed.';
  if (intent.partial_finish) {
    const partial = intent.partial_finish;
    const activations = values => Array.isArray(values) && values.length <= 128 && values.every(value => object(value)
      && ['session_id','turn_id','execution_epoch_id','node_id','activation_id'].every(key => boundedId(value[key]))
      && Number.isSafeInteger(value.generation) && value.generation > 0 && (!nodes || nodes.includes(value.node_id)))
      && new Set(values.map(exactKey)).size === values.length;
    const references = values => Array.isArray(values) && values.length <= 128 && values.every(boundedId) && new Set(values).size === values.length;
    if (intent.closure !== 'finished' || !activations(partial.selected_activations) || !activations(partial.stop_activations)
      || !references(partial.missing_conditions) || !references(partial.missing_condition_ids)) return 'Partial Finish evidence is malformed.';
  } else if (intent.closure && intent.closure !== 'cancelled') return 'Stop closure is malformed.';
  if (nodes && intent.unrun_nodes.some(node => !nodes.includes(node))) return 'Stop request names a node outside the retained graph.';
  if (intent.unrun_nodes.some(node => startedNodes.includes(node))) return 'Stop request conflicts with retained started work.';
  return null;
}
const stopLines = view => {
  const intent = view.stop_requested; if (!intent) return [];
  const partial = intent.partial_finish;
  return partial ? ['Partial Finish confirmed',
    ...intent.unrun_nodes.map(node => `${node} · Skipped by partial Finish`),
    ...partial.stop_activations.map(item => `${item.node_id} · generation ${item.generation} · Stop requested for partial Finish`),
    ...partial.missing_condition_ids.map(id => `${id} · Missing or unmet check`),
    ...(partial.selected_activations.length ? partial.selected_activations.map(item => `${item.node_id} · generation ${item.generation} · Selected accepted result`)
      : ['No result selected.'])] : ['Stop requested', ...intent.unrun_nodes.map(node => `${node} · Stopped before starting`)];
};

function exactIdentity(exact, view) {
  return object(exact) && exact.session_id === view.owner.session_id && exact.turn_id === view.turn_id
    && ['execution_epoch_id', 'node_id', 'activation_id'].every(field => id(exact[field]))
    && Number.isSafeInteger(exact.generation) && exact.generation > 0;
}
function resolution(value, label) {
  requireValue(object(value) && ['available', 'missing', 'not_recorded'].includes(value.status), `${label} availability is unsupported.`);
  if (value.status !== 'not_recorded') requireValue(id(value.reference), `${label} reference is missing.`);
}
function outputIdentity(content, exact) {
  return object(content) && exactKey(content.activation) === exactKey(exact)
    && typeof content.text === 'string' && ['partial', 'final'].includes(content.kind);
}
function validateExecutionView(view) {
  requireValue(object(view) && id(view.turn_id) && id(view.owner?.session_id)
    && Number.isSafeInteger(view.revision) && view.revision > 0 && Array.isArray(view.activations)
    && ['running', 'needs_attention', 'completed', 'cancelled', 'finished'].includes(view.state), 'Execution history entry is invalid or unsupported.');
  resolution(view.request, 'Request');
  if (view.request.status === 'available') requireValue(view.request.content?.turn_id === view.turn_id
    && typeof view.request.content.display_input === 'string' && Array.isArray(view.request.content.context), 'Execution request identity or content is invalid.');
  const identities = new Set(); const guidanceCommands = new Set();
  for (const row of view.activations) {
    const exact = row?.activation?.activation;
    requireValue(exactIdentity(exact, view) && !identities.has(exactKey(exact))
      && ['unstarted', 'running', 'accepted', 'failed', 'interrupted', 'superseded'].includes(row.activation.state)
      && typeof row.currently_accepted === 'boolean'
      && (!row.currently_accepted || row.activation.state === 'accepted'), 'Execution activation identity or acceptance is invalid.');
    identities.add(exactKey(exact)); resolution(row.output, 'Output');
    if (row.output.status === 'available') requireValue(outputIdentity(row.output.content, exact)
      && row.output.content.kind === 'final', 'Final output belongs to another activation or evidence kind.');
    requireValue(Array.isArray(row.partial_outputs) && Array.isArray(row.reserved_outputs)
      && (row.stream === undefined || Array.isArray(row.stream)), 'Execution output collections are invalid.');
    for (const partial of row.partial_outputs) requireValue(outputIdentity(partial, exact)
      && partial.kind === 'partial', 'Partial output identity is invalid.');
    for (const reserved of row.reserved_outputs) {
      requireValue(id(reserved?.reference) && outputIdentity(reserved.content?.output, exact)
        && Number.isSafeInteger(reserved.content.original_byte_len)
        && reserved.content.original_byte_len >= new TextEncoder().encode(reserved.content.output.text).length,
      'Reserved output identity or truncation evidence is invalid.');
    }
    requireValue(row.guidance === undefined || Array.isArray(row.guidance), 'Guidance collection is invalid.');
    for (const item of row.guidance || []) {
      const amendment = item?.amendment; const delivery = item?.delivery;
      requireValue(object(amendment) && exactKey(amendment.activation) === exactKey(exact)
        && id(amendment.control_command_id) && id(amendment.instruction) && id(amendment.request)
        && !guidanceCommands.has(amendment.control_command_id), 'Guidance amendment identity is invalid.');
      guidanceCommands.add(amendment.control_command_id);
      resolution(item.instruction, 'Guidance instruction');
      requireValue(item.instruction.status === 'not_recorded' || item.instruction.reference === amendment.instruction,
        'Guidance instruction belongs to another amendment.');
      if (item.instruction.status === 'available') requireValue(typeof item.instruction.content === 'string', 'Guidance instruction text is invalid.');
      requireValue(object(delivery) && ['handoff_recorded', 'delivered', 'unknown'].includes(delivery.status)
        && (delivery.status !== 'delivered' || (Number.isSafeInteger(delivery.receipt_revision) && delivery.receipt_revision > 0))
        && (delivery.status !== 'unknown' || id(delivery.reason)), 'Guidance delivery evidence is invalid or unsupported.');
    }
    let sequence = 0;
    for (const event of row.stream || []) {
      const content = event?.content; const payload = content?.payload;
      requireValue(id(event?.reference) && content?.schema_version === 1 && exactKey(content.activation) === exactKey(exact)
        && content.sequence === sequence++, 'Observed output identity or sequence is invalid.');
      requireValue(object(payload) && ['text', 'reasoning_summary', 'provider_retry', 'tool_proposed', 'tool_result'].includes(payload.kind), 'This output observation is unsupported.');
      if (payload.kind === 'provider_retry') requireValue(typeof payload.reason === 'string', 'Observed provider retry is invalid.');
      else if (['text', 'reasoning_summary'].includes(payload.kind)) requireValue(typeof payload.delta === 'string', 'Observed text is invalid.');
      else {
        const byteField = payload.kind === 'tool_proposed' ? 'arguments_bytes' : 'result_bytes';
        const hashField = payload.kind === 'tool_proposed' ? 'arguments_sha256' : 'result_sha256';
        requireValue(id(payload.name) && id(payload.call_id) && Number.isSafeInteger(payload[byteField])
          && payload[byteField] >= 0 && /^[a-f0-9]{64}$/i.test(payload[hashField])
          && (payload.kind !== 'tool_result' || typeof payload.is_error === 'boolean'), 'Observed tool evidence is invalid.');
      }
    }
  }
  if (Object.hasOwn(view, 'stop_requested')) {
    const problem = stopIntentProblem(view.stop_requested, {revision: view.revision, state: view.state,
      startedNodes: view.activations.filter(row => ['running', 'accepted', 'failed', 'interrupted'].includes(row.activation.state))
        .map(row => row.activation.activation.node_id)});
    requireValue(!problem, problem);
  }
  return view;
}

export function historyPresentation(entry) {
  requireValue(object(entry), 'History entry is invalid.');
  if (Object.hasOwn(entry, 'history_version')) {
    if (entry.history_version === 'legacy_v1') {
      requireValue(object(entry.turn) && id(entry.turn.id) && !Object.hasOwn(entry.turn, 'history_version'), 'Legacy history entry is invalid.');
      return entry.turn;
    }
    if (entry.history_version === 'execution_v2') {
      const view = validateExecutionView(entry.turn);
      return { history_version: 'execution_v2', id: view.turn_id, session_id: view.owner.session_id,
        status: view.state, superseded: view.superseded === true, execution: view };
    }
    throw new Error('This history version is unsupported.');
  }
  // Compatibility for actual old endpoint/fixtures; a present version tag never
  // falls through here. Legacy IDs remain opaque, including whitespace/slashes.
  requireValue(id(entry.id), 'History identity is missing.');
  return entry;
}

function outputRows(row) {
  const values = [];
  if (row.output.status === 'available') values.push({text: row.output.content.text,
    disposition: row.currently_accepted ? 'accepted' : 'output evidence', reference: row.output.reference});
  else if (row.output.status === 'missing') values.push({text: 'Output evidence is unavailable.', disposition: 'missing output', plain: true});
  else values.push({text: 'Final output was not recorded.', disposition: 'no final output', plain: true});
  for (const partial of row.partial_outputs) values.push({text: partial.text, disposition: 'partial output'});
  for (const reserved of row.reserved_outputs) {
    if (row.output.status === 'available' && row.output.reference === reserved.reference) continue;
    const content = reserved.content; const truncated = content.original_byte_len > new TextEncoder().encode(content.output.text).length;
    values.push({text: content.output.text, reference: reserved.reference,
      disposition: `${content.output.kind === 'partial' ? 'partial output' : 'output evidence'}${truncated ? ' · truncated' : ''}`,
      originalByteLength: truncated ? content.original_byte_len : null});
  }
  // A provider retry abandons only the text of its own round: drop what was
  // streamed since the last tool boundary and keep earlier rounds.
  const parts = [];
  let roundStart = 0;
  for (const event of row.stream || []) {
    const payload = event.content.payload;
    if (payload.kind === 'text') parts.push(payload.delta);
    else if (payload.kind === 'provider_retry') parts.length = roundStart;
    else if (payload.kind === 'tool_proposed' || payload.kind === 'tool_result') roundStart = parts.length;
  }
  const text = parts.join('');
  if (text && row.output.status !== 'available') values.push({text, disposition: 'observed output'});
  return values;
}
const labelFor = row => `${row.definition_name?.status === 'available' && typeof row.definition_name.content === 'string' && row.definition_name.content.trim() ? row.definition_name.content : row.activation.activation.node_id} · generation ${row.activation.activation.generation} · ${row.activation.state}`;
const outputLabel = (row, output) => `${labelFor(row)}${row.activation.state === output.disposition ? '' : ` · ${output.disposition}`}`;
const outputText = row => row.text === '' ? `Recorded empty ${row.disposition.includes('partial') ? 'partial output' : 'output'}` : row.text;

// This is presentation only; the retained body remains available for exact audit.
function instructionPresentation(text) {
  try {
    const value = JSON.parse(text);
    if (value?.kind === 'authenticated_control_context_v1' && typeof value.instruction === 'string'
        && object(value.original) && Array.isArray(value.original.references) && Array.isArray(value.original.attachment_ids)
        && Array.isArray(value.references) && Array.isArray(value.attachments)
        && Object.keys(value).sort().join(',') === 'attachments,instruction,kind,original,references') {
      return {text: value.instruction || 'Recorded empty instruction',
        contextNote: `${value.references.length} retained context references · ${value.attachments.length} retained attachments`, retained: text};
    }
  } catch { /* Plain instructions remain literal, including malformed JSON. */ }
  return {text: text === '' ? 'Recorded empty instruction' : text};
}

function guidanceRows(row) {
  return (row.guidance || []).map(item => ({
    commandId: item.amendment.control_command_id, reference: item.amendment.instruction,
    status: item.delivery.status,
    disposition: item.delivery.status === 'delivered' ? 'Input received by the Agent'
      : item.delivery.status === 'handoff_recorded' ? 'Handoff recorded' : 'Delivery unknown',
    note: item.delivery.status === 'delivered' ? 'The Agent acknowledged the input. Follow its output for the result.'
      : item.delivery.status === 'handoff_recorded' ? 'The Agent’s input acknowledgement is not recorded.' : item.delivery.reason,
    ...(item.instruction.status === 'available' ? instructionPresentation(item.instruction.content)
      : {text: item.instruction.status === 'missing' ? `Instruction text is unavailable. Reference: ${item.instruction.reference}` : 'Instruction text was not recorded.'}),
  }));
}

export function executionHistorySummary(view) {
  validateExecutionView(view);
  const request = view.request.status === 'available' ? view.request.content : null;
  return {
    userInput: request?.display_input ?? (view.request.status === 'missing' ? 'Request text is unavailable.' : 'Request text was not recorded.'),
    guidance: view.activations.flatMap(row => guidanceRows(row).map(item => ({...item, label: labelFor(row), activation: row.activation.activation}))),
    output: [...view.activations.flatMap(row => outputRows(row).map(output => `${outputLabel(row, output)}\n${outputText(output)}${output.originalByteLength ? `\n[Truncated recorded evidence; original ${output.originalByteLength} bytes]` : ''}`)), ...stopLines(view)].join('\n\n'),
    context: request?.context || [], createdAt: request?.recorded_at_unix_ms ?? 0,
    model: request?.model ? `${request.model.provider_id} / ${request.model.model_id} (requested)` : '',
  };
}

function inspectableReference(reference, sessionId, turnId) {
  const metadata = reference?.metadata;
  if (!object(metadata) || metadata.source_session_id !== sessionId || !id(reference.reference_id)) return false;
  const sourceId = reference.kind === 'ways_decision' ? metadata.decision_id : metadata.reference_id;
  if (!id(sourceId)) return false;
  // Canonical request capture gives this context its own identity in the receiving
  // turn. The source evidence identity remains in the retained metadata.
  const prefix = `context:${turnId}:`;
  const captured = reference.reference_id.startsWith(prefix)
    && /^(0|[1-9][0-9]*)$/.test(reference.reference_id.slice(prefix.length));
  if (reference.reference_id !== sourceId && !captured) return false;
  if (reference.kind === 'ways_decision') return true;
  return reference.kind === 'coordination_reference' && metadata.history_version === 'execution_v2'
    && ['source_turn_id', 'execution_epoch_id', 'node_id', 'activation_id'].every(field => id(metadata[field]))
    && Number.isSafeInteger(metadata.generation) && metadata.generation > 0 && ['output', 'event'].includes(metadata.type);
}

export function renderExecutionHistoryTurn(view, {renderMarkdown, onGraph, onReference}) {
  const summary = executionHistorySummary(view); const fragment = document.createDocumentFragment();
  const message = (role, text, className = '', exact = null) => {
    const row = document.createElement('div'); row.className = `smsg ${className}`.trim(); row.dataset.turnId = view.turn_id;
    if (exact) { row.dataset.activationId = exact.activation_id; row.dataset.epochId = exact.execution_epoch_id; row.dataset.generation = String(exact.generation); }
    const label = document.createElement('div'); label.className = 'smsg-role'; label.textContent = role;
    const body = document.createElement('div'); body.className = 'smsg-body'; body.textContent = text;
    row.append(label, body); fragment.append(row); return {row, body};
  };
  const user = message('you', summary.userInput, 'user');
  if (summary.context.length) {
    const context = document.createElement('div'); context.className = 'chat-refs';
    for (const reference of summary.context) {
      const uploaded = reference.kind === 'upload' && id(reference.reference_id);
      const inspectable = typeof onReference === 'function' && inspectableReference(reference, view.owner.session_id, view.turn_id);
      const chip = document.createElement(uploaded ? 'a' : inspectable ? 'button' : 'span'); chip.className = 'chat-ref';
      chip.textContent = reference.display_name || reference.reference_id || 'Context';
      if (uploaded) { chip.href = `/api/sessions/${encodeURIComponent(view.owner.session_id)}/attachments/${encodeURIComponent(reference.reference_id)}/content`; chip.target = '_blank'; chip.rel = 'noopener noreferrer'; }
      if (inspectable) { const retained = structuredClone(reference); retained.reference_id = reference.kind === 'ways_decision' ? reference.metadata.decision_id : reference.metadata.reference_id; chip.type = 'button'; chip.setAttribute('aria-label', `Inspect ${chip.textContent}`); chip.addEventListener('click', () => onReference(structuredClone(retained))); }
      context.append(chip);
    }
    user.row.append(context);
  }
  const review = document.createElement('button'); review.type = 'button'; review.className = 'btn ghost sm';
  review.textContent = 'Open Agent graph'; review.addEventListener('click', () => onGraph({sessionId: view.owner.session_id, turnId: view.turn_id})); user.row.append(review);
  if (view.kept_way) {const kept=document.createElement('p');kept.className='small muted';kept.textContent='The selected attempt below was kept in this Session.';user.row.append(kept);}
  for (const activation of view.activations) {
    const exact = activation.activation.activation; const label = labelFor(activation);
    for (const item of guidanceRows(activation)) {
      const {row, body} = message(`${label} · guidance · ${item.disposition}`, item.text, 'execution-guidance', exact);
      row.dataset.commandId = item.commandId; row.dataset.evidenceRef = item.reference; row.dataset.guidanceDelivery = item.status;
      body.style.whiteSpace = 'pre-wrap';
      const note = document.createElement('div'); note.className = 'small muted'; note.textContent = item.note;
      const command = document.createElement('div'); command.className = 'small muted'; command.textContent = `Command ${item.commandId}`;
      row.append(note, command);
      if (item.retained) {
        const details = document.createElement('details'); const label = document.createElement('summary'); label.textContent = item.contextNote;
        const data = document.createElement('pre'); data.textContent = item.retained; data.style.whiteSpace = 'pre-wrap'; details.append(label, data); row.append(details);
      }
    }
    for (const output of outputRows(activation)) {
      const selected = view.kept_way && exact.activation_id === view.kept_way.activation_id && exact.execution_epoch_id === view.kept_way.execution_epoch_id && exact.generation === view.kept_way.generation;
      const {row, body} = message(`${outputLabel(activation, output)}${selected ? ' · kept' : ''}`, '', '', exact);
      if (output.reference) row.dataset.evidenceRef = output.reference;
      if (output.plain || output.text === '') body.textContent = outputText(output);
      else { body.classList.add('prose'); body.innerHTML = renderMarkdown(output.text); }
      if (output.originalByteLength) { const note = document.createElement('div'); note.className = 'small muted'; note.textContent = `Truncated recorded evidence; original ${output.originalByteLength} bytes.`; row.append(note); }
    }
    if (activation.failure && typeof activation.failure.explanation === 'string') {
      const steps = {
        continue: 'Continue this activation from the turn controls; it retries the same input.',
        finish_partial: 'Finish the partial result, or approve a larger budget and Continue.',
        review_then_finish: 'Review the listed file changes, then Finish the partial result.',
        inspect: 'Inspect the recorded evidence before deciding.',
      };
      const why = document.createElement('p'); why.className = 'small activation-failure';
      why.dataset.failureClass = activation.failure.class;
      why.textContent = `${label} stopped: ${activation.failure.explanation} Suggested next step: ${steps[activation.failure.next_step] || steps.inspect}`;
      fragment.append(why);
    }
    const observations = (activation.stream || []).filter(event => event.content.payload.kind !== 'text');
    if (observations.length) {
      const details = document.createElement('details'); details.className = 'chat-reasoning';
      details.dataset.turnId = view.turn_id; details.dataset.activationId = exact.activation_id; details.dataset.epochId = exact.execution_epoch_id;
      const heading = document.createElement('summary'); heading.textContent = `${label} · observed route`; details.append(heading);
      for (const event of observations) {
        const payload = event.content.payload; const item = document.createElement('pre'); item.dataset.evidenceRef = event.reference;
        if (payload.kind === 'reasoning_summary') item.textContent = payload.delta;
        else if (payload.kind === 'provider_retry') item.textContent = `Provider response ended early; the Agent retried once. ${payload.reason}`;
        else if (payload.kind === 'tool_proposed') item.textContent = `Proposed ${payload.name} · call ${payload.call_id}\nInput: ${payload.arguments_bytes} bytes · SHA-256 ${payload.arguments_sha256}`;
        else item.textContent = `Observed ${payload.name} result · call ${payload.call_id} · ${payload.is_error ? 'error' : 'returned'}\n${payload.result_bytes} bytes · SHA-256 ${payload.result_sha256}`;
        details.append(item);
      }
      fragment.append(details);
    }
  }
  for (const text of stopLines(view)) {
    const line = document.createElement('p'); line.className = 'small muted turn-stop-evidence'; line.dataset.turnId = view.turn_id; line.textContent = text; fragment.append(line);
  }
  const state = document.createElement('div'); state.className = 'small muted'; state.dataset.turnId = view.turn_id;
  state.textContent = `Turn: ${view.state.replaceAll('_', ' ')}`; fragment.append(state); return fragment;
}

// Frames only invalidate retained history. Coalesce reads per actual Session
// object, never abort a slower read on every token, and queue one reread if a
// frame arrived while it was in flight. Reconnect still uses canonical GET.
export function createExecutionHistoryInvalidator({getSession, reload, schedule = (fn, delay) => setTimeout(fn, delay)}) {
  const pending = new WeakMap();
  function queue(session, state) {
    if (state.scheduled || state.running) return;
    state.scheduled = true;
    schedule(async () => {
      state.scheduled = false;
      if (getSession() !== session) { pending.delete(session); return; }
      state.dirty = false; state.running = true;
      try { await reload(session.id, {preserveLive: true}); }
      finally { state.running = false; if (state.dirty && getSession() === session) queue(session, state); }
    }, 100);
  }
  const observe = frame => {
    const session = getSession(); const event = frame?.event?.content;
    const control = frame?.kind === 'activation-control-changed';
    const exact = control ? frame.activation : event?.activation;
    const validEvent = control ? id(frame.blocker_id) && id(frame.canonical_command_id)
      && Number.isSafeInteger(frame.turn_revision) && frame.turn_revision > 0
      : event?.schema_version === 1 && id(frame?.event?.reference) && Number.isSafeInteger(event.sequence) && event.sequence >= 0;
    if (!session || !validEvent || exact?.session_id !== session.id || !id(exact?.turn_id) || !id(exact?.activation_id)
        || !id(exact?.execution_epoch_id) || !id(exact?.node_id)
        || !Number.isSafeInteger(exact.generation) || exact.generation < 1) return;
    let state = pending.get(session);
    if (!state) { state = {scheduled: false, running: false, dirty: false}; pending.set(session, state); }
    state.dirty = true; queue(session, state); return exact.turn_id;
  };
  // Durable Ways results arrive after activation settlement. Token streams can
  // finish before acceptance, so their last read cannot certify final history.
  observe.attemptResults = results => {
    const session = getSession(); const set = results?.attempt_set;
    if (!session || set?.session_id !== session.id || !id(set.id) || !Array.isArray(set.lanes)
        || !Array.isArray(results.lane_states)) return;
    const terminal = results.lane_states.filter(item => Number.isInteger(item?.index)
      && set.lanes.some(lane => lane.index === item.index)
      && ['completed', 'failed', 'interrupted', 'cancelled'].includes(item.state))
      .map(item => [item.index, item.state]).sort((a,b) => a[0] - b[0]);
    if (!terminal.length) return;
    const signature = JSON.stringify([set.id, terminal]);
    let state = pending.get(session);
    if (!state) { state = {scheduled: false, running: false, dirty: false}; pending.set(session, state); }
    if (state.attemptSignature === signature) return;
    state.attemptSignature = signature;
    state.dirty = true; queue(session, state);
  };
  return observe;
}
